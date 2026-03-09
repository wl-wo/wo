/// Dynamic syscall handling for IPC

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tracing::{debug, warn};

/// Syscall request types
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SyscallRequest {
    /// List pinned/configured applications
    #[serde(rename = "list_applications")]
    ListApplications,
    /// Browse all installed applications
    #[serde(rename = "browse_applications")]
    BrowseApplications,
    /// Launch an application (fire-and-forget, non-blocking)
    #[serde(rename = "launch")]
    Launch {
        command: String,
        args: Option<Vec<String>>,
    },
    /// Execute a shell command
    #[serde(rename = "exec")]
    Exec {
        command: String,
        args: Option<Vec<String>>,
        #[serde(default)]
        capture_output: bool,
    },
    /// Read file contents
    #[serde(rename = "read")]
    Read {
        path: String,
        #[serde(default)]
        max_bytes: Option<usize>,
    },
    /// Write file contents
    #[serde(rename = "write")]
    Write {
        path: String,
        content: String,
        #[serde(default)]
        append: bool,
    },
    /// List directory contents
    #[serde(rename = "listdir")]
    ListDir {
        path: String,
    },
    /// Get file/directory metadata
    #[serde(rename = "stat")]
    Stat {
        path: String,
    },
    /// Delete file
    #[serde(rename = "delete")]
    Delete {
        path: String,
    },
    /// Custom extension syscall
    #[serde(rename = "custom")]
    Custom {
        name: String,
        #[serde(default)]
        payload: serde_json::Value,
    },
    #[serde(rename = "quit")]
    Quit,
    /// Power off the system
    #[serde(rename = "shutdown")]
    Shutdown,
    /// Reboot the system
    #[serde(rename = "restart")]
    Restart,
    /// Log out the current session
    #[serde(rename = "logout")]
    Logout,
    /// Lock the screen
    #[serde(rename = "lock")]
    Lock,
    /// Suspend the system
    #[serde(rename = "sleep")]
    Sleep,
}

/// Trait for external syscall handlers
pub trait ExtensionHandler: Send + Sync {
    /// Handle a custom syscall request
    /// Returns JSON response or error
    fn handle(&self, name: &str, payload: serde_json::Value) -> Result<serde_json::Value>;
}

/// Syscall response
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status")]
pub enum SyscallResponse {
    Success {
        data: serde_json::Value,
    },
    Error {
        message: String,
    },
}

/// Syscall handler with security policies
pub struct SyscallHandler {
    /// Allow executing arbitrary commands
    pub allow_exec: bool,
    /// Allow reading files outside home directory
    pub allow_read_system: bool,
    /// Allow writing files
    pub allow_write: bool,
    /// Custom extension handlers
    extensions: std::collections::HashMap<String, Arc<dyn ExtensionHandler>>,
    /// Application configurations for launcher
    pub applications: Vec<serde_json::Value>,
}

impl Default for SyscallHandler {
    fn default() -> Self {
        Self {
            allow_exec: true,
            allow_read_system: false,
            allow_write: false,
            extensions: std::collections::HashMap::new(),
            applications: vec![],
        }
    }
}

impl SyscallHandler {
    pub fn new(allow_exec: bool, allow_write: bool) -> Self {
        Self {
            allow_exec,
            allow_write,
            ..Default::default()
        }
    }

    /// Set applications list from config
    pub fn with_applications(mut self, apps: Vec<serde_json::Value>) -> Self {
        self.applications = apps;
        self
    }

    /// Register a custom extension handler
    pub fn register_extension(&mut self, name: String, handler: Arc<dyn ExtensionHandler>) {
        self.extensions.insert(name, handler);
    }

    /// Unregister an extension handler
    pub fn unregister_extension(&mut self, name: &str) -> Option<Arc<dyn ExtensionHandler>> {
        self.extensions.remove(name)
    }

    /// Process a syscall request
    pub fn handle(&self, request: SyscallRequest) -> SyscallResponse {
        match self.handle_inner(request) {
            Ok(data) => SyscallResponse::Success { data },
            Err(e) => {
                warn!("Syscall error: {:#}", e);
                SyscallResponse::Error {
                    message: format!("{:#}", e),
                }
            }
        }
    }

