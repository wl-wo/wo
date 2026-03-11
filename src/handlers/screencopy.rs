/// wlr-screencopy-unstable-v1 protocol handler for the Wo compositor.
///
/// Implements `zwlr_screencopy_manager_v1` allowing clients like grim, OBS, etc.
/// to request frame copies of outputs or output regions.

use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    protocol::{wl_buffer::WlBuffer, wl_shm},
};

use crate::state::WoState;

/// State for a pending screencopy frame capture.
#[derive(Debug)]
pub struct ScreencopyFrameState {
    /// Whether the cursor overlay was requested.
    pub overlay_cursor: bool,
    /// Capture region in output-logical coordinates (x, y, w, h).
    pub region: (i32, i32, i32, i32),
    /// Output width at frame creation time.
    pub output_width: u32,
    /// Output height at frame creation time.
    pub output_height: u32,
    /// Whether a buffer has been attached via `copy` or `copy_with_damage`.
    pub buffer_attached: bool,
    /// Whether we're waiting for damage (`copy_with_damage`).
    pub with_damage: bool,
}

/// Compositor-wide screencopy manager state.
pub struct ScreencopyManagerState {
    /// Pending frame requests waiting for the next render pass.
    pub pending_frames: Arc<Mutex<Vec<PendingFrame>>>,
}

/// A frame request queued for the next render pass.
pub struct PendingFrame {
    pub frame: ZwlrScreencopyFrameV1,
    pub buffer: WlBuffer,
    pub region: (i32, i32, i32, i32),
    pub overlay_cursor: bool,
    pub with_damage: bool,
}

impl ScreencopyManagerState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + 'static,
        D: Dispatch<ZwlrScreencopyManagerV1, ()> + 'static,
        D: Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState> + 'static,
    {
        display.create_global::<D, ZwlrScreencopyManagerV1, ()>(3, ());
        info!("Registered zwlr_screencopy_manager_v1 global (v3)");

        Self {
            pending_frames: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Take all pending frame requests for processing in the render loop.
    pub fn take_pending_frames(&self) -> Vec<PendingFrame> {
        let mut pending = self.pending_frames.lock().unwrap();
        std::mem::take(&mut *pending)
    }

    /// Check if there are any pending frame requests.
    pub fn has_pending_frames(&self) -> bool {
        let pending = self.pending_frames.lock().unwrap();
        !pending.is_empty()
    }
}

// ── GlobalDispatch for the manager ──────────────────────────────────────────

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for WoState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
        debug!("Client bound zwlr_screencopy_manager_v1");
    }
}

// ── Dispatch for the manager interface ──────────────────────────────────────

impl Dispatch<ZwlrScreencopyManagerV1, ()> for WoState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output: _,
            } => {
                let (w, h) = state.output_size;
                let frame_state = ScreencopyFrameState {
                    overlay_cursor: overlay_cursor != 0,
                    region: (0, 0, w as i32, h as i32),
                    output_width: w,
                    output_height: h,
                    buffer_attached: false,
                    with_damage: false,
                };

                let frame_resource = data_init.init(frame, frame_state);
                send_buffer_info(&frame_resource, w, h);
                debug!(
                    "CaptureOutput: {}x{}, cursor_overlay={}",
                    w, h, overlay_cursor != 0
                );
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output: _,
                x,
                y,
                width,
                height,
            } => {
                let (ow, oh) = state.output_size;
                // Clamp region to output bounds
                let rx = x.max(0);
                let ry = y.max(0);
                let rw = width.min(ow as i32 - rx).max(1);
                let rh = height.min(oh as i32 - ry).max(1);

                let frame_state = ScreencopyFrameState {
                    overlay_cursor: overlay_cursor != 0,
                    region: (rx, ry, rw, rh),
                    output_width: rw as u32,
                    output_height: rh as u32,
                    buffer_attached: false,
                    with_damage: false,
                };

                let frame_resource = data_init.init(frame, frame_state);
                send_buffer_info(&frame_resource, rw as u32, rh as u32);
                debug!(
                    "CaptureOutputRegion: ({},{}) {}x{}, cursor_overlay={}",
                    rx, ry, rw, rh, overlay_cursor != 0
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {
                debug!("Client destroyed screencopy manager");
            }
            _ => {}
        }
    }
}

/// Send buffer format info events to the frame client.
fn send_buffer_info(frame: &ZwlrScreencopyFrameV1, width: u32, height: u32) {
    let stride = width * 4; // ARGB8888 / BGRx

    // SHM buffer support (always available)
    frame.buffer(
        wl_shm::Format::Argb8888,
        width,
        height,
        stride,
    );

    // DMABUF support (v3)
    if frame.version() >= 3 {
        frame.linux_dmabuf(
            drm_fourcc::DrmFourcc::Argb8888 as u32,
            width,
            height,
        );
        frame.buffer_done();
    }
}

// ── Dispatch for the frame interface ────────────────────────────────────────

impl Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameState> for WoState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameState,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                queue_frame_copy(state, resource, buffer, data, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                queue_frame_copy(state, resource, buffer, data, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                debug!("Screencopy frame destroyed");
            }
            _ => {}
        }
    }
}

/// Queue a frame copy request for processing in the next render pass.
fn queue_frame_copy(
    state: &mut WoState,
    frame: &ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    data: &ScreencopyFrameState,
    with_damage: bool,
) {
    if let Some(ref screencopy_state) = state.screencopy_state {
        let pending = PendingFrame {
            frame: frame.clone(),
            buffer,
            region: data.region,
            overlay_cursor: data.overlay_cursor,
            with_damage,
        };

        screencopy_state
            .pending_frames
            .lock()
            .unwrap()
            .push(pending);

        debug!(
            "Queued screencopy frame: region={:?}, with_damage={}",
            data.region, with_damage
        );
    } else {
        warn!("Screencopy frame copy requested but screencopy state not initialized");
        frame.failed();
    }
}
