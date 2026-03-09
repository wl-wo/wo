use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_dmabuf,
    wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, DmabufFeedback, ImportNotifier}
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::state::WoState;

impl DmabufHandler for WoState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        tracing::info!("dmabuf_imported: accepting DMABUF unconditionally");
        // Frames from Electron arrive via the custom IPC channel, not via the
        // Wayland DMABUF protocol. However, native Wayland clients (and Electron's
        // own GPU subprocess when it connects as a Wayland client) import DMABUFs
        // through this path. We accept unconditionally here without touching the
        // GPU — actual validation happens when the buffer is used for rendering.
        //
        // IMPORTANT: Dropping the notifier WITHOUT calling .successful() sends a
        // failure response to the client, preventing it from creating DMABUF-backed
        // wl_buffers entirely and causing the app (and rendering) to freeze.
        let _ = notifier.successful::<WoState>();
        tracing::info!("dmabuf_imported: done");
    }

    fn new_surface_feedback(
        &mut self,
        _surface: &WlSurface,
        _global: &DmabufGlobal,
    ) -> Option<DmabufFeedback> {
        // Returning None lets the client use the global/default DMABUF feedback.
        // Per-surface feedback was causing the compositor to freeze inside
        // dispatch_clients() — Smithay's internal feedback-sending machinery
        // (format table FD transfer) hangs when sending per-surface feedback.
        None
    }
}

delegate_dmabuf!(WoState);
