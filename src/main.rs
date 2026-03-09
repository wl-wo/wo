//! Wo Wayland compositor

mod backend;
mod config;
mod dmabuf;
mod electron;
mod handlers;
mod nested;

use ::input::Libinput;
use anyhow::Context;
use calloop::EventLoop;
use gbm::Modifier;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use smithay::backend::allocator::{Fourcc, Slot};

use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::wayland::xdg_activation::{XdgActivationToken, XdgActivationTokenData};
use smithay::{
    backend::{
        allocator::{
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Swapchain,
        },
        drm::{
            gbm::framebuffer_from_bo, DrmDevice, DrmDeviceFd, DrmNode, PlaneConfig,
            PlaneState,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{gles::GlesRenderer, Bind, Color32F, Frame, ImportDma, Renderer},
        session::{Session, libseat::LibSeatSession},
        udev::primary_gpu,
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        drm::control::{connector, Device as _, ModeTypeFlags},
        rustix::fs::OFlags,
        wayland_server::{Display, ListeningSocket},
    },
    utils::{Buffer, DeviceFd, Physical, Rectangle, Transform},
    wayland::{
        xdg_activation::XdgActivationHandler,
    },
};
use std::path::Path;
use std::{
    io::Write,
    time::{Duration, Instant},
};
use tracing::{error, info, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub mod state;
pub mod syscall;

use crate::{
    config::Config,
    dmabuf::import_electron_frame,
    electron::{ElectronIpc, ElectronProcess},
    state::{BackendData, WoState},
};

struct EventLoopData {
    display: Display<WoState>,
    state: WoState,
    msg_rx: tokio::sync::mpsc::Receiver<crate::electron::ElectronMessage>,
    swapchain: Swapchain<GbmAllocator<DrmDeviceFd>>,
    drm_device_fd: DrmDeviceFd,
    drm: DrmDevice,
    drm_surface: smithay::backend::drm::DrmSurface,
    output_rect: Rectangle<i32, Physical>,
    src_rect: Rectangle<f64, Buffer>,
    bg: Color32F,
    width: u32,
    height: u32,
    frame_time: Duration,
    last_render: std::time::Instant,
    autostart_tasks: Vec<(crate::config::AutostartConfig, std::time::Instant)>,
    first_render: bool,
    wayland_socket: ListeningSocket,
    frame_timestamps: std::collections::HashMap<String, Instant>,
    gles_tex_cache: std::collections::HashMap<String, smithay::backend::renderer::gles::GlesTexture>,
    input_events_pending: bool,
    session: smithay::backend::session::libseat::LibSeatSession,
    active: bool,
    vblank_pending: bool,
    /// When vblank_pending was first set; used to detect stuck vblank state.
    vblank_since: Option<Instant>,
    consecutive_flip_failures: u32,
    /// DRI render node path for spawning Electron processes (e.g. `/dev/dri/renderD128`).
    electron_render_node: String,
    /// Total successful page flips (for startup diagnostics).
    total_flips: u64,
    /// Hold the buffer the CRTC is currently scanning out so its GBM BO is
    /// not recycled. Promoted from `pending_slot` on VBlank.
    current_slot: Option<Slot<smithay::backend::allocator::gbm::GbmBuffer>>,
    /// Hold the buffer submitted for the next page flip, waiting for VBlank
    /// confirmation. Without this, the CRTC's active scanout buffer could be
    /// freed prematurely, causing every-other-frame flicker.
    pending_slot: Option<Slot<smithay::backend::allocator::gbm::GbmBuffer>>,
}

const MAX_CONSECUTIVE_FLIP_FAILURES: u32 = 5;

fn write_flip_failure_report(report: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/wo-compositor-fatal.log")
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {report}");
    }
}

fn is_permission_denied_message(msg: &str) -> bool {
    let lowered = msg.to_ascii_lowercase();
    lowered.contains("permission denied") || lowered.contains("eacces") || lowered.contains("eperm")
}

fn is_eagain_message(msg: &str) -> bool {
    let lowered = msg.to_ascii_lowercase();
    lowered.contains("resource temporarily unavailable")
        || lowered.contains("eagain")
        || lowered.contains("wouldblock")
}

/// Transient errors from `session.open()` that should be retried with event
/// loop pumping:
///
/// • **EAGAIN** – libseat's seat manager hasn't finished processing a prior
///   request.  Well-documented; all compositors retry.
///
/// • **EACCES / EPERM** – On logind-managed sessions (common with Ly, greetd,
///   and similar display managers) the session activation inside logind may
///   lag behind libseat's own `is_active()` flag.  logind's `TakeDevice`
///   D-Bus method returns EACCES until it has fully activated the session on
///   the foreground VT.  Pumping the event loop lets the remaining activation
///   messages propagate, after which the retry succeeds.
fn is_transient_open_error(msg: &str) -> bool {
    is_eagain_message(msg) || is_permission_denied_message(msg)
}

fn enumerate_drm_card_paths() -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("card"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    paths
}

/// Resolve the DRI render node (e.g. `/dev/dri/renderD128`) that belongs to
/// the same physical GPU as `card_path` (e.g. `/dev/dri/card1`).
///
/// The lookup uses sysfs: `/sys/class/drm/card<N>/device/drm/renderD*`.
/// Falls back to `/dev/dri/renderD128` if the mapping cannot be determined.
fn render_node_for_card(card_path: &std::path::Path) -> String {
    let card_name = card_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("card0");
    let sys_dir = format!("/sys/class/drm/{card_name}/device/drm");
    if let Ok(entries) = std::fs::read_dir(&sys_dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if let Some(s) = name.to_str() {
                if s.starts_with("renderD") {
                    let path = format!("/dev/dri/{s}");
                    info!(card = %card_path.display(), render_node = %path, "resolved render node");
                    return path;
                }
            }
        }
    }
    let fallback = "/dev/dri/renderD128".to_string();
    warn!(
        card = %card_path.display(),
        "could not resolve render node via sysfs, falling back to {fallback}"
    );
    fallback
}

fn is_egl_context_or_surface_lost(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    lowered.contains("context has been lost")
        || lowered.contains("egl_bad_surface")
        || lowered.contains("bad_surface")
        || lowered.contains("not current draw surface")
        || lowered.contains("egl_bad_alloc")
        || lowered.contains("failed to allocate resources")
}

fn reinitialize_renderer(data: &mut EventLoopData) -> bool {
    let gbm_device = match GbmDevice::new(data.drm_device_fd.clone()) {
        Ok(device) => device,
        Err(e) => {
            error!("Failed to recreate GBM device after context loss: {e}");
            return false;
        }
    };

    let egl_display = match unsafe { EGLDisplay::new(gbm_device) } {
        Ok(display) => display,
        Err(e) => {
            error!("Failed to recreate EGL display after context loss: {e}");
            return false;
        }
    };

    let egl_context = match EGLContext::new(&egl_display) {
        Ok(context) => context,
        Err(e) => {
            error!("Failed to recreate EGL context after context loss: {e}");
            return false;
        }
    };

    let new_renderer = match unsafe { GlesRenderer::new(egl_context) } {
        Ok(renderer) => renderer,
        Err(e) => {
            error!("Failed to recreate GLES renderer after context loss: {e}");
            return false;
        }
    };

    let renderer_arc = match data.state.backend.renderer() {
        Some(arc) => arc,
        None => {
            error!("Renderer backend is missing during context-loss recovery");
            return false;
        }
    };

    match renderer_arc.lock() {
        Ok(mut renderer) => {
            *renderer = new_renderer;
        }
        Err(e) => {
            error!("Failed to lock renderer during context-loss recovery: {e}");
            return false;
        }
    }

    data.gles_tex_cache.clear();
    data.state.texture_cache = crate::dmabuf::TextureCache::default();
    data.state.damaged_windows.extend(
        data.state
            .config
            .windows
            .iter()
            .map(|window| window.name.clone()),
    );
    data.first_render = true;
    data.vblank_pending = false;
    info!("Successfully reinitialized renderer after EGL context loss");
    true
}

fn main() -> Result<(), anyhow::Error> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/wo-compositor-debug.log")
        .ok();

    let registry = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "wo=info,smithay=warn".into()))
        .with(tracing_subscriber::fmt::layer());

    if let Some(file) = log_file {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file)),
            )
            .init();
    } else {
        registry.init();
    }

    info!("wo compositor starting");

    let config = Config::load().context("loading config")?;

    // Determine backend priority: nested (gles) is prioritized with DRM fallback
    let force_drm = matches!(config.compositor.nested, Some(false));

    if !force_drm {
        // Try nested mode first (prioritizes gles renderer)
        info!("Attempting nested mode (gles renderer)");
        match nested::run_nested(config.clone()) {
            Ok(_) => return Ok(()),
            Err(e) => {
                info!("Nested mode failed ({}), falling back to DRM/KMS", e);
            }
        }
    }

    // Fall back to DRM/KMS mode
    info!("Running in DRM/KMS mode");
    run_drm(config)
}

