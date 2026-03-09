/// xdg-desktop-portal implementation for Wo compositor

use anyhow::{Context, Result};
use pipewire as pw;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Portal session state
#[derive(Debug, Clone)]
pub struct PortalSession {
    pub session_id: String,
    pub source_type: SourceType,
    pub target: Option<String>,  // "screen" or window name
    pub active: bool,
}

/// Portal source types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    Monitor,
    Window,
    Virtual,
}

/// Portal capture request
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureRequest {
    pub source_type: SourceType,
    pub target: Option<String>,
    pub cursor_mode: Option<u32>,
    pub restore_token: Option<String>,
}

/// Portal capture response
#[derive(Debug, Clone, Serialize)]
pub struct CaptureResponse {
    pub session_id: String,
    pub streams: Vec<StreamInfo>,
}

/// Stream information for PipeWire
#[derive(Debug, Clone, Serialize)]
pub struct StreamInfo {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub source_type: SourceType,
}

/// xdg-desktop-portal handler
pub struct WoPortal {
    sessions: Arc<Mutex<HashMap<String, PortalSession>>>,
    next_session_id: Arc<Mutex<u64>>,
    streams: Arc<Mutex<HashMap<u32, Arc<ScreenCaptureStream>>>>,
    compositor_socket: Option<String>,
}

impl WoPortal {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(Mutex::new(1)),
            streams: Arc::new(Mutex::new(HashMap::new())),
            compositor_socket: None,
        }
    }

    pub fn with_compositor_socket(mut self, socket: String) -> Self {
        self.compositor_socket = Some(socket);
        self
    }

    /// Request capture from compositor via IPC
    fn request_compositor_capture(&self, window: &Option<String>, source_type: SourceType) -> Result<()> {
        if let Some(socket_path) = &self.compositor_socket {
            use std::os::unix::net::UnixStream;
            use std::io::Write;

            // Connect to compositor IPC
            if let Ok(mut stream) = UnixStream::connect(socket_path) {
                let request = serde_json::json!({
                    "type": "portal_capture_request",
                    "source_type": source_type,
                    "window": window,
                });
                let msg = serde_json::to_string(&request)?;
                let _ = stream.write_all(msg.as_bytes());
                info!("Requested capture from compositor: {:?}", source_type);
            } else {
                warn!("Could not connect to compositor socket: {}", socket_path);
            }
        }
        Ok(())
    }

    /// Create a new portal session
    pub fn create_session(&self) -> String {
        let mut next_id = self.next_session_id.lock().unwrap();
        let session_id = format!("wo_portal_session_{}", *next_id);
        *next_id += 1;

        let session = PortalSession {
            session_id: session_id.clone(),
            source_type: SourceType::Monitor,
            target: None,
            active: false,
        };

        self.sessions.lock().unwrap().insert(session_id.clone(), session);
        info!("Created portal session: {}", session_id);
        session_id
    }

    /// Select sources for a session
    pub fn select_sources(
        &self,
        session_id: &str,
        request: CaptureRequest,
    ) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(session_id)
            .context("session not found")?;

        session.source_type = request.source_type;
        session.target = request.target.clone();

        debug!(
            "Selected sources for session {}: source_type={:?}, target={:?}",
            session_id, request.source_type, request.target
        );
        Ok(())
    }

    /// Start a capture session
    pub fn start_session(
        &self,
        session_id: &str,
        width: u32,
        height: u32,
    ) -> Result<CaptureResponse> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(session_id)
            .context("session not found")?;

        session.active = true;

        // Generate a PipeWire node ID based on session
        let node_id = session_id
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(1) + 100; // Offset to avoid conflicts

        // Create the actual PipeWire stream
        let stream = Arc::new(ScreenCaptureStream::new(
            node_id,
            width,
            height,
            session.target.clone(),
        )?);

        // Connect the stream — returns the real PipeWire-assigned node ID
        let real_node_id = stream.connect()?;

        // Store the stream keyed by its real PipeWire node ID
        let mut streams = self.streams.lock().unwrap();
        streams.insert(real_node_id, stream);

        // Request capture from compositor
        self.request_compositor_capture(&session.target, session.source_type)?;

        let stream_info = StreamInfo {
            node_id: real_node_id,
            width,
            height,
            source_type: session.source_type,
        };

        info!(
            "Started portal session {}: {}x{} (node {})",
            session_id, width, height, real_node_id
        );

        Ok(CaptureResponse {
            session_id: session_id.to_string(),
            streams: vec![stream_info],
        })
    }

    /// Stop a capture session
    pub fn stop_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(session_id) {
            session.active = false;
            info!("Stopped portal session: {}", session_id);
        }
        Ok(())
    }

    /// Close a portal session
    pub fn close_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_id);
        info!("Closed portal session: {}", session_id);
    }

    /// Get active sessions
    pub fn get_active_sessions(&self) -> Vec<PortalSession> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.active)
            .cloned()
            .collect()
    }

    /// Get session by ID
    pub fn get_session(&self, session_id: &str) -> Option<PortalSession> {
        self.sessions.lock().unwrap().get(session_id).cloned()
    }

    /// Push a DMABUF frame to appropriate streams
    pub fn push_frame_to_streams(
        &self,
        window_name: Option<&str>,
        dmabuf_fd: i32,
        _width: u32,
        _height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    ) -> Result<()> {
        let streams = self.streams.lock().unwrap();
        
        for stream in streams.values() {
            // Check if this stream is for this window
            let matches = match (&stream.window_name, window_name) {
                (Some(stream_win), Some(win)) => stream_win == win,
                (None, None) => true, // Monitor/screen capture
                _ => false,
            };

            if matches && stream.is_active() {
                stream.push_frame(dmabuf_fd, stride, offset, modifier)?;
            }
        }

        Ok(())
    }

    /// Get the stream for a given node ID
    pub fn get_stream(&self, node_id: u32) -> Option<Arc<ScreenCaptureStream>> {
        self.streams.lock().unwrap().get(&node_id).cloned()
    }

    /// Push raw pixel data (ARGB8888/BGRx) to matching portal streams.
    /// Called from the compositor after offscreen capture or full-screen compositing.
    pub fn push_pixels_to_streams(
        &self,
        window_name: Option<&str>,
        pixels: &[u8],
    ) {
        let streams = self.streams.lock().unwrap();
        for stream in streams.values() {
            let matches = match (&stream.window_name, window_name) {
                (Some(stream_win), Some(win)) => stream_win == win,
                (None, None) => true,      // Monitor capture gets full screen
                (None, Some(_)) => true,   // Monitor capture sees all windows
                _ => false,
            };
            if matches && stream.is_active() {
                stream.push_pixels(pixels);
            }
        }
    }
}