    fn handle_inner(&self, request: SyscallRequest) -> Result<serde_json::Value> {
        match request {
            SyscallRequest::ListApplications => {
                Ok(serde_json::Value::Array(self.applications.clone()))
            }
            SyscallRequest::BrowseApplications => {
                browse_applications()
            }
            SyscallRequest::Launch { command, args } => self.handle_launch(&command, args.as_deref()),
            SyscallRequest::Exec {
                command,
                args,
                capture_output,
            } => self.handle_exec(&command, args.as_deref(), capture_output),
            SyscallRequest::Read { path, max_bytes } => self.handle_read(&path, max_bytes),
            SyscallRequest::Write {
                path,
                content,
                append,
            } => self.handle_write(&path, &content, append),
            SyscallRequest::ListDir { path } => self.handle_list_dir(&path),
            SyscallRequest::Stat { path } => self.handle_stat(&path),
            SyscallRequest::Delete { path } => self.handle_delete(&path),
            SyscallRequest::Custom { name, payload } => self.handle_custom(&name, payload),
            SyscallRequest::Quit => {
                std::process::exit(0);
            }
            SyscallRequest::Shutdown => {
                handle_power_command("systemctl", &["poweroff"])
            }
            SyscallRequest::Restart => {
                handle_power_command("systemctl", &["reboot"])
            }
            SyscallRequest::Logout => {
                if let Ok(session_id) = std::env::var("XDG_SESSION_ID") {
                    handle_power_command("loginctl", &["terminate-session", &session_id])
                } else {
                    // Fallback: exit the compositor
                    std::process::exit(0);
                }
            }
            SyscallRequest::Lock => {
                handle_power_command("loginctl", &["lock-session"])
            }
            SyscallRequest::Sleep => {
                handle_power_command("systemctl", &["suspend"])
            }
        }
    }

    fn handle_launch(
        &self,
        command: &str,
        args: Option<&[String]>,
    ) -> Result<serde_json::Value> {
        if !self.allow_exec {
            bail!("exec syscalls are disabled");
        }

        tracing::info!("Launching command (fire-and-forget): {} {:?}", command, args);

        let mut cmd = Command::new("sh");
        let shell_cmd = if let Some(args) = args {
            format!("{} {}", command, args.join(" "))
        } else {
            command.to_string()
        };
        cmd.arg("-c").arg(&shell_cmd);

        // Detach from the compositor process so the child survives independently
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Use pre_exec to setsid so the child runs in its own session
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                tracing::info!("Launched process '{}' with PID {}", command, pid);
                Ok(serde_json::json!({
                    "pid": pid,
                    "command": command,
                }))
            }
            Err(e) => {
                bail!("failed to launch '{}': {}", command, e);
            }
        }
    }

    fn handle_exec(
        &self,
        command: &str,
        args: Option<&[String]>,
        capture_output: bool,
    ) -> Result<serde_json::Value> {
        if !self.allow_exec {
            bail!("exec syscalls are disabled");
        }

        debug!("Executing command: {} {:?}", command, args);

        let mut cmd = Command::new(command);
        if let Some(args) = args {
            cmd.args(args);
        }

        if capture_output {
            let output = cmd.output().context("executing command")?;
            Ok(serde_json::json!({
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "exit_code": output.status.code(),
                "success": output.status.success(),
            }))
        } else {
            // Non-blocking: spawn detached so we don't block the event loop
            use std::process::Stdio;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());

            match cmd.spawn() {
                Ok(child) => {
                    Ok(serde_json::json!({
                        "pid": child.id(),
                        "launched": true,
                    }))
                }
                Err(e) => {
                    bail!("failed to execute '{}': {}", command, e);
                }
            }
        }
    }

    fn handle_read(&self, path: &str, max_bytes: Option<usize>) -> Result<serde_json::Value> {
        let path = self.validate_read_path(path)?;
        debug!("Reading file: {:?}", path);

        let content = std::fs::read_to_string(&path).context("reading file")?;
        let content = if let Some(max) = max_bytes {
            content.chars().take(max).collect()
        } else {
            content
        };

        Ok(serde_json::json!({
            "content": content,
            "path": path.display().to_string(),
        }))
    }

    fn handle_write(&self, path: &str, content: &str, append: bool) -> Result<serde_json::Value> {
        if !self.allow_write {
            bail!("write syscalls are disabled");
        }

        let path = self.validate_write_path(path)?;
        debug!("Writing to file: {:?}", path);

        if append {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .context("opening file for append")?;
            file.write_all(content.as_bytes())
                .context("writing to file")?;
        } else {
            std::fs::write(&path, content).context("writing file")?;
        }

        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "bytes_written": content.len(),
        }))
    }

    fn handle_list_dir(&self, path: &str) -> Result<serde_json::Value> {
        let path = self.validate_read_path(path)?;
        debug!("Listing directory: {:?}", path);

        let entries: Vec<_> = std::fs::read_dir(&path)
            .context("reading directory")?
            .filter_map(|e| {
                let entry = e.ok()?;
                let metadata = entry.metadata().ok()?;
                Some(serde_json::json!({
                    "name": entry.file_name().to_string_lossy().to_string(),
                    "is_dir": metadata.is_dir(),
                    "is_file": metadata.is_file(),
                    "size": metadata.len(),
                }))
            })
            .collect();

        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "entries": entries,
        }))
    }

    fn handle_stat(&self, path: &str) -> Result<serde_json::Value> {
        let path = self.validate_read_path(path)?;
        debug!("Getting metadata for: {:?}", path);

        let metadata = std::fs::metadata(&path).context("getting metadata")?;

        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "is_dir": metadata.is_dir(),
            "is_file": metadata.is_file(),
            "is_symlink": metadata.file_type().is_symlink(),
            "size": metadata.len(),
            "readonly": metadata.permissions().readonly(),
        }))
    }

    fn handle_delete(&self, path: &str) -> Result<serde_json::Value> {
        if !self.allow_write {
            bail!("delete syscalls are disabled");
        }

        let path = self.validate_write_path(path)?;
        debug!("Deleting: {:?}", path);

        let metadata = std::fs::metadata(&path).context("checking path")?;
        if metadata.is_dir() {
            std::fs::remove_dir_all(&path).context("removing directory")?;
        } else {
            std::fs::remove_file(&path).context("removing file")?;
        }

        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "deleted": true,
        }))
    }

    fn validate_read_path(&self, path: &str) -> Result<PathBuf> {
        let path = shellexpand::tilde(path).to_string();
        let path = PathBuf::from(path);

        if !path.exists() {
            bail!("path does not exist: {}", path.display());
        }

        // If not allowing system reads, restrict to home directory
        if !self.allow_read_system {
            let home = std::env::var("HOME")
                .ok()
                .map(PathBuf::from)
                .context("HOME not set")?;
            let canonical = path.canonicalize().context("canonicalizing path")?;
            if !canonical.starts_with(&home) && !canonical.starts_with("/proc") {
                bail!(
                    "path outside home directory: {}",
                    canonical.display()
                );
            }
        }

        Ok(path)
    }

    fn validate_write_path(&self, path: &str) -> Result<PathBuf> {
        let path = shellexpand::tilde(path).to_string();
        let path = PathBuf::from(path);

        // Always restrict writes to home directory
        let home = std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
            .context("HOME not set")?;

        let parent = path.parent().context("invalid path")?;
        if parent.exists() {
            let canonical = parent.canonicalize().context("canonicalizing parent")?;
            if !canonical.starts_with(&home) {
                bail!(
                    "write path outside home directory: {}",
                    canonical.display()
                );
            }
        }

        Ok(path)
    }

    fn handle_custom(&self, name: &str, payload: serde_json::Value) -> Result<serde_json::Value> {
        let handler = self
            .extensions
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown extension: {}", name))?;

        handler.handle(name, payload)
    }
}