/// Render a single frame - called by VBlank events
fn render_frame(data: &mut EventLoopData) {
    // Bail if session is paused (VT switched away)
    if !data.active {
        trace!("Skipping render: session inactive");
        return;
    }

    // Vblank-pending gate: skip if a page flip is outstanding.
    // Safety net: if vblank_pending has been stuck for >500ms, the VBlank event
    // was likely lost (e.g. during DRM master transitions). Force-clear so
    // rendering can resume.
    if data.vblank_pending {
        if let Some(since) = data.vblank_since {
            if since.elapsed() > Duration::from_millis(500) {
                warn!(
                    "vblank_pending stuck for {:?} (total_flips={}), force-clearing",
                    since.elapsed(),
                    data.total_flips,
                );
                data.vblank_pending = false;
                data.vblank_since = None;
                data.current_slot = data.pending_slot.take();
            } else {
                return;
            }
        } else {
            return;
        }
    }

    // Frame pacing: respect adaptive timing for input responsiveness
    let adaptive_frame_time = if data.input_events_pending {
        Duration::from_millis(8)
    } else {
        data.frame_time
    };
    
    if !data.first_render && data.last_render.elapsed() < adaptive_frame_time {
        return;
    }
    data.last_render = std::time::Instant::now();
    data.input_events_pending = false;

    // First render: mark all windows damaged and log diagnostic state
    if data.first_render {
        info!(
            "First render: output={}x{}, windows={}, bg={:?}",
            data.width, data.height,
            data.state.config.windows.len(),
            data.bg,
        );
        for win_cfg in &data.state.config.windows {
            data.state.damaged_windows.insert(win_cfg.name.clone());
        }
        data.first_render = false;
    }

    // Acquire swapchain slot
    let slot = match data.swapchain.acquire() {
        Ok(Some(slot)) => slot,
        Ok(None) => {
            if data.total_flips == 0 {
                warn!("Swapchain: no free slots on first frame");
            }
            return;
        }
        Err(e) => {
            warn!("Error acquiring swapchain slot: {e}");
            return;
        }
    };

    let mut target_dmabuf = match slot.export() {
        Ok(dmabuf) => dmabuf,
        Err(e) => {
            warn!("Error exporting swapchain dmabuf: {e}");
            data.swapchain.submitted(&slot);
            return;
        }
    };

    // Perform actual rendering
    let mut context_lost = false;
    let render_ok = (|| {
        let renderer_arc = match data.state.backend.renderer() {
            Some(arc) => arc,
            None => {
                warn!("No renderer available");
                return false;
            }
        };
        let mut renderer_guard = match renderer_arc.lock() {
            Ok(guard) => guard,
            Err(e) => {
                warn!("Failed to lock renderer: {e}");
                return false;
            }
        };

        let renderer: &mut GlesRenderer = &mut renderer_guard;

        // Prepare Wayland surface elements before starting the frame
        use smithay::backend::renderer::element::{surface::render_elements_from_surface_tree, surface::WaylandSurfaceRenderElement, Kind, RenderElement};
        use smithay::utils::Scale as ScaleF;
        
        let scale = ScaleF::from(1.0);
        let mut wayland_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        
        for window in data.state.space.elements() {
            if let Some(location) = data.state.space.element_location(window) {
                let surface = window.toplevel()
                    .map(|t| t.wl_surface().clone())
                    .or_else(|| window.x11_surface().and_then(|x| x.wl_surface().clone()));
                
                if let Some(surface) = surface {
                    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
                        renderer,
                        &surface,
                        (location.x, location.y),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                    
                    for element in elements {
                        wayland_elements.push(element);
                    }
                }
            }
        }

        let mut target = match renderer.bind(&mut target_dmabuf) {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to bind renderer to dmabuf: {e}");
                return false;
            }
        };

        let mut frame = match renderer.render(
            &mut target,
            data.output_rect.size,
            Transform::Normal,
        ) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to start frame: {e}");
                return false;
            }
        };

        if let Err(e) = frame.clear(data.bg, &[data.output_rect]) {
            warn!("Clear failed: {e}");
            return false;
        }

        // Render Electron window textures sorted by z_order (lowest first)
        let mut sorted_windows: Vec<_> = data.state.config.windows.iter().collect();
        sorted_windows.sort_by_key(|w| w.z_order);

        for win_cfg in sorted_windows {
            if let Some(texture) = data.gles_tex_cache.get(&win_cfg.name) {
                let (x, y, _w, _h) = data.state
                    .window_positions
                    .get(&win_cfg.name)
                    .copied()
                    .unwrap_or((win_cfg.x, win_cfg.y, win_cfg.width, win_cfg.height));

                let mapped = data.state.window_mapped.get(&win_cfg.name).copied().unwrap_or(true);
                if !mapped {
                    continue;
                }

                if let Err(e) = frame.render_texture_at(
                    texture,
                    (x, y).into(),
                    1,
                    1.0,
                    Transform::Normal,
                    &[data.output_rect],
                    &[],
                    1.0,
                ) {
                    if is_egl_context_or_surface_lost(&e.to_string()) {
                        error!("EGL context lost while rendering Electron texture");
                        context_lost = true;
                        return false;
                    }
                    warn!("Electron texture rendering failed for {}: {e}", win_cfg.name);
                }
            }
        }

        for element in wayland_elements {
            let src = element.src();
            let dst = element.geometry(scale);
            if let Err(e) = element.draw(&mut frame, src, dst, &[data.output_rect], &[]) {
                if is_egl_context_or_surface_lost(&e.to_string()) {
                    error!("EGL context lost during space rendering");
                    context_lost = true;
                    return false;
                }
                trace!("Element rendering failed: {e}");
            }
        }

        // Send frame callbacks
        let presented_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        for window in data.state.space.elements() {
            window.send_frame(
                &data.state.backend.output,
                presented_at,
                Some(data.frame_time),
                |_, _| Some(data.state.backend.output.clone()),
            );
        }

        let dirty_surfaces: Vec<_> = data.state.dirty_surfaces.drain().collect();
        for surface in dirty_surfaces {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
                &data.state.backend.output,
                presented_at,
                Some(data.frame_time),
                |_, _| Some(data.state.backend.output.clone()),
            );
        }

        // Finalize the GL frame — flushes all rendering commands to the
        // framebuffer.  Without this, the GBM BO may contain stale or
        // incomplete pixel data, causing flicker.
        match frame.finish() {
            Ok(_) => {}
            Err(e) => {
                warn!("Failed to finish frame: {e}");
                if is_egl_context_or_surface_lost(&e.to_string()) {
                    error!("EGL context/surface lost while finishing frame");
                    context_lost = true;
                    return false;
                }
            }
        }

        true
    })();

    if !render_ok {
        if data.total_flips == 0 {
            warn!("Rendering failed on first frame, skipping page flip");
        }
        data.swapchain.submitted(&slot);
        data.state.damaged_windows.clear();
        if context_lost {
            let _ = reinitialize_renderer(data);
        }
        return;
    }

    data.state.damaged_windows.clear();
    data.state.dirty_surfaces.clear();

    // Create framebuffer and schedule page flip
    let fb = match framebuffer_from_bo(&data.drm_device_fd, &slot, false) {
        Ok(fb) => fb,
        Err(e) => {
            warn!("Error creating framebuffer: {e}");
            data.swapchain.submitted(&slot);
            return;
        }
    };

    let plane_state = PlaneState {
        handle: data.drm_surface.plane(),
        config: Some(PlaneConfig {
            src: data.src_rect,
            dst: data.output_rect,
            transform: Transform::Normal,
            alpha: 1.0,
            damage_clips: None,
            fb: *fb.as_ref(),
            fence: None,
        }),
    };

    let tex_count = data.gles_tex_cache.len();

    // Schedule page flip (or full modeset commit for the first frame).
    // smithay's DrmSurface tracks pending state (mode, connectors). When
    // commit_pending() is true (e.g. first frame after surface creation or
    // VT switch), we must use commit() which includes ALLOW_MODESET.
    // page_flip() only does a non-blocking flip without modesetting and will
    // fail with EINVAL if the CRTC doesn't have a mode set yet.
    //
    // IMPORTANT: Even when commit_pending() is false (Ly left the CRTC in a
    // matching mode), the first frame still needs a full modeset commit to
    // ensure CRTC ACTIVE=1 and a valid framebuffer is bound. page_flip alone
    // may "succeed" but produce no output if the CRTC was left without an
    // active framebuffer by the display manager.
    let commit_pending = data.drm_surface.commit_pending();
    let force_initial = data.total_flips == 0;
    let result = if commit_pending || force_initial {
        info!(
            "Performing modeset commit (commit_pending={}, first_frame={}, textures={})",
            commit_pending, force_initial, tex_count
        );
        data.drm_surface.commit([plane_state.clone()], true)
    } else {
        data.drm_surface.page_flip([plane_state.clone()], true)
    };

    match result {
        Ok(_) => {
            data.consecutive_flip_failures = 0;
            data.vblank_pending = true;
            data.vblank_since = Some(Instant::now());
            data.swapchain.submitted(&slot);
            // Hold the slot as pending until VBlank confirms the flip.
            // current_slot (the active scanout buffer) stays alive until then.
            data.pending_slot = Some(slot);
            data.total_flips += 1;
            if data.total_flips == 1 {
                info!("First successful page flip (commit_pending={})", commit_pending);
            }
        }
        Err(e) => {
            data.consecutive_flip_failures = data.consecutive_flip_failures.saturating_add(1);
            let page_flip_err = e.to_string();
            let would_block = {
                let lowered = page_flip_err.to_ascii_lowercase();
                lowered.contains("wouldblock")
                    || lowered.contains("resource temporarily unavailable")
                    || lowered.contains("eagain")
            };

            let page_flip_report = format!(
                "DRM page_flip failed (count={}): {}{}",
                data.consecutive_flip_failures,
                page_flip_err,
                if would_block { " [WouldBlock/EAGAIN]" } else { "" }
            );
            warn!("{page_flip_report}. Attempting forced commit.");
            write_flip_failure_report(&page_flip_report);

            if let Err(commit_err) = data.drm_surface.commit([plane_state], true) {
                error!("Fatal DRM failure: {commit_err}");
                let commit_report = format!(
                    "DRM commit failed after page_flip failure (count={}): {}",
                    data.consecutive_flip_failures,
                    commit_err
                );
                write_flip_failure_report(&commit_report);
                data.vblank_pending = false;
                data.vblank_since = None;
                if data.consecutive_flip_failures >= MAX_CONSECUTIVE_FLIP_FAILURES {
                    let report = format!(
                        "Exceeded consecutive DRM flip failures ({}). Last page_flip error: {e}; last commit error: {commit_err}. Exiting compositor.",
                        data.consecutive_flip_failures
                    );
                    error!("{report}");
                    write_flip_failure_report(&report);
                    data.active = false;
                    data.state.running = false;
                    return;
                }
            } else {
                data.consecutive_flip_failures = 0;
                data.vblank_pending = true;
                data.vblank_since = Some(Instant::now());
                data.swapchain.submitted(&slot);
                data.pending_slot = Some(slot);
                data.total_flips += 1;
            }
        }
    }
}

