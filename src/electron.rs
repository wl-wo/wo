/// IPC protocol between the Wo compositor and the Electron helper process.

use std::{
    os::fd::{FromRawFd, OwnedFd},
    os::unix::{
        io::{AsRawFd, RawFd},
        net::UnixListener,
    },
    path::Path,
    io::{IoSliceMut},
    sync::{Arc, Mutex},
    collections::HashMap,
};


use anyhow::{bail, Context, Result};
use nix::{
    cmsg_space,
    sys::{
        socket::{recvmsg, send, setsockopt, sockopt::ReceiveTimeout, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr},
        time::TimeVal,
    },
    unistd,
};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, error, info, warn, trace};

use crate::config::WindowConfig;

pub const MAGIC_HELLO: u32 = 0x574F484C;
pub const MAGIC_FRAME: u32 = 0x574F4652;
pub const MAGIC_MOUSE_MOVE: u32 = 0x574F4D4D;
pub const MAGIC_MOUSE_BUTTON: u32 = 0x574F4D42;
pub const MAGIC_KEYBOARD: u32 = 0x574F4B42;
pub const MAGIC_SCROLL: u32 = 0x574F5343;
pub const MAGIC_ACTION: u32 = 0x574F4341;
pub const MAGIC_FOCUS_CHANGE: u32 = 0x574F4643;  // "WOFC"
pub const MAGIC_WINDOW_META: u32 = 0x574F574D;  // "WOWM"
pub const MAGIC_WINDOW_POS: u32 = 0x574F5750;   // "WOWP"
pub const MAGIC_SYSCALL: u32 = 0x574F5359;      // "WOSY" - fixed conflict!
pub const MAGIC_FRAME_ACK: u32 = 0x574F4641;    // "WOFA"
pub const MAGIC_SURFACE_BUFFER: u32 = 0x574F5342; // "WOSB" — Wayland surface pixels sent to Electron
pub const MAGIC_SHM_BUFFER: u32 = 0x574F534D;     // "WOSM" — Wayland SHM surface sent to Electron via process FD
pub const MAGIC_DMABUF_FRAME: u32 = 0x574F4446; // "WODF" — Wayland surface DMABUF fds sent to Electron
pub const MAGIC_FORWARD_POINTER: u32 = 0x574F5045;  // "WOPE"
pub const MAGIC_FORWARD_KEYBOARD: u32 = 0x574F4B45; // "WOKE"
pub const MAGIC_FORWARD_RELATIVE_POINTER: u32 = 0x574F5245; // "WORE"
pub const MAGIC_FORWARD_POINTER_BUTTON: u32 = 0x574F5042; // "WOPB"
pub const MAGIC_FORWARD_POINTER_SCROLL: u32 = 0x574F5053; // "WOPS"
pub const MAGIC_POINTER_LOCK_REQUEST: u32 = 0x574F504C; // "WOPL" — server-to-client pointer lock request
pub const MAGIC_ENV_UPDATE: u32 = 0x574F4555; // "WOEU" — environment variable update broadcast


#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct PlaneInfoWire {
    pub offset: u32,
    pub stride: u32,
    pub mod_hi: u32,
    pub mod_lo: u32,
}

/// Parsed plane description with the received file descriptor.
#[derive(Debug)]
pub struct PlaneInfo {
    pub fd:       OwnedFd,
    pub offset:   u32,
    pub stride:   u32,
    pub modifier: u64,
}

/// A fully parsed frame from an Electron window.
#[derive(Debug)]
pub struct ElectronFrame {
    /// Which window this frame belongs to (matches `WindowConfig::name`).
    pub name:    String,
    pub seq:     u64,
    pub width:   u32,
    pub height:  u32,
    pub format:  u32,  // DRM fourcc
    pub planes:  Vec<PlaneInfo>,
}

/// Window position update from client
#[derive(Debug, Clone)]
pub struct WindowPositionUpdate {
    pub window_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Syscall request from client
#[derive(Debug, Clone)]
pub struct SyscallRequest {
    pub window_name: String,
    pub syscall_type: String,  // "exec", "read", "write", etc.
    pub payload: String,  // JSON payload with request parameters
}

/// Messages that can be received from Electron clients
#[derive(Debug)]
pub enum ElectronMessage {
    Frame(ElectronFrame),
    WindowPosition(WindowPositionUpdate),
    Syscall(SyscallRequest),
    Action(CompositorActionMessage),
    /// Forwarded pointer motion from web UI canvas (window-local coordinates)
    ForwardedPointer { window_name: String, x: f64, y: f64 },
    /// Forwarded keyboard event from web UI canvas (evdev keycode)
    ForwardedKeyboard { window_name: String, key: u32, pressed: bool, time: u32 },
    /// Forwarded relative motion from web UI canvas
    ForwardedRelativePointer { window_name: String, dx: f64, dy: f64 },
    /// Forwarded pointer button from web UI
    ForwardedPointerButton { window_name: String, x: f64, y: f64, button: u32, pressed: bool, time: u32 },
    /// Forwarded pointer scroll from web UI
    ForwardedPointerScroll { window_name: String, dx: f64, dy: f64 },
}

/// Actions that Electron clients can request from the compositor.
#[derive(Debug, Clone)]
pub enum CompositorAction {
    /// Request the compositor to quit
    Quit { code: i32 },
    /// Custom extension action with JSON payload
    Custom { action: String, payload: Option<String> },
}

/// A message sent from an Electron client to the compositor.
#[derive(Debug, Clone)]
pub struct CompositorActionMessage {
    pub window_name: String,
    pub action: CompositorAction,
}

/// Input event types that can be sent to Electron clients.
#[derive(Debug, Clone)]
pub enum ElectronInputEvent {
    MouseMove { x: f64, y: f64 },
    MouseButton { button: u32, pressed: bool, time: u32 },
    Keyboard { key: u32, pressed: bool, time: u32 },
    Scroll { vertical: i32, horizontal: i32, time: u32 },
    FocusChange { window_name: String, focused: bool },
    WindowMetadata { metadata: String },  // JSON payload
    PointerLockRequest { window_name: String, lock: bool },
    /// Environment variable update (e.g. DISPLAY after XWayland is ready).
    EnvUpdate { vars: String },
}

/// A bidirectional connection to an Electron client.
pub struct ElectronClientConnection {
    pub name: String,
    fd: OwnedFd,
    /// Serializes all writes to this socket.  The IPC reader thread and the
    /// compositor main thread both write ACKs / input events to the same
    /// socket; without this lock interleaved `write(2)` calls corrupt the
    /// byte stream Electron receives, breaking `FRAME_ACK` parsing and
    /// permanently sticking `inFlightFrameSeqs` at the backpressure limit.
    write_lock: Mutex<()>,
}

impl ElectronClientConnection {
    /// Serialize a raw byte buffer to the socket, holding `write_lock`.
    fn write_bytes(&self, buf: &[u8]) -> Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        write_all(self.fd.as_raw_fd(), buf)
    }

