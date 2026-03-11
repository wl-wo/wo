use smithay::{
    backend::{drm::DrmNode, renderer::{ImportDma, gles::GlesRenderer}},
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatHandler, SeatState, pointer::PointerHandle},
    output::Output,
    reexports::wayland_server::{
        Display, DisplayHandle, Resource, backend::{ClientData as WlClientData, ClientId, DisconnectReason, ObjectId}, protocol::wl_surface::WlSurface
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states}, cursor_shape::CursorShapeManagerState, dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufState}, fractional_scale::{
            self as fractional_scale_mod,
            FractionalScaleHandler, FractionalScaleManagerState,
        }, output::OutputManagerState,        selection::{
            SelectionHandler, data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, set_data_device_focus
            }
        },
        relative_pointer::RelativePointerManagerState,
        pointer_constraints::{self, PointerConstraintsHandler, PointerConstraintsState},
        keyboard_shortcuts_inhibit::{KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor},
        shell::xdg::{XdgShellState, decoration::XdgDecorationState}, shm::ShmState, single_pixel_buffer::SinglePixelBufferState, tablet_manager::TabletSeatHandler, viewporter::ViewporterState, xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData}, xwayland_shell::XWaylandShellState
    },
    xwayland::{XWayland, xwm::X11Wm},
};
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;

pub mod portal;
use crate::{
    config::Config,
    cursor::CursorThemeManager,
    dmabuf::TextureCache,
    electron::{ElectronIpc, ElectronProcess},
    handlers::screencopy::ScreencopyManagerState,
    syscall::SyscallHandler,
};
use portal::WoPortal;
use smithay::backend::allocator::dmabuf::Dmabuf;

/// A damage rectangle in buffer coordinates.
#[derive(Debug, Clone)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// mmap-backed memfd slot for zero-copy SHM export.
///
/// Instead of copying pixels into a `Vec` and later `write()`-ing to a memfd,
/// we keep the memfd persistently mmap'd so the compositor can `memcpy`
/// directly from the Wayland SHM pool into the mapped region — eliminating
/// two copies per frame.  Electron reads the same memfd via
/// `/proc/<pid>/fd/<fd>` without any additional data movement.
pub struct MappedShmSlot {
    pub fd: std::os::unix::io::OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: The mmap pointer is only accessed on the compositor main thread.
unsafe impl Send for MappedShmSlot {}

impl MappedShmSlot {
    /// Create a new slot backed by a memfd of `size` bytes.
    pub fn new(size: usize) -> anyhow::Result<Self> {
        use anyhow::Context;
        let fd = nix::sys::memfd::memfd_create(
            std::ffi::CStr::from_bytes_with_nul(b"wayland_window\0").unwrap(),
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )
        .context("creating memfd for MappedShmSlot")?;
        nix::unistd::ftruncate(&fd, size as i64).context("ftruncate memfd")?;

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            anyhow::bail!("mmap failed for MappedShmSlot");
        }

        Ok(Self { fd, ptr: ptr as *mut u8, len: size })
    }

