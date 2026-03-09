use serde::Deserialize;
use std::path::PathBuf;
use anyhow::{Context, Result};

/// Compositor configuration
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Global compositor settings
    #[serde(default)]
    pub compositor: CompositorConfig,

    /// Electron-rendered window definitions
    #[serde(default)]
    pub windows: Vec<WindowConfig>,

    /// Root/fullscreen desktop windows (alternative to [[windows]])
    #[serde(default)]
    pub root: Vec<WindowConfig>,

    /// Local asset directories to serve
    #[serde(default)]
    pub assets: Vec<AssetConfig>,

    /// Commands to autostart
    #[serde(default)]
    pub autostart: Vec<AutostartConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompositorConfig {
    /// DRM device path (e.g. "/dev/dri/card0"). Auto-detected if absent.
    pub drm_device: Option<String>,

    /// Force nested mode (run inside another compositor). Auto-detected if absent.
    pub nested: Option<bool>,

    /// Exit when Escape key is pressed (nested mode only)
    #[serde(default = "default_true")]
    pub exit_on_escape: bool,

    /// Initial window width in nested mode
    #[serde(default = "default_nested_width")]
    pub nested_width: u32,

    /// Initial window height in nested mode
    #[serde(default = "default_nested_height")]
    pub nested_height: u32,

    /// Wayland socket name (defaults to "wo-0")
    #[serde(default = "default_socket_name")]
    pub socket_name: String,

    /// IPC socket path the compositor listens on for Electron connections
    #[serde(default = "default_ipc_socket")]
    pub ipc_socket: String,

    /// Path to the Electron executable
    #[serde(default = "default_electron_path")]
    pub electron_path: String,

    /// Background colour as [r, g, b, a] 0.0–1.0
    #[serde(default = "default_background")]
    pub background: [f32; 4],

    /// Enable xdg-desktop-portal integration for screen/window capture
    #[serde(default)]
    pub enable_portal: bool,

    /// Path to the xdg-desktop-portal socket
    #[serde(default = "default_portal_socket")]
    pub portal_socket: String,

    /// Allow executing shell commands and file I/O via IPC syscalls
    #[serde(default)]
    pub enable_syscalls: bool,

    /// Send focus change notifications to windows
    #[serde(default)]
    pub focus_notify: bool,

    /// Pinned applications for the launcher dock
    #[serde(default)]
    pub applications: Vec<ApplicationConfig>,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            drm_device: None,
            nested: None,
            exit_on_escape: true,
            nested_width: default_nested_width(),
            nested_height: default_nested_height(),
            socket_name: default_socket_name(),
            ipc_socket: default_ipc_socket(),
            electron_path: default_electron_path(),
            background: default_background(),
            enable_portal: false,
            portal_socket: default_portal_socket(),
            enable_syscalls: false,
            focus_notify: false,
            applications: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct WindowConfig {
    pub name: String,
    pub url: Option<String>,
    pub html: Option<String>,
    pub css: Option<String>,
    pub js: Option<String>,
    
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub z_order: i32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub focusable: bool,
    #[serde(default = "default_true")]
    pub interactive: bool,
    #[serde(default)]
    pub floating: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetConfig {
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_asset_prefix")]
    pub serve_at: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct AutostartConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub delay: u64,
    #[serde(default)]
    pub restart: bool,
}

/// Application launcher configuration
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ApplicationConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub icon: Option<ApplicationIconConfig>,
    #[serde(default)]
    pub multi_instance: bool,
}

/// Icon configuration for applications
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ApplicationIconConfig {
    /// Iconify icon name (e.g., "mdi:firefox")
    Iconify(String),
    /// Full icon configuration with type and data
    Full {
        #[serde(rename = "type")]
        icon_type: IconType,
        data: String,
        #[serde(default)]
        mime_type: Option<String>,
        #[serde(default)]
        fallback: Option<Box<ApplicationIconConfig>>,
    },
}

/// Icon type specification
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IconType {
    Iconify,
    Base64,
    Url,
    Path,
}

impl ApplicationIconConfig {
    /// Convert to JSON representation for sending to clients
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            ApplicationIconConfig::Iconify(name) => serde_json::json!({
                "type": "iconify",
                "data": name,
            }),
            ApplicationIconConfig::Full {
                icon_type,
                data,
                mime_type,
                fallback,
            } => {
                let mut obj = serde_json::json!({
                    "type": match icon_type {
                        IconType::Iconify => "iconify",
                        IconType::Base64 => "base64",
                        IconType::Url => "url",
                        IconType::Path => "path",
                    },
                    "data": data,
                });
                if let Some(mime) = mime_type {
                    obj["mimeType"] = serde_json::Value::String(mime.clone());
                }
                if let Some(fb) = fallback {
                    obj["fallback"] = fb.to_json();
                }
                obj
            }
        }
    }
}

impl ApplicationConfig {
    /// Convert to JSON representation for sending to clients
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "name": &self.name,
            "command": &self.command,
        });
        if let Some(icon) = &self.icon {
            obj["icon"] = icon.to_json();
        }
        if self.multi_instance {
            obj["multi_instance"] = serde_json::Value::Bool(true);
        }
        obj
    }
}

// Defaults

fn default_socket_name() -> String { "wo-0".into() }
fn default_ipc_socket()  -> String { "/run/user/1000/wo-ipc.sock".into() }
fn default_electron_path() -> String { "electron".into() }
fn default_background()  -> [f32; 4] { [0.1, 0.1, 0.1, 1.0] }
fn default_portal_socket() -> String { "/run/user/1000/wo-portal.sock".into() }
fn default_width()       -> u32 { 1920 }
fn default_height()      -> u32 { 1080 }
fn default_nested_width() -> u32 { 1920 }
fn default_nested_height() -> u32 { 1080 }
fn default_fps()         -> u32 { 60 }
fn default_format()      -> String { "ARGB8888".into() }
fn default_asset_prefix() -> String { "file".into() }
fn default_true()        -> bool { true }

impl Config {
    pub fn load() -> Result<Self> {
        // Try multiple locations in order of priority:
        // 1. WO_CONFIG environment variable
        // 2. ./config.toml (current directory)
        // 3. ~/.config/wo/config.toml (user config)
        
        let paths = vec![
            std::env::var("WO_CONFIG").ok().map(PathBuf::from),
            Some(PathBuf::from("./config.toml")),
            Some(config_path()),
        ];

        for path_opt in paths.into_iter().flatten() {
            if path_opt.exists() {
                tracing::info!("Loading config from {path_opt:?}");
                let text = std::fs::read_to_string(&path_opt)
                    .with_context(|| format!("reading config {path_opt:?}"))?;

                return toml::from_str(&text)
                    .with_context(|| format!("parsing config {path_opt:?}"));
            }
        }

        tracing::warn!("No config found, using defaults");
        Ok(Self::default())
    }

    pub fn config_dir() -> PathBuf {
        dirs_config()
    }
}

fn config_path() -> PathBuf {
    dirs_config().join("config.toml")
}

fn dirs_config() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".config/wo")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            compositor: CompositorConfig::default(),
            windows: vec![],
            root: vec![],
            assets: vec![],
            autostart: vec![],
        }
    }
}
