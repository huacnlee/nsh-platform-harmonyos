//! Worker-owned native paint planes, ordered backdrop roots and group effects.
use std::sync::Arc;

use nsh_core::{BlendMode, Matrix as Transform, ViewStyle};
use ohos_drawing_binding::{Bitmap, BitmapFormat, Brush, Canvas, ClipOperation, Matrix};

use crate::render::{
    composition::{filter::Image, geometry::Bounds, worker::Key},
    geometry::Size,
};

pub(crate) struct PaintNode {
    pub(crate) source: Option<Key>,
    pub(crate) shaders: bool,
    pub(crate) shader_backdrop: bool,
    pub(crate) capture: bool,
    pub(crate) overlay: Option<Key>,
    pub(crate) transform: Transform,
    pub(crate) size: Size,
    pub(crate) style: Arc<ViewStyle>,
    pub(crate) opacity: f32,
    pub(crate) clip: bool,
    pub(crate) bounds: Bounds,
    pub(crate) children: Vec<PaintNode>,
}
pub(crate) struct Root {
    pub(crate) id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) node: PaintNode,
}
pub(crate) struct Plan {
    pub(crate) scale: f32,
    pub(crate) roots: Vec<Root>,
}
pub(crate) struct Frame {
    pub(crate) id: u32,
    pub(crate) image: Image,
}

