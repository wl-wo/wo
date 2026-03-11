//! XCursor theme loading and per-shape texture caching.
//!
//! Loads cursor images from an installed XCursor theme and converts them to
//! GLES textures on first use, caching the result for subsequent frames.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::backend::renderer::ImportMem;
use smithay::utils::Size;
use std::collections::HashMap;
use tracing::{info, warn};

/// Cached cursor texture with its hotspot offset.
#[derive(Clone)]
pub struct CursorImage {
    pub texture: GlesTexture,
    pub width: u32,
    pub height: u32,
    pub xhot: u32,
    pub yhot: u32,
}

/// Manages cursor theme loading and GL texture caching.
pub struct CursorThemeManager {
    theme: xcursor::CursorTheme,
    size: u32,
    cache: HashMap<String, Option<CursorImage>>,
}

impl CursorThemeManager {
    /// Load an XCursor theme by name with the given nominal cursor size.
    pub fn new(theme_name: &str, size: u32) -> Self {
        info!(
            "Loading cursor theme '{}' (size {})",
            theme_name, size
        );
        let theme = xcursor::CursorTheme::load(theme_name);
        Self {
            theme,
            size,
            cache: HashMap::new(),
        }
    }

    /// Get (or lazily load) the GL texture for the given cursor icon name.
    ///
    /// Returns `None` if the icon cannot be found in the theme or the texture
    /// import fails.
    pub fn get_cursor(
        &mut self,
        name: &str,
        renderer: &mut GlesRenderer,
    ) -> Option<&CursorImage> {
        if !self.cache.contains_key(name) {
            let img = self.load_cursor(name, renderer);
            self.cache.insert(name.to_string(), img);
        }
        self.cache.get(name).and_then(|opt| opt.as_ref())
    }

    fn load_cursor(
        &self,
        name: &str,
        renderer: &mut GlesRenderer,
    ) -> Option<CursorImage> {
        let icon_path = self.theme.load_icon(name).or_else(|| {
            // Try common XCursor aliases
            let fallback = match name {
                "default" => Some("left_ptr"),
                "pointer" => Some("hand2"),
                "text" => Some("xterm"),
                "wait" => Some("watch"),
                "progress" => Some("left_ptr_watch"),
                "help" => Some("question_arrow"),
                "crosshair" => Some("cross"),
                "move" => Some("fleur"),
                "not-allowed" => Some("crossed_circle"),
                "no-drop" => Some("crossed_circle"),
                "grab" => Some("openhand"),
                "grabbing" => Some("closedhand"),
                "e-resize" => Some("right_side"),
                "w-resize" => Some("left_side"),
                "n-resize" => Some("top_side"),
                "s-resize" => Some("bottom_side"),
                "ne-resize" => Some("top_right_corner"),
                "nw-resize" => Some("top_left_corner"),
                "se-resize" => Some("bottom_right_corner"),
                "sw-resize" => Some("bottom_left_corner"),
                "ew-resize" => Some("sb_h_double_arrow"),
                "ns-resize" => Some("sb_v_double_arrow"),
                "nesw-resize" => Some("fd_double_arrow"),
                "nwse-resize" => Some("bd_double_arrow"),
                "col-resize" => Some("sb_h_double_arrow"),
                "row-resize" => Some("sb_v_double_arrow"),
                "all-resize" | "all-scroll" => Some("fleur"),
                "context-menu" => Some("left_ptr"),
                "vertical-text" => Some("xterm"),
                "alias" => Some("link"),
                "copy" => Some("copy"),
                "cell" => Some("plus"),
                "zoom-in" => Some("zoom_in"),
                "zoom-out" => Some("zoom_out"),
                _ => None,
            };
            fallback.and_then(|fb| self.theme.load_icon(fb))
        })?;

        let data = std::fs::read(&icon_path).ok()?;
        let images = xcursor::parser::parse_xcursor(&data)?;

        // Pick the image closest to our target size.
        let best = images
            .iter()
            .min_by_key(|img| (img.size as i32 - self.size as i32).unsigned_abs())?;

        // XCursor pixels are ARGB (native byte order). Convert to RGBA for
        // GL import with Fourcc::Abgr8888 (which is RGBA in memory on little-endian).
        let pixel_count = (best.width * best.height) as usize;
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for pixel_argb in best.pixels_argb.chunks_exact(4) {
            // ARGB → RGBA
            rgba.push(pixel_argb[1]); // R
            rgba.push(pixel_argb[2]); // G
            rgba.push(pixel_argb[3]); // B
            rgba.push(pixel_argb[0]); // A
        }

        let size = Size::from((best.width as i32, best.height as i32));
        match renderer.import_memory(&rgba, Fourcc::Abgr8888, size, false) {
            Ok(texture) => {
                info!(
                    "Loaded cursor '{}' ({}x{}, hotspot {},{}) from {}",
                    name,
                    best.width,
                    best.height,
                    best.xhot,
                    best.yhot,
                    icon_path.display()
                );
                Some(CursorImage {
                    texture,
                    width: best.width,
                    height: best.height,
                    xhot: best.xhot,
                    yhot: best.yhot,
                })
            }
            Err(e) => {
                warn!("Failed to import cursor texture for '{}': {e}", name);
                None
            }
        }
    }
}