    /// Resize the slot, remapping if the new size differs.
    pub fn ensure_size(&mut self, new_size: usize) -> anyhow::Result<()> {
        use anyhow::Context;
        if new_size == self.len {
            return Ok(());
        }
        // munmap old
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        nix::unistd::ftruncate(&self.fd, new_size as i64).context("ftruncate resize")?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                new_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.fd),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            anyhow::bail!("mmap failed on resize");
        }
        self.ptr = ptr as *mut u8;
        self.len = new_size;
        Ok(())
    }

    /// Get a mutable slice over the mapped region.
    ///
    /// # Safety
    /// Caller must ensure no concurrent reader is accessing the slot
    /// (the ping-pong scheme guarantees this).
    #[inline]
    pub unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.ptr, self.len)
    }

    /// Copy `src` into the mapped region starting at byte offset `offset`.
    /// # Safety
    /// Same as `as_mut_slice`.
    #[inline]
    pub unsafe fn write_at(&mut self, offset: usize, src: &[u8]) {
        debug_assert!(offset + src.len() <= self.len);
        std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(offset), src.len());
    }

    /// Write a single pixel (4 bytes) at the given byte offset.
    #[inline]
    pub unsafe fn write_pixel(&mut self, offset: usize, pixel: [u8; 4]) {
        debug_assert!(offset + 4 <= self.len);
        std::ptr::copy_nonoverlapping(pixel.as_ptr(), self.ptr.add(offset), 4);
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

impl Drop for MappedShmSlot {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

/// Ping-pong pair of mmap'd memfd slots for a window.
pub struct ShmSlotPair {
    pub slots: [MappedShmSlot; 2],
    pub write_idx: usize,
}

impl ShmSlotPair {
    pub fn new(size: usize) -> anyhow::Result<Self> {
        Ok(Self {
            slots: [MappedShmSlot::new(size)?, MappedShmSlot::new(size)?],
            write_idx: 0,
        })
    }

    /// Get the current write slot (mutably) and flip the index for next frame.
    pub fn write_slot_mut(&mut self) -> &mut MappedShmSlot {
        let idx = self.write_idx;
        self.write_idx = 1 - self.write_idx;
        &mut self.slots[idx]
    }

    /// Peek the current write slot index (before flip).
    pub fn current_write_idx(&self) -> usize {
        self.write_idx
    }
}

#[derive(Default)]
pub struct ClientData {
    pub compositor_state: CompositorClientState,
}

impl WlClientData for ClientData {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub struct BackendData {
    pub renderer: Option<std::sync::Arc<std::sync::Mutex<GlesRenderer>>>,
    pub output: Output,
    pub size: (u32, u32),
    pub dmabuf_formats: Option<Vec<drm_fourcc::DrmFormat>>,
}

impl BackendData {
    pub fn renderer(&self) -> Option<std::sync::Arc<std::sync::Mutex<GlesRenderer>>> {
        self.renderer.clone()
    }
}

pub struct WoState {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    // pub dmabuf_global:    DmabufGlobal,
    pub dmabuf_default_feedback: DmabufFeedback,
    pub dmabuf_scanout_feedback: DmabufFeedback,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub popup_manager: PopupManager,
    pub viewporter_state: ViewporterState,
    pub xdg_activation_state: XdgActivationState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub relative_pointer_manager_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub output_manager: OutputManagerState,

    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    pub output_size: (u32, u32),
    pub pointer_window_focus: Option<String>,
    pub keyboard_window_focus: Option<String>,

    pub space: Space<Window>,
    pub output: Output,

    pub electron_processes: Vec<ElectronProcess>,
    pub texture_cache: TextureCache,
    pub window_positions: std::collections::HashMap<String, (i32, i32, u32, u32)>,
    pub electron_ipc: Option<ElectronIpc>,
    pub syscall_handler: Option<SyscallHandler>,
    pub portal: Option<std::sync::Arc<WoPortal>>,
    /// wlr-screencopy protocol state for frame capture requests.
    pub screencopy_state: Option<ScreencopyManagerState>,
    /// Mapping state for named Electron windows. If false, the window should
    /// not be rendered (unmapped/minimized).
    pub window_mapped: std::collections::HashMap<String, bool>,
    /// Damage tracking: which window textures have been updated and need rendering
    pub damaged_windows: std::collections::HashSet<String>,
    /// Dirty flag for window metadata; set when windows appear/close/move/resize
    pub metadata_dirty: bool,
    /// Wayland surfaces that committed a new buffer and need offscreen capture.
    pub dirty_surfaces: std::collections::HashSet<WlSurface>,
    /// Ping-pong mmap'd memfd pairs per window for zero-copy SHM export.
    pub window_shm_slots: std::collections::HashMap<String, ShmSlotPair>,
    pub grab_state: Option<crate::handlers::xdg_shell::GrabState>,
    pub cursor_theme_manager: CursorThemeManager,

    /// Set to true inside new_toplevel to enable checkpoint logging on the
    /// same main-loop tick where the window appears.
    pub debug_new_toplevel: bool,

    /// Stable name mapping for Wayland windows (surface ObjectId → "wayland-N").
    pub wayland_window_names: std::collections::HashMap<ObjectId, String>,
    /// Reverse lookup: stable name → surface ObjectId for action dispatch.
    pub wayland_name_to_id: std::collections::HashMap<String, ObjectId>,
    /// Monotonic counter for assigning stable Wayland window IDs.
    pub next_wayland_id: u32,
    /// Surfaces that negotiated SSD via the xdg-decoration protocol.
    /// For these windows comraw renders its own title bar; for others (CSD)
    /// the client draws its own decorations and no extra title offset is added.
    pub ssd_windows: std::collections::HashSet<ObjectId>,
    /// Stable name mapping for X11 (XWayland) windows (X11 window_id → "x11-N").
    pub x11_window_names: std::collections::HashMap<u32, String>,

    /// XWayland instance handle (if running).
    pub xwayland: Option<XWayland>,
    /// X11 window manager attached to the XWayland instance.
    pub xwm: Option<X11Wm>,
    /// XWayland shell protocol state for associating X11 windows with wl_surfaces.
    pub xwayland_shell_state: Option<XWaylandShellState>,
    pub all_windows: Vec<Window>,

    pub backend: &'static BackendData,
    pub config: Config,
    pub running: bool,
    pub can_switch_vt: bool,
    pub pending_vt_switch: Option<u32>,
}

impl WoState {
    pub fn new(
        display: &mut Display<Self>,
        config: Config,
        render_node: DrmNode,
        backend: BackendData,
    ) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        // Register zxdg_output_manager_v1 so clients like GTK4 can get logical
        // output dimensions via the XDG output protocol.
        let output_manager = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let output_size = backend.size;
        let output = backend.output.clone();

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "wo");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("keyboard");
        seat.add_pointer();

        // Initialize syscall handler if enabled
        let syscall_handler = if config.compositor.enable_syscalls {
            let apps: Vec<serde_json::Value> = config
                .compositor
                .applications
                .iter()
                .map(|app| app.to_json())
                .collect();
            Some(SyscallHandler::new(true, false).with_applications(apps))
        } else {
            None
        };

        // Initialize portal if enabled
        let portal = if config.compositor.enable_portal {
            Some(std::sync::Arc::new(WoPortal::new()))
        } else {
            None
        };

        // Initialize wlr-screencopy protocol
        let screencopy_state = Some(ScreencopyManagerState::new::<Self>(&dh));

        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let dmabuf_state = DmabufState::new();

        // Get DMABUF formats from renderer if available
        let (default_feedback, scanout_feedback) = if let Some(renderer_arc) = &backend.renderer {
            let renderer_guard = renderer_arc.lock().expect("renderer mutex poisoned");
            let dmabuf_formats = renderer_guard.dmabuf_formats();
            let default_fb =
                DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats.clone())
                    .build()
                    .expect("building default dmabuf feedback");
            let scanout_fb =
                DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats.clone())
                    .add_preference_tranche(
                        render_node.dev_id(),
                        Some(TrancheFlags::Scanout),
                        dmabuf_formats,
                    )
                    .build()
                    .expect("building scanout dmabuf feedback");
            (default_fb, scanout_fb)
        } else if let Some(ref formats) = backend.dmabuf_formats {
            // For nested mode with pre-collected formats
            let default_fb = DmabufFeedbackBuilder::new(render_node.dev_id(), formats.clone())
                .build()
                .expect("building default dmabuf feedback");
            let scanout_fb = DmabufFeedbackBuilder::new(render_node.dev_id(), formats.clone())
                .add_preference_tranche(
                    render_node.dev_id(),
                    Some(TrancheFlags::Scanout),
                    formats.clone(),
                )
                .build()
                .expect("building scanout dmabuf feedback");
            (default_fb, scanout_fb)
        } else {
            // Fallback: create empty DMABUF feedbacks
            let default_fb = DmabufFeedbackBuilder::new(render_node.dev_id(), vec![])
                .build()
                .expect("building empty default dmabuf feedback");
            let scanout_fb = DmabufFeedbackBuilder::new(render_node.dev_id(), vec![])
                .build()
                .expect("building empty scanout dmabuf feedback");
            (default_fb, scanout_fb)
        };
        // Disable DMABUF advertising to force clients to use SHM for the pixel export pipeline.
        /*
        let dmabuf_global = dmabuf_state.create_global_with_default_feedback::<Self>(
            &dh,
            &default_feedback,
        );
        */
        let popup_manager = PopupManager::default();
        let space = Space::default();

        let cursor_theme_manager = CursorThemeManager::new(
            &config.compositor.cursor_theme,
            config.compositor.cursor_size,
        );

        let mut state = Self {
            display_handle: dh,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            // dmabuf_global,
            dmabuf_default_feedback: default_feedback,
            dmabuf_scanout_feedback: scanout_feedback,
            seat_state,
            data_device_state,
            popup_manager,
            viewporter_state,
            xdg_activation_state,
            single_pixel_buffer_state,
            fractional_scale_manager_state,
            cursor_shape_manager_state,
            relative_pointer_manager_state,
            pointer_constraints_state,
            keyboard_shortcuts_inhibit_state,
            output_manager,
            seat,
            pointer_location: (0.0, 0.0).into(),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            pointer_window_focus: None,
            keyboard_window_focus: None,
            output_size,
            space,
            output,
            electron_processes: vec![],
            texture_cache: TextureCache::default(),
            window_positions: std::collections::HashMap::new(),
            electron_ipc: None,
            syscall_handler,
            portal,
            screencopy_state,
            xwayland: None,
            xwm: None,
            xwayland_shell_state: None,
            all_windows: Vec::new(),
            backend: Box::leak(Box::new(backend)),
            config,
            running: true,
            window_mapped: std::collections::HashMap::new(),
            damaged_windows: std::collections::HashSet::new(),
            metadata_dirty: true,
            dirty_surfaces: std::collections::HashSet::new(),
            window_shm_slots: std::collections::HashMap::new(),
            grab_state: None,
            cursor_theme_manager,
            debug_new_toplevel: false,
            wayland_window_names: std::collections::HashMap::new(),
            wayland_name_to_id: std::collections::HashMap::new(),
            next_wayland_id: 0,
            ssd_windows: std::collections::HashSet::new(),
            x11_window_names: std::collections::HashMap::new(),
            can_switch_vt: false,
            pending_vt_switch: None,
        };

        // Map the output into the space at origin.
        state.space.map_output(&state.output, (0, 0));

        // Initialize mapping state for configured windows as mapped (visible).
        for win in &state.config.windows {
            state.window_mapped.insert(win.name.clone(), true);
        }

        state
    }

    /// Return the effective title-bar height for a Wayland window.
    /// SSD windows get TITLE_H (comraw draws the title bar); CSD windows get 0
    /// (the client draws its own decorations).
    pub fn title_h_for_surface(&self, surface: &WlSurface) -> i32 {
        if self.ssd_windows.contains(&surface.id()) {
            crate::handlers::xdg_shell::TITLE_H
        } else {
            0
        }
    }

    /// Return the effective title-bar height for a Window element.
    /// X11 windows always use SSD (comraw draws the chrome).
    pub fn title_h_for_window(&self, window: &Window) -> i32 {
        if window.x11_surface().is_some() {
            return crate::handlers::xdg_shell::TITLE_H;
        }
        window
            .toplevel()
            .map(|t| self.title_h_for_surface(t.wl_surface()))
            .unwrap_or(0)
    }

    /// Clamp a requested size to the client's min/max size hints.
    /// Returns `(width, height)` clamped to the XDG toplevel's declared
    /// min_size / max_size constraints.  A constraint component of 0 means
    /// "no limit" per the xdg-shell protocol.
    pub fn clamp_to_size_hints(&self, window: &Window, mut w: i32, mut h: i32) -> (i32, i32) {
        if let Some(toplevel) = window.toplevel() {
            use smithay::wayland::shell::xdg::SurfaceCachedState;
            with_states(toplevel.wl_surface(), |states| {
                let mut cached = states.cached_state.get::<SurfaceCachedState>();
                let current = cached.current();
                let min = current.min_size;
                let max = current.max_size;
                // min_size: (0,0) means no minimum
                if min.w > 0 {
                    w = w.max(min.w);
                }
                if min.h > 0 {
                    h = h.max(min.h);
                }
                // max_size: (0,0) means no maximum
                if max.w > 0 {
                    w = w.min(max.w);
                }
                if max.h > 0 {
                    h = h.min(max.h);
                }
            });
        }
        (w.max(1), h.max(1))
    }

    /// Return true if the given Wayland surface currently has keyboard focus.
    pub fn surface_has_keyboard_focus(&self, surface: &WlSurface) -> bool {
        self.seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .map(|focused| &focused == surface)
            .unwrap_or(false)
    }

    /// Get the stable name for a window (Wayland or X11), if assigned.
    pub fn wayland_window_name(&self, window: &Window) -> Option<&String> {
        let surface_id = window.toplevel().map(|t| t.wl_surface().id());
        if let Some(id) = surface_id {
            if let Some(name) = self.wayland_window_names.get(&id) {
                return Some(name);
            }
        }
        if let Some(x11) = window.x11_surface() {
            return self.x11_window_names.get(&x11.window_id());
        }
        None
    }

    /// Find a Wayland or X11 Window element by its stable name (even if minimized).
    pub fn find_wayland_window(&self, name: &str) -> Option<Window> {
        self.all_windows.iter().find(|w| {
            if let Some(toplevel) = w.toplevel() {
                if let Some(win_name) = self.wayland_window_names.get(&toplevel.wl_surface().id()) {
                    if win_name == name { return true; }
                }
            }
            if let Some(x11) = w.x11_surface() {
                if let Some(win_name) = self.x11_window_names.get(&x11.window_id()) {
                    if win_name == name { return true; }
                }
            }
            false
        }).cloned()
    }

    /// Focus the topmost mapped native window in `space`.
    pub fn refocus_topmost_native_window(&mut self) {
        let top = self.space.elements().next().cloned();

        let Some(window) = top else {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, None, serial);
            }
            self.keyboard_window_focus = None;
            self.pointer_window_focus = None;
            return;
        };

        let surface = window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .or_else(|| window.x11_surface().and_then(|x| x.wl_surface()));

        if let Some(surface) = surface {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface), serial);
            }

            if let Some(name) = self.wayland_window_name(&window).cloned() {
                self.keyboard_window_focus = Some(name.clone());
                self.pointer_window_focus = Some(name);
            }
        }
    }

    /// Forward input events from the compositor to Electron windows.
    /// This is called by the nested compositor's event loop.
    /// The actual forwarding is handled by process_input_event.
    pub fn forward_input_to_electron<B: smithay::backend::input::InputBackend>(
        &mut self,
        _event: &smithay::backend::input::InputEvent<B>,
    ) {
        // The actual forwarding is done by process_input_event which is called separately
        // This is a no-op wrapper that exists for API compatibility with nested.rs
        // All input forwarding is handled through the individual input event handlers
        // (on_keyboard, on_pointer_motion, on_pointer_button, etc.)
    }

    /// Return an exported DMABUF for the most-recent texture of a named
    /// Electron window, if available. Callers can use this to create a
    /// Wayland `wl_buffer` via `DmabufState` APIs for zero-copy sharing.
    pub fn exported_dmabuf_for_window(&self, name: &str) -> Option<Dmabuf> {
        self.texture_cache
            .get_dmabuf(name)
            .map(|t| t.dmabuf.clone())
    }

    /// Return the Wayland surface (and local pointer position) currently under the pointer.
    pub fn surface_under(
        &self,
    ) -> Option<(
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        Point<f64, Logical>,
    )> {
        let pos = self.pointer_location;
        self.space
            .element_under(pos)
            .and_then(|(window, window_pos)| {
                let local = pos - window_pos.to_f64();
                // Translate global space coordinate to surface-local coordinate
                // window.geometry().loc is the offset from surface origin to content area.
                let geo = window.geometry();
                let surface_local: Point<f64, Logical> = (local.x + geo.loc.x as f64, local.y + geo.loc.y as f64).into();
                window
                    .surface_under(surface_local, smithay::desktop::WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, p.to_f64() + window_pos.to_f64()))
            })
    }
}