impl Default for WoPortal {
    fn default() -> Self {
        Self::new()
    }
}

/// D-Bus handler for xdg-desktop-portal
pub struct PortalDBusHandler {
    portal: Arc<WoPortal>,
}

impl PortalDBusHandler {
    pub fn new(portal: Arc<WoPortal>) -> Self {
        Self { portal }
    }

    /// Handle D-Bus method calls
    pub fn handle_method_call(
        &self,
        interface: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        debug!("Portal method call: {}.{}", interface, method);

        match (interface, method) {
            ("org.freedesktop.portal.ScreenCast", "CreateSession") => {
                let session_id = self.portal.create_session();
                Ok(serde_json::json!({
                    "session_handle": format!("/org/freedesktop/portal/desktop/session/{}", session_id),
                }))
            }
            ("org.freedesktop.portal.ScreenCast", "SelectSources") => {
                let request: CaptureRequest = serde_json::from_value(args)?;
                let session_id = request.restore_token.clone().unwrap_or_else(|| "default".to_string());
                self.portal.select_sources(&session_id, request)?;
                Ok(serde_json::json!({ "success": true }))
            }
            ("org.freedesktop.portal.ScreenCast", "Start") => {
                let session_id = args
                    .get("session_handle")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                let width = args.get("width").and_then(|v| v.as_u64()).unwrap_or(1920) as u32;
                let height = args.get("height").and_then(|v| v.as_u64()).unwrap_or(1080) as u32;

                let response = self.portal.start_session(session_id, width, height)?;
                Ok(serde_json::to_value(response)?)
            }
            _ => {
                warn!("Unhandled portal method: {}.{}", interface, method);
                Ok(serde_json::json!({ "error": "not implemented" }))
            }
        }
    }
}

/// PipeWire stream implementation for screen capture with real frame streaming.
/// Spawns a dedicated PipeWire thread that produces video frames sourced from
/// the compositor's offscreen capture (raw ARGB8888/BGRx pixels).
pub struct ScreenCaptureStream {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub window_name: Option<String>,
    active: Arc<Mutex<bool>>,
    frame_counter: Arc<Mutex<u64>>,
    frame_queue: Arc<Mutex<Vec<PipeWireFrame>>>,
    /// Latest raw pixel buffer (ARGB8888/BGRx) for PipeWire consumption.
    /// The PipeWire process callback copies from this into PipeWire-allocated buffers.
    latest_pixels: Arc<Mutex<Option<Vec<u8>>>>,
}

