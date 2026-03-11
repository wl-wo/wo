/// DMABUF import helpers.
///
/// When a [`crate::electron::ElectronFrame`] arrives the compositor:
/// 1. Looks up (or allocates) a `TextureSlot` for that window name.
/// 2. Imports the DMABUF fds into the EGL / GL renderer.
/// 3. Uses the resulting texture for compositing.
///
/// On the export side (serving Wayland clients) the compositor creates a
/// `zwp_linux_dmabuf_v1` buffer from the same backing GBM BO so that other
/// Wayland clients can access the pixel data zero-copy.

use std::collections::HashMap;

use anyhow::{Context, Result};
use smithay::backend::{
    allocator::{dmabuf::{Dmabuf, DmabufFlags}, Fourcc, Modifier},
    renderer::{gles::{GlesRenderer, GlesTexture}, ImportDma},
};

use crate::electron::ElectronFrame;

/// In-flight texture imported from an Electron DMABUF frame.
#[allow(dead_code)]
pub struct ElectronTexture {
    pub name:    String,
    pub width:   u32,
    pub height:  u32,
    pub texture: GlesTexture,
    /// Keep-alive handle so the underlying DMABUF fds stay open.
    pub dmabuf:  Dmabuf,
}

/// Cached DMABUF for a window with mutable pixel storage.
/// Allows efficient pixel updates without recreating the DMABUF.
#[allow(dead_code)]
pub struct WindowDmabufCache {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub stride: u32,
    pub dmabuf: Dmabuf,
    /// Mutable memfd fd for updating pixel data in-place
    pub memfd_fd: std::os::unix::io::OwnedFd,
}

impl WindowDmabufCache {
    /// Update pixels in the cached DMABUF without recreating it.
    /// Returns Ok(true) if pixels were successfully updated.
    #[allow(dead_code)]
    pub fn update_pixels(&mut self, pixels: &[u8]) -> Result<()> {
        use nix::unistd;

        // Seek to start
        unistd::lseek(&self.memfd_fd, 0, unistd::Whence::SeekSet)
            .context("seeking memfd to start")?;

        // Write new pixel data
        unistd::write(&self.memfd_fd, pixels)
            .context("writing pixels to memfd")?;

        Ok(())
    }
}

/// Cached DMABUF data for Electron frames (texture is re-imported each frame to avoid
/// lifetime issues with GlesRenderer contexts).
pub struct CachedDmabufFrame {
    pub name:    String,
    pub width:   u32,
    pub height:  u32,
    pub dmabuf:  Dmabuf,
}

/// Create a cached DMABUF for window pixel storage with in-place update capability.
#[allow(dead_code)]
pub fn create_window_dmabuf_cache(
    window_name: String,
    width: i32,
    height: i32,
    stride: u32,
    pixels: &[u8],
) -> Result<WindowDmabufCache> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::allocator::Modifier;

    // Create a memfd for the pixel data
    let memfd_fd = nix::sys::memfd::memfd_create(
        std::ffi::CStr::from_bytes_with_nul(b"wayland_window\0").unwrap(),
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC
    )
    .context("creating memfd for window DMABUF")?;

    // Truncate to size
    let size = pixels.len() as u64;
    nix::unistd::ftruncate(&memfd_fd, size as i64)
        .context("truncating memfd")?;

    // Write initial pixel data
    nix::unistd::write(&memfd_fd, pixels)
        .context("writing initial pixels to memfd")?;

    // Seek back to start for DMABUF builder
    nix::unistd::lseek(&memfd_fd, 0, nix::unistd::Whence::SeekSet)
        .context("seeking memfd to start")?;

    // Build DMABUF with the memfd
    let mut builder = Dmabuf::builder(
        (width, height),
        Fourcc::Argb8888,
        Modifier::Invalid,
        DmabufFlags::empty(),
    );

    // Clone the fd for DMABUF ownership
    let dup_fd = nix::unistd::dup(&memfd_fd).context("dup memfd for DMABUF")?;

    builder.add_plane(dup_fd, 0, 0, stride);

    let dmabuf = builder.build().context("building window DMABUF")?;

    Ok(WindowDmabufCache {
        name: window_name,
        width,
        height,
        stride,
        dmabuf,
        memfd_fd,
    })
}

/// Convert an [`ElectronFrame`] into a texture and import it into the renderer.
/// 
/// Now returns just the texture for immediate rendering; use `cache_frame_for_reimport`
/// to cache the DMABUF for later re-importing.
pub fn import_electron_frame(
    renderer: &mut GlesRenderer,
    frame: ElectronFrame,
) -> Result<(ElectronTexture, CachedDmabufFrame)> {
    use tracing::{debug, trace};
    
    trace!("Importing DMABUF frame: {}x{} format=0x{:08X} planes={}", 
        frame.width, frame.height, frame.format, frame.planes.len());
    
    let fourcc: Fourcc = fourcc_from_u32(frame.format)
        .with_context(|| format!("unknown fourcc 0x{:08X}", frame.format))?;

    let modifier = frame
        .planes
        .first()
        .map(|p| Modifier::from(p.modifier))
        .unwrap_or(Modifier::Invalid);

    trace!("Fourcc: {:?}, Modifier: {:?}", fourcc, modifier);

    let mut builder = Dmabuf::builder(
        (frame.width as i32, frame.height as i32),
        fourcc,
        modifier,
        DmabufFlags::empty(),
    );

    // Duplicate fds before passing to Dmabuf builder
    // This allows both the Dmabuf and the imported texture to safely own independent copies
    for (idx, plane) in frame.planes.into_iter().enumerate() {
        let dup_fd = nix::unistd::dup(&plane.fd)
            .with_context(|| format!("Failed to dup plane {} fd", idx))?;
        
        if !builder.add_plane(dup_fd, idx as u32, plane.offset, plane.stride) {
            debug!("Failed to add plane {} to DMABUF", idx);
            anyhow::bail!("too many planes in DMABUF frame");
        }
        trace!("Plane {}: offset={} stride={}", idx, plane.offset, plane.stride);
    }

    let dmabuf = builder.build().context("building DMABUF")?;
    trace!("DMABUF built successfully");
    
    let texture = renderer
        .import_dmabuf(&dmabuf, None)
        .context("importing DMABUF into renderer")?;
    
    trace!("DMABUF imported into renderer successfully");
    
    // Create a cache-friendly version with fresh DMABUF for later re-import
    let cache_dmabuf = dmabuf.clone();
    
    Ok((
        ElectronTexture {
            name: frame.name.clone(),
            width: frame.width,
            height: frame.height,
            texture,
            dmabuf,
        },
        CachedDmabufFrame {
            name:   frame.name,
            width:  frame.width,
            height: frame.height,
            dmabuf: cache_dmabuf,
        },
    ))
}