fn run_drm(mut config: Config) -> Result<(), anyhow::Error> {
    let (session, session_notifier) = LibSeatSession::new().map_err(|err| {
        anyhow::anyhow!(
            "Failed to create libseat session: {err}. wo requires a seat-managed session to access DRM/input devices safely. Run from a proper login session (logind/seatd)."
        )
    })?;
    info!("LibSeat session created successfully");
    info!(
        seat = %session.seat(),
        libseat_backend = %std::env::var("LIBSEAT_BACKEND").unwrap_or_else(|_| "auto (not set)".into()),
        "libseat environment"
    );
    let mut session = session;

    // Create the event loop early—before opening any DRM devices.
    // Niri, wlroots, and KWin all register the session notifier on the event
    // loop before attempting to open device fds. Without this, calloop never
    // polls the libseat fd, seat.dispatch() is never called, the EnableSeat
    // message from seatd/logind stays unread, and libseat_open_device()
    // returns EAGAIN for every attempt.
    let mut event_loop: EventLoop<Option<EventLoopData>> =
        EventLoop::try_new().context("creating event loop")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    loop_handle
        .insert_source(session_notifier, |event, _, data: &mut Option<EventLoopData>| {
            use smithay::backend::session::Event;
            let Some(data) = data.as_mut() else {
                // Pre-init phase: ActivateSession received before DRM setup is
                // complete. The session is now active; device opening can proceed.
                return;
            };
            match event {
                Event::PauseSession => {
                    info!("Session paused (VT switched away). Pausing DRM device and halting rendering.");
                    data.active = false;
                    data.vblank_pending = false;
                    data.vblank_since = None;
                    data.drm.pause();
                }
                Event::ActivateSession => {
                    info!("Session activated (VT switched back). Activating DRM device and resuming rendering.");
                    if let Err(e) = data.drm.activate(false) {
                        error!("Failed to activate DRM device on session resume: {e}");
                        data.active = false;
                        data.vblank_pending = false;
                        data.vblank_since = None;
                        return;
                    }
                    data.active = true;
                    data.vblank_pending = false;
                    data.vblank_since = None;
                    if let Err(e) = data.display.flush_clients() {
                        warn!("Failed to flush Wayland clients on activation: {e}");
                    }
                    render_frame(data);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("Error inserting session notifier: {e:?}"))?;
    info!("Session notifier registered for VT switching");

    // Dispatch the event loop until the seat is active and ready for device
    // requests. This must run even when session.is_active() is already true:
    // LibSeatSession::new() may have set the active flag via an internal
    // dispatch(0), but calloop still needs to poll the libseat fd at least
    // once *after* the notifier is registered before open_device() will
    // succeed. Without this, libseat_open_device() returns EAGAIN immediately.
    if !session.is_active() {
        info!("Waiting for seat activation (EnableSeat from seat manager)...");
    }
    let mut pre_init: Option<EventLoopData> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        event_loop
            .dispatch(
                Some(remaining.min(Duration::from_millis(200))),
                &mut pre_init,
            )
            .context("dispatching event loop for session activation")?;
        if session.is_active() {
            info!("Session is active, ready to open devices");
            break;
        }
    }
    if !session.is_active() {
        return Err(anyhow::anyhow!(
            "Seat session never became active. \
             Run wo from a proper TTY login session managed by logind or seatd. \
             If another compositor holds the seat, switch to a free VT first."
        ));
    }

    // One unconditional drain after the activation break: the dispatch() call
    // that set is_active()=true may have left additional buffered libseat events
    // unread.  A zero-timeout pump clears them so that the first open_device()
    // request sees a clean state and does not get a spurious EAGAIN.
    let _ = event_loop.dispatch(Some(Duration::from_millis(0)), &mut pre_init);

    let seat_name = session.seat();

    info!("Using seat: {}", seat_name);

    let mut display: Display<WoState> = Display::new().context("creating Wayland display")?;
    let dh = display.handle();

    let configured_gpu = config
        .compositor
        .drm_device
        .as_ref()
        .map(std::path::PathBuf::from);
    let detected_primary_gpu = primary_gpu(&seat_name).ok().flatten();

    let mut gpu_candidates = Vec::new();
    if let Some(path) = configured_gpu {
        gpu_candidates.push(path);
    } else {
        if let Some(path) = detected_primary_gpu {
            gpu_candidates.push(path);
        }
        for path in enumerate_drm_card_paths() {
            if !gpu_candidates.iter().any(|p| p == &path) {
                gpu_candidates.push(path);
            }
        }
    }

    if gpu_candidates.is_empty() {
        return Err(anyhow::anyhow!(
            "no DRM card candidates found (checked primary GPU and /dev/dri/card*)"
        ));
    }

    info!(
        candidates = ?gpu_candidates,
        "trying DRM devices in priority order"
    );

    let mut chosen_gpu_path = None;
    let mut chosen_drm_fd = None;
    let mut open_errors: Vec<String> = Vec::new();

    // Open DRM in blocking mode. Some drivers can return EAGAIN for
    // O_NONBLOCK opens even when no other compositor/session owns the device.
    // libseat already provides asynchronous behavior and transient EAGAIN
    // handling through the dispatch/retry loop below.
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY;

    // Round-robin retry across all GPU candidates for transient errors.
    //
    // Both EAGAIN and EACCES/EPERM are retried with event-loop pumping:
    //
    // • EAGAIN: libseat's seat manager hasn't finished processing an earlier
    //   request — a seat-level condition, not GPU-specific.
    //
    // • EACCES/EPERM: On logind-managed sessions (e.g. launched by Ly, greetd)
    //   the session activation inside logind can lag behind libseat's own
    //   is_active() flag.  logind's TakeDevice D-Bus call returns EACCES until
    //   it has fully activated the session on the foreground VT.  Pumping the
    //   event loop lets the remaining activation messages propagate.
    //
    // After each round the loop dispatches the event loop for 50 ms and retries
    // all still-pending candidates.  Budget: MAX_TRANSIENT_ROUNDS × 50 ms ≈ 2 s.
    const MAX_TRANSIENT_ROUNDS: u32 = 40;
    let mut transient_pending: Vec<std::path::PathBuf> = gpu_candidates;
    'outer: for round in 0..=MAX_TRANSIENT_ROUNDS {
        let mut still_transient: Vec<std::path::PathBuf> = Vec::new();
        for gpu_path in transient_pending {
            if round == 0 {
                info!("Opening DRM device: {:?}", gpu_path);
            }
            match session.open(&gpu_path, open_flags) {
                Ok(fd) => {
                    if round > 0 {
                        info!(gpu = %gpu_path.display(), round, "device opened after transient retries");
                    }
                    info!(gpu = %gpu_path.display(), "using GPU");
                    chosen_gpu_path = Some(gpu_path);
                    chosen_drm_fd = Some(fd);
                    break 'outer;
                }
                Err(e) => {
                    let detail = e.to_string();
                    if is_transient_open_error(&detail) {
                        if round == 0 {
                            let kind = if is_eagain_message(&detail) {
                                "EAGAIN"
                            } else {
                                "permission denied (transient on logind sessions)"
                            };
                            info!(
                                gpu = %gpu_path.display(),
                                "libseat returned {kind}; will retry after event loop pump..."
                            );
                        }
                        still_transient.push(gpu_path);
                    } else {
                        warn!(
                            gpu = %gpu_path.display(),
                            round,
                            "Failed to open DRM device (non-transient): {detail}"
                        );
                        open_errors.push(format!("{}: {detail}", gpu_path.display()));
                    }
                }
            }
        }
        transient_pending = still_transient;
        if transient_pending.is_empty() {
            break;
        }
        if round < MAX_TRANSIENT_ROUNDS {
            let _ = event_loop.dispatch(Some(Duration::from_millis(50)), &mut pre_init);
        } else {
            // Final round exhausted; log diagnostics for each remaining candidate.
            for gpu_path in &transient_pending {
                let perms_hint = match std::fs::metadata(gpu_path) {
                    Ok(meta) => {
                        use std::os::unix::fs::MetadataExt;
                        format!("(device node mode={:#o}, uid={}, gid={})",
                                meta.mode(), meta.uid(), meta.gid())
                    }
                    Err(e) => format!("(could not stat device: {e})"),
                };
                open_errors.push(format!(
                    "{}: transient open error persisted after {} retries {}. \
                     If this is EACCES, ensure Ly's PAM config includes pam_systemd.so \
                     and the logind session is active (`loginctl session-status`)",
                    gpu_path.display(),
                    MAX_TRANSIENT_ROUNDS,
                    perms_hint,
                ));
            }
        }
    }

    let gpu_path = chosen_gpu_path.ok_or_else(|| {
        let detail = if open_errors.is_empty() {
            "no candidates produced a detailed error".to_string()
        } else {
            open_errors.join("; ")
        };
        anyhow::anyhow!(
            "no usable DRM device found. Tried: {detail}. Check for stale holders with: fuser /dev/dri/card*"
        )
    })?;
    let drm_fd = chosen_drm_fd.context("no usable DRM device fd found")?;

    let drm_device_fd = DrmDeviceFd::new(DeviceFd::from(drm_fd));

    // DRM master management: DrmDeviceFd::new() already attempts to acquire
    // master internally. On logind-managed sessions (Ly, greetd, etc.) this
    // call typically fails because logind manages master status via TakeDevice.
    // The fd is still usable for modesetting—logind grants master implicitly.
    // Following niri's approach: do NOT explicitly call acquire_master_lock()
    // here. Instead, rely on DrmDevice::pause()/activate() for VT switching,
    // which checks is_privileged() and skips redundant master management on
    // logind sessions.

    // Create the DRM device. The second parameter (`disable_connectors`) is
    // false following niri's approach: preserve the existing connector state
    // instead of resetting it. Resetting (true) requires DRM master and can
    // fail on logind-managed sessions in unprivileged mode.
    //
    // Atomic vs legacy modesetting is auto-detected by smithay based on driver
    // capability. Use SMITHAY_USE_LEGACY=1 to force legacy mode if needed.
    let (mut drm, _drm_notifier) = DrmDevice::new(drm_device_fd.clone(), false)
        .map_err(|e| anyhow::anyhow!("creating DRM device: {e}"))?;
    info!(
        atomic = drm.is_atomic(),
        "DRM device created"
    );

    let gbm_device = GbmDevice::new(drm_device_fd.clone()).context("creating GBM device")?;

    let egl_display =
        unsafe { EGLDisplay::new(gbm_device.clone()) }.context("creating EGL display")?;
    let egl_context = EGLContext::new(&egl_display).context("creating EGL context")?;
    let renderer: GlesRenderer =
        unsafe { GlesRenderer::new(egl_context).context("creating GLES renderer")? };

    let resources = drm.resource_handles().context("DRM resource handles")?;
    let connector_h = resources
        .connectors()
        .iter()
        .find_map(|&h| {
            let info = drm.get_connector(h, false).ok()?;
            if info.state() == connector::State::Connected {
                Some(h)
            } else {
                None
            }
        })
        .context("no connected DRM connector")?;

    let connector_info = drm.get_connector(connector_h, true)?;
    let &mode = connector_info
        .modes()
        .iter()
        .max_by_key(|m| (m.mode_type().contains(ModeTypeFlags::PREFERRED), m.size()))
        .context("no DRM mode")?;

    let crtc_h = connector_info
        .encoders()
        .iter()
        .filter_map(|&encoder_h| drm.get_encoder(encoder_h).ok())
        .find_map(|encoder_info| encoder_info.crtc())
        .or_else(|| {
            connector_info
                .encoders()
                .iter()
                .filter_map(|&encoder_h| drm.get_encoder(encoder_h).ok())
                .flat_map(|encoder_info| resources.filter_crtcs(encoder_info.possible_crtcs()))
                .next()
        })
        .context("no suitable CRTC for connected connector")?;

    let drm_surface = drm
        .create_surface(crtc_h, mode, &[connector_h])
        .context("creating DRM surface")?;

    let (w, h) = mode.size();
    let (width, height) = (w as u32, h as u32);

    let output = Output::new(
        "wo-output".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Wo".into(),
            model: "Virtual".into(),
        },
    );

    let output_mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: mode.vrefresh() as i32 * 1000,
    };

    output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);
    output.create_global::<WoState>(&dh);

    // Get renderer formats before moving renderer into BackendData
    let renderer_formats = renderer.dmabuf_formats();

    let backend = BackendData {
        output: output.clone(),
        size: (width, height),
        renderer: Some(std::sync::Arc::new(std::sync::Mutex::new(renderer))),
        dmabuf_formats: None,
    };
    let render_node =
        DrmNode::from_path(&gpu_path).context("resolving DRM node for dmabuf feedback")?;

    // Merge [[root]] entries into config.windows BEFORE WoState::new so that
    // render_frame, window_mapped, and respawn logic all see them.
    for root_cfg in &config.root.clone() {
        let mut full = root_cfg.clone();
        full.x = 0;
        full.y = 0;
        full.width = width;
        full.height = height;
        full.z_order = std::i32::MIN / 2;
        config.windows.push(full);
    }

    let mut state = WoState::new(&mut display, config.clone(), render_node, backend);
    state.can_switch_vt = true;

    let (ipc, msg_rx) = ElectronIpc::listen(&config.compositor.ipc_socket).context("IPC socket")?;

    state.electron_ipc = Some(ipc);

    std::env::set_var("WAYLAND_DISPLAY", &config.compositor.socket_name);

    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("electron");
    let electron_render_node = render_node_for_card(&gpu_path);

    // All windows (including merged [[root]] entries) are now in config.windows.
    for win_cfg in &config.windows {
        match ElectronProcess::spawn(
            win_cfg,
            &config.compositor.electron_path,
            &app_dir,
            &config.compositor.ipc_socket,
            &electron_render_node,
        ) {
            Ok(proc) => state.electron_processes.push(proc),
            Err(e) => error!(window = %win_cfg.name, "failed to spawn Electron: {e:#}"),
        }
    }

    let mut autostart_tasks: Vec<(crate::config::AutostartConfig, std::time::Instant)> = Vec::new();
    let now = std::time::Instant::now();
    for task in &config.autostart {
        let when = now + std::time::Duration::from_millis(task.delay);
        autostart_tasks.push((task.clone(), when));
    }
    let swapchain_format = if renderer_formats
        .iter()
        .any(|fmt| fmt.code == Fourcc::Xrgb8888)
    {
        Fourcc::Xrgb8888
    } else if renderer_formats
        .iter()
        .any(|fmt| fmt.code == Fourcc::Argb8888)
    {
        Fourcc::Argb8888
    } else {
        renderer_formats
            .iter()
            .next()
            .map(|fmt| fmt.code)
            .unwrap_or(Fourcc::Xrgb8888)
    };

    let mut swapchain_modifiers: Vec<Modifier> = Vec::new();
    for fmt in renderer_formats.iter().filter(|fmt| fmt.code == swapchain_format) {
        if !swapchain_modifiers.contains(&fmt.modifier) {
            swapchain_modifiers.push(fmt.modifier);
        }
    }
    if swapchain_modifiers.is_empty() {
        swapchain_modifiers.push(Modifier::Invalid);
    }

    info!(
        ?swapchain_format,
        modifiers = swapchain_modifiers.len(),
        "using swapchain format/modifiers from renderer"
    );

    let swapchain: Swapchain<GbmAllocator<DrmDeviceFd>> = Swapchain::new(
        GbmAllocator::new(
            gbm_device,
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        ),
        width,
        height,
        swapchain_format,
        swapchain_modifiers,
    );

    let output_rect: Rectangle<i32, Physical> =
        Rectangle::new((0, 0).into(), (width as i32, height as i32).into());
    let src_rect: Rectangle<f64, Buffer> =
        Rectangle::from_size((width as i32, height as i32).into()).to_f64();
    let bg = Color32F::from(config.compositor.background);

    // Initialize libinput for input events (only when session backend is available)
    let mut libinput_context =
        Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput_context
        .udev_assign_seat(&session.seat())
        .map_err(|()| anyhow::anyhow!("Failed to assign seat to libinput"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context);

    // Prepare frame pacing using the active DRM mode refresh when available.
    let refresh_hz = (output_mode.refresh as f64) / 1000.0;
    let frame_time = if refresh_hz > 1.0 {
        Duration::from_secs_f64((1.0 / refresh_hz).clamp(1.0 / 240.0, 1.0 / 30.0))
    } else {
        Duration::from_millis(16)
    };
    let last_render = std::time::Instant::now();

    let wayland_socket =
        ListeningSocket::bind(&config.compositor.socket_name).context("binding Wayland socket")?;
    info!(socket = %config.compositor.socket_name, "Wayland socket bound");

    // Move all state into EventLoopData for access in event loop callbacks
    let mut loop_data = EventLoopData {
        display,
        state,
        msg_rx,
        swapchain,
        drm_device_fd: drm_device_fd.clone(),
        drm,
        drm_surface,
        output_rect,
        src_rect,
        bg,
        width,
        height,
        frame_time,
        last_render,
        autostart_tasks,
        first_render: true,
        wayland_socket,
        frame_timestamps: std::collections::HashMap::new(),
        input_events_pending: false,
        session,
        active: true,
        vblank_pending: false,
        vblank_since: None,
        consecutive_flip_failures: 0,
        gles_tex_cache: std::collections::HashMap::new(),
        electron_render_node: electron_render_node,
        total_flips: 0,
        current_slot: None,
        pending_slot: None,
    };

    let signal_for_handler = loop_signal.clone();
    ctrlc::set_handler(move || {
        info!("Received Ctrl+C, initiating shutdown");
        signal_for_handler.stop();
    })
    .expect("Error setting Ctrl-C handler");

    loop_handle
        .insert_source(_drm_notifier, |event, _, data: &mut Option<EventLoopData>| {
            let data = data.as_mut().expect("EventLoopData initialized");
            use smithay::backend::drm::DrmEvent;
            if !data.active {
                if let DrmEvent::VBlank(_) = event {
                    data.vblank_pending = false;
                    data.vblank_since = None;
                    data.current_slot = data.pending_slot.take();
                }
                trace!("Ignoring DRM event while session is inactive");
                return;
            }
            if let DrmEvent::VBlank(_) = event {
                // The flip is complete: the pending buffer is now being scanned.
                // Promote it to current (releasing the old scanout buffer).
                data.vblank_pending = false;
                data.vblank_since = None;
                data.current_slot = data.pending_slot.take();
                render_frame(data);
            }
        })
        .map_err(|e| anyhow::anyhow!("Error inserting DRM notifier: {e:?}"))?;

    // Insert libinput backend into event loop (operate on `loop_data.state`)
    loop_handle
        .insert_source(
            libinput_backend,
            move |event, _, data: &mut Option<EventLoopData>| {
                let data = data.as_mut().expect("EventLoopData initialized");
                data.state.process_input_event(event);
                data.input_events_pending = true;
            },
        )
        .map_err(|e| anyhow::anyhow!("Error inserting libinput source: {e:?}"))?;

    // Kickstart the render loop with an initial frame
    info!("Kickstarting VBlank-driven rendering");
    render_frame(&mut loop_data);

    // Main event loop - purely event-driven (blocking)
    let mut main_data: Option<EventLoopData> = Some(loop_data);
    event_loop
        .run(None, &mut main_data, |opt_data| {
            let data = opt_data.as_mut().expect("EventLoopData initialized");
            if !data.state.running {
                loop_signal.stop();
                return;
            }

            if let Some(vt) = data.state.pending_vt_switch.take() {
                if let Err(e) = data.session.change_vt(vt as i32) {
                    warn!(vt, "Failed to switch VT: {e}");
                } else {
                    info!(vt, "Switched to VT");
                }
            }

            while let Ok(Some(stream)) = data.wayland_socket.accept() {
                if let Err(e) = data.display.handle().insert_client(
                    stream,
                    std::sync::Arc::new(crate::state::ClientData::default()),
                ) {
                    error!("Failed to insert Wayland client: {e}");
                }
            }
            if let Err(e) = data.display.dispatch_clients(&mut data.state) {
                error!("Failed to dispatch Wayland clients: {e}");
            }
            if let Err(e) = data.display.flush_clients() {
                error!("Failed to flush Wayland clients: {e}");
            }
            data.state.space.refresh();
            if let Err(e) = data.display.flush_clients() {
                error!("Failed to flush after space refresh: {e}");
            }

            // Process IPC messages with input prioritization and frame backpressure
            let mut input_batch = Vec::new();
            let mut frames_batch = Vec::new();
            let mut other_batch = Vec::new();
            
            while let Ok(msg) = data.msg_rx.try_recv() {
                use crate::electron::ElectronMessage;
                match msg {
                    ElectronMessage::ForwardedPointer { .. }
                    | ElectronMessage::ForwardedKeyboard { .. }
                    | ElectronMessage::ForwardedRelativePointer { .. }
                    | ElectronMessage::ForwardedPointerButton { .. }
                    | ElectronMessage::ForwardedPointerScroll { .. } => {
                        input_batch.push(msg);
                    }
                    ElectronMessage::Frame(_) => {
                        frames_batch.push(msg);
                    }
                    _ => {
                        other_batch.push(msg);
                    }
                }
            }

            // Process input events first for low latency
            for msg in input_batch {
                use crate::electron::ElectronMessage;
                match msg {
                    ElectronMessage::ForwardedPointer { window_name, x, y } => {
                        data.state.handle_forwarded_pointer_event(&window_name, x, y);
                    }
                    ElectronMessage::ForwardedKeyboard { window_name, key, pressed, time } => {
                        data.state.handle_forwarded_keyboard_event(&window_name, key, pressed, time);
                    }
                    ElectronMessage::ForwardedRelativePointer { window_name, dx, dy } => {
                        data.state.handle_forwarded_relative_pointer_event(&window_name, dx, dy);
                    }
                    ElectronMessage::ForwardedPointerButton { window_name, x, y, button, pressed, time } => {
                        data.state.handle_forwarded_pointer_button(&window_name, x, y, button, pressed, time);
                    }
                    ElectronMessage::ForwardedPointerScroll { window_name, dx, dy } => {
                        data.state.handle_forwarded_pointer_scroll(&window_name, dx, dy);
                    }
                    _ => {}
                }
            }

            // Process frames with backpressure (8ms throttle per window)
            let now = Instant::now();
            let mut latest_frames = std::collections::HashMap::new();
            for msg in frames_batch {
                if let crate::electron::ElectronMessage::Frame(frame) = msg {
                    let name = frame.name.clone();
                    
                    if let Some(last_ts) = data.frame_timestamps.get(&name) {
                        if now.duration_since(*last_ts) < Duration::from_millis(8) {
                            trace!("Dropping frame {} due to backpressure", name);
                            continue;
                        }
                    }
                    
                    data.frame_timestamps.insert(name.clone(), now);
                    latest_frames.insert(name, frame);
                }
            }

            // Import latest frames
            for (_name, frame) in latest_frames {
                let frame_name = frame.name.clone();
                let frame_dims = (frame.width, frame.height);
                if let Some(renderer_arc) = data.state.backend.renderer() {
                    match renderer_arc.lock() {
                        Ok(ref mut renderer) => {
                            match import_electron_frame(renderer, frame) {
                                Ok((tex, cached)) => {
                                    let is_first = !data.gles_tex_cache.contains_key(&frame_name);
                                    data.state.damaged_windows.insert(frame_name.clone());
                                    data.state.texture_cache.insert_dmabuf(cached);
                                    data.gles_tex_cache.insert(frame_name.clone(), tex.texture);
                                    if is_first {
                                        info!(
                                            "First DMABUF imported for '{}' ({}x{}, total_textures={})",
                                            frame_name, frame_dims.0, frame_dims.1,
                                            data.gles_tex_cache.len(),
                                        );
                                    }
                                }
                                Err(e) => warn!("DMABUF import failed for '{}': {e:#}", frame_name),
                            }
                        }
                        Err(e) => warn!("Failed to lock renderer: {e}"),
                    }
                }
            }
            
            // Evict textures for windows no longer in the DMABUF cache
            data.gles_tex_cache.retain(|name, _| data.state.texture_cache.get_dmabuf(name).is_some());

            // Process other messages
            for msg in other_batch {
                use crate::electron::ElectronMessage;
                match msg {
                    ElectronMessage::WindowPosition(pos) => {
                        trace!(
                            "Window position: {} at ({}, {})",
                            pos.window_name, pos.x, pos.y
                        );
                        data.state.window_positions.insert(
                            pos.window_name.clone(),
                            (pos.x, pos.y, pos.width, pos.height),
                        );
                        data.state.metadata_dirty = true;
                    }
                    ElectronMessage::Syscall(req) => {
                        if let Some(ref handler) = data.state.syscall_handler {
                            trace!("Syscall from {}: {}", req.window_name, req.syscall_type);

                            if let Ok(syscall_req) =
                                serde_json::from_str::<crate::syscall::SyscallRequest>(&req.payload)
                            {
                                let response = handler.handle(syscall_req);
                                let response_json = serde_json::to_string(&response)
                                    .unwrap_or_else(|_| "{}".to_string());

                                if let Some(ref ipc) = data.state.electron_ipc {
                                    let _ =
                                        ipc.send_syscall_response(&req.window_name, &response_json);
                                }
                            } else {
                                warn!("Failed to parse syscall payload: {}", req.payload);
                            }
                        } else {
                            warn!("Syscall requested but syscalls are disabled");
                        }
                    }
                    ElectronMessage::Action(action_msg) => {
                        use crate::electron::CompositorAction;
                        info!(
                            "Action from {}: {:?}",
                            action_msg.window_name, action_msg.action
                        );

                        match action_msg.action {
                            CompositorAction::Quit { code } => {
                                info!("Quit requested with code {}", code);
                                data.state.running = false;
                            }
                            CompositorAction::Custom {
                                action: act_str,
                                payload,
                            } => {
                                info!("Custom action '{}': <payload>", act_str);

                                let payload_json: Option<serde_json::Value> = payload
                                    .as_ref()
                                    .and_then(|p| serde_json::from_str(p).ok());

                                let target_name: Option<String> = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("window"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        if action_msg.window_name.starts_with("wayland-") {
                                            Some(action_msg.window_name.clone())
                                        } else {
                                            None
                                        }
                                    });

                                let wayland_window = target_name.as_ref()
                                    .and_then(|name| data.state.find_wayland_window(name));

                                if let Some(window) = wayland_window {
                                    let th = data.state.title_h_for_window(&window);
                                    match act_str.as_str() {
                                        "move" => {
                                            let x = payload_json.as_ref()
                                                .and_then(|p| p.get("x"))
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0) as i32;
                                            let y = payload_json.as_ref()
                                                .and_then(|p| p.get("y"))
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0) as i32;
                                            data.state.space.map_element(window, (x, y + th), false);
                                        }
                                        "resize" => {
                                            let new_w = payload_json.as_ref()
                                                .and_then(|p| p.get("width"))
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0) as i32;
                                            let new_h = payload_json.as_ref()
                                                .and_then(|p| p.get("height"))
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0) as i32;
                                            if let Some(toplevel) = window.toplevel() {
                                                let content_h = (new_h - th).max(1);
                                                let (cw, ch) = data.state.clamp_to_size_hints(&window, new_w, content_h);
                                                toplevel.with_pending_state(|s| {
                                                    s.size = Some((cw, ch).into());
                                                });
                                                toplevel.send_configure();
                                            }
                                        }
                                        "close" => {
                                            if let Some(toplevel) = window.toplevel() {
                                                toplevel.send_close();
                                            }
                                            data.state.space.unmap_elem(&window);
                                            data.state.all_windows.retain(|w| w != &window);
                                            if let Some(name) = target_name.as_ref() {
                                                data.state.window_mapped.insert(name.clone(), false);
                                            }
                                            data.state.refocus_topmost_native_window();
                                            data.state.metadata_dirty = true;
                                        }
                                        "focus" => {
                                            let surface = window
                                                .toplevel()
                                                .map(|t| t.wl_surface().clone())
                                                .or_else(|| {
                                                    window
                                                        .x11_surface()
                                                        .and_then(|x| x.wl_surface().clone())
                                                });

                                            if let Some(surface) = surface {
                                                if data.state.space.element_location(&window).is_none() {
                                                    use smithay::wayland::compositor::with_states;
                                                    let parent_surface = window.toplevel().and_then(|t| {
                                                        with_states(t.wl_surface(), |states| {
                                                            states.data_map
                                                                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                                                .and_then(|d| d.lock().ok().and_then(|data| data.parent.clone()))
                                                        })
                                                    });
                                                    let (ow, oh) = (
                                                        data.state.output_size.0 as i32,
                                                        data.state.output_size.1 as i32,
                                                    );
                                                    let location = if let Some(ref parent_wl) = parent_surface {
                                                        let parent_center = data
                                                            .state
                                                            .space
                                                            .elements()
                                                            .find(|w| {
                                                                w.toplevel()
                                                                    .map(|t| t.wl_surface() == parent_wl)
                                                                    .unwrap_or(false)
                                                            })
                                                            .map(|parent_win| {
                                                                let loc = data
                                                                    .state
                                                                    .space
                                                                    .element_location(parent_win)
                                                                    .unwrap_or_default();
                                                                let geo = parent_win.geometry();
                                                                (
                                                                    loc.x + geo.size.w / 2,
                                                                    loc.y + geo.size.h / 2,
                                                                )
                                                            })
                                                            .unwrap_or_else(|| (ow / 2, oh / 2));
                                                        let dialog_y = (parent_center.1 - 75)
                                                            .max(handlers::xdg_shell::PANEL_H);
                                                        (parent_center.0 - 200, dialog_y)
                                                    } else {
                                                        let suggest_w = (ow * 2 / 3).max(640).min(1600);
                                                        let suggest_h = (oh * 2 / 3).max(480).min(1000);
                                                        let center_y = ((oh - suggest_h) / 2)
                                                            .max(handlers::xdg_shell::PANEL_H + 20);
                                                        ((ow - suggest_w) / 2, center_y)
                                                    };
                                                    let tname = target_name.clone().unwrap();
                                                    data.state.window_mapped.insert(tname, true);
                                                    data.state.space.map_element(window.clone(), location, true);
                                                    data.state.metadata_dirty = true;
                                                }

                                                // Use xdg-activation for xdg toplevels; directly set
                                                // keyboard focus for X11 windows.
                                                if window.toplevel().is_some() {
                                                    use smithay::wayland::compositor::with_states;
                                                    let token = XdgActivationToken::from(format!(
                                                        "electron-focus-{}",
                                                        Instant::now().elapsed().as_millis()
                                                    ));
                                                    let token_data = XdgActivationTokenData {
                                                        surface: Some(surface.clone()),
                                                        serial: None,
                                                        timestamp: Instant::now(),
                                                        app_id: window.toplevel().and_then(|t| {
                                                            with_states(t.wl_surface(), |states| {
                                                                states
                                                                    .data_map
                                                                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                                                    .and_then(|d| {
                                                                        d.lock().ok().and_then(
                                                                            |data| data.app_id.clone(),
                                                                        )
                                                                    })
                                                            })
                                                        }),
                                                        client_id: None,
                                                        user_data: std::sync::Arc::new(
                                                            smithay::utils::user_data::UserDataMap::new(),
                                                        ),
                                                    };
                                                    data.state.request_activation(
                                                        token,
                                                        token_data,
                                                        surface.clone(),
                                                    );

                                                    if let Some(keyboard) = data.state.seat.get_keyboard() {
                                                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                                        keyboard.set_focus(
                                                            &mut data.state,
                                                            Some(surface.clone()),
                                                            serial,
                                                        );
                                                    }

                                                    if let Some(name) = target_name.as_ref() {
                                                        data.state.keyboard_window_focus = Some(name.clone());
                                                        data.state.pointer_window_focus = Some(name.clone());
                                                    }
                                                } else if let Some(keyboard) = data.state.seat.get_keyboard() {
                                                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                                    info!("Focus action: setting keyboard focus to X11 window: {:?}", target_name);
                                                    keyboard.set_focus(
                                                        &mut data.state,
                                                        Some(surface.clone()),
                                                        serial,
                                                    );

                                                    if let Some(name) = target_name.as_ref() {
                                                        data.state.keyboard_window_focus = Some(name.clone());
                                                        data.state.pointer_window_focus = Some(name.clone());
                                                    }
                                                }
                                            }
                                        }
                                        "minimize" => {
                                            data.state.space.unmap_elem(&window);
                                            data.state.window_mapped.insert(target_name.clone().unwrap(), false);
                                            data.state.refocus_topmost_native_window();
                                            data.state.metadata_dirty = true;
                                        }
                                        "maximize" => {
                                            if let Some(toplevel) = window.toplevel() {
                                                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
                                                use wo::handlers::xdg_shell::PANEL_H;
                                                let (ow, oh) = (data.state.output_size.0 as i32, data.state.output_size.1 as i32);
                                                let y_offset = th.max(PANEL_H);
                                                let content_h = (oh - y_offset).max(1);
                                                toplevel.with_pending_state(|s| {
                                                    s.states.set(XdgState::Maximized);
                                                    s.size = Some((ow, content_h).into());
                                                });
                                                toplevel.send_configure();
                                                data.state.space.map_element(window.clone(), (0, y_offset), false);
                                            }
                                        }
                                        "pointer_motion" => {
                                            let x = payload_json.as_ref().and_then(|p| p.get("x")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let y = payload_json.as_ref().and_then(|p| p.get("y")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let tname = target_name.as_deref().unwrap_or("");
                                            data.state.handle_forwarded_pointer_event(tname, x, y);
                                        }
                                        "pointer_button" => {
                                            let x = payload_json.as_ref().and_then(|p| p.get("x")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let y = payload_json.as_ref().and_then(|p| p.get("y")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let button = payload_json.as_ref().and_then(|p| p.get("button")).and_then(|v| v.as_u64()).unwrap_or(272) as u32;
                                            let pressed = payload_json.as_ref().and_then(|p| p.get("pressed")).and_then(|v| v.as_bool()).unwrap_or(false);
                                            let tname = target_name.as_deref().unwrap_or("");
                                            data.state.handle_forwarded_pointer_button(tname, x, y, button, pressed, 0);
                                        }
                                        "pointer_scroll" => {
                                            let dx = payload_json.as_ref().and_then(|p| p.get("dx")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let dy = payload_json.as_ref().and_then(|p| p.get("dy")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                                            let tname = target_name.as_deref().unwrap_or("");
                                            data.state.handle_forwarded_pointer_scroll(tname, dx, dy);
                                        }
                                        "pointer_leave" => {
                                            use smithay::input::pointer::MotionEvent;
                                            if let Some(pointer) = data.state.seat.get_pointer() {
                                                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                                let loc = data.state.pointer_location;
                                                pointer.motion(&mut data.state, None, &MotionEvent {
                                                    location: loc,
                                                    serial,
                                                    time: 0,
                                                });
                                                pointer.frame(&mut data.state);
                                            }
                                        }
                                        "keyboard_key" => {
                                            let keycode = payload_json
                                                .as_ref()
                                                .and_then(|p| p.get("keycode"))
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0) as u32;
                                            let pressed = payload_json
                                                .as_ref()
                                                .and_then(|p| p.get("pressed"))
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(false);
                                            let key_state = if pressed {
                                                smithay::backend::input::KeyState::Pressed
                                            } else {
                                                smithay::backend::input::KeyState::Released
                                            };
                                            let xkb_keycode: u32 = keycode + 8;
                                            let time = std::time::Instant::now()
                                                .duration_since(data.last_render)
                                                .as_millis() as u32;
                                            if let Some(keyboard) = data.state.seat.get_keyboard() {
                                                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                                keyboard.input::<(), _>(
                                                    &mut data.state,
                                                    xkb_keycode.into(),
                                                    key_state,
                                                    serial,
                                                    time,
                                                    |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                } else {
                                    // Electron window actions (existing logic)
                                    match act_str.as_str() {
                                        "focus" => {
                                            data.state
                                                .window_mapped
                                                .insert(action_msg.window_name.clone(), true);
                                            // Give keyboard and pointer logical focus to this window
                                            data.state.keyboard_window_focus = Some(action_msg.window_name.clone());
                                            data.state.pointer_window_focus = Some(action_msg.window_name.clone());
                                            data.state.metadata_dirty = true;
                                        }
                                        "map" => {
                                            data.state
                                                .window_mapped
                                                .insert(action_msg.window_name.clone(), true);
                                            data.state.metadata_dirty = true;
                                        }
                                        "unmap" | "minimize" => {
                                            data.state
                                                .window_mapped
                                                .insert(action_msg.window_name.clone(), false);
                                            // Clear any frontend keyboard/pointer focus targeting this window
                                            if data.state.keyboard_window_focus.as_deref() == Some(&action_msg.window_name) {
                                                data.state.keyboard_window_focus = None;
                                            }
                                            if data.state.pointer_window_focus.as_deref() == Some(&action_msg.window_name) {
                                                data.state.pointer_window_focus = None;
                                            }
                                            data.state.metadata_dirty = true;
                                        }
                                        "close" => {
                                            if let Some(idx) = data
                                                .state
                                                .electron_processes
                                                .iter()
                                                .position(|p| p.name == action_msg.window_name)
                                            {
                                                let mut proc =
                                                    data.state.electron_processes.remove(idx);
                                                proc.kill();
                                            }
                                            data.state.texture_cache.remove(&action_msg.window_name);
                                            data.state.window_positions.remove(&action_msg.window_name);
                                            data.state.window_mapped.remove(&action_msg.window_name);
                                            // Clear any stored focus referring to this window
                                            if data.state.keyboard_window_focus.as_deref() == Some(&action_msg.window_name) {
                                                data.state.keyboard_window_focus = None;
                                            }
                                            if data.state.pointer_window_focus.as_deref() == Some(&action_msg.window_name) {
                                                data.state.pointer_window_focus = None;
                                            }
                                            if let Some(ref ipc) = data.state.electron_ipc {
                                                let _ = ipc
                                                    .clients
                                                    .lock()
                                                    .unwrap()
                                                    .remove(&action_msg.window_name);
                                            }
                                            data.state.metadata_dirty = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            if !data.state.running {
                loop_signal.stop();
                return;
            }

            // Execute any due autostart tasks (scheduled at startup)
            if !data.autostart_tasks.is_empty() {
                let now = std::time::Instant::now();
                let mut pending: Vec<(crate::config::AutostartConfig, std::time::Instant)> =
                    Vec::new();
                for (task, when) in data.autostart_tasks.drain(..) {
                    if when <= now {
                        let cmd = task.command.clone();
                        // Handle simple wo://window/<name> built-in action
                        if let Some(name) = cmd.strip_prefix("wo://window/") {
                            if let Some(win_cfg) =
                                data.state.config.windows.iter().find(|w| w.name == name)
                            {
                                match ElectronProcess::spawn(
                                    win_cfg,
                                    &data.state.config.compositor.electron_path,
                                    &Path::new(env!("CARGO_MANIFEST_DIR")).join("electron"),
                                    &data.state.config.compositor.ipc_socket,
                                    &data.electron_render_node,
                                ) {
                                    Ok(proc) => data.state.electron_processes.push(proc),
                                    Err(e) => {
                                        error!(window = %name, "autostart spawn failed: {e:#}")
                                    }
                                }
                            } else {
                                warn!("autostart: window config not found: {}", name);
                            }
                        } else if cmd == "wo://portal/start" {
                            let portal_bin = std::env::current_exe()
                                .ok()
                                .and_then(|p| p.parent().map(|d| d.join("wo-portal")))
                                .unwrap_or_else(|| std::path::PathBuf::from("wo-portal"));
                            match std::process::Command::new(&portal_bin).spawn() {
                                Ok(_) => info!("Spawned portal service: {:?}", portal_bin),
                                Err(e) => error!("Failed to spawn portal: {e:#}"),
                            }
                        } else if let Some(path) = cmd.strip_prefix("file://") {
                            // Execute script file (path expected absolute or relative)
                            let p = path.to_string();
                            if task.restart {
                                std::thread::spawn(move || loop {
                                    match std::process::Command::new(&p).spawn() {
                                        Ok(mut child) => {
                                            let _ = child.wait();
                                        }
                                        Err(e) => {
                                            eprintln!("autostart exec failed {}: {e}", p);
                                            break;
                                        }
                                    }
                                });
                            } else {
                                let _ = std::process::Command::new(&p).spawn();
                            }
                        } else {
                            // Generic command: if contains whitespace, run via shell
                            if cmd.contains(' ') {
                                if task.restart {
                                    let c = cmd.clone();
                                    std::thread::spawn(move || loop {
                                        match std::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(&c)
                                            .spawn()
                                        {
                                            Ok(mut child) => {
                                                let _ = child.wait();
                                            }
                                            Err(e) => {
                                                eprintln!("autostart shell failed {}: {e}", c);
                                                break;
                                            }
                                        }
                                    });
                                } else {
                                    let _ = std::process::Command::new("sh")
                                        .arg("-c")
                                        .arg(&cmd)
                                        .spawn();
                                }
                            } else {
                                // Simple binary invocation
                                if task.restart {
                                    let c = cmd.clone();
                                    std::thread::spawn(move || loop {
                                        match std::process::Command::new(&c).spawn() {
                                            Ok(mut child) => {
                                                let _ = child.wait();
                                            }
                                            Err(e) => {
                                                eprintln!("autostart exec failed {}: {e}", c);
                                                break;
                                            }
                                        }
                                    });
                                } else {
                                    let _ = std::process::Command::new(&cmd).spawn();
                                }
                            }
                        }
                    } else {
                        pending.push((task, when));
                    }
                }
                data.autostart_tasks = pending;
            }

            // Poll for exited Electron helper processes (non-blocking) and mark metadata dirty
            if !data.state.electron_processes.is_empty() {
                for idx in (0..data.state.electron_processes.len()).rev() {
                    if let Some(pid_u32) = data.state.electron_processes.get(idx).map(|p| p.child.id()) {
                        let pid = pid_u32 as u32;
                        let npid = nix::unistd::Pid::from_raw(pid as i32);
                        match waitpid(Some(npid), Some(WaitPidFlag::WNOHANG)) {
                            Ok(WaitStatus::StillAlive) => {}
                            Ok(status) => {
                                let name = data.state.electron_processes[idx].name.clone();
                                info!(window = %name, pid = pid, "Electron process exited: {:?}", status);
                                let mut proc = data.state.electron_processes.remove(idx);
                                let _ = proc.child.try_wait();
                                data.state.metadata_dirty = true;
                            }
                            Err(e) => {
                                warn!("waitpid failed for electron pid {}: {e}", pid);
                            }
                        }
                    }
                }
            }

            // Send window metadata update to all clients if dirty
            if data.state.metadata_dirty {
                let mut metadata_list = Vec::new();

                // Electron-managed windows
                for win_cfg in &data.state.config.windows {
                    let (x, y, w, h) = data
                        .state
                        .window_positions
                        .get(&win_cfg.name)
                        .copied()
                        .unwrap_or((win_cfg.x, win_cfg.y, win_cfg.width, win_cfg.height));

                    let mapped = data.state
                        .window_mapped
                        .get(&win_cfg.name)
                        .copied()
                        .unwrap_or(true);

                    let is_focused = data.state.pointer_window_focus.as_ref() == Some(&win_cfg.name);

                    metadata_list.push(serde_json::json!({
                        "name": win_cfg.name,
                        "x": x,
                        "y": y,
                        "width": w,
                        "height": h,
                        "z_order": win_cfg.z_order,
                        "focused": is_focused,
                        "mapped": mapped,
                        "source": "electron",
                        "ssd": true,
                    }));
                }

                // Native Wayland windows from the all_windows tracking list.
                // Snapshot to avoid borrow issues during parent lookups.
                let wayland_windows: Vec<_> = data.state.all_windows.clone();
                for window in wayland_windows.iter() {
                    let window_name = match data.state.wayland_window_name(window) {
                        Some(n) => n.clone(),
                        None => continue,
                    };

                    let title = if let Some(x11) = window.x11_surface() {
                        x11.title().to_string()
                    } else {
                        window.toplevel()
                            .and_then(|t| {
                                smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                                    states.data_map
                                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                        .and_then(|d| d.lock().ok().and_then(|data| data.title.clone()))
                                })
                            })
                            .unwrap_or_else(|| window_name.clone())
                    };

                    let app_id = if let Some(x11) = window.x11_surface() {
                        Some(x11.class().to_string())
                    } else {
                        window.toplevel()
                            .and_then(|t| {
                                smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                                    states.data_map
                                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                        .and_then(|d| d.lock().ok().and_then(|data| data.app_id.clone()))
                                })
                            })
                    };

                    let loc = data.state.space.element_location(window).unwrap_or_default();
                    let geo = window.geometry();

                    let bbox_w = geo.size.w + geo.loc.x.max(0) * 2;
                    let bbox_h = geo.size.h + geo.loc.y.max(0) * 2;

                    let th = data.state.title_h_for_window(window);
                    let is_ssd = th > 0;

                    let (parent_surface, parent_x11_id) = if let Some(x11) = window.x11_surface() {
                        (None, x11.is_transient_for())
                    } else {
                        (window.toplevel().and_then(|t| {
                            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                                states.data_map
                                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                    .and_then(|d| d.lock().ok().and_then(|data| data.parent.clone()))
                            })
                        }), None)
                    };
                    
                    let parent_name = if let Some(parent_wl) = parent_surface.as_ref() {
                        use smithay::reexports::wayland_server::Resource;
                        data.state.wayland_window_names.get(&parent_wl.id()).cloned()
                    } else if let Some(parent_id) = parent_x11_id {
                        data.state.x11_window_names.get(&parent_id).cloned()
                    } else {
                        None
                    };
                    
                    let is_dialog = parent_name.is_some();

                    let is_focused = if let Some(x11) = window.x11_surface() {
                        x11.wl_surface()
                            .map(|s| data.state.surface_has_keyboard_focus(&s))
                            .unwrap_or(false)
                    } else {
                        window.toplevel()
                            .map(|t| data.state.surface_has_keyboard_focus(t.wl_surface()))
                            .unwrap_or(false)
                    };

                    let mapped = data.state.space.element_location(window).is_some();

                    let space_idx = data.state.space.elements()
                        .position(|w| w == window)
                        .map(|pos| 1000 - pos as i32)
                        .unwrap_or(0);

                    let source = if window.x11_surface().is_some() {
                        "x11"
                    } else {
                        "wayland"
                    };

                    metadata_list.push(serde_json::json!({
                        "name": window_name,
                        "title": title,
                        "app_id": app_id,
                        "x": loc.x - geo.loc.x.max(0),
                        "y": loc.y - th - geo.loc.y.max(0),
                        "width": bbox_w,
                        "height": bbox_h + th,
                        "z_order": space_idx,
                        "focused": is_focused,
                        "source": source,
                        "mapped": mapped,
                        "ssd": is_ssd,
                        "dialog": is_dialog,
                        "parent_name": parent_name,
                    }));
                }

                if !metadata_list.is_empty() {
                    let metadata_msg = serde_json::json!({
                        "type": "metadata",
                        "windows": metadata_list,
                    });

                    if let Some(ref ipc) = data.state.electron_ipc {
                        let _ = ipc.broadcast_metadata(&metadata_msg.to_string());
                    }
                }
                data.state.metadata_dirty = false;
            }

            if data.active {
                render_frame(data);
            }
        })
        .context("running event loop")?;

    info!("DRM compositor exiting cleanly");
    Ok(())
}