/// Frame data for PipeWire streaming
#[derive(Clone, Debug)]
pub struct PipeWireFrame {
    pub pts: u64,  // Presentation timestamp
    pub dmabuf_fd: i32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
}

impl ScreenCaptureStream {
    pub fn new(node_id: u32, width: u32, height: u32, window_name: Option<String>) -> Result<Self> {
        info!(
            "Created PipeWire stream {} for {}x{} capture (window: {:?})",
            node_id, width, height, window_name
        );
        
        Ok(Self {
            node_id,
            width,
            height,
            window_name,
            active: Arc::new(Mutex::new(false)),
            frame_counter: Arc::new(Mutex::new(0)),
            frame_queue: Arc::new(Mutex::new(Vec::new())),
            latest_pixels: Arc::new(Mutex::new(None)),
        })
    }

    /// Push a frame to the PipeWire stream with DMABUF
    pub fn push_frame(&self, dmabuf_fd: i32, stride: u32, offset: u32, modifier: u64) -> Result<()> {
        if !*self.active.lock().unwrap() {
            return Ok(()); // Stream not active
        }

        let mut counter = self.frame_counter.lock().unwrap();
        let pts = *counter;
        *counter += 1;

        // Queue frame for consumption by PipeWire
        let frame = PipeWireFrame {
            pts,
            dmabuf_fd,
            stride,
            offset,
            modifier,
            width: self.width,
            height: self.height,
        };

        let mut queue = self.frame_queue.lock().unwrap();
        queue.push(frame.clone());
        
        // Keep queue bounded to prevent memory issues
        if queue.len() > 30 {  // 30 frame buffer
            queue.remove(0);
        }

        debug!(
            "Stream {} frame {}: DMABUF fd={}, stride={}, offset={}, modifier={:#x}",
            self.node_id, pts, dmabuf_fd, stride, offset, modifier
        );

        Ok(())
    }

    /// Get the next frame from the queue (for PipeWire processing)
    pub fn dequeue_frame(&self) -> Option<PipeWireFrame> {
        let mut queue = self.frame_queue.lock().unwrap();
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// Push raw pixel data (ARGB8888/BGRx) for PipeWire consumption.
    /// The PipeWire process callback will copy from this buffer into PipeWire-allocated buffers.
    pub fn push_pixels(&self, pixels: &[u8]) {
        if !self.is_active() {
            return;
        }
        let mut counter = self.frame_counter.lock().unwrap();
        *counter += 1;
        let mut latest = self.latest_pixels.lock().unwrap();
        *latest = Some(pixels.to_vec());
    }

    /// Connect and activate the stream.
    /// Spawns a PipeWire thread that creates a Video/Source node.
    /// Returns the real PipeWire-assigned node ID (which consumers use to subscribe).
    pub fn connect(&self) -> Result<u32> {
        *self.active.lock().unwrap() = true;

        let width = self.width;
        let height = self.height;
        let fallback_node_id = self.node_id;
        let active = self.active.clone();
        let latest_pixels = self.latest_pixels.clone();

        let (node_tx, node_rx) = std::sync::mpsc::sync_channel::<u32>(1);

        std::thread::Builder::new()
            .name(format!("pw-capture-{}", self.node_id))
            .spawn(move || {
                if let Err(e) = run_pw_output_stream(
                    width,
                    height,
                    active.clone(),
                    latest_pixels,
                    node_tx,
                ) {
                    warn!("PipeWire stream thread failed: {e:#}");
                    // Mark inactive on failure so the stream reports itself as dead.
                    *active.lock().unwrap() = false;
                }
            })
            .context("Failed to spawn PipeWire thread")?;

        // Wait for PipeWire to assign the real node ID.
        match node_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(real_node_id) => {
                info!(
                    "Stream {}: PipeWire ready with node_id {} ({}x{}, 60fps)",
                    fallback_node_id, real_node_id, width, height
                );
                Ok(real_node_id)
            }
            Err(_) => {
                warn!(
                    "Stream {}: PipeWire did not become ready within 5s, using fallback node_id",
                    fallback_node_id
                );
                Ok(fallback_node_id)
            }
        }
    }

    pub fn is_active(&self) -> bool {
        *self.active.lock().unwrap()
    }

