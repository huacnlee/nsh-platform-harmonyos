//! Platform-level request validation, PNG output buffer sizing and image URI
//! deduplication for offscreen Canvas 2D rendering.
//!
//! Policy limits (`MAX_SIDE`, `MAX_PIXELS`, `MAX_PENDING`) are enforced upstream
//! in `nsh_core::canvas::render` before `Command::RenderCanvas` is emitted; this
//! module only guards HarmonyOS FFI / buffer-size invariants.

use std::sync::Arc;

use nsh_core::canvas::{
    Drawing,
    render::{ImageFormat, RenderFailure},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RenderSpec {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) scale: f32,
    pub(crate) format: ImageFormat,
    pub(crate) byte_len: usize,
}

pub(crate) fn validate_request(
    width: u32,
    height: u32,
    scale: f32,
    format: ImageFormat,
) -> Result<RenderSpec, RenderFailure> {
    if format != ImageFormat::Png {
        return Err(RenderFailure::Failed(format!(
            "unsupported offscreen canvas image format: {}",
            format.mime()
        )));
    }
    if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
        return Err(RenderFailure::Failed(format!(
            "invalid offscreen canvas dimensions {width}x{height} at scale {scale}"
        )));
    }
    if i32::try_from(width).is_err() || i32::try_from(height).is_err() {
        return Err(RenderFailure::TooLarge(format!(
            "{width}x{height} exceeds HarmonyOS drawing image dimensions"
        )));
    }
    let byte_len = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            RenderFailure::TooLarge(format!(
                "{width}x{height} pixel buffer overflows address space"
            ))
        })?;
    Ok(RenderSpec {
        width,
        height,
        scale,
        format,
        byte_len,
    })
}

/// Initial output buffer capacity for `OH_ImagePackerNative_PackToDataFromPixelmap`.
/// Accounts for incompressible RGBA pixels plus PNG scanline filter bytes and chunk overhead.
pub(crate) fn pack_buffer_size(width: u32, height: u32) -> usize {
    let raw = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    raw.saturating_add(raw / 16)
        .saturating_add(height as usize)
        .saturating_add(4096)
        .max(4096)
}

/// Deduplicated image URIs referenced by `drawing`, preserving first-seen order.
pub(crate) fn unique_image_uris(drawing: &Drawing) -> Vec<Arc<str>> {
    let mut uris: Vec<Arc<str>> = Vec::new();
    for image in drawing.images() {
        if !uris.iter().any(|existing| existing == &image.uri) {
            uris.push(image.uri.clone());
        }
    }
    uris
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::canvas::{CanvasImage, CanvasOp, Num};

    #[test]
    fn validates_platform_dimensions_scale_and_overflow() {
        let spec = validate_request(200, 100, 2.0, ImageFormat::Png).unwrap();
        assert_eq!(spec.width, 200);
        assert_eq!(spec.height, 100);
        assert_eq!(spec.scale, 2.0);
        assert_eq!(spec.format, ImageFormat::Png);
        assert_eq!(spec.byte_len, 200 * 100 * 4);

        assert!(matches!(
            validate_request(0, 100, 1.0, ImageFormat::Png),
            Err(RenderFailure::Failed(_))
        ));
        assert!(matches!(
            validate_request(100, 100, 0.0, ImageFormat::Png),
            Err(RenderFailure::Failed(_))
        ));
        assert!(matches!(
            validate_request(100, 100, f32::NAN, ImageFormat::Png),
            Err(RenderFailure::Failed(_))
        ));
        assert!(matches!(
            validate_request(i32::MAX as u32 + 1, 16, 1.0, ImageFormat::Png),
            Err(RenderFailure::TooLarge(_))
        ));
        assert!(matches!(
            validate_request(16, i32::MAX as u32 + 1, 1.0, ImageFormat::Png),
            Err(RenderFailure::TooLarge(_))
        ));
    }

    #[test]
    fn pack_buffer_size_handles_tiny_and_large_canvases() {
        assert!(pack_buffer_size(1, 1) >= 4096);
        let large = pack_buffer_size(2048, 1024);
        assert!(large > 2048 * 1024 * 4);
    }

    #[test]
    fn deduplicates_drawing_image_uris_in_order() {
        let mut drawing = Drawing::default();
        for uri in ["asset://a.png", "asset://b.png", "asset://a.png"] {
            drawing.ops.push(CanvasOp::DrawImage {
                image: CanvasImage { uri: uri.into() },
                source: None,
                dest: [Num::Const(0.0); 4],
            });
        }
        let uris = unique_image_uris(&drawing);
        assert_eq!(uris.len(), 2);
        assert_eq!(uris[0].as_ref(), "asset://a.png");
        assert_eq!(uris[1].as_ref(), "asset://b.png");
    }
}