pub(super) trait Sources {
    fn image(&self, key: Key) -> Option<(&Image, f32)>;
    fn shade(
        &mut self,
        key: Key,
        image: Image,
        backdrop: Option<Image>,
        scale: f32,
    ) -> Result<Image, String>;
}
struct Renderer<'a, S> {
    source: &'a mut S,
    scale: f32,
    pixels: u64,
}
const MAX_PIXELS: u64 = 67_108_864;
impl Plan {
    pub(super) fn render(&self, source: &mut impl Sources) -> Result<Option<Vec<Frame>>, String> {
        fn ready(node: &PaintNode, source: &impl Sources) -> bool {
            node.source.is_none_or(|key| source.image(key).is_some())
                && node.overlay.is_none_or(|key| source.image(key).is_some())
                && node.children.iter().all(|child| ready(child, source))
        }
        if !self.roots.iter().all(|root| ready(&root.node, source)) {
            return Ok(None);
        }
        let mut renderer = Renderer {
            source,
            scale: self.scale,
            pixels: 0,
        };
        let mut frames = Vec::with_capacity(self.roots.len());
        for root in &self.roots {
            let pixels = u64::from(root.width) * u64::from(root.height);
            renderer.reserve(pixels)?;
            let mut canvas = Canvas::with_bitmap(Bitmap::new(
                root.width,
                root.height,
                BitmapFormat::default(),
            ));
            canvas.clear(0);
            renderer.node(&root.node, &mut canvas, 0)?;
            frames.push(Frame {
                id: root.id,
                image: Image::copy(canvas.bitmap().unwrap(), 0.0, 0.0),
            });
            // Frames remain live until publication, and count against the same budget.
        }
        Ok(Some(frames))
    }
}
impl<S: Sources> Renderer<'_, S> {
    fn reserve(&mut self, pixels: u64) -> Result<(), String> {
        self.pixels = self
            .pixels
            .checked_add(pixels)
            .ok_or("scene pixel budget overflow")?;
        if self.pixels > MAX_PIXELS {
            return Err("scene intermediate pixel budget exceeded".into());
        }
        Ok(())
    }
    fn node(
        &mut self,
        node: &PaintNode,
        destination: &mut Canvas,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 256 {
            return Err("scene hierarchy exceeds 256 levels".into());
        }
        if node.opacity <= 0.0 || node.style.hidden {
            return Ok(());
        }
        destination.save();
        destination.concat(&pixel_matrix(node.transform, self.scale));
        let effects = node.style.effects.as_ref();
        let filters = effects.map_or(&[][..], |effects| effects.filters.as_slice());
        let masks = effects.map_or(&[][..], |effects| effects.mask_images.as_slice());
        let backdrop = effects.map_or(&[][..], |effects| effects.backdrop_filters.as_slice());
        // These are precisely the properties that establish a backdrop root.
        let grouped = node.shaders
            || node.capture
            || !masks.is_empty()
            || node.opacity < 1.0
            || !filters.is_empty()
            || !backdrop.is_empty()
            || node.style.blend_mode != BlendMode::Normal;
        if grouped {
            let [left, top, right, bottom] = node.bounds.0.map(|v| v * self.scale);
            let (left, top) = (left.floor(), top.floor());
            let (width, height) = (
                (right.ceil() - left).max(1.0) as u32,
                (bottom.ceil() - top).max(1.0) as u32,
            );
            let pixels = u64::from(width) * u64::from(height);
            self.reserve(pixels)?;
            let mut layer =
                Canvas::with_bitmap(Bitmap::new(width, height, BitmapFormat::default()));
            layer.clear(0);
            layer.translate(-left, -top);
            let mut shader_backdrop = None;
            if !backdrop.is_empty() || node.shader_backdrop {
                let matrix = destination.total_matrix();
                if let Some(inverse) = matrix.inverted() {
                    let prefix =
                        Canvas::with_bitmap(Bitmap::new(width, height, BitmapFormat::default()));
                    self.reserve(pixels)?;
                    prefix.clear(0);
                    prefix.translate(-left, -top);
                    prefix.concat(&inverse);
                    prefix.draw_bitmap(destination.bitmap().unwrap(), 0.0, 0.0);
                    let prefix = Image::copy(prefix.bitmap().unwrap(), left, top);
                    if node.shader_backdrop {
                        shader_backdrop = Some(Image::copy(&prefix.bitmap, prefix.x, prefix.y));
                    }
                    if !backdrop.is_empty() {
                        let prefix = prefix
                            .backdrop(backdrop, self.scale)
                            .map_err(str::to_owned)?;
                        layer.save();
                        self.clip(node, &layer);
                        layer.draw_bitmap(&prefix.bitmap, prefix.x, prefix.y);
                        layer.restore();
                    }
                    self.pixels -= pixels;
                }
            }
            self.contents(node, &mut layer, depth)?;
            let image = Image::copy(layer.bitmap().unwrap(), left, top)
                .filtered(filters, self.scale)
                .map_err(str::to_owned)?;
            let image = if masks.is_empty() {
                image
            } else {
                let canvas = Canvas::with_bitmap(image.bitmap);
                let mut blend = Brush::new();
                blend.set_blend_mode(ohos_drawing_binding::BlendMode::DstIn);
                // All mask layers compose together before their alpha masks
                // the filtered group; no pixels persist between scene frames.
                canvas.save_layer(None, Some(&blend));
                canvas.translate(-image.x, -image.y);
                canvas.scale(self.scale, self.scale);
                for mask in masks.iter().rev() {
                    let brush = crate::render::decor::gradient(mask, node.size);
                    canvas.attach_brush(&brush);
                    canvas.draw_rect(&ohos_drawing_binding::Rect::new(
                        0.0,
                        0.0,
                        node.size.width,
                        node.size.height,
                    ));
                    canvas.detach_brush();
                }
                canvas.restore();
                Image::copy(canvas.bitmap().unwrap(), image.x, image.y)
            };
            let image = if (node.shaders || node.capture)
                && let Some(key) = node.source
            {
                self.source.shade(key, image, shader_backdrop, self.scale)?
            } else {
                image
            };
            self.draw_group(&image, destination, node.opacity, node.style.blend_mode)?;
            self.pixels -= pixels;
        } else {
            self.contents(node, destination, depth)?;
        }
        destination.restore();
        Ok(())
    }
    fn clip(&self, node: &PaintNode, canvas: &Canvas) {
        canvas.save();
        canvas.scale(self.scale, self.scale);
        let path = crate::render::decor::outline(&node.style, node.size, 0.0);
        // Restore only the matrix; retaining this clip is intentional.
        let matrix = canvas.total_matrix();
        canvas.restore();
        let old = canvas.total_matrix();
        canvas.set_matrix(&matrix);
        canvas.clip_path(&path, ClipOperation::Intersect, true);
        canvas.set_matrix(&old);
    }
    fn contents(
        &mut self,
        node: &PaintNode,
        canvas: &mut Canvas,
        depth: usize,
    ) -> Result<(), String> {
        if let Some((image, scale)) = node.source.and_then(|key| self.source.image(key)) {
            canvas.save();
            canvas.scale(self.scale / scale, self.scale / scale);
            canvas.draw_bitmap(&image.bitmap, image.x, image.y);
            canvas.restore();
        }
        canvas.save();
        if node.clip {
            self.clip(node, canvas);
        }
        for child in &node.children {
            self.node(child, canvas, depth + 1)?;
        }
        if let Some((image, scale)) = node.overlay.and_then(|key| self.source.image(key)) {
            canvas.save();
            canvas.scale(self.scale / scale, self.scale / scale);
            canvas.draw_bitmap(&image.bitmap, image.x, image.y);
            canvas.restore();
        }
        canvas.restore();
        crate::render::decor::Decoration::default().outline(
            canvas,
            &node.style,
            node.size,
            self.scale,
        );
        Ok(())
    }
    fn draw_group(
        &mut self,
        image: &Image,
        destination: &mut Canvas,
        opacity: f32,
        mode: BlendMode,
    ) -> Result<(), String> {
        let mut brush = Brush::new();
        brush.set_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
        if mode == BlendMode::PlusDarker {
            let bitmap = destination.bitmap().unwrap();
            let (width, height) = (bitmap.width(), bitmap.height());
            let pixels = u64::from(width) * u64::from(height);
            self.reserve(pixels)?;
            let plane = Canvas::with_bitmap(Bitmap::new(width, height, BitmapFormat::default()));
            plane.clear(0);
            plane.set_matrix(&destination.total_matrix());
            plane.attach_brush(&brush);
            plane.draw_bitmap(&image.bitmap, image.x, image.y);
            plane.detach_brush();
            // Apply the live native clip by drawing the result through the destination canvas.
            let mut result = Image::copy(bitmap, 0.0, 0.0);
            crate::render::composition::blend::plus_darker(
                result.bitmap.pixels_mut(),
                plane.bitmap().unwrap().pixels(),
            );
            destination.save();
            destination.reset_matrix();
            brush.set_alpha(255);
            brush.set_blend_mode(ohos_drawing_binding::BlendMode::Src);
            destination.attach_brush(&brush);
            destination.draw_bitmap(&result.bitmap, 0.0, 0.0);
            destination.detach_brush();
            destination.restore();
            self.pixels -= pixels;
        } else {
            brush.set_blend_mode(crate::render::decor::drawing_blend(mode));
            destination.attach_brush(&brush);
            destination.draw_bitmap(&image.bitmap, image.x, image.y);
            destination.detach_brush();
        }
        Ok(())
    }
}
fn pixel_matrix(matrix: Transform, scale: f32) -> Matrix {
    let m = matrix.0;
    let s = scale as f64;
    Matrix::from_values([
        m[0] as f32,
        m[4] as f32,
        (m[12] * s) as f32,
        m[1] as f32,
        m[5] as f32,
        (m[13] * s) as f32,
        (m[3] / s) as f32,
        (m[7] / s) as f32,
        m[15] as f32,
    ])
}