impl SeatHandler for WoState {
    type KeyboardFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    type PointerFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    type TouchFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, target: Option<&Self::KeyboardFocus>) {
        let dh = &self.display_handle;
        let _ = target;
        let focus = None;
        set_data_device_focus(dh, seat, focus);
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
    }
}

smithay::delegate_seat!(WoState);

impl SelectionHandler for WoState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for WoState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for WoState {}
impl ServerDndGrabHandler for WoState {}

smithay::delegate_data_device!(WoState);

impl XdgActivationHandler for WoState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Grant all activation requests by giving keyboard focus to the surface.
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(surface), serial);
        }
    }
}

smithay::delegate_xdg_activation!(WoState);

impl FractionalScaleHandler for WoState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        tracing::info!("new_fractional_scale: setting preferred_scale(1.0)");
        // Immediately advertise scale 1.0.  Without this, GTK4 apps (and
        // Firefox's GTK layer) wait indefinitely for the preferred_scale
        // event before committing their first buffer, causing a visible freeze.
        with_states(&surface, |states| {
            fractional_scale_mod::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(1.0);
            });
        });
        tracing::info!("new_fractional_scale: done");
    }
}

smithay::delegate_fractional_scale!(WoState);
smithay::delegate_viewporter!(WoState);
smithay::delegate_single_pixel_buffer!(WoState);

