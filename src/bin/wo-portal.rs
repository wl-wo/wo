/// xdg-desktop-portal-wo - Standalone portal service for Wo compositor
///
/// This is a standalone D-Bus service that implements the xdg-desktop-portal
/// interfaces for screen capture. Applications can use this to request screen
/// or window capture without direct compositor integration.

use anyhow::Result;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::{Connection, interface, fdo};

use wo::state::portal::{WoPortal, CaptureRequest, SourceType};

fn request_screen_share_consent(
    app_id: &str,
    session_id: &str,
    requested_source: SourceType,
) -> Result<Option<(SourceType, Option<String>)>> {
    let mut stream = UnixStream::connect("/tmp/wo-portal-ui.sock")?;
    stream.set_read_timeout(Some(Duration::from_secs(95)))?;

    let requested = match requested_source {
        SourceType::Monitor => "Monitor",
        SourceType::Window => "Window",
        SourceType::Virtual => "Virtual",
    };

    let req = serde_json::json!({
        "type": "screen_share_request",
        "appName": if app_id.is_empty() { "Application" } else { app_id },
        "sessionId": session_id,
        "requestedSource": requested,
    });

    let payload = format!("{}\n", serde_json::to_string(&req)?);
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    if response.trim().is_empty() {
        return Ok(None);
    }

    let json: serde_json::Value = serde_json::from_str(response.trim())?;
    let allowed = json.get("allowed").and_then(|v| v.as_bool()).unwrap_or(false);
    if !allowed {
        return Ok(None);
    }

    let source = match json.get("sourceType").and_then(|v| v.as_str()).unwrap_or("Monitor") {
        "Window" => SourceType::Window,
        "Virtual" => SourceType::Virtual,
        _ => SourceType::Monitor,
    };
    let window_name = json
        .get("windowName")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(Some((source, window_name)))
}