/// Execute a power management command (systemctl, loginctl)
fn handle_power_command(program: &str, args: &[&str]) -> Result<serde_json::Value> {
    use std::process::Stdio;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(_child) => Ok(serde_json::json!({ "ok": true, "command": format!("{} {}", program, args.join(" ")) })),
        Err(e) => bail!("failed to execute '{}': {}", program, e),
    }
}

/// Synchronously browse and return all installed applications from system
pub fn browse_applications() -> Result<serde_json::Value> {
    use std::fs;
    use std::path::Path;

    let mut apps = Vec::new();
    let dirs = vec!["/usr/share/applications"];
    let mut seen = std::collections::HashSet::new();
    const MAX_APPS: usize = 500; // Limit to prevent excessive scanning

    'outer: for dir in dirs {
        if !Path::new(dir).exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if apps.len() >= MAX_APPS {
                    break 'outer;
                }

                let path = entry.path();
                if path.extension().map(|e| e == "desktop").unwrap_or(false) {
                    // Use timeout to prevent hanging on problematic files
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.len() > 100_000 || content.contains("NoDisplay=true") {
                            continue;
                        }

                        let name = content
                            .lines()
                            .find_map(|l| l.strip_prefix("Name="))
                            .and_then(|s| {
                                let trimmed = s.trim();
                                if trimmed.is_empty() {
                                    None
                                } else {
                                    Some(trimmed.to_string())
                                }
                            });
                        let exec = content
                            .lines()
                            .find_map(|l| l.strip_prefix("Exec="))
                            .and_then(|s| s.split_whitespace().next())
                            .and_then(|s| s.split('/').last())
                            .and_then(|s| {
                                let trimmed = s.trim();
                                if trimmed.is_empty() {
                                    None
                                } else {
                                    Some(trimmed.to_string())
                                }
                            });
                        let icon = content
                            .lines()
                            .find_map(|l| l.strip_prefix("Icon="))
                            .map(|s| s.trim().to_string());

                        if let (Some(n), Some(e)) = (name, exec) {
                            if seen.insert(n.clone()) {
                                let mut app = serde_json::json!({
                                    "name": n,
                                    "command": e,
                                });
                                if let Some(i) = icon {
                                    app["icon"] = serde_json::json!({
                                        "type": "iconify",
                                        "data": i,
                                    });
                                }
                                apps.push(app);
                            }
                        }
                    }
                }
            }
        }
    }

    debug!("Browsed {} applications from system", apps.len());
    Ok(serde_json::Value::Array(apps))
}