    pub fn disconnect(&self) {
        *self.active.lock().unwrap() = false;
        debug!("Stream {}: disconnected", self.node_id);
    }

    pub fn frame_count(&self) -> u64 {
        *self.frame_counter.lock().unwrap()
    }

    pub fn queue_size(&self) -> usize {
        self.frame_queue.lock().unwrap().len()
    }
}

impl Drop for ScreenCaptureStream {
    fn drop(&mut self) {
        self.disconnect();
        debug!("Stream {}: dropped (pushed {} frames)", self.node_id, self.frame_count());
    }
}

// ── PipeWire output stream implementation ───────────────────────────────────

/// User data passed into PipeWire stream callbacks.
struct PwStreamUserData {
    active: Arc<Mutex<bool>>,
    latest_pixels: Arc<Mutex<Option<Vec<u8>>>>,
    width: u32,
    _height: u32,
    /// One-shot channel to report the assigned PipeWire node ID back to the
    /// caller of `connect()`. Consumed (taken) on the first state transition
    /// to `Paused`.
    node_tx: Option<std::sync::mpsc::SyncSender<u32>>,
}

/// Run a PipeWire output (Video/Source) stream on a dedicated thread.
///
/// The stream advertises BGRx video at the requested resolution and frame rate.
/// When a consumer (OBS, xdg-desktop-portal client, etc.) connects, the
/// `process` callback copies the latest pixel buffer from `latest_pixels` into
/// PipeWire-allocated mapped buffers.
fn run_pw_output_stream(
    width: u32,
    height: u32,
    active: Arc<Mutex<bool>>,
    latest_pixels: Arc<Mutex<Option<Vec<u8>>>>,
    node_tx: std::sync::mpsc::SyncSender<u32>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| anyhow::anyhow!("PipeWire MainLoop creation failed: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow::anyhow!("PipeWire Context creation failed: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow::anyhow!("PipeWire daemon connection failed: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "wo-screen-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| anyhow::anyhow!("PipeWire stream creation failed: {e}"))?;

    let user_data = PwStreamUserData {
        active,
        latest_pixels,
        width,
        _height: height,
        node_tx: Some(node_tx),
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|stream_ref, ud, old, new| {
            info!("PipeWire stream state: {:?} -> {:?}", old, new);
            // Report the assigned node ID once the stream reaches Paused
            // (the first stable state after connection where node_id is valid).
            if matches!(new, pw::stream::StreamState::Paused) {
                if let Some(tx) = ud.node_tx.take() {
                    let node_id = stream_ref.node_id();
                    info!("PipeWire assigned node_id: {}", node_id);
                    let _ = tx.send(node_id);
                }
            }
        })
        .param_changed(|_, _, _id, _param| {
            // Format negotiation is handled automatically by PipeWire.
        })
        .process(|stream_ref, ud| {
            if !*ud.active.lock().unwrap() {
                return;
            }

            let pixels_guard = ud.latest_pixels.lock().unwrap();
            if let Some(ref pixels) = *pixels_guard {
                if let Some(mut buffer) = stream_ref.dequeue_buffer() {
                    let datas = buffer.datas_mut();
                    if !datas.is_empty() {
                        let data = &mut datas[0];
                        if let Some(slice) = data.data() {
                            let copy_len = slice.len().min(pixels.len());
                            slice[..copy_len].copy_from_slice(&pixels[..copy_len]);
                        }
                        let chunk = data.chunk_mut();
                        *chunk.size_mut() = pixels.len() as u32;
                        *chunk.stride_mut() = (ud.width * 4) as i32;
                        *chunk.offset_mut() = 0;
                    }
                }
            }
        })
        .register()
        .map_err(|e| anyhow::anyhow!("PipeWire listener registration failed: {e}"))?;

    // Build SPA pod describing the video format we produce: BGRx at (width x height), 60 fps.
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGBA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width, height },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 60, denom: 1 },
            pw::spa::utils::Fraction { num: 1, denom: 1 },
            pw::spa::utils::Fraction {
                num: 120,
                denom: 1
            }
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("SPA pod serialization failed: {e:?}"))?
    .0
    .into_inner();

    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse SPA pod from serialized bytes"))?];

    stream
        .connect(
            pw::spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| anyhow::anyhow!("PipeWire stream connect failed: {e}"))?;

    info!("PipeWire output stream connected, entering main loop");
    mainloop.run();

    info!("PipeWire output stream main loop exited");
    Ok(())
}
