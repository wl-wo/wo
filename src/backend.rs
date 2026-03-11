//! Unified rendering backend trait for nested and DRM compositors
//!
//! This module provides a trait-based abstraction for rendering that works
//! with both Nested (GLES via winit) and DRM/KMS backends.

use anyhow::Result;
use smithay::utils::{Physical, Rectangle};
use crate::dmabuf::ElectronTexture;

/// Common interface for rendering frames across different backends
#[allow(dead_code)]
pub trait RenderBackend {
    /// Begin rendering a new frame with the given size and transform
    fn begin_frame(&mut self, width: i32, height: i32) -> Result<()>;

    /// Clear the frame with the given color
    fn clear_frame(&mut self, r: f32, g: f32, b: f32, a: f32) -> Result<()>;

    /// Render a texture at the given position
    fn render_texture(
        &mut self,
        texture: &ElectronTexture,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<()>;

    /// Finish rendering the current frame
    fn finish_frame(&mut self) -> Result<()>;

    /// Submit the rendered frame to the display/framebuffer
    fn submit_frame(&mut self) -> Result<()>;

    /// Get the output rectangle dimensions
    fn output_rect(&self) -> Rectangle<i32, Physical>;
}

/// Frame abstraction for rendering operations
#[allow(dead_code)]
pub struct Frame {
    pub transform: smithay::utils::Transform,
    pub output_rect: Rectangle<i32, Physical>,
}

impl Frame {
    #[allow(dead_code)]
    pub fn new(output_rect: Rectangle<i32, Physical>) -> Self {
        Frame {
            transform: smithay::utils::Transform::Normal,
            output_rect,
        }
    }
}
