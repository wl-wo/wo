//! Nested compositor backend using Smithay's winit backend
//!
//! This module provides nested compositor support for running wo inside
//! another Wayland compositor or X11 session.

use anyhow::{Context, Result};
use drm_fourcc::DrmFourcc;
use nix::sys::memfd::MFdFlags;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use smithay::{
    backend::{
        drm::DrmNode,
        input::{Axis, KeyState, KeyboardKeyEvent},
        renderer::{
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Element, Kind, RenderElement,
            },
            gles::{GlesRenderer, GlesTexture},
            Bind, Color32F, Frame as _, ImportDma, Offscreen, Renderer as _,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{Display, ListeningSocket},
    utils::{
        Logical, Physical, Point, Rectangle,
        Scale as ScaleF, Size, Transform,
    },
    wayland::{seat::WaylandFocus, xdg_activation::XdgActivationHandler},
};
use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, trace, warn};

use crate::{
    config::Config,
    dmabuf::import_electron_frame,
    electron::{CompositorAction, ElectronIpc, ElectronMessage, ElectronProcess},
    handlers::xdg_shell::PANEL_H,
    state::{BackendData, WoState},
};
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::{xwm::X11Wm, XWayland, XWaylandEvent};

fn find_render_node() -> DrmNode {
    for i in 128..136 {
        let path = format!("/dev/dri/renderD{}", i);
        if let Ok(node) = DrmNode::from_path(&path) {
            info!("Using render node: {}", path);
            return node;
        }
    }

    for i in 0..4 {
        let path = format!("/dev/dri/card{}", i);
        if let Ok(node) = DrmNode::from_path(&path) {
            warn!("Using card node as fallback: {}", path);
            return node;
        }
    }

    warn!("No render nodes found in /dev/dri/, this may cause issues");
    DrmNode::from_dev_id(226 << 8 | 128)
        .unwrap_or_else(|_| panic!("Could not find or create any render node"))
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

pub fn run_nested(config: Config) -> Result<()> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    info!("Starting nested compositor mode");

    if config.compositor.exit_on_escape {
        info!("Press ESC to exit");
    } else {
        info!("Press Ctrl+C or close window to exit");
    }

    let mut display: Display<WoState> = Display::new().context("creating Wayland display")?;
    let mut dh = display.handle();

    let socket = ListeningSocket::bind(&config.compositor.socket_name)
        .with_context(|| format!("binding Wayland socket {}", config.compositor.socket_name))?;
    info!(socket = %config.compositor.socket_name, "Wayland socket created");

    let (mut backend, mut winit_evt_loop) = match winit::init::<GlesRenderer>() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to create winit backend: {:?}", e);
            return Err(anyhow::anyhow!("winit backend creation failed"));
        }
    };

    let size = backend.window_size();
    let (width, height) = (size.w, size.h);

    info!(width, height, "Nested window size");

    let output = Output::new(
        "wo-nested".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Wo".into(),
            model: "Nested".into(),
        },
    );

    let output_mode = OutputMode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    };

    output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);
    output.create_global::<WoState>(&dh);

    let render_node = find_render_node();

    let dmabuf_formats = {
        match backend.bind() {
            Ok((renderer, _target)) => {
                let formats = renderer.dmabuf_formats().into_iter().collect::<Vec<_>>();
                info!("Collected {} dmabuf formats from renderer", formats.len());
                formats
            }
            Err(e) => {
                warn!(
                    "Could not bind backend to get formats: {}, using empty list",
                    e
                );
                vec![]
            }
        }
    };

    let backend_data = BackendData {
        renderer: None,
        output: output.clone(),
        size: (width as u32, height as u32),
        dmabuf_formats: Some(dmabuf_formats),
    };

    let mut state = WoState::new(&mut display, config.clone(), render_node, backend_data);

    let (ipc, mut msg_rx) =
        ElectronIpc::listen(&config.compositor.ipc_socket).context("IPC socket")?;
    state.electron_ipc = Some(ipc);

    let mut calloop_loop =
        calloop::EventLoop::<WoState>::try_new().context("calloop EventLoop creation")?;
    let calloop_handle = calloop_loop.handle();

    state.xwayland_shell_state = Some(XWaylandShellState::new::<WoState>(&dh));

    match XWayland::spawn(
        &dh,
        None, // auto-detect display number
        std::iter::empty::<(String, String)>(),
        true, // open abstract socket for X11 compatibility
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| {}, // no extra user data
    ) {
        Ok((xwayland, xw_client)) => {
            let wm_handle = calloop_handle.clone();
            let xw_client = std::rc::Rc::new(std::cell::RefCell::new(Some(xw_client)));
            let xw_client_cb = xw_client.clone();

            if let Err(e) =
                calloop_handle.insert_source(xwayland, move |event, _, state: &mut WoState| {
                    match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => {
                            info!("XWayland ready on DISPLAY :{display_number}");
                            std::env::set_var("DISPLAY", format!(":{display_number}"));
                            if let Some(client) = xw_client_cb.borrow_mut().take() {
                                match X11Wm::start_wm(wm_handle.clone(), x11_socket, client) {
                                    Ok(wm) => {
                                        state.xwm = Some(wm);
                                        info!("X11 window manager attached");
                                    }
                                    Err(e) => error!("X11Wm::start_wm failed: {e:#}"),
                                }
                            }
                        }
                        XWaylandEvent::Error => {
                            warn!("XWayland server encountered an error during startup");
                        }
                    }
                })
            {
                error!("Failed to register XWayland event source: {e:#}");
            } else {
                info!("XWayland server starting");
            }
        }
        Err(e) => {
            warn!("XWayland spawn failed (X11 support disabled): {e:#}");
        }
    }

    std::env::set_var("WAYLAND_DISPLAY", &config.compositor.socket_name);
    info!("Set WAYLAND_DISPLAY={}", config.compositor.socket_name);

    let app_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("electron");
    // Nested mode has no DRM card; use the first available render node.
    let nested_render_node = std::fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flat_map(|e| e.filter_map(Result::ok))
        .find(|e| e.file_name().to_str().map_or(false, |n| n.starts_with("renderD")))
        .map(|e| e.path().to_string_lossy().into_owned())
        .unwrap_or_else(|| "/dev/dri/renderD128".to_string());

    for root_cfg in &config.root {
        let mut full = root_cfg.clone();
        full.x = 0;
        full.y = 0;
        full.width = width as u32;
        full.height = height as u32;
        full.z_order = std::i32::MIN / 2;
        match ElectronProcess::spawn(
            &full,
            &config.compositor.electron_path,
            &app_dir,
            &config.compositor.ipc_socket,
            &nested_render_node,
        ) {
            Ok(proc) => state.electron_processes.push(proc),
            Err(e) => error!(window = %full.name, "failed to spawn Electron root: {e:#}"),
        }
    }

    for win_cfg in &config.windows {
        match ElectronProcess::spawn(
            win_cfg,
            &config.compositor.electron_path,
            &app_dir,
            &config.compositor.ipc_socket,
            &nested_render_node,
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

    let bg = Color32F::from(config.compositor.background);
    let frame_time = Duration::from_millis(16);
    let mut last_render = std::time::Instant::now();
    let event_start_time = std::time::Instant::now();

    let mut gles_tex_cache: HashMap<String, GlesTexture> = HashMap::new();
    let mut offscreen_tex_cache: HashMap<String, (GlesTexture, i32, i32)> = HashMap::new();
    use crate::electron::ElectronFrame;
    let mut latest_frames: HashMap<String, ElectronFrame> = HashMap::new();
    let mut last_wayland_elem_count: usize = 0;
    // Frame tracking for backpressure management
    let mut frame_timestamps: HashMap<String, Instant> = HashMap::new();
    let _max_frame_age = Duration::from_millis(33); // Drop frames older than 2 frames @60Hz

    info!("Nested compositor running");

    let mut loop_iter: u64 = 0;
    // Main event loop
    loop {
        loop_iter += 1;
        let loop_t0 = std::time::Instant::now();

        if !state.running {
            info!("Compositor shutting down");
            break;
        }

        let winit_t0 = std::time::Instant::now();
        let _dispatch_result = winit_evt_loop.dispatch_new_events(|event| {
            match event {
                WinitEvent::Resized { size, .. } => {
                    info!("Window resized to {}x{}", size.w, size.h);
                    let new_mode = OutputMode {
                        size: (size.w as i32, size.h as i32).into(),
                        refresh: 60_000,
                    };
                    output.change_current_state(Some(new_mode), None, None, None);
                }
                WinitEvent::Input(input_event) => {
                    if config.compositor.exit_on_escape {
                        use smithay::backend::input::InputEvent as IE;
                        match &input_event {
                            IE::Keyboard { event } => {
                                if event.state() == KeyState::Pressed {
                                    use smithay::input::keyboard::xkb::Keycode;
                                    if event.key_code() == Keycode::new(1) {
                                        info!("ESC pressed, exiting");
                                        state.running = false;
                                        return;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    state.forward_input_to_electron(&input_event);
                    state.process_input_event(input_event);
                }
                WinitEvent::CloseRequested => {
                    info!("Close requested");
                    state.running = false;
                }
                WinitEvent::Redraw => {
                    // Redraw requested
                }
                WinitEvent::Focus(_) => {
                    // Focus changed
                }
            }
        });
        let winit_elapsed = winit_t0.elapsed();
        if winit_elapsed.as_millis() > 50 {
            warn!("dispatch_new_events took {}ms!", winit_elapsed.as_millis());
        }

        // Check again after events
        if !state.running {
            break;
        }

        while let Ok(Some(stream)) = socket.accept() {
            if let Err(e) = dh.insert_client(
                stream,
                std::sync::Arc::new(crate::state::ClientData::default()),
            ) {
                error!("Failed to insert Wayland client: {e}");
            }
        }
        let dispatch_t0 = std::time::Instant::now();
        let log_ckpt = state.debug_new_toplevel;
        if log_ckpt {
            info!("checkpoint A: before dispatch_clients");
        }
        if let Err(e) = display.dispatch_clients(&mut state) {
            error!("Failed to dispatch Wayland clients: {e}");
        }
        if log_ckpt {
            info!(
                "checkpoint B: after dispatch_clients (debug_flag={})",
                state.debug_new_toplevel
            );
        }
        if let Err(e) = display.flush_clients() {
            error!("Failed to flush Wayland clients: {e}");
        }
        if log_ckpt {
            info!("checkpoint C: after flush_clients");
        }
        state.space.refresh();
        if log_ckpt {
            info!("checkpoint D: after space.refresh");
        }
        if let Err(e) = display.flush_clients() {
            error!("Failed to flush after space refresh: {e}");
        }
        if log_ckpt {
            info!("checkpoint E: after second flush");
        }
        state.debug_new_toplevel = false;

        let calloop_t0 = std::time::Instant::now();
        if let Err(e) = calloop_loop.dispatch(Duration::ZERO, &mut state) {
            warn!("calloop dispatch error: {e}");
        }
        let calloop_ms = calloop_t0.elapsed().as_millis();
        if calloop_ms > 20 {
            warn!("calloop dispatch took {}ms!", calloop_ms);
        }

        let cur_wayland_count = state.space.elements().count();
        if cur_wayland_count != last_wayland_elem_count {
            info!(
                "Wayland space count: {} window(s) in space",
                cur_wayland_count
            );
            last_wayland_elem_count = cur_wayland_count;
        }
        let dispatch_elapsed = dispatch_t0.elapsed();
        if dispatch_elapsed.as_millis() > 50 {
            warn!(
                "dispatch_clients+flush took {}ms!",
                dispatch_elapsed.as_millis()
            );
        }

        let emsg_t0 = std::time::Instant::now();
        let mut emsg_count = 0u32;
        let mut input_events_pending = false;
        
        // Process messages with priority: input events first, then frames
        let mut frames_batch: Vec<ElectronMessage> = Vec::new();
        let mut input_batch: Vec<ElectronMessage> = Vec::new();
        
        while let Ok(msg) = msg_rx.try_recv() {
            emsg_count += 1;
            match &msg {
                ElectronMessage::Frame(_) => frames_batch.push(msg),
                ElectronMessage::ForwardedPointer { .. }
                | ElectronMessage::ForwardedKeyboard { .. }
                | ElectronMessage::ForwardedPointerButton { .. }
                | ElectronMessage::ForwardedRelativePointer { .. }
                | ElectronMessage::ForwardedPointerScroll { .. } => {
                    input_batch.push(msg);
                    input_events_pending = true;
                }
                _ => process_electron_message(msg, &mut state, event_start_time),
            }
        }
        
        // Process input events immediately for low latency
        for msg in input_batch {
            process_electron_message(msg, &mut state, event_start_time);
        }
        
        // Process frames with backpressure handling
        let now = Instant::now();
        for msg in frames_batch {
            if let ElectronMessage::Frame(frame) = msg {
                let nm = frame.name.clone();
                trace!("main-loop: dequeued frame seq={} window={}", frame.seq, nm);
                
                // Drop frames that are too old (backpressure)
                if let Some(last_ts) = frame_timestamps.get(&nm) {
                    if now.duration_since(*last_ts) < Duration::from_millis(8) {
                        // Skip frame if less than 8ms since last update (>120fps throttle)
                        trace!("Dropping frame {} due to backpressure", nm);
                        continue;
                    }
                }
                
                frame_timestamps.insert(nm.clone(), now);
                // Only keep the latest frame per window
                latest_frames.insert(nm, frame);
            }
        }
        
        let emsg_ms = emsg_t0.elapsed().as_millis();
        if emsg_ms > 20 {
            warn!(
                "electron message processing took {}ms (count={})",
                emsg_ms, emsg_count
            );
        }

        let adaptive_frame_time = if input_events_pending {
            Duration::from_millis(8)
        } else {
            frame_time
        };
        
        if last_render.elapsed() < adaptive_frame_time {
            continue;
        }
        last_render = std::time::Instant::now();

        if !autostart_tasks.is_empty() {
            let now = std::time::Instant::now();
            let mut pending = Vec::new();
            for (task, when) in autostart_tasks.drain(..) {
                if when <= now {
                    execute_autostart_task(&task, &mut state, &app_dir);
                } else {
                    pending.push((task, when));
                }
            }
            autostart_tasks = pending;
        }

        if !state.electron_processes.is_empty() {
            for idx in (0..state.electron_processes.len()).rev() {
                if let Some(pid_u32) = state.electron_processes.get(idx).map(|p| p.child.id()) {
                    let pid = pid_u32 as u32;
                    let npid = nix::unistd::Pid::from_raw(pid as i32);
                    match waitpid(Some(npid), Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) => {}
                        Ok(status) => {
                            let name = state.electron_processes[idx].name.clone();
                            info!(window = %name, pid = pid, "Electron process exited: {:?}", status);
                            let mut proc = state.electron_processes.remove(idx);
                            let _ = proc.child.try_wait();
                            state.metadata_dirty = true;
                        }
                        Err(e) => {
                            warn!("waitpid failed for electron pid {}: {e}", pid);
                        }
                    }
                }
            }
        }

        let meta_t0 = std::time::Instant::now();
        if state.metadata_dirty {
            // Simplified metadata is small enough to stay under IPC limits
            send_window_metadata(&state);
            state.metadata_dirty = false;
        }
        let meta_ms = meta_t0.elapsed().as_millis();
        if meta_ms > 20 {
            warn!("send_window_metadata took {}ms!", meta_ms);
        }

        // Render frame
        let render_t_start = std::time::Instant::now();
        let size = backend.window_size();
        let output_rect: Rectangle<i32, Physical> =
            Rectangle::new((0, 0).into(), (size.w as i32, size.h as i32).into());

        // Render in a scope to release borrows before submission.
        // Use a labeled block so early exits (bind/render failures) still reach
        // the frame-callback section below — Wayland clients MUST receive frame
        // callbacks or they stall indefinitely waiting for the next vblank signal.
        let mut render_ok = false;
        let render_t0 = std::time::Instant::now();
        'render: {
            // Bind backend for rendering (this returns (renderer, target))
            let bind_t0 = std::time::Instant::now();
            let (renderer, mut target) = match backend.bind() {
                Ok(t) => {
                    let bind_ms = bind_t0.elapsed().as_millis();
                    if bind_ms > 50 {
                        warn!("backend.bind() took {}ms!", bind_ms);
                    }
                    t
                }
                Err(e) => {
                    warn!("Failed to bind backend: {}", e);
                    if is_egl_context_or_surface_lost(&e.to_string()) {
                        error!(
                            "EGL context/surface lost while binding nested backend; shutting down so context can be recreated"
                        );
                        state.running = false;
                    }
                    break 'render;
                }
            };

            for (_, frame) in latest_frames.drain() {
                let win_name = frame.name.clone();
                let (width, height) = (frame.width, frame.height);
                
                // Import each new frame - each has unique pixel data even if size unchanged
                match import_electron_frame(renderer, frame) {
                    Ok((tex, cached)) => {
                        trace!("Frame imported for {} ({}x{})", win_name, width, height);
                        state.texture_cache.insert_dmabuf(cached);
                        gles_tex_cache.insert(win_name.clone(), tex.texture);
                    }
                    Err(e) => {
                        warn!("DMABUF import failed for {}: {e:#}", win_name);
                    }
                }
            }

            // Evict textures for windows no longer in the DMABUF cache.
            gles_tex_cache.retain(|name, _| state.texture_cache.get_dmabuf(name).is_some());

            let imported_textures = &gles_tex_cache;

            // Prepare cursor elements before starting the frame
            let cursor_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = 
                if let smithay::input::pointer::CursorImageStatus::Surface(ref cursor_surface) = state.cursor_status {
                    use smithay::backend::renderer::element::{surface::render_elements_from_surface_tree, Kind};
                    use smithay::utils::Scale as ScaleF;
                    
                    let scale = ScaleF::from(1.0);
                    render_elements_from_surface_tree(
                        renderer,
                        cursor_surface,
                        (state.pointer_location.x as i32, state.pointer_location.y as i32),
                        scale,
                        1.0,
                        Kind::Cursor,
                    )
                } else {
                    Vec::new()
                };

            // Load themed cursor texture for Named cursor status
            let themed_cursor = if let smithay::input::pointer::CursorImageStatus::Named(ref icon) = state.cursor_status {
                state.cursor_theme_manager.get_cursor(icon.name(), renderer).cloned()
            } else if cursor_elements.is_empty() {
                if !matches!(state.cursor_status, smithay::input::pointer::CursorImageStatus::Hidden) {
                    state.cursor_theme_manager.get_cursor("default", renderer).cloned()
                } else {
                    None
                }
            } else {
                None
            };

            // Wayland windows are exclusively rendered by comraw via off-screen buffers.
            // The native space.elements() rendering pass has been removed here.

            let mut frame = match renderer.render(
                &mut target,
                Size::from((size.w as i32, size.h as i32)),
                Transform::Flipped180,
            ) {
                Ok(frame) => frame,
                Err(e) => {
                    warn!("Failed to begin frame: {}", e);
                    if is_egl_context_or_surface_lost(&e.to_string()) {
                        error!(
                            "EGL context/surface lost while starting frame; shutting down so context can be recreated"
                        );
                        state.running = false;
                    }
                    break 'render;
                }
            };

            // Clear background
            if let Err(e) = frame.clear(bg.into(), &[output_rect]) {
                warn!("Failed to clear frame: {}", e);
                break 'render;
            }

            let mut windows: Vec<_> = state
                .config
                .windows
                .iter()
                .chain(state.config.root.iter())
                .collect();
            windows.sort_by_key(|w| w.z_order);

            for win in windows {
                let mapped = state.window_mapped.get(&win.name).copied().unwrap_or(true);
                if !mapped {
                    continue;
                }

                if let Some(tex) = imported_textures.get(&win.name) {
                    if let Err(e) = frame.render_texture_at(
                        tex,
                        (win.x, win.y).into(),
                        1,
                        1.0,
                        Transform::Normal,
                        &[output_rect],
                        &[],
                        1.0,
                    ) {
                        warn!("Failed to render texture for {}: {}", win.name, e);
                    }
                }
            }

            // (Native rendering loop was removed)

            // NOTE: Wayland surfaces are rendered via offscreen GL to SHM for comraw,
            // allowing the client to fully control window compositing and layout.

            // Render cursor on top of everything
            if !cursor_elements.is_empty() {
                for element in &cursor_elements {
                    use smithay::utils::Scale as ScaleF;
                    let scale = ScaleF::from(1.0);
                    let src = element.src();
                    let dst = element.geometry(scale);
                    if let Err(e) = element.draw(&mut frame, src, dst, &[output_rect], &[]) {
                        trace!("Failed to render cursor element: {}", e);
                    }
                }
            } else if let Some(ref cur) = themed_cursor {
                use smithay::backend::renderer::Frame as _;
                let cx = state.pointer_location.x as i32 - cur.xhot as i32;
                let cy = state.pointer_location.y as i32 - cur.yhot as i32;
                if let Err(e) = frame.render_texture_at(
                    &cur.texture,
                    (cx, cy).into(),
                    1,
                    1.0,
                    Transform::Normal,
                    &[output_rect],
                    &[],
                    1.0,
                ) {
                    trace!("Failed to render themed cursor: {}", e);
                }
            }

            match frame.finish() {
                Ok(_) => {
                    render_ok = true;
                }
                Err(e) => {
                    warn!("Failed to finish frame: {}", e);
                    if is_egl_context_or_surface_lost(&e.to_string()) {
                        error!(
                            "EGL context/surface lost while finishing frame; shutting down so context can be recreated"
                        );
                        state.running = false;
                    }
                }
            }
            let render_ms = render_t0.elapsed().as_millis();
            if render_ms > 50 {
                warn!("render phase took {}ms!", render_ms);
            }

            drop(target);

            // Export dirty Wayland surfaces via offscreen rendering to SHM
            let mut exported_pixels: Vec<(String, u32, u32, u32, Vec<u8>)> = Vec::new();
            {
                use smithay::reexports::wayland_server::Resource;

                let dirty: Vec<_> = state.dirty_surfaces.drain().collect();

                for surface in dirty {
                    let surface_id = surface.id();
                    let (window_name, window_geo) = if let Some(n) =
                        state.wayland_window_names.get(&surface_id).cloned()
                    {
                        let sid = surface_id.clone();
                        let geo = state.space.elements().find_map(|w| {
                            if w.wl_surface().map(|s| s.id()) == Some(sid.clone()) {
                                state.space.element_geometry(w)
                            } else {
                                None
                            }
                        });
                        (n, geo)
                    } else {
                        let sid = surface_id.clone();
                        let x11_result = state.space.elements().find_map(|w| {
                            let x11 = w.x11_surface()?;
                            let wl = x11.wl_surface()?;
                            if wl.id() == sid.clone() {
                                let name = state.x11_window_names.get(&x11.window_id()).cloned()?;
                                let geo = state.space.element_geometry(w);
                                Some((name, geo))
                            } else {
                                None
                            }
                        });
                        match x11_result {
                            Some((n, geo)) => (n, geo),
                            None => continue,
                        }
                    };

                    let Some(geo) = window_geo else { continue };

                    // Calculate bbox (bounding box) for CSD windows with shadows/decorations
                    // geo.loc is the offset from surface origin to content area
                    // For libadwaita: geo.loc is typically (10, 10) for 10px shadows
                    // bbox needs to include the decorations, so expand by geo.loc on both sides
                    let bbox_w = (geo.size.w + geo.loc.x.max(0) * 2) as u32;
                    let bbox_h = (geo.size.h + geo.loc.y.max(0) * 2) as u32;

                    let (w, h) = (bbox_w, bbox_h);
                    if w == 0 || h == 0 {
                        continue;
                    }

                    // Read SHM buffer directly (for software-rendered clients)
                    // TODO: Implement GL offscreen rendering for DMABUF/hardware clients
                    use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
                    use smithay::wayland::compositor::with_states;
                    use smithay::wayland::shm::with_buffer_contents;

                    let pixels_opt: Option<(u32, u32, u32, Vec<u8>)> =
                        with_states(&surface, |states| {
                            let rss_data = states.data_map.get::<RendererSurfaceStateUserData>()?;
                            let rss = rss_data.lock().ok()?;
                            let buffer = rss.buffer()?;

                            let mut result: Option<(u32, u32, u32, Vec<u8>)> = None;
                            let _ = with_buffer_contents(buffer, |ptr, len, spec| {
                                let pixel_bytes =
                                    (spec.stride.max(0) * spec.height.max(0)) as usize;
                                if pixel_bytes > 0 && pixel_bytes <= len {
                                    let slice =
                                        unsafe { std::slice::from_raw_parts(ptr, pixel_bytes) };
                                    result = Some((
                                        spec.width as u32,
                                        spec.height as u32,
                                        spec.stride as u32,
                                        slice.to_vec(),
                                    ));
                                }
                            });
                            result
                        });

                    if let Some((bw, bh, stride, pixels)) = pixels_opt {
                        exported_pixels.push((window_name, bw, bh, stride, pixels));
                    } else if let Some(ref ipc) = state.electron_ipc {
                        // If no SHM pixels, check if it's a DMABUF (optimal for games)
                        let dmabuf_opt = with_states(&surface, |states| {
                            let rss_data = states.data_map.get::<RendererSurfaceStateUserData>()?;
                            let rss = rss_data.lock().ok()?;
                            let buffer = rss.buffer()?;
                            smithay::wayland::dmabuf::get_dmabuf(buffer).ok().cloned()
                        });

                        if let Some(dmabuf) = dmabuf_opt {
                            if let Err(e) = ipc.broadcast_dmabuf_frame(&window_name, &dmabuf) {
                                debug!("dmabuf broadcast failed for {}: {}", window_name, e);
                            }
                        }
                    } else {
                        // Surface has no SHM buffer - use GL offscreen rendering for DMABUF/hardware clients
                        let scale = ScaleF::from(1.0);
                        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                            render_elements_from_surface_tree(
                                renderer,
                                &surface,
                                (0, 0),
                                scale,
                                1.0,
                                Kind::Unspecified,
                            );

                        // Create or retrieve offscreen texture for this window
                        let cache_key = window_name.clone();
                        let needs_new_tex = offscreen_tex_cache
                            .get(&cache_key)
                            .map(|(_, tw, th)| *tw != w as i32 || *th != h as i32)
                            .unwrap_or(true);

                        if needs_new_tex {
                            match renderer.create_buffer(
                                DrmFourcc::Argb8888,
                                Size::from((w as i32, h as i32)),
                            ) {
                                Ok(tex) => {
                                    offscreen_tex_cache
                                        .insert(cache_key.clone(), (tex, w as i32, h as i32));
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to create offscreen texture for {}: {}",
                                        window_name, e
                                    );
                                    continue;
                                }
                            }
                        }

                        if let Some((offscreen_tex, _, _)) = offscreen_tex_cache.get_mut(&cache_key)
                        {
                            // Render surface tree to offscreen texture
                            let render_result =
                                (|| -> Result<(), smithay::backend::renderer::gles::GlesError> {
                                    let mut target = renderer.bind(offscreen_tex)?;
                                    let output_rect = Rectangle::new(
                                        Point::new(0, 0),
                                        Size::new(w as i32, h as i32),
                                    );
                                    let mut frame = renderer.render(
                                        &mut target,
                                        Size::from((w as i32, h as i32)),
                                        Transform::Normal,
                                    )?;
                                    frame.clear(Color32F::TRANSPARENT, &[output_rect])?;

                                    // Draw each element in the surface tree
                                    // Elements are positioned relative to surface origin (0, 0)
                                    // For CSD windows, we DON'T offset - surface origin should already
                                    // include the decoration area, and geometry() positions are correct
                                    for element in &elements {
                                        let src = element.src();
                                        let dst = element.geometry(scale);
                                        element.draw(&mut frame, src, dst, &[output_rect], &[])?;
                                    }
                                    let _ = frame.finish();
                                    Ok(())
                                })();

                            if let Err(e) = render_result {
                                warn!("Failed to render surface tree for {}: {}", window_name, e);
                                continue;
                            }

                            // Read back pixels from offscreen texture
                            // Skip GL rendering for now - only export SHM buffers
                            warn!(
                                "Skipping GL offscreen render for {} - only SHM buffers supported",
                                window_name
                            );
                            continue;
                        }
                    }
                }

                // Store exported pixels for memfd write after render block
                state.pending_shm_exports = exported_pixels;
                state.dirty_surfaces.clear();
            } // 'render block ends here, backend borrow is released


            let did_bind = render_ok;
            
            if did_bind {
                let submit_t0 = std::time::Instant::now();
                if !render_ok {
                    debug!("render_ok=false, skipping backend.submit() for this frame");
                } else if let Err(e) = backend.submit(None) {
                    warn!("Failed to submit frame: {}", e);
                    if is_egl_context_or_surface_lost(&e.to_string()) {
                        error!(
                            "EGL context/surface lost while submitting frame; shutting down so context can be recreated"
                        );
                        state.running = false;
                    }
                } else {
                    trace!("Frame submitted successfully");
                }
                let submit_ms = submit_t0.elapsed().as_millis();
                if submit_ms > 50 {
                    warn!("backend.submit() (eglSwapBuffers) took {}ms! This may indicate a blocked vsync/EGL fence.", submit_ms);
                }
            }

            let presented_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let windows_snapshot: Vec<_> = state.space.elements().cloned().collect();
            for window in windows_snapshot {
                window.send_frame(&state.output, presented_at, None, |_, _| {
                    Some(state.output.clone())
                });
            }

            // Write exported pixel buffers to SHM and broadcast to Electron
            if did_bind {
                use std::os::unix::io::AsRawFd;

                let exports = std::mem::take(&mut state.pending_shm_exports);
                for (window_name, w, h, stride, pixels) in exports {
                    let memfd_fd = state
                        .window_shm_buffers
                        .entry(window_name.clone())
                        .or_insert_with(|| {
                            nix::sys::memfd::memfd_create(
                                std::ffi::CStr::from_bytes_with_nul(b"wayland_window\0").unwrap(),
                                MFdFlags::MFD_CLOEXEC,
                            )
                            .expect("creating memfd")
                        });

                    let size = pixels.len() as i64;
                    let stat = nix::sys::stat::fstat(&*memfd_fd).unwrap();
                    if stat.st_size != size {
                        nix::unistd::ftruncate(&*memfd_fd, size).expect("truncating memfd");
                    }

                    nix::unistd::lseek(&*memfd_fd, 0, nix::unistd::Whence::SeekSet)
                        .expect("seeking memfd");
                    nix::unistd::write(&*memfd_fd, &pixels).expect("writing to memfd");

                    if let Some(ref ipc) = state.electron_ipc {
                        let pid = std::process::id();
                        if let Err(e) = ipc.broadcast_shm_buffer(
                            &window_name,
                            w,
                            h,
                            stride,
                            pid,
                            memfd_fd.as_raw_fd() as i32,
                        ) {
                            debug!(
                                "nested shm buffer broadcast failed for {}: {}",
                                window_name, e
                            );
                        }
                    }
                }
            }

            let render_total_ms = render_t_start.elapsed().as_millis();
            if render_total_ms > 50 {
                warn!("render+submit+callbacks total took {}ms!", render_total_ms);
            }

            std::thread::sleep(Duration::from_micros(100));

            // Log if the entire loop iteration was slow.
            let total_loop_ms = loop_t0.elapsed().as_millis();
            if total_loop_ms > 50 {
                warn!("SLOW LOOP iter={} total={}ms", loop_iter, total_loop_ms);
            }
        }
    }

    // Clean up GL textures while context is still active
    offscreen_tex_cache.clear();

    info!("Nested compositor exiting cleanly");
    Ok(())
}

fn execute_autostart_task(
    task: &crate::config::AutostartConfig,
    state: &mut WoState,
    app_dir: &Path,
) {
    let cmd = &task.command;

    if let Some(name) = cmd.strip_prefix("wo://window/") {
        if let Some(win_cfg) = state.config.windows.iter().find(|w| w.name == name) {
            // Best-effort render node for nested autostart
            let render_node = std::fs::read_dir("/dev/dri")
                .ok()
                .into_iter()
                .flat_map(|e| e.filter_map(Result::ok))
                .find(|e| e.file_name().to_str().map_or(false, |n| n.starts_with("renderD")))
                .map(|e| e.path().to_string_lossy().into_owned())
                .unwrap_or_else(|| "/dev/dri/renderD128".to_string());
            match ElectronProcess::spawn(
                win_cfg,
                &state.config.compositor.electron_path,
                app_dir,
                &state.config.compositor.ipc_socket,
                &render_node,
            ) {
                Ok(proc) => state.electron_processes.push(proc),
                Err(e) => error!(window = %name, "autostart spawn failed: {e:#}"),
            }
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
        let _ = std::process::Command::new(path).spawn();
    } else if cmd.contains(' ') {
        let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
    } else {
        let _ = std::process::Command::new(cmd).spawn();
    }
}

fn process_electron_message(
    msg: ElectronMessage,
    state: &mut WoState,
    event_start_time: std::time::Instant,
) {
    match msg {
        ElectronMessage::Frame(_) => {
            // Frames are handled via the latest_frames buffer; should not reach here.
        }
        ElectronMessage::WindowPosition(pos) => {
            debug!(
                "Window position update: {} at ({}, {})",
                pos.window_name, pos.x, pos.y
            );
            state.window_positions.insert(
                pos.window_name.clone(),
                (pos.x, pos.y, pos.width, pos.height),
            );
            state.metadata_dirty = true;
        }
        ElectronMessage::Syscall(req) => {
            info!(window = %req.window_name, payload = %req.payload, "syscall received");
            if let Some(ref handler) = state.syscall_handler {
                if let Ok(syscall_req) =
                    serde_json::from_str::<crate::syscall::SyscallRequest>(&req.payload)
                {
                    let response = handler.handle(syscall_req);
                    let response_json =
                        serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                    if let Some(ref ipc) = state.electron_ipc {
                        let _ = ipc.send_syscall_response(&req.window_name, &response_json);
                    }
                }
            }
        }
        ElectronMessage::Action(action_msg) => {
            info!(
                "Action from {}: {:?}",
                action_msg.window_name, action_msg.action
            );

            match action_msg.action {
                CompositorAction::Quit { code } => {
                    info!("Quit requested with code {}", code);
                    state.running = false;
                }
                CompositorAction::Custom {
                    action: act_str,
                    payload,
                } => {
                    // Parse optional payload JSON to find a target window.
                    let payload_json: Option<serde_json::Value> =
                        payload.as_ref().and_then(|p| serde_json::from_str(p).ok());

                    // Determine target window name (either from payload or action source).
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

                    // Check if target is a Wayland window by looking up in stable name map.
                    let wayland_window = target_name
                        .as_ref()
                        .and_then(|name| state.find_wayland_window(name));

                    if let Some(window) = wayland_window {
                        let th = state.title_h_for_window(&window);
                        match act_str.as_str() {
                            "move" => {
                                let x = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("x"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32;
                                let y = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("y"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32;
                                // comraw reports the MacWindow origin (title bar top);
                                // the Wayland surface sits th pixels below that.
                                state.space.map_element(window, (x, y + th), false);
                            }
                            "resize" => {
                                let new_w = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("width"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    as i32;
                                let new_h = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("height"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    as i32;
                                if let Some(toplevel) = window.toplevel() {
                                    // comraw sends the total MacWindow height (title bar + content);
                                    // the Wayland surface only needs the content height.
                                    let content_h = (new_h - th).max(1);
                                    let (cw, ch) =
                                        state.clamp_to_size_hints(&window, new_w, content_h);
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
                                state.space.unmap_elem(&window);
                                state.all_windows.retain(|w| w != &window);
                                if let Some(name) = target_name.as_ref() {
                                    state.window_mapped.insert(name.clone(), false);
                                }
                                state.refocus_topmost_native_window();
                                state.metadata_dirty = true;
                            }
                            "focus" => {
                                let surface = window
                                    .toplevel()
                                    .map(|t| t.wl_surface().clone())
                                    .or_else(|| {
                                        window.x11_surface().and_then(|x| x.wl_surface().clone())
                                    });
                                if state.space.element_location(&window).is_none() {
                                    use smithay::wayland::compositor::with_states;
                                    let parent_surface = window.toplevel().and_then(|t| {
                                        with_states(t.wl_surface(), |states| {
                                            states
                                                .data_map
                                                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                                                .and_then(|d| d.lock().ok().and_then(|data| data.parent.clone()))
                                        })
                                    });
                                    let (ow, oh) =
                                        (state.output_size.0 as i32, state.output_size.1 as i32);
                                    let location = if let Some(ref parent_wl) = parent_surface {
                                        let parent_center = state
                                            .space
                                            .elements()
                                            .find(|w| {
                                                w.toplevel()
                                                    .map(|t| t.wl_surface() == parent_wl)
                                                    .unwrap_or(false)
                                            })
                                            .map(|parent_win| {
                                                let loc = state
                                                    .space
                                                    .element_location(parent_win)
                                                    .unwrap_or_default();
                                                let geo = parent_win.geometry();
                                                (loc.x + geo.size.w / 2, loc.y + geo.size.h / 2)
                                            })
                                            .unwrap_or_else(|| (ow / 2, oh / 2));
                                        let dialog_y = (parent_center.1 - 75).max(PANEL_H);
                                        (parent_center.0 - 200, dialog_y)
                                    } else {
                                        let suggest_w = (ow * 2 / 3).max(640).min(1600);
                                        let suggest_h = (oh * 2 / 3).max(480).min(1000);
                                        let center_y = ((oh - suggest_h) / 2).max(PANEL_H + 20);
                                        ((ow - suggest_w) / 2, center_y)
                                    };
                                    let tname = target_name.clone().unwrap();
                                    state.window_mapped.insert(tname, true);
                                    state.space.map_element(window.clone(), location, true);
                                    state.metadata_dirty = true;
                                }

                                if let Some(surface) = surface {
                                    if window.toplevel().is_some() {
                                        use smithay::wayland::compositor::with_states;
                                        use smithay::wayland::xdg_activation::{
                                            XdgActivationToken, XdgActivationTokenData,
                                        };
                                        let token = XdgActivationToken::from(format!(
                                            "electron-focus-{}",
                                            event_start_time.elapsed().as_millis()
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
                                                            d.lock().ok().and_then(|data| data.app_id.clone())
                                                        })
                                                })
                                            }),
                                            client_id: None,
                                            user_data: std::sync::Arc::new(
                                                smithay::utils::user_data::UserDataMap::new(),
                                            ),
                                        };
                                        state.request_activation(token, token_data, surface.clone());

                                        if let Some(keyboard) = state.seat.get_keyboard() {
                                            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                            keyboard.set_focus(state, Some(surface.clone()), serial);
                                        }

                                        if let Some(name) = target_name.as_ref() {
                                            state.keyboard_window_focus = Some(name.clone());
                                            state.pointer_window_focus = Some(name.clone());
                                        }
                                    } else if let Some(keyboard) = state.seat.get_keyboard() {
                                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                        keyboard.set_focus(state, Some(surface.clone()), serial);

                                        if let Some(name) = target_name.as_ref() {
                                            state.keyboard_window_focus = Some(name.clone());
                                            state.pointer_window_focus = Some(name.clone());
                                        }
                                    }
                                }
                            }
                            "minimize" => {
                                // Unmap the window from the space to hide it.
                                // The client receives no buffer commits while unmapped.
                                state.space.unmap_elem(&window);
                                state
                                    .window_mapped
                                    .insert(target_name.clone().unwrap(), false);
                                state.refocus_topmost_native_window();
                                state.metadata_dirty = true;
                            }
                            "maximize" => {
                                if let Some(toplevel) = window.toplevel() {
                                    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
                                    let (ow, oh) =
                                        (state.output_size.0 as i32, state.output_size.1 as i32);
                                    let y_offset = th.max(PANEL_H);
                                    let content_h = (oh - y_offset).max(1);
                                    toplevel.with_pending_state(|s| {
                                        s.states.set(XdgState::Maximized);
                                        s.size = Some((ow, content_h).into());
                                    });
                                    toplevel.send_configure();
                                    state
                                        .space
                                        .map_element(window.clone(), (0, y_offset), false);
                                }
                            }
                            "pointer_motion" => {
                                if window.wl_surface().is_none() {
                                    // Surface not ready yet.
                                } else if let Some(loc) = state.space.element_location(&window) {
                                    let px = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("x"))
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);
                                    let py = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("y"))
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);

                                    let geo = window.geometry();
                                    let surface_x = px;
                                    let surface_y = py;

                                    let global: Point<f64, Logical> = (
                                        loc.x as f64 - geo.loc.x.max(0) as f64 + surface_x,
                                        loc.y as f64 - geo.loc.y.max(0) as f64 + surface_y,
                                    )
                                        .into();
                                    state.pointer_location = global;
                                    let under = window.surface_under(
                                        (surface_x, surface_y),
                                        smithay::desktop::WindowSurfaceType::ALL,
                                    );
                                    let focus = under.map(|(s, off)| {
                                        let surface_global: Point<f64, Logical> = (
                                            loc.x as f64 - geo.loc.x.max(0) as f64 + off.x as f64,
                                            loc.y as f64 - geo.loc.y.max(0) as f64 + off.y as f64,
                                        )
                                            .into();
                                        (s, surface_global)
                                    });
                                    let time = event_start_time.elapsed().as_millis() as u32;
                                    if let Some(pointer) = state.seat.get_pointer() {
                                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                        pointer.motion(
                                            state,
                                            focus,
                                            &smithay::input::pointer::MotionEvent {
                                                location: global,
                                                serial,
                                                time,
                                            },
                                        );
                                        pointer.frame(state);
                                    }
                                }
                            }
                            "pointer_button" => {
                                if window.wl_surface().is_none() {
                                    // Surface not ready yet.
                                } else if let Some(loc) = state.space.element_location(&window) {
                                    let button = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("button"))
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(272)
                                        as u32;
                                    let pressed = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("pressed"))
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    let px = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("x"))
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);
                                    let py = payload_json
                                        .as_ref()
                                        .and_then(|p| p.get("y"))
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);

                                    let geo = window.geometry();
                                    let surface_x = px;
                                    let surface_y = py;

                                    let global: Point<f64, Logical> = (
                                        loc.x as f64 - geo.loc.x.max(0) as f64 + surface_x,
                                        loc.y as f64 - geo.loc.y.max(0) as f64 + surface_y,
                                    )
                                        .into();
                                    state.pointer_location = global;
                                    if pressed {
                                        let surface = window
                                            .toplevel()
                                            .map(|t| t.wl_surface().clone())
                                            .or_else(|| {
                                                window.x11_surface().and_then(|x| x.wl_surface())
                                            });
                                        if let Some(surface) = surface {
                                            let serial =
                                                smithay::utils::SERIAL_COUNTER.next_serial();
                                            if let Some(keyboard) = state.seat.get_keyboard() {
                                                keyboard.set_focus(state, Some(surface), serial);
                                            }
                                        }
                                    }
                                    let btn_state = if pressed {
                                        smithay::backend::input::ButtonState::Pressed
                                    } else {
                                        smithay::backend::input::ButtonState::Released
                                    };
                                    let time = event_start_time.elapsed().as_millis() as u32;
                                    if let Some(pointer) = state.seat.get_pointer() {
                                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                        pointer.button(
                                            state,
                                            &smithay::input::pointer::ButtonEvent {
                                                button,
                                                state: btn_state,
                                                serial,
                                                time,
                                            },
                                        );
                                        pointer.frame(state);
                                    }
                                }
                            }
                            "pointer_leave" => {
                                let time = event_start_time.elapsed().as_millis() as u32;
                                if let Some(pointer) = state.seat.get_pointer() {
                                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                    pointer.motion(
                                        state,
                                        None,
                                        &smithay::input::pointer::MotionEvent {
                                            location: state.pointer_location,
                                            serial,
                                            time,
                                        },
                                    );
                                    pointer.frame(state);
                                }
                            }
                            "pointer_scroll" => {
                                let dx = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("dx"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let dy = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("dy"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let time = event_start_time.elapsed().as_millis() as u32;
                                if let Some(pointer) = state.seat.get_pointer() {
                                    let mut frame = smithay::input::pointer::AxisFrame::new(time)
                                        .value(Axis::Vertical, dy)
                                        .value(Axis::Horizontal, dx);
                                    if dy.abs() > 0.0 {
                                        frame =
                                            frame.v120(Axis::Vertical, (dy / 120.0 * 120.0) as i32);
                                    }
                                    if dx.abs() > 0.0 {
                                        frame = frame
                                            .v120(Axis::Horizontal, (dx / 120.0 * 120.0) as i32);
                                    }
                                    pointer.axis(state, frame);
                                    pointer.frame(state);
                                }
                            }
                            "keyboard_key" => {
                                let keycode = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("keycode"))
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                                let pressed = payload_json
                                    .as_ref()
                                    .and_then(|p| p.get("pressed"))
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let key_state = if pressed {
                                    KeyState::Pressed
                                } else {
                                    KeyState::Released
                                };
                                // Convert evdev scancode to XKB keycode (evdev + 8)
                                let xkb_keycode: u32 = keycode + 8;
                                let time = event_start_time.elapsed().as_millis() as u32;
                                if let Some(keyboard) = state.seat.get_keyboard() {
                                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                                    keyboard.input::<(), _>(
                                        state,
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
                            "map" => {
                                state
                                    .window_mapped
                                    .insert(action_msg.window_name.clone(), true);
                                state.metadata_dirty = true;
                            }
                            "focus" => {
                                state
                                    .window_mapped
                                    .insert(action_msg.window_name.clone(), true);
                                // Give keyboard and pointer logical focus to this window
                                state.keyboard_window_focus = Some(action_msg.window_name.clone());
                                state.pointer_window_focus = Some(action_msg.window_name.clone());
                                state.metadata_dirty = true;
                            }
                            "unmap" | "minimize" => {
                                state
                                    .window_mapped
                                    .insert(action_msg.window_name.clone(), false);
                                // Clear focus targeting this window so input isn't captured while minimized
                                if state.keyboard_window_focus.as_deref()
                                    == Some(&action_msg.window_name)
                                {
                                    state.keyboard_window_focus = None;
                                }
                                if state.pointer_window_focus.as_deref()
                                    == Some(&action_msg.window_name)
                                {
                                    state.pointer_window_focus = None;
                                }
                                state.metadata_dirty = true;
                            }
                            "close" => {
                                if let Some(idx) = state
                                    .electron_processes
                                    .iter()
                                    .position(|p| p.name == action_msg.window_name)
                                {
                                    let mut proc = state.electron_processes.remove(idx);
                                    proc.kill();
                                }
                                state.texture_cache.remove(&action_msg.window_name);
                                state.window_positions.remove(&action_msg.window_name);
                                state.window_mapped.remove(&action_msg.window_name);
                                if state.keyboard_window_focus.as_deref()
                                    == Some(&action_msg.window_name)
                                {
                                    state.keyboard_window_focus = None;
                                }
                                if state.pointer_window_focus.as_deref()
                                    == Some(&action_msg.window_name)
                                {
                                    state.pointer_window_focus = None;
                                }
                                state.metadata_dirty = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        ElectronMessage::ForwardedPointer { window_name, x, y } => {
            state.handle_forwarded_pointer_event(&window_name, x, y);
        }
        ElectronMessage::ForwardedKeyboard {
            window_name,
            key,
            pressed,
            time,
        } => {
            state.handle_forwarded_keyboard_event(&window_name, key, pressed, time);
        }
        ElectronMessage::ForwardedRelativePointer {
            window_name,
            dx,
            dy,
        } => {
            state.handle_forwarded_relative_pointer_event(&window_name, dx, dy);
        }
        ElectronMessage::ForwardedPointerButton {
            window_name,
            x,
            y,
            button,
            pressed,
            time,
        } => {
            state.handle_forwarded_pointer_button(&window_name, x, y, button, pressed, time);
        }
        ElectronMessage::ForwardedPointerScroll {
            window_name,
            dx,
            dy,
        } => {
            state.handle_forwarded_pointer_scroll(&window_name, dx, dy);
        }
    }
}

fn send_window_metadata(state: &WoState) {
    let mut metadata_list = Vec::new();

    // Include configured Electron-managed windows
    for win_cfg in &state.config.windows {
        let (x, y, w, h) = state
            .window_positions
            .get(&win_cfg.name)
            .copied()
            .unwrap_or((win_cfg.x, win_cfg.y, win_cfg.width, win_cfg.height));
        let mapped = state
            .window_mapped
            .get(&win_cfg.name)
            .copied()
            .unwrap_or(true);

        let is_focused = state.pointer_window_focus.as_ref() == Some(&win_cfg.name);

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

    // Include native Wayland client windows from all_windows tracking (includes minimized).
    let wayland_windows: Vec<_> = state.all_windows.clone();
    for window in wayland_windows.iter() {
        let window_name = match state.wayland_window_name(window) {
            Some(n) => n.clone(),
            None => continue,
        };

        let title = if let Some(x11) = window.x11_surface() {
            x11.title().to_string()
        } else {
            window
                .toplevel()
                .and_then(|t| {
                    smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                        states
                            .data_map
                            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                            .and_then(|d| d.lock().ok().and_then(|data| data.title.clone()))
                    })
                })
                .unwrap_or_else(|| window_name.clone())
        };

        let app_id = if let Some(x11) = window.x11_surface() {
            Some(x11.class().to_string())
        } else {
            window.toplevel().and_then(|t| {
                smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().ok().and_then(|data| data.app_id.clone()))
                })
            })
        };

        let loc = state.space.element_location(window).unwrap_or_default();
        let geo = window.geometry();

        let bbox_w = geo.size.w + geo.loc.x.max(0) * 2;
        let bbox_h = geo.size.h + geo.loc.y.max(0) * 2;

        let th = state.title_h_for_window(window);
        let is_ssd = th > 0;

        let (parent_surface, parent_x11_id) = if let Some(x11) = window.x11_surface() {
            (None, x11.is_transient_for())
        } else {
            (window.toplevel().and_then(|t| {
                smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().ok().and_then(|data| data.parent.clone()))
                })
            }), None)
        };
        
        let parent_name = if let Some(parent_wl) = parent_surface.as_ref() {
            use smithay::reexports::wayland_server::Resource;
            state.wayland_window_names.get(&parent_wl.id()).cloned()
        } else if let Some(parent_id) = parent_x11_id {
            state.x11_window_names.get(&parent_id).cloned()
        } else {
            None
        };
        
        let is_dialog = parent_name.is_some();

        let is_focused = if let Some(x11) = window.x11_surface() {
            x11.wl_surface()
                .map(|s| state.surface_has_keyboard_focus(&s))
                .unwrap_or(false)
        } else {
            window
                .toplevel()
                .map(|t| state.surface_has_keyboard_focus(t.wl_surface()))
                .unwrap_or(false)
        };

        let space_idx = state
            .space
            .elements()
            .position(|w| w == window)
            .map(|pos| 1000 - pos as i32)
            .unwrap_or(0);

        let mapped = state.space.element_location(window).is_some();
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
            "mapped": mapped,
            "source": if window.x11_surface().is_some() { "x11" } else { "wayland" },
            "ssd": is_ssd,
            "dialog": is_dialog,
            "parent_name": parent_name,
        }));
    }

    let metadata_msg = serde_json::json!({
        "type": "metadata",
        "windows": metadata_list,
    });

    if let Some(ref ipc) = state.electron_ipc {
        let _ = ipc.broadcast_metadata(&metadata_msg.to_string());
    }
}