impl TabletSeatHandler for WoState {}

smithay::delegate_cursor_shape!(WoState);

impl PointerConstraintsHandler for WoState {
    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }

    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // When a new constraint is created, check if it's a lock and notify Electron.
        pointer_constraints::with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(constraint) = constraint {
                if let smithay::wayland::pointer_constraints::PointerConstraint::Locked(_) =
                    &*constraint
                {
                    // Call activate to actually lock it in Smithay
                    constraint.activate();

                    if let Some(target_window) = self.space.elements().find(|w| {
                        w.toplevel()
                            .map(|t| t.wl_surface() == surface)
                            .unwrap_or(false)
                            || w.x11_surface()
                                .and_then(|x| x.wl_surface())
                                .map(|s| s == *surface)
                                .unwrap_or(false)
                    }) {
                        if let Some(name) = self.wayland_window_name(target_window) {
                            if let Some(ref ipc) = self.electron_ipc {
                                let _ = ipc.send_to_window(
                                    &name,
                                    &crate::electron::ElectronInputEvent::PointerLockRequest {
                                        window_name: name.clone(),
                                        lock: true,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        });
    }
}

smithay::delegate_pointer_constraints!(WoState);
smithay::delegate_relative_pointer!(WoState);

impl KeyboardShortcutsInhibitHandler for WoState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        // Automatically activate: games/fullscreen apps that request inhibition
        // should receive all key events without compositor interception.
        inhibitor.activate();
    }
}

smithay::delegate_keyboard_shortcuts_inhibit!(WoState);

/// Propagate environment variables to the DBus session and systemd user
/// manager so that applications launched via desktop entries, D-Bus
/// activation, or Steam inherit DISPLAY and WAYLAND_DISPLAY.
pub fn propagate_environment(vars: &[&str]) {
    use tracing::{info, warn};

    let var_args: Vec<String> = vars
        .iter()
        .filter_map(|&name| {
            std::env::var(name).ok().map(|val| format!("{name}={val}"))
        })
        .collect();

    if var_args.is_empty() {
        return;
    }

    let names: Vec<&str> = vars
        .iter()
        .filter(|&&name| std::env::var(name).is_ok())
        .copied()
        .collect();

    // dbus-update-activation-environment sets variables for D-Bus-activated services.
    let mut dbus_cmd = std::process::Command::new("dbus-update-activation-environment");
    dbus_cmd.arg("--systemd");
    for arg in &var_args {
        dbus_cmd.arg(arg);
    }
    match dbus_cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        Ok(mut child) => {
            let _ = child.wait();
            info!("Propagated {:?} to DBus activation environment", names);
        }
        Err(e) => warn!("dbus-update-activation-environment failed: {e}"),
    }

    // systemctl --user import-environment sets variables for systemd user services.
    let mut systemctl_cmd = std::process::Command::new("systemctl");
    systemctl_cmd.arg("--user").arg("import-environment");
    for name in &names {
        systemctl_cmd.arg(name);
    }
    match systemctl_cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
        Ok(mut child) => {
            let _ = child.wait();
            info!("Propagated {:?} to systemd user environment", names);
        }
        Err(e) => warn!("systemctl --user import-environment failed: {e}"),
    }
}