    /// Non-blocking variant: drops the message silently on EAGAIN.
    /// Uses try_lock so the caller never blocks waiting for write_lock.
    /// If a concurrent write is in progress the message is dropped,
    /// which is acceptable for input events and metadata broadcasts.
    fn write_bytes_nonblocking(&self, buf: &[u8]) -> Result<()> {
        let _guard = match self.write_lock.try_lock() {
            Ok(g) => g,
            Err(_) => return Ok(()),
        };
        write_nonblocking(self.fd.as_raw_fd(), buf)
    }

    /// Send a FRAME_ACK for `seq` to unblock Electron's inFlight counter.
    /// Uses a blocking write: the message is 12 bytes on a local Unix socket so
    /// it completes in nanoseconds, and dropping ACKs would permanently stall
    /// Electron's backpressure mechanism (inFlightFrameSeqs never decrements).
    pub fn write_frame_ack(&self, seq: u64) -> Result<()> {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&MAGIC_FRAME_ACK.to_le_bytes());
        buf[4..12].copy_from_slice(&seq.to_le_bytes());
        self.write_bytes(&buf)
    }

    /// Send a Wayland surface's pixel content to the Electron client.
    /// Wire format: magic(4) + name_len(4) + name(N) + width(4) + height(4) + stride(4) + data_len(4) + pixels(data_len)
    /// Uses non-blocking write — if the socket buffer is full the frame is dropped.
    pub fn send_surface_buffer(
        &self,
        window_name: &str,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) -> Result<()> {
        let name_bytes = window_name.as_bytes();
        let header_len = 4 + 4 + name_bytes.len() + 4 + 4 + 4 + 4;
        let mut buf = vec![0u8; header_len + pixels.len()];
        let mut off = 0;
        buf[off..off + 4].copy_from_slice(&MAGIC_SURFACE_BUFFER.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        off += 4;
        buf[off..off + name_bytes.len()].copy_from_slice(name_bytes);
        off += name_bytes.len();
        buf[off..off + 4].copy_from_slice(&width.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&height.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&stride.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&(pixels.len() as u32).to_le_bytes());
        off += 4;
        buf[off..].copy_from_slice(pixels);
        self.write_bytes_nonblocking(&buf)
    }

    /// Send a Wayland surface's SHM buffer metadata to the Electron client.
    /// Wire format: magic(4) + name_len(4) + name(N) + width(4) + height(4) + stride(4) + pid(4) + fd(4)
    /// The compositor PID and fd number let Electron open /proc/<pid>/fd/<fd>
    /// to mmap pixels without copying.
    /// Wire format:
    ///   magic(4) + name_len(4) + name(N) + width(4) + height(4) + stride(4) + pid(4) + fd(4)
    ///   + num_rects(4) + [x(4) + y(4) + w(4) + h(4)] * num_rects
    pub fn send_shm_buffer(
        &self,
        window_name: &str,
        width: u32,
        height: u32,
        stride: u32,
        pid: u32,
        memfd_fd: i32,
        damage_rects: &[crate::state::DamageRect],
    ) -> Result<()> {
        let name_bytes = window_name.as_bytes();
        let num_rects = damage_rects.len() as u32;
        let header_len = 4 + 4 + name_bytes.len() + 4 + 4 + 4 + 4 + 4 + 4 + (damage_rects.len() * 16);
        let mut buf = vec![0u8; header_len];
        let mut off = 0;
        buf[off..off + 4].copy_from_slice(&MAGIC_SHM_BUFFER.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        off += 4;
        buf[off..off + name_bytes.len()].copy_from_slice(name_bytes);
        off += name_bytes.len();
        buf[off..off + 4].copy_from_slice(&width.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&height.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&stride.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&pid.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&(memfd_fd as u32).to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&num_rects.to_le_bytes());
        off += 4;
        for rect in damage_rects {
            buf[off..off + 4].copy_from_slice(&(rect.x as u32).to_le_bytes());
            off += 4;
            buf[off..off + 4].copy_from_slice(&(rect.y as u32).to_le_bytes());
            off += 4;
            buf[off..off + 4].copy_from_slice(&(rect.width as u32).to_le_bytes());
            off += 4;
            buf[off..off + 4].copy_from_slice(&(rect.height as u32).to_le_bytes());
            off += 4;
        }

        self.write_bytes_nonblocking(&buf)
    }

    /// Send a DMABUF-backed Wayland surface frame to the Electron client.
    /// Sends the window name, dimensions, format, and plane information as metadata,
    /// with the actual file descriptors sent via Unix socket ancillary data (zero-copy).
    /// Wire format: magic(4) + name_len(4) + name(N) + width(4) + height(4) + format(4) + num_planes(4) + planes*24
    pub fn send_dmabuf_frame(
        &self,
        window_name: &str,
        dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<()> {
        use smithay::backend::allocator::Buffer as _;

        let name_bytes = window_name.as_bytes();

        // Get buffer properties
        let num_planes = dmabuf.num_planes();
        let (w, h) = {
            let size = dmabuf.size();
            (size.w as u32, size.h as u32)
        };
        let format = dmabuf.format();

        // Build metadata message: magic + name_len + name + width + height + format + num_planes + plane_info
        let header_len = 4 + 4 + name_bytes.len() + 4 + 4 + 4 + 4 + (num_planes * 24);
        let mut buf = vec![0u8; header_len];
        let mut off = 0;

        buf[off..off + 4].copy_from_slice(&MAGIC_DMABUF_FRAME.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        off += 4;
        buf[off..off + name_bytes.len()].copy_from_slice(name_bytes);
        off += name_bytes.len();

        // Width and height
        buf[off..off + 4].copy_from_slice(&w.to_le_bytes());
        off += 4;
        buf[off..off + 4].copy_from_slice(&h.to_le_bytes());
        off += 4;

        let fourcc = format.code as u32;
        buf[off..off + 4].copy_from_slice(&fourcc.to_le_bytes());
        off += 4;

        // Number of planes
        buf[off..off + 4].copy_from_slice(&(num_planes as u32).to_le_bytes());
        off += 4;

        // For each plane: fd(4) + offset(8) + stride(4) + modifier(8)
        // Note: We'll collect actual plane data from dmabuf handles iterator
        let mut plane_idx = 0;
        for _handle in dmabuf.handles() {
            // fd: will be sent via ancillary data, so we put 0 as placeholder
            buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
            off += 4;
            // offset: 0 for now (can be extended later)
            buf[off..off + 8].copy_from_slice(&0u64.to_le_bytes());
            off += 8;
            // stride: will depend on format (approximate)
            let stride = (w * 4) as u32; // ARGB8888 = 4 bytes/pixel, adjust as needed
            buf[off..off + 4].copy_from_slice(&stride.to_le_bytes());
            off += 4;
            // modifier (8 bytes) - DrmModifier converts to u64
            let modifier_val: u64 = format.modifier.into();
            buf[off..off + 8].copy_from_slice(&modifier_val.to_le_bytes());
            off += 8;
            plane_idx += 1;
        }

        // Send metadata with file descriptors as ancillary data
        self.sendmsg_with_fds(&buf, dmabuf)
    }

    /// Send a message with file descriptors via Unix socket ancillary data.
    fn sendmsg_with_fds(
        &self,
        metadata: &[u8],
        dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<()> {
        use std::os::unix::io::AsRawFd;

        let _guard = match self.write_lock.try_lock() {
            Ok(g) => g,
            Err(_) => return Ok(()), // Drop if lock contested
        };

        // Build ancillary data for file descriptors
        let fds: Vec<RawFd> = dmabuf
            .handles()
            .map(|h| h.as_raw_fd())
            .collect();

        if fds.is_empty() {
            // No FDs to send, just send metadata
            return write_nonblocking(self.fd.as_raw_fd(), metadata);
        }

        // Use nix sendmsg with file descriptor passing via control message
        // Convert metadata slice to IoSlice for sendmsg
        let iov = [std::io::IoSlice::new(metadata)];

        let mut cmsgs = vec![];
        cmsgs.push(ControlMessage::ScmRights(&fds[..]));

        match nix::sys::socket::sendmsg::<UnixAddr>(
            self.fd.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::MSG_NOSIGNAL,
            None,
        ) {
            Ok(_) => Ok(()),
            Err(e) => {
                debug!("sendmsg failed: {}", e);
                Ok(()) // Non-blocking: drop on error
            }
        }
    }

    /// Send an input event to the Electron client.
    pub fn send_input_event(&self, event: &ElectronInputEvent) -> Result<()> {

        match event {
            ElectronInputEvent::MouseMove { x, y } => {
                let mut buf = [0u8; 20];
                buf[0..4].copy_from_slice(&MAGIC_MOUSE_MOVE.to_le_bytes());
                buf[4..12].copy_from_slice(&x.to_le_bytes());
                buf[12..20].copy_from_slice(&y.to_le_bytes());
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::MouseButton { button, pressed, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_MOUSE_BUTTON.to_le_bytes());
                buf[4..8].copy_from_slice(&button.to_le_bytes());
                buf[8..12].copy_from_slice(&(*pressed as u32).to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::Keyboard { key, pressed, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_KEYBOARD.to_le_bytes());
                buf[4..8].copy_from_slice(&key.to_le_bytes());
                buf[8..12].copy_from_slice(&(*pressed as u32).to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::Scroll { vertical, horizontal, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_SCROLL.to_le_bytes());
                buf[4..8].copy_from_slice(&vertical.to_le_bytes());
                buf[8..12].copy_from_slice(&horizontal.to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::FocusChange { window_name, focused } => {
                let name_bytes = window_name.as_bytes();
                let mut buf = vec![0u8; 12 + name_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_FOCUS_CHANGE.to_le_bytes());
                buf[4..8].copy_from_slice(&(*focused as u32).to_le_bytes());
                buf[8..12].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf[12..].copy_from_slice(name_bytes);
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::WindowMetadata { metadata } => {
                let meta_bytes = metadata.as_bytes();
                let mut buf = vec![0u8; 8 + meta_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_WINDOW_META.to_le_bytes());
                buf[4..8].copy_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
                buf[8..].copy_from_slice(meta_bytes);
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::PointerLockRequest { window_name, lock } => {
                let name_bytes = window_name.as_bytes();
                let mut buf = vec![0u8; 12 + name_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_POINTER_LOCK_REQUEST.to_le_bytes());
                buf[4..8].copy_from_slice(&(*lock as u32).to_le_bytes());
                buf[8..12].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf[12..].copy_from_slice(name_bytes);
                self.write_bytes(&buf)?;
            },
            ElectronInputEvent::EnvUpdate { vars } => {
                let var_bytes = vars.as_bytes();
                let mut buf = vec![0u8; 8 + var_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_ENV_UPDATE.to_le_bytes());
                buf[4..8].copy_from_slice(&(var_bytes.len() as u32).to_le_bytes());
                buf[8..].copy_from_slice(var_bytes);
                self.write_bytes(&buf)?;
            },
        }
        Ok(())
    }

    /// Send an input event to the Electron client without blocking.
    /// If the socket send buffer is full (EAGAIN), the event is silently
    /// dropped — this prevents the compositor main thread from stalling
    /// when a client is slow to drain its receive buffer.
    pub fn try_send_input_event(&self, event: &ElectronInputEvent) -> Result<()> {
        match event {
            ElectronInputEvent::MouseMove { x, y } => {
                let mut buf = [0u8; 20];
                buf[0..4].copy_from_slice(&MAGIC_MOUSE_MOVE.to_le_bytes());
                buf[4..12].copy_from_slice(&x.to_le_bytes());
                buf[12..20].copy_from_slice(&y.to_le_bytes());
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::MouseButton { button, pressed, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_MOUSE_BUTTON.to_le_bytes());
                buf[4..8].copy_from_slice(&button.to_le_bytes());
                buf[8..12].copy_from_slice(&(*pressed as u32).to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::Keyboard { key, pressed, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_KEYBOARD.to_le_bytes());
                buf[4..8].copy_from_slice(&key.to_le_bytes());
                buf[8..12].copy_from_slice(&(*pressed as u32).to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::Scroll { vertical, horizontal, time } => {
                let mut buf = [0u8; 16];
                buf[0..4].copy_from_slice(&MAGIC_SCROLL.to_le_bytes());
                buf[4..8].copy_from_slice(&vertical.to_le_bytes());
                buf[8..12].copy_from_slice(&horizontal.to_le_bytes());
                buf[12..16].copy_from_slice(&time.to_le_bytes());
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::FocusChange { window_name, focused } => {
                let name_bytes = window_name.as_bytes();
                let mut buf = vec![0u8; 12 + name_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_FOCUS_CHANGE.to_le_bytes());
                buf[4..8].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf[8..12].copy_from_slice(&(*focused as u32).to_le_bytes());
                buf[12..].copy_from_slice(name_bytes);
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::WindowMetadata { metadata } => {
                let meta_bytes = metadata.as_bytes();
                let mut buf = vec![0u8; 8 + meta_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_WINDOW_META.to_le_bytes());
                buf[4..8].copy_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
                buf[8..].copy_from_slice(meta_bytes);
                self.write_bytes_nonblocking(&buf)
            },
            ElectronInputEvent::PointerLockRequest { window_name, lock } => {
                let name_bytes = window_name.as_bytes();
                let mut buf = vec![0u8; 12 + name_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_POINTER_LOCK_REQUEST.to_le_bytes());
                buf[4..8].copy_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buf[8..12].copy_from_slice(&(*lock as u32).to_le_bytes());
                buf[12..].copy_from_slice(name_bytes);
                self.write_bytes_nonblocking(&buf)
            }
            ElectronInputEvent::EnvUpdate { vars } => {
                let var_bytes = vars.as_bytes();
                let mut buf = vec![0u8; 8 + var_bytes.len()];
                buf[0..4].copy_from_slice(&MAGIC_ENV_UPDATE.to_le_bytes());
                buf[4..8].copy_from_slice(&(var_bytes.len() as u32).to_le_bytes());
                buf[8..].copy_from_slice(var_bytes);
                self.write_bytes_nonblocking(&buf)
            }
        }
    }
}




pub struct ElectronProcess {
    pub name:   String,
    pub config: WindowConfig,
    pub child:  std::process::Child,
}

impl ElectronProcess {
    /// Spawn `electron <app-dir>` for the given window config.
    /// The app dir is resolved to `<config-dir>/wo-electron`.
    ///
    /// `render_node` is the DRI render node (e.g. `/dev/dri/renderD128`) that
    /// matches the GPU the compositor opened.  The native `wo_dmabuf.node`
    /// module inside Electron uses it to allocate GBM buffers that are
    /// importable by the compositor's EGL context.
    pub fn spawn(
        config: &WindowConfig,
        electron_bin: &str,
        app_dir: &Path,
        ipc_socket: &str,
        render_node: &str,
    ) -> Result<Self> {
        use std::process::{Command, Stdio};
        
        tracing::info!("Spawning Electron for window '{}' with config: {:?}", config.name, config);

        let serialized = serde_json::to_string(config)
            .context("serializing window config")?;
        
        tracing::info!("Serialized config: {}", serialized);

        let main_js = app_dir.join("dist/main.js");
        
        let child = Command::new(electron_bin)
            .arg(&main_js)
            .env("WO_IPC_SOCKET", ipc_socket)
            .env("WO_WINDOW_CONFIG", &serialized)
            .env("WO_DRM_RENDER_NODE", render_node)
            // Headless Ozone backend: Electron is used purely for offscreen
            // rendering — it must not try to connect to a Wayland or X11
            // display server (there is none for it to connect to; *we* are
            // the compositor).  The headless platform lets Chromium's OSR
            // paint pipeline work while the native wo_dmabuf module handles
            // GPU buffer allocation via its own GBM/EGL context on the
            // render node.
            .arg("--ozone-platform=headless")
            .arg("--no-sandbox")
            .arg("--disable-gpu-sandbox")
            // Use the *software* GL implementation inside Chromium for its
            // own compositing (we do not need hardware-accelerated web
            // content — the paint callback gives us BGRA bitmaps that the
            // native module imports into a GBM BO).  This avoids Chromium
            // fighting with the compositor over the DRM render node.
            .arg("--use-gl=swiftshader")
            // Make sure OSR (offscreen) rendering actually produces
            // paint callbacks at the requested frame rate.
            .arg("--enable-features=Vulkan")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning electron for window '{}'", config.name))?;

        info!(window = %config.name, pid = child.id(), render_node, "spawned Electron process");

        Ok(Self {
            name: config.name.clone(),
            config: config.clone(),
            child,
        })
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for ElectronProcess {
    fn drop(&mut self) {
        self.kill();
    }
}


pub struct ElectronIpc {
    pub tx: mpsc::Sender<ElectronMessage>,
    /// Map of window name -> client connection for sending input events.
    pub clients: Arc<Mutex<HashMap<String, Arc<ElectronClientConnection>>>>,
}

impl ElectronIpc {
    /// Bind the socket and start accepting connections.
    pub fn listen(socket_path: &str) -> Result<(Self, mpsc::Receiver<ElectronMessage>)> {
        // Remove stale socket file.
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding IPC socket {socket_path}"))?;
        info!(socket = socket_path, "IPC socket listening");

        // 64 slots to avoid frame/input starvation under load.  The old
        // value of 8 caused silent frame drops when games submitted at
        // >60 FPS while input events competed for the same channel.
        let (tx, rx) = mpsc::channel(64);
        let tx2 = tx.clone();
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let clients2 = clients.clone();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let tx = tx2.clone();
                        let clients = clients2.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = handle_connection(stream.as_raw_fd(), tx, clients) {
                                error!("electron IPC error: {e:#}");
                            }
                        });
                    }
                    Err(e) => error!("IPC accept error: {e}"),
                }
            }
        });

        Ok((Self { tx, clients }, rx))
    }

    /// Send input event to a specific window (non-blocking).
    /// If the socket buffer is full or the write lock is contended, the event
    /// is silently dropped. This prevents the compositor main thread from
    /// stalling when an Electron client is slow to drain its receive buffer.
    pub fn send_to_window(&self, window_name: &str, event: &ElectronInputEvent) -> Result<()> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients.get(window_name).cloned()
        };
        // clients Mutex released — safe to do I/O without blocking other IPC ops.
        if let Some(client) = client {
            client.try_send_input_event(event)?;
        }
        Ok(())
    }

    /// Broadcast window metadata to all windows
    pub fn broadcast_metadata(&self, metadata: &str) -> Result<()> {
        let snapshot: Vec<_> = {
            let clients = self.clients.lock().unwrap();
            clients.values().cloned().collect()
        };
        // clients Mutex released — safe to do I/O.
        let event = ElectronInputEvent::WindowMetadata {
            metadata: metadata.to_string(),
        };
        for client in &snapshot {
            if let Err(e) = client.try_send_input_event(&event) {
                warn!("metadata broadcast to '{}' failed (stream may be corrupted): {e:#}", client.name);
            }
        }
        Ok(())
    }

    /// Broadcast environment variable updates to all connected Electron clients.
    /// `vars` is a JSON object like `{"DISPLAY":":0"}`.
    pub fn broadcast_env_update(&self, vars: &str) -> Result<()> {
        let snapshot: Vec<_> = {
            let clients = self.clients.lock().unwrap();
            clients.values().cloned().collect()
        };
        let event = ElectronInputEvent::EnvUpdate {
            vars: vars.to_string(),
        };
        for client in &snapshot {
            if let Err(e) = client.try_send_input_event(&event) {
                warn!("env update broadcast to '{}' failed: {e:#}", client.name);
            }
        }
        Ok(())
    }

    /// Send syscall response to a window (non-blocking).
    /// If the socket buffer is full, the response is dropped and a warning is
    /// logged. This is preferable to blocking the compositor event loop — the
    /// Electron client will time out its IPC promise rather than deadlocking
    /// the entire compositor.
    pub fn send_syscall_response(&self, window_name: &str, response: &str) -> Result<()> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients.get(window_name).cloned()
        };
        // clients Mutex released — safe to do I/O.
        if let Some(client) = client {
            let response_bytes = response.as_bytes();
            let mut buf = vec![0u8; 8 + response_bytes.len()];
            buf[0..4].copy_from_slice(&MAGIC_SYSCALL.to_le_bytes());
            buf[4..8].copy_from_slice(&(response_bytes.len() as u32).to_le_bytes());
            buf[8..].copy_from_slice(response_bytes);
            if let Err(e) = client.write_bytes_nonblocking(&buf) {
                warn!("syscall response to '{}' dropped (socket full): {e:#}", client.name);
            }
        }
        Ok(())
    }

    /// Acknowledge that a frame was consumed/imported so the sender can apply backpressure.
    pub fn send_frame_ack(&self, window_name: &str, seq: u64) -> Result<()> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients.get(window_name).cloned()
        };
        // clients Mutex released — safe to do I/O.
        // ACKs are only 12 bytes; a blocking write is acceptable here since
        // it completes in nanoseconds on a local socket and dropping ACKs
        // would permanently stall Electron's backpressure.
        if let Some(client) = client {
            client.write_frame_ack(seq)?;
        }
        Ok(())
    }

    /// Broadcast a Wayland surface's pixel content to all Electron clients.
    /// Non-blocking: drops the buffer for clients that can't keep up.
    pub fn broadcast_surface_buffer(
        &self,
        window_name: &str,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) -> Result<()> {
        let snapshot: Vec<_> = {
            let clients = self.clients.lock().unwrap();
            clients.values().cloned().collect()
        };
        // clients Mutex released — safe to do I/O.
        for client in &snapshot {
            if let Err(e) = client.send_surface_buffer(window_name, width, height, stride, pixels) {
                debug!("surface buffer to '{}' dropped: {e:#}", client.name);
            }
        }
        Ok(())
    }

    /// Broadcast a Wayland surface's SHM buffer metadata to all Electron clients.
    /// Non-blocking: drops the buffer for clients that can't keep up.
    pub fn broadcast_shm_buffer(
        &self,
        window_name: &str,
        width: u32,
        height: u32,
        stride: u32,
        pid: u32,
        memfd_fd: i32,
        damage_rects: &[crate::state::DamageRect],
    ) -> Result<()> {
        let snapshot: Vec<_> = {
            let clients = self.clients.lock().unwrap();
            clients.values().cloned().collect()
        };
        // clients Mutex released — safe to do I/O.
        for client in &snapshot {
            if let Err(e) = client.send_shm_buffer(window_name, width, height, stride, pid, memfd_fd, damage_rects) {
                debug!("shm buffer to '{}' dropped: {e:#}", client.name);
            }
        }
        Ok(())
    }

    /// Broadcast a Wayland surface's DMABUF frame to all Electron clients.
    /// Sends file descriptors via Unix socket ancillary data (zero-copy).
    /// Non-blocking: drops the frame for clients that can't keep up.
    pub fn broadcast_dmabuf_frame(
        &self,
        window_name: &str,
        dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<()> {
        let snapshot: Vec<_> = {
            let clients = self.clients.lock().unwrap();
            clients.values().cloned().collect()
        };
        // clients Mutex released — safe to do I/O.
        for client in &snapshot {
            if let Err(e) = client.send_dmabuf_frame(window_name, dmabuf) {
                debug!("dmabuf frame to '{}' dropped: {e:#}", client.name);
            }
        }
        Ok(())
    }
}


fn handle_connection(
    fd: RawFd,
    tx: mpsc::Sender<ElectronMessage>,
    clients: Arc<Mutex<HashMap<String, Arc<ElectronClientConnection>>>>,
) -> Result<()> {
    // Set a 5-second receive timeout before any blocking read so the thread
    // cannot hang indefinitely if Electron connects but stalls mid-handshake.
    {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let timeout = TimeVal::new(5, 0);
        if let Err(e) = setsockopt(&borrowed, ReceiveTimeout, &timeout) {
            warn!("Failed to set SO_RCVTIMEO on IPC socket: {e}");
        }
    }

    // 1. Read HelloMessage
    let mut buf = [0u8; 4096];
    let name = recv_hello(fd, &mut buf)?;
    info!(window = %name, "Electron window connected");

    let fd_for_write = {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        unistd::dup(borrowed).context("duplicating IPC socket fd")?
    };

    // Register this client connection for input event dispatch
    let client = Arc::new(ElectronClientConnection {
        name: name.clone(),
        fd: fd_for_write,
        write_lock: Mutex::new(()),
    });
    clients.lock().unwrap().insert(name.clone(), client.clone());

    // Clear the handshake-only SO_RCVTIMEO so the reader thread blocks
    // indefinitely between messages. A static web page may produce no paint
    // events for many seconds; a 5-second timeout would incorrectly kill the
    // connection during these normal idle periods.
    {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let no_timeout = TimeVal::new(0, 0);
        if let Err(e) = setsockopt(&borrowed, ReceiveTimeout, &no_timeout) {
            warn!("Failed to clear SO_RCVTIMEO on IPC socket: {e}");
        }
    }

    // 2. Stream Messages (frames, syscalls, positions, etc.)
    let mut msg_count = 0u32;
    loop {
        match recv_message(fd, &name, &mut buf) {
            Ok(msg) => {
                msg_count += 1;
                trace!(window = %name, msg_num = msg_count, "received IPC message");
                match msg {
                    ElectronMessage::Frame(frame) => {
                        let seq = frame.seq;
                        info!(window = %name, seq, "received FRAME, sending ACK");
                        if let Err(e) = client.write_frame_ack(seq) {
                            error!(window = %name, seq, "failed to send FRAME_ACK: {e:#}");
                        }
                        match tx.try_send(ElectronMessage::Frame(frame)) {
                            Ok(()) => {
                                trace!(window = %name, "frame queued for compositor");
                            }
                            Err(TrySendError::Full(_)) => {
                                debug!(window = %name, seq, "dropping frame (channel full, already ACKed)");
                            }
                            Err(TrySendError::Closed(_)) => {
                                warn!(window = %name, "compositor channel closed, exiting IPC reader");
                                break;
                            }
                        }
                    }
                    _ => {
                        // Non-frame messages (WindowPosition, Syscall, Action): use try_send
                        // to avoid blocking the IPC reader thread. If the channel is full,
                        // the message is dropped — non-critical messages will arrive again
                        // on the next event (positions, actions), or are processed
                        // asynchronously anyway.
                        match tx.try_send(msg) {
                            Ok(()) => {
                                trace!(window = %name, "non-frame message queued");
                            }
                            Err(TrySendError::Full(_)) => {
                                debug!(window = %name, "dropping non-frame IPC message (channel full)");
                            }
                            Err(TrySendError::Closed(_)) => {
                                warn!(window = %name, "compositor channel closed, exiting IPC reader");
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // Connection closed = normal Electron exit, but warn so it's visible.
                warn!(window = %name, msg_count, "IPC connection closed: {e}");
                break;
            }
        }
    }

    // Clean up client registration on disconnect
    clients.lock().unwrap().remove(&name);
    info!(window = %name, msg_count, "IPC reader thread exiting");

    Ok(())
}

fn recv_message(fd: RawFd, name: &str, buf: &mut [u8]) -> Result<ElectronMessage> {
    // Read magic to determine message type
    read_exact(fd, &mut buf[..4]).context("reading message magic")?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    trace!("recv_message: got magic 0x{:08X}", magic);

    match magic {
        MAGIC_FRAME => {
            trace!("recv_message: parsing FRAME message");
            // Read header: magic(4) + name_len(4)
            read_exact(fd, &mut buf[4..8]).context("reading FRAME name_len")?;
            let name_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if name_len > 256 { bail!("window name too long in FRAME: {name_len}"); }

            // Read the window name
            read_exact(fd, &mut buf[8..8+name_len]).context("reading FRAME window name")?;
            let frame_name = String::from_utf8_lossy(&buf[8..8+name_len]).into_owned();

            // Rest of frame header: seq(8) + width(4) + height(4) + format(4) + num_planes(4)
            const REST_HEADER: usize = 8 + 4 + 4 + 4 + 4;
            read_exact(fd, &mut buf[..REST_HEADER]).context("reading FRAME header fields")?;

            let frame = parse_frame_message(fd, &frame_name, buf)
                .context("parsing FRAME message data")?;
            trace!("recv_message: FRAME parsed successfully: {}x{}", frame.width, frame.height);
            Ok(ElectronMessage::Frame(frame))
        }
        MAGIC_WINDOW_POS => {
            trace!("recv_message: parsing WINDOW_POS message");
            // Window position: magic(4) + name_len(4) + name(N) + x(4) + y(4) + width(4) + height(4)
            read_exact(fd, &mut buf[4..8]).context("reading WINDOW_POS name_len")?;
            let name_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if name_len > 256 { bail!("window name too long in WINDOW_POS: {name_len}"); }
            let mut pos_buf = vec![0u8; name_len + 16];
            read_exact(fd, &mut pos_buf).context("reading WINDOW_POS data")?;
            let x      = i32::from_le_bytes(pos_buf[name_len..name_len+4].try_into().unwrap());
            let y      = i32::from_le_bytes(pos_buf[name_len+4..name_len+8].try_into().unwrap());
            let width  = u32::from_le_bytes(pos_buf[name_len+8..name_len+12].try_into().unwrap());
            let height = u32::from_le_bytes(pos_buf[name_len+12..name_len+16].try_into().unwrap());

            Ok(ElectronMessage::WindowPosition(WindowPositionUpdate {
                window_name: name.to_string(),
                x, y, width, height,
            }))
        }
        MAGIC_SYSCALL => {
            trace!("recv_message: parsing SYSCALL message");
            // Syscall: magic(4) + type_len(4) + payload_len(4) + type_str + payload_str
            read_exact(fd, &mut buf[4..12]).context("reading SYSCALL header")?;
            let type_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            let payload_len = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;

            if type_len > 256 || payload_len > 1024 * 1024 {
                bail!("syscall message too large: type_len={} payload_len={}", type_len, payload_len);
            }

            read_exact(fd, &mut buf[..type_len]).context("reading SYSCALL type")?;
            let syscall_type = String::from_utf8_lossy(&buf[..type_len]).into_owned();

            let mut payload_buf = vec![0u8; payload_len];
            read_exact(fd, &mut payload_buf).context("reading SYSCALL payload")?;
            let payload = String::from_utf8_lossy(&payload_buf).into_owned();

            Ok(ElectronMessage::Syscall(SyscallRequest {
                window_name: name.to_string(),
                syscall_type,
                payload,
            }))
        }
        MAGIC_ACTION => {
            trace!("recv_message: parsing ACTION message");
            // Action: magic(4) + action_len(4) + action_str(action_len) + payload_len(4) + payload_str
            read_exact(fd, &mut buf[4..8]).context("reading ACTION action_len")?;
            let action_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;

            if action_len > 256 {
                bail!("action string too large: action_len={}", action_len);
            }

            read_exact(fd, &mut buf[..action_len]).context("reading ACTION string")?;
            let action = String::from_utf8_lossy(&buf[..action_len]).into_owned();

            // Now read payload_len
            read_exact(fd, &mut buf[..4]).context("reading ACTION payload_len")?;
            let payload_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;

            if payload_len > 1024 * 1024 {
                bail!("action payload too large: payload_len={}", payload_len);
            }

            let payload = if payload_len > 0 {
                let mut payload_buf = vec![0u8; payload_len];
                read_exact(fd, &mut payload_buf).context("reading ACTION payload")?;
                Some(String::from_utf8_lossy(&payload_buf).into_owned())
            } else {
                None
            };

            let compositor_action = if action == "quit" {
                let code = payload.as_ref()
                    .and_then(|p| {
                        // Payload may be JSON like {"code":0} or a plain integer
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                            v.get("code").and_then(|c| c.as_i64()).map(|c| c as i32)
                        } else {
                            p.parse::<i32>().ok()
                        }
                    })
                    .unwrap_or(0);
                CompositorAction::Quit { code }
            } else {
                CompositorAction::Custom { action, payload }
            };

            Ok(ElectronMessage::Action(CompositorActionMessage {
                window_name: name.to_string(),
                action: compositor_action,
            }))
        }
        MAGIC_FORWARD_POINTER => {
            trace!("recv_message: parsing FORWARD_POINTER message");
            read_exact(fd, &mut buf[4..8]).context("reading FORWARD_POINTER window_len")?;
            let window_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if window_len > 256 { bail!("window name too long in FORWARD_POINTER: {window_len}"); }
            let mut data_buf = vec![0u8; window_len + 16];
            read_exact(fd, &mut data_buf).context("reading FORWARD_POINTER data")?;
            let window_name = String::from_utf8_lossy(&data_buf[..window_len]).into_owned();
            let x = f64::from_le_bytes(data_buf[window_len..window_len+8].try_into().unwrap());
            let y = f64::from_le_bytes(data_buf[window_len+8..window_len+16].try_into().unwrap());
            Ok(ElectronMessage::ForwardedPointer { window_name, x, y })
        }
        MAGIC_FORWARD_KEYBOARD => {
            trace!("recv_message: parsing FORWARD_KEYBOARD message");
            read_exact(fd, &mut buf[4..8]).context("reading FORWARD_KEYBOARD window_len")?;
            let window_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if window_len > 256 { bail!("window name too long in FORWARD_KEYBOARD: {window_len}"); }
            let mut data_buf = vec![0u8; window_len + 12];
            read_exact(fd, &mut data_buf).context("reading FORWARD_KEYBOARD data")?;
            let window_name = String::from_utf8_lossy(&data_buf[..window_len]).into_owned();
            let key     = u32::from_le_bytes(data_buf[window_len..window_len+4].try_into().unwrap());
            let pressed = u32::from_le_bytes(data_buf[window_len+4..window_len+8].try_into().unwrap()) != 0;
            let time    = u32::from_le_bytes(data_buf[window_len+8..window_len+12].try_into().unwrap());
            Ok(ElectronMessage::ForwardedKeyboard { window_name, key, pressed, time })
        }
        MAGIC_FORWARD_RELATIVE_POINTER => {
            trace!("recv_message: parsing FORWARD_RELATIVE_POINTER message");
            read_exact(fd, &mut buf[4..8]).context("reading FORWARD_RELATIVE_POINTER window_len")?;
            let window_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if window_len > 256 { bail!("window name too long in FORWARD_RELATIVE_POINTER: {window_len}"); }
            let mut data_buf = vec![0u8; window_len + 16];
            read_exact(fd, &mut data_buf).context("reading FORWARD_RELATIVE_POINTER data")?;
            let window_name = String::from_utf8_lossy(&data_buf[..window_len]).into_owned();
            let dx = f64::from_le_bytes(data_buf[window_len..window_len+8].try_into().unwrap());
            let dy = f64::from_le_bytes(data_buf[window_len+8..window_len+16].try_into().unwrap());
            Ok(ElectronMessage::ForwardedRelativePointer { window_name, dx, dy })
        }
        MAGIC_FORWARD_POINTER_BUTTON => {
            trace!("recv_message: parsing FORWARD_POINTER_BUTTON message");
            read_exact(fd, &mut buf[4..8]).context("reading FORWARD_POINTER_BUTTON window_len")?;
            let window_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if window_len > 256 { bail!("window name too long in FORWARD_POINTER_BUTTON: {window_len}"); }
            let mut data_buf = vec![0u8; window_len + 28];
            read_exact(fd, &mut data_buf).context("reading FORWARD_POINTER_BUTTON data")?;
            let window_name = String::from_utf8_lossy(&data_buf[..window_len]).into_owned();
            let x       = f64::from_le_bytes(data_buf[window_len..window_len+8].try_into().unwrap());
            let y       = f64::from_le_bytes(data_buf[window_len+8..window_len+16].try_into().unwrap());
            let button  = u32::from_le_bytes(data_buf[window_len+16..window_len+20].try_into().unwrap());
            let pressed = u32::from_le_bytes(data_buf[window_len+20..window_len+24].try_into().unwrap()) != 0;
            let time    = u32::from_le_bytes(data_buf[window_len+24..window_len+28].try_into().unwrap());
            Ok(ElectronMessage::ForwardedPointerButton { window_name, x, y, button, pressed, time })
        }
        MAGIC_FORWARD_POINTER_SCROLL => {
            trace!("recv_message: parsing FORWARD_POINTER_SCROLL message");
            read_exact(fd, &mut buf[4..8]).context("reading FORWARD_POINTER_SCROLL window_len")?;
            let window_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
            if window_len > 256 { bail!("window name too long in FORWARD_POINTER_SCROLL: {window_len}"); }
            let mut data_buf = vec![0u8; window_len + 16];
            read_exact(fd, &mut data_buf).context("reading FORWARD_POINTER_SCROLL data")?;
            let window_name = String::from_utf8_lossy(&data_buf[..window_len]).into_owned();
            let dx = f64::from_le_bytes(data_buf[window_len..window_len+8].try_into().unwrap());
            let dy = f64::from_le_bytes(data_buf[window_len+8..window_len+16].try_into().unwrap());
            Ok(ElectronMessage::ForwardedPointerScroll { window_name, dx, dy })
        }
        _ => {
            error!("recv_message: unknown magic 0x{:08X}", magic);
            bail!("unknown message magic: 0x{magic:08X}");
        }
    }
}

fn parse_frame_message(fd: RawFd, name: &str, buf: &mut [u8]) -> Result<ElectronFrame> {
    // Buffer contains: seq(8) + width(4) + height(4) + format(4) + num_planes(4)
    
    let seq        = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let width      = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let height     = u32::from_le_bytes(buf[12..16].try_into().unwrap());
    let format     = u32::from_le_bytes(buf[16..20].try_into().unwrap());
    let num_planes = u32::from_le_bytes(buf[20..24].try_into().unwrap()) as usize;

    if num_planes == 0 || num_planes > 4 {
        bail!("invalid num_planes: {num_planes}");
    }

    // Read PlaneInfoWire structs (16 bytes each) + SCM_RIGHTS fds.
    const PLANE_SIZE: usize = 16;
    let plane_bytes = num_planes * PLANE_SIZE;
    read_exact(fd, &mut buf[..plane_bytes])?;

    // Receive file descriptors via recvmsg SCM_RIGHTS.
    let fds = recv_fds(fd, num_planes)?;
    if fds.len() != num_planes {
        bail!("expected {num_planes} fds, got {}", fds.len());
    }

    let mut planes = Vec::with_capacity(num_planes);
    for (i, fd) in fds.into_iter().enumerate() {
        let base = i * PLANE_SIZE;
        let offset   = u32::from_le_bytes(buf[base..base+4].try_into().unwrap());
        let stride   = u32::from_le_bytes(buf[base+4..base+8].try_into().unwrap());
        let mod_hi   = u32::from_le_bytes(buf[base+8..base+12].try_into().unwrap());
        let mod_lo   = u32::from_le_bytes(buf[base+12..base+16].try_into().unwrap());
        let modifier = ((mod_hi as u64) << 32) | mod_lo as u64;
        planes.push(PlaneInfo { fd, offset, stride, modifier });
    }

    Ok(ElectronFrame {
        name: name.to_string(),
        seq, width, height, format, planes,
    })
}

fn read_exact(fd: RawFd, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::io::FromRawFd;
    use std::io::Read;
    // SAFETY: fd is valid for the lifetime of this call.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let r = f.read_exact(buf);
    std::mem::forget(f); // don't close fd
    r.context("IPC read")
}

fn write_all(fd: RawFd, buf: &[u8]) -> Result<()> {
    use std::os::unix::io::FromRawFd;
    use std::io::Write;
    // SAFETY: fd is valid for the lifetime of this call.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let r = f.write_all(buf);
    std::mem::forget(f); // don't close fd
    r.context("IPC write")
}

/// Non-blocking socket write: sends `buf` only if the entire message fits in the
/// kernel send buffer without blocking. If there is not enough space, the message
/// is silently dropped. This prevents both:
///   1. Blocking the compositor main thread on a slow IPC client.
///   2. Partial writes that would corrupt the framed byte stream — on SOCK_STREAM a
///      partial `send(MSG_DONTWAIT)` writes some bytes and returns n < buf.len(),
///      leaving the receiver with a truncated message. All subsequent messages
///      (including FRAME_ACKs) then misalign, causing Electron's inFlightFrameSeqs
///      counter to stall at its limit and stopping all frame delivery.
///
/// Strategy: query the number of bytes already queued in the kernel send buffer
/// via TIOCOUTQ. Compare against SO_SNDBUF. Only call send() when the full
/// message provably fits, ensuring atomic all-or-nothing delivery.
fn write_nonblocking(fd: RawFd, buf: &[u8]) -> Result<()> {
    // Query bytes currently waiting in the kernel send buffer (unread by receiver).
    let mut queued: libc::c_int = 0;
    let ioctl_ret = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &mut queued as *mut libc::c_int) };
    if ioctl_ret == -1 {
        // If TIOCOUTQ is unavailable (shouldn't happen on Linux), fall back to a
        // plain MSG_DONTWAIT send and treat partial writes as drops.
        return match send(fd, buf, MsgFlags::MSG_DONTWAIT) {
            Ok(n) if n == buf.len() => Ok(()),
            Ok(_) => Ok(()), // partial write — drop silently, stream may be corrupt
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("IPC non-blocking write: {e}")),
        };
    }

    // SO_SNDBUF: Linux reports 2× the requested value; use it directly as a
    // conservative upper bound for available send-buffer capacity.
    let sndbuf = nix::sys::socket::getsockopt(
        &unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
        nix::sys::socket::sockopt::SndBuf,
    )
    .unwrap_or(212992) as libc::c_int; // 212992 is the Linux default SO_SNDBUF

    let available = sndbuf.saturating_sub(queued) as usize;
    if available < buf.len() {
        // Not enough room — drop this non-critical message to avoid a partial write.
        return Ok(());
    }

    match send(fd, buf, MsgFlags::MSG_DONTWAIT) {
        Ok(n) if n == buf.len() => Ok(()),
        // With the space check above a partial write should not happen, but handle
        // it defensively: drop and signal the caller so it can log a warning.
        Ok(n) => Err(anyhow::anyhow!(
            "IPC partial non-blocking write: {n}/{} bytes sent despite space check",
            buf.len()
        )),
        Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("IPC non-blocking write: {e}")),
    }
}

fn recv_hello(fd: RawFd, buf: &mut [u8]) -> Result<String> {
    // magic(4) + name_len(4)
    read_exact(fd, &mut buf[..8])?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC_HELLO {
        bail!("bad hello magic: 0x{magic:08X}");
    }
    let name_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    if name_len > 256 {
        bail!("name too long: {name_len}");
    }
    read_exact(fd, &mut buf[..name_len])?;
    // Save name before overwriting the buffer with the width/height fields below.
    let name = String::from_utf8_lossy(&buf[..name_len]).into_owned();
    // width(4) + height(4) – consume but ignore (comes from config)
    read_exact(fd, &mut buf[..8])?;
    Ok(name)
}

fn recv_fds(sock_fd: RawFd, count: usize) -> Result<Vec<OwnedFd>> {
    let mut cmsg_buf = cmsg_space!([RawFd; 4]);
    let mut iov_buf  = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut iov_buf[..])];

    let msg = recvmsg::<UnixAddr>(sock_fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty())
        .context("recvmsg for fds")?;

    let mut fds: Vec<OwnedFd> = Vec::new();
    for cmsg in msg.cmsgs().context("iterating control messages")? {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            for fd in received {
                let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                fds.push(owned);
            }
        }
    }
    if fds.len() < count {
        bail!("received {} fds, need {}", fds.len(), count);
    }
    fds.truncate(count);
    Ok(fds)
}