/// Export the texture backing DMABUF for Wayland clients.
#[allow(dead_code)]
pub fn export_dmabuf_for_client(texture: &ElectronTexture) -> Dmabuf {
    texture.dmabuf.clone()
}

/// Re-import a cached DMABUF frame into a renderer (used during rendering).
#[allow(dead_code)]
pub fn reimport_cached_frame(
    renderer: &mut GlesRenderer,
    cached: &CachedDmabufFrame,
) -> Result<GlesTexture> {
    renderer
        .import_dmabuf(&cached.dmabuf, None)
        .context("re-importing cached DMABUF")
}

/// Create a temporary DMABUF shm-backed frame from pixel data.
/// This is for transmitting window pixel data as DMABUF fds rather than raw pixels.
/// Each window gets its own DMABUF.
#[allow(dead_code)]
pub fn create_temp_dmabuf_from_pixels(
    width: i32,
    height: i32,
    stride: u32,
    pixels: &[u8],
) -> Result<Dmabuf> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::allocator::Modifier;

    // Create a memfd for the pixel data
    let memfd_fd = nix::sys::memfd::memfd_create(
        "wayland_window",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC
    )
    .context("creating memfd for DMABUF pixels")?;

    // Truncate to size
    let size = pixels.len() as u64;
    nix::unistd::ftruncate(&memfd_fd, size as i64)
        .context("truncating memfd")?;

    // Write pixel data directly using nix::unistd::write (no File wrapper)
    nix::unistd::write(&memfd_fd, pixels)
        .context("writing pixels to memfd")?;

    // Seek back to start
    nix::unistd::lseek(&memfd_fd, 0, nix::unistd::Whence::SeekSet)
        .context("seeking memfd to start")?;

    // memfd_fd is already an OwnedFd, just use it directly in builder
    // Build DMABUF with single plane
    let mut builder = Dmabuf::builder(
        (width, height),
        Fourcc::Argb8888,
        Modifier::Invalid,
        DmabufFlags::empty(),
    );

    builder.add_plane(memfd_fd, 0, 0, stride);

    builder.build().context("building DMABUF from pixels")
}

/// Export an offscreen texture as a DMABUF for zero-copy transmission.
/// Used to export Wayland window textures to comraw via IPC using individual DMABUFs.
#[allow(dead_code)]
pub fn export_texture_as_dmabuf(
    _renderer: &GlesRenderer,
    _texture: &GlesTexture,
    _width: u32,
    _height: u32,
) -> Result<Dmabuf> {
    // For now, we'll read pixels and create a DMABUF from them
    // A more optimal approach would use EGLImage to back the DMABUF, but
    // this is simpler and still zero-copy at the IPC boundary
    // This would need a mutable reference, which we don't have
    // So instead, for each window we'll create DMABUF on-demand after pixelreads
    anyhow::bail!("texture export not directly supported; use create_temp_dmabuf_from_pixels after reading pixels")
}

// ── Texture cache ─────────────────────────────────────────────────────────────

/// Keeps the most-recent DMABUF frame per window (textures are imported on-demand).
#[derive(Default)]
pub struct TextureCache {
    inner: HashMap<String, CachedDmabufFrame>,
}

impl TextureCache {
    pub fn insert_dmabuf(&mut self, frame: CachedDmabufFrame) {
        self.inner.insert(frame.name.clone(), frame);
    }

    pub fn get_dmabuf(&self, name: &str) -> Option<&CachedDmabufFrame> {
        self.inner.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<CachedDmabufFrame> {
        self.inner.remove(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CachedDmabufFrame> {
        self.inner.values()
    }
}

// ── Fourcc helpers ────────────────────────────────────────────────────────────

fn fourcc_from_u32(v: u32) -> Option<Fourcc> {
    Fourcc::try_from(v).ok()
}

/// Map a human-readable format string from the config to a DRM fourcc code.
#[allow(dead_code)]
pub fn format_str_to_fourcc(s: &str) -> Option<u32> {
    match s.to_uppercase().as_str() {
        "ARGB8888"              => Some(u32::from_le_bytes(*b"AR24")), // DRM_FORMAT_ARGB8888
        "BGRA8888"              => Some(u32::from_le_bytes(*b"BA24")), // DRM_FORMAT_BGRA8888
        "XRGB8888"              => Some(u32::from_le_bytes(*b"XR24")), // DRM_FORMAT_XRGB8888
        "ABGR8888"              => Some(u32::from_le_bytes(*b"AB24")), // DRM_FORMAT_ABGR8888
        "XBGR8888"              => Some(u32::from_le_bytes(*b"XB24")), // DRM_FORMAT_XBGR8888
        _                       => None,
    }
}
