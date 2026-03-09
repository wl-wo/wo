use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::state::WoState;

// ── Compositor ───────────────────────────────────────────────────────────────

impl CompositorHandler for WoState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        if let Some(data) = client.get_data::<crate::state::ClientData>() {
            return &data.compositor_state;
        }
        static LOGGED_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED_ONCE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::info!("Client missing ClientData (likely XWayland), using fallback compositor state");
        } else {
            tracing::debug!("Client missing ClientData (likely XWayland), using fallback compositor state");
        }
        static FALLBACK: std::sync::OnceLock<CompositorClientState> = std::sync::OnceLock::new();
        FALLBACK.get_or_init(CompositorClientState::default)
    }

    fn commit(&mut self, surface: &WlSurface) {
        if self.debug_new_toplevel {
            tracing::info!("commit handler ENTER (debug_new_toplevel=true)");
        }
        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &root).unwrap_or(false))
                .cloned()
            {
                window.on_commit();
                self.dirty_surfaces.insert(root.clone());
            } else if let Some(window) = self
                .space
                .elements()
                .find(|w| {
                    w.x11_surface()
                        .and_then(|x| x.wl_surface())
                        .map(|s| s == root)
                        .unwrap_or(false)
                })
                .cloned()
            {
                window.on_commit();
                self.dirty_surfaces.insert(root.clone());
            }

        }

        self.popup_manager.commit(surface);
        crate::handlers::xdg_shell::ensure_initial_configure(surface, &self.space, &mut self.popup_manager);
        if self.debug_new_toplevel {
            tracing::info!("commit handler EXIT");
        }
    }
}

delegate_compositor!(WoState);

// ── SHM ──────────────────────────────────────────────────────────────────────

impl BufferHandler for WoState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for WoState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_shm!(WoState);