/// D-Bus interface implementation for org.freedesktop.impl.portal.ScreenCast
struct ScreenCastImpl {
    portal: Arc<WoPortal>,
    session_map: Mutex<std::collections::HashMap<String, String>>,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastImpl {
    /// CreateSession method - Creates a new screen cast session
    async fn create_session(
        &self,
        #[zbus(header)] _hdr: zbus::message::Header<'_>,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        app_id: &str,
        _options: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> fdo::Result<(u32, std::collections::HashMap<String, zbus::zvariant::OwnedValue>)> {
        info!("CreateSession called: app_id={}, session={:?}", app_id, session_handle);

        let internal_session_id = self.portal.create_session();
        let external_session_id = session_handle
            .as_str()
            .split('/')
            .last()
            .unwrap_or("default")
            .to_string();
        self.session_map
            .lock()
            .unwrap()
            .insert(external_session_id, internal_session_id);

        let response: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = 
            std::collections::HashMap::new();
        
        Ok((0, response)) // 0 = success
    }

    /// SelectSources method - Selects which sources to capture
    async fn select_sources(
        &self,
        #[zbus(header)] _hdr: zbus::message::Header<'_>,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        app_id: &str,
        options: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> fdo::Result<(u32, std::collections::HashMap<String, zbus::zvariant::OwnedValue>)> {
        info!("SelectSources called for session: {:?}", session_handle);
        
        // Extract source type from options
        let source_type = if let Some(types) = options.get("types") {
            if let Ok(type_val) = types.downcast_ref::<u32>() {
                match type_val {
                    1 => SourceType::Monitor,
                    2 => SourceType::Window,
                    4 => SourceType::Virtual,
                    _ => SourceType::Monitor,
                }
            } else {
                SourceType::Monitor
            }
        } else {
            SourceType::Monitor
        };

        // Get session ID from session_handle path
        let external_session_id = session_handle
            .as_str()
            .split('/')
            .last()
            .unwrap_or("default");
        let session_id = self
            .session_map
            .lock()
            .unwrap()
            .get(external_session_id)
            .cloned()
            .unwrap_or_else(|| external_session_id.to_string());

        let consent = match request_screen_share_consent(app_id, &session_id, source_type) {
            Ok(v) => v,
            Err(e) => {
                warn!("Screen share consent UI failed: {}", e);
                None
            }
        };

        let Some((selected_source, selected_target)) = consent else {
            info!("Screen share request denied/cancelled for session: {}", session_id);
            let response: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
                std::collections::HashMap::new();
            return Ok((1, response));
        };

        let request = CaptureRequest {
            source_type: selected_source,
            target: selected_target,
            cursor_mode: options
                .get("cursor_mode")
                .and_then(|v| v.downcast_ref::<u32>().ok()),
            restore_token: None,
        };

        if let Err(e) = self.portal.select_sources(&session_id, request) {
            return Err(fdo::Error::Failed(format!("Failed to select sources: {}", e)));
        }

        let response: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = 
            std::collections::HashMap::new();
        
        Ok((0, response))
    }

    /// Start method - Starts the screen cast and returns active stream node ID
    async fn start(
        &self,
        #[zbus(header)] _hdr: zbus::message::Header<'_>,
        _handle: zbus::zvariant::ObjectPath<'_>,
        session_handle: zbus::zvariant::ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        _options: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> fdo::Result<(u32, std::collections::HashMap<String, zbus::zvariant::OwnedValue>)> {
        info!("Start called for session: {:?}", session_handle);

        let external_session_id = session_handle
            .as_str()
            .split('/')
            .last()
            .unwrap_or("default");
        let session_id = self
            .session_map
            .lock()
            .unwrap()
            .get(external_session_id)
            .cloned()
            .unwrap_or_else(|| external_session_id.to_string());

        // Get screen dimensions from compositor or use defaults
        let width = 1920;
        let height = 1080;

        match self.portal.start_session(&session_id, width, height) {
            Ok(capture_response) => {
                use zbus::zvariant::OwnedValue;
                
                let mut response: std::collections::HashMap<String, OwnedValue> = 
                    std::collections::HashMap::new();
                
                
                // Log stream activation and frame queueing begins
                for stream in &capture_response.streams {
                    let src_type = match stream.source_type {
                        wo::state::portal::SourceType::Monitor => "Monitor",
                        wo::state::portal::SourceType::Window => "Window",
                        wo::state::portal::SourceType::Virtual => "Virtual",
                    };
                    info!("Activated stream {}: {} [{}x{}] with frame queueing", 
                        stream.node_id, src_type, stream.width, stream.height);
                }
                
                // Return success status - actual streaming happens via frame queueing
                let stream_count = capture_response.streams.len() as i32;
                response.insert("status".to_string(), OwnedValue::from(0i32));
                response.insert("stream_count".to_string(), OwnedValue::from(stream_count));
                if let Some(first) = capture_response.streams.first() {
                    response.insert("primary_node_id".to_string(), OwnedValue::from(first.node_id as i32));
                }
                
                for stream in &capture_response.streams {
                    info!("  Frame queue for stream {}: 0/{} frames queued", stream.node_id, 30);
                }
                
                Ok((0, response))
            }
            Err(e) => {
                Err(fdo::Error::Failed(format!("Failed to start session: {}", e)))
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("xdg-desktop-portal-wo starting");

    // Load config to get compositor IPC socket path
    let config = wo::config::Config::load().unwrap_or_else(|_| {
        info!("Using default config");
        wo::config::Config::default()
    });

    // Create the portal backend with compositor socket
    let portal = Arc::new(
        WoPortal::new().with_compositor_socket(config.compositor.ipc_socket.clone())
    );
    
    // Connect to the session bus
    let connection = Connection::session().await?;

    // Create the ScreenCast interface implementation
    let screencast = ScreenCastImpl {
        portal: portal.clone(),
        session_map: Mutex::new(std::collections::HashMap::new()),
    };

    // Register the interface at the portal path
    connection
        .object_server()
        .at("/org/freedesktop/portal/desktop", screencast)
        .await?;

    // Request the well-known name
    connection
        .request_name("org.freedesktop.impl.portal.desktop.wo")
        .await?;

    info!("Portal service registered as org.freedesktop.impl.portal.desktop.wo");
    info!("Compositor IPC socket: {}", config.compositor.ipc_socket);
    info!("Listening for screen capture requests...");

    // Keep the service running
    std::future::pending::<()>().await;

    Ok(())
}
