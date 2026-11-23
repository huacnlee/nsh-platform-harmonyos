//! Text and custom painting, with CSS scene geometry.

use std::rc::{Rc, Weak};

use nsh_core::ViewKind;
use ohos_arkui_binding::types::advanced::NodeDirtyFlag;

use super::{Error, Node, Result, callback};
use crate::{
    render::{
        geometry::{Rect, Size},
        paint_node::PaintCanvas,
        shadow_geometry::{PaintBounds, effective_background, has_non_native_outer_shadow},
    },
    text::typography::{NativeText, argb},
};

impl Node {
    pub(crate) fn promote_direct_paint(&self) -> Result<()> {
        if let Some(paint) = self.drawing.borrow().as_ref() {
            paint.promote_to_carrier(-1)?;
        }
        if let Some(paint) = self.outlined.borrow().as_ref() {
            paint.promote_to_carrier(i32::MAX)?;
        }
        Ok(())
    }

    pub(crate) fn compute_drawing_bounds(&self, style: &nsh_core::ViewStyle) -> [i32; 4] {
        let frame = self.frame.get();
        let scale = self.scale.get();
        if has_non_native_outer_shadow(style, scale) {
            let b = PaintBounds::shadows(style, frame.width, frame.height, scale);
            [b.x, b.y, b.width, b.height]
        } else {
            [
                0,
                0,
                (frame.width * scale).round() as i32,
                (frame.height * scale).round() as i32,
            ]
        }
    }

    /// Custom ink belongs to a retained native modifier, so moving its tree to
    /// or from a texture target preserves the draw callback and its resources.
    pub(crate) fn enable_drawing(&self) -> Result<()> {
        if self.drawing.borrow().is_some() {
            return Ok(());
        }
        let allow_direct =
            self.outlined.borrow().is_none() && self.native.borrow().children().is_empty();
        let weak = self.weak.clone();
        let paint = crate::render::paint_node::PaintNode::new_attached(
            &self.native,
            -1,
            allow_direct,
            move |canvas| {
                if let Some(node) = weak.upgrade() {
                    node.draw(canvas);
                }
            },
        )?;
        *self.drawing.borrow_mut() = Some(paint);
        self.invalidate_paint()
    }

    pub(crate) fn invalidate_paint(&self) -> Result<()> {
        if self.batch_created.get() {
            self.paint_deferred.set(true);
            return Ok(());
        }
        self.paint_deferred.set(false);
        if let Some(paint) = self.drawing.borrow().as_ref() {
            let bounds = self.compute_drawing_bounds(&self.style.borrow());
            self.drawing_bounds.set(bounds);
            paint.bounds(bounds)?;
            paint.invalidate()?;
        }
        if let Some(paint) = self.outlined.borrow().as_ref() {
            let frame = self.frame.get();
            let pad = (self.applied.get().ink_outset * self.scale.get()).ceil() as i32;
            paint.bounds([
                -pad,
                -pad,
                (frame.width * self.scale.get()).ceil() as i32 + pad * 2,
                (frame.height * self.scale.get()).ceil() as i32 + pad * 2,
            ])?;
            paint.invalidate()?;
        }
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }

    pub(crate) fn scene_active(&self) -> bool {
        self.scene_prepared.get()
    }

    pub(crate) fn invalidate_composites(&self) {
        self.shaders.invalidate_visibility();
        self.composite_dirty.set(true);
        self.capture_cache.set(None);
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(node) = at {
            let was_dirty = node.composite_dirty.replace(true);
            let had_cache = node.capture_cache.take().is_some();
            if was_dirty && !had_cache {
                break;
            }
            at = node.parent_node.borrow().upgrade();
        }
    }

    pub(crate) fn invalidate_scene(&self) {
        self.shaders.invalidate_visibility();
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.invalidate();
        }
    }

    pub(crate) fn scene_opacity(&self) -> f32 {
        self.visual_opacity_for(self.style.borrow().opacity)
    }

    pub(crate) fn has_native_paint(&self, style: &nsh_core::ViewStyle) -> bool {
        self.id == nsh_core::ROOT_ID
            || self.scene_root.get()
            || self.kind != ViewKind::View
            || self.shaders.contains(self.id)
            || !style.background_color.is_transparent()
            || style.borders.widths.any(|v| v.0 > 0.0)
            || style.outline.is_visible()
            || style.effects.as_ref().is_some_and(|effects| {
                !effects.background_images.is_empty() || !effects.box_shadows.is_empty()
            })
    }

    pub(crate) fn supports_image_color_filter(&self, style: &nsh_core::ViewStyle) -> bool {
        self.image.borrow().is_some()
            && self.children.borrow().is_empty()
            && style.tint_color.is_none()
            && style.background_color.is_transparent()
            && !style.borders.widths.any(|v| v.0 > 0.0)
            && !style.outline.is_visible()
            && style.effects.as_ref().is_none_or(|effects| {
                effects.background_images.is_empty() && effects.box_shadows.is_empty()
            })
    }

    pub(crate) fn needs_scene_texture_snapshot(&self, style: &nsh_core::ViewStyle) -> bool {
        if self.scene_ancestor_captured.get() {
            return self.has_native_paint(style);
        }
        if self.id != nsh_core::ROOT_ID
            && matches!(self.kind, ViewKind::View | ViewKind::Text)
            && self.component.borrow().is_none()
            && self.canvas.borrow().is_none()
            && !self.shaders.contains(self.id)
            && !self.navigation_capture.get()
            && !self.has_selection()
            && self.selection.range.get().is_none()
            && (self.kind != ViewKind::Text || self.children.borrow().is_empty())
        {
            let widths = style.borders.widths.to_array().map(|v| v.0);
            let colors = style.borders.colors;
            let uniform_border = widths.iter().all(|&w| (w - widths[0]).abs() <= 1e-4)
                && (widths[0] <= 0.0 || colors.iter().all(|&c| c == colors[0]));
            if uniform_border {
                return false;
            }
        }
        self.has_native_paint(style)
    }

    pub(crate) fn direct_scene_text(&self) -> Option<crate::render::composition::draw::DirectText> {
        let drawing = self.text_drawing.borrow().as_ref()?.clone();
        let content = self.content.get();
        let raster_scale = self
            .raster
            .borrow()
            .as_ref()
            .and_then(|layout| layout.0.downcast_ref::<NativeText>())
            .map_or(self.scale.get(), |raster| raster.scale);
        let tint = self
            .tint
            .get()
            .or_else(|| {
                self.live
                    .borrow()
                    .as_ref()
                    .and_then(|live| live.text_color())
            })
            .map(argb);
        Some(crate::render::composition::draw::DirectText {
            drawing,
            x: content.x,
            y: content.y + self.raster_origin_y.get(),
            raster_scale,
            tint,
        })
    }

    pub(crate) fn capture_bounds(
        &self,
        depth: usize,
    ) -> Result<crate::render::composition::geometry::Bounds> {
        use crate::render::composition::geometry::Bounds;
        if depth > 256 {
            return Err(Error::Unsupported(
                "compositor hierarchy exceeds 256 levels",
            ));
        }
        if let Some(bounds) = self.capture_cache.get() {
            return Ok(bounds);
        }
        let frame = self.frame.get();
        let mut bounds = Bounds::new(frame.width, frame.height)
            .shaders(self.shaders.overflow(self.id), &self.shader_layers.borrow())
            .own(&self.style.borrow());
        if !self.clips_children() {
            for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
                if (child.kind == ViewKind::Modal
                    && !child
                        .modal
                        .borrow()
                        .as_ref()
                        .is_some_and(|modal| modal.is_main_window_host()))
                    || child.style.borrow().hidden
                {
                    continue;
                }
                let child_frame = child.frame.get();
                let filters = child
                    .style
                    .borrow()
                    .effects
                    .as_ref()
                    .map(|effects| effects.filters.clone())
                    .unwrap_or_default();
                let matrix = child
                    .input_transform
                    .get()
                    .mul(&nsh_core::Matrix::of(
                        nsh_core::TransformKind::TranslateX,
                        child_frame.x as f64,
                    ))
                    .mul(&nsh_core::Matrix::of(
                        nsh_core::TransformKind::TranslateY,
                        child_frame.y as f64,
                    ));
                let child = child
                    .capture_bounds(depth + 1)?
                    .filters(&filters)
                    .transformed(matrix)
                    .ok_or(Error::Unsupported("unbounded projected compositor capture"))?;
                bounds.union(child);
            }
        }
        self.capture_cache.set(Some(bounds));
        Ok(bounds)
    }

    pub(crate) fn paint_visible(&self) -> bool {
        self.active.get()
            && !self.retained.get()
            && !self.backface_away.get()
            && !self.style.borrow().hidden
            && self.header_item_visible()
            && self
                .screen
                .borrow()
                .as_ref()
                .is_none_or(|screen| screen.paint_visible())
            && self
                .parent_node
                .borrow()
                .upgrade()
                .is_none_or(|parent| parent.paint_visible())
    }

    pub(crate) fn text(
        self: &Rc<Self>,
        raster: &NativeText,
        content: Rect,
        layout: nsh_core::TextLayout,
    ) -> Result<bool> {
        let same_layout = self
            .raster
            .borrow()
            .as_ref()
            .is_some_and(|old| std::sync::Arc::ptr_eq(&old.0, &layout.0));
        let same_placement =
            self.content.get() == content && self.raster_origin_y.get() == raster.origin_y;
        if !same_layout {
            let change = self
                .raster
                .borrow()
                .as_ref()
                .and_then(|old| old.0.downcast_ref::<NativeText>())
                .map(|old| {
                    nsh_core::transition::recolor(
                        &old.source,
                        &raster.source,
                        &self.style.borrow(),
                        self.platform.get().reduce_motion,
                    )
                });
            if let Some(change) = change {
                if change.starts() {
                    self.live
                        .borrow_mut()
                        .get_or_insert_with(Default::default)
                        .recolor(change);
                    self.animator.track(self);
                } else if let Some(live) = self.live.borrow_mut().as_mut() {
                    live.recolor(change);
                }
            }
            self.selection.range.take();
            *self.text_drawing.borrow_mut() = raster
                .drawing
                .lock()
                .expect("text drawing")
                .take()
                .map(|d| std::sync::Arc::new(std::sync::Mutex::new(d)));
        }
        if self.text_drawing.borrow().is_some() {
            if self.batch_created.get() {
                self.paint_deferred.set(true);
            } else {
                self.enable_drawing()?;
            }
        }
        let has_links = !raster.links.is_empty();
        let links_changed = self.has_text_links.replace(has_links) != has_links || has_links;
        *self.raster.borrow_mut() = Some(layout);
        self.content.set(content);
        self.raster_origin_y.set(raster.origin_y);
        if self.has_selection() || self.wrapper.borrow().is_some() {
            self.ensure_wrapper()?;
        }
        if !same_layout || !same_placement {
            self.invalidate_paint()?;
        }
        if !same_layout || links_changed {
            self.set_events(self.interaction.mask())?;
        }
        Ok(links_changed)
    }

    pub(super) fn draw(&self, canvas: &PaintCanvas) {
        let frame = self.frame.get();
        let scale = self.scale.get();
        let has_area = frame.width > 0.0 && frame.height > 0.0;
        let (has_outer_shadows, has_inner_effects, remaining_gradients) = {
            let style = self.style.borrow();
            let (_, remaining_gradients) = effective_background(style.background_color, &style);
            let outer = has_area && has_non_native_outer_shadow(&style, scale);
            let inner = has_area
                && (remaining_gradients > 0
                    || style
                        .effects
                        .as_ref()
                        .is_some_and(|e| e.box_shadows.iter().any(|s| s.inset && s.is_visible())));
            (outer, inner, remaining_gradients)
        };
        if !has_outer_shadows && !has_inner_effects {
            self.decoration.clear_paint_cache();
            if !has_area
                || (self.canvas.borrow().is_none()
                    && self.text_drawing.borrow().is_none()
                    && self.selection.range.get().is_none())
            {
                return;
            }
        }
        let [ox, oy, _, _] = self.drawing_bounds.get();
        let shifted = ox != 0 || oy != 0;
        if shifted {
            canvas.save();
            canvas.translate(-ox as f32, -oy as f32);
        }
        if has_outer_shadows || has_inner_effects {
            let style = self.style.borrow();
            let size = Size::new(frame.width, frame.height);
            if has_outer_shadows {
                self.decoration.shadows(canvas, &style, size, scale, false);
            }
            if has_inner_effects {
                self.decoration
                    .background(canvas, &style, size, scale, remaining_gradients);
            }
        }
        if let Some(drawing) = self.canvas.borrow().as_ref() {
            callback(|| drawing.draw(canvas));
        }
        self.draw_selection(canvas, false);
        if let Some(drawing) = self.text_drawing.borrow().as_ref() {
            let content = self.content.get();
            let tint = self
                .tint
                .get()
                .or_else(|| {
                    self.live
                        .borrow()
                        .as_ref()
                        .and_then(|live| live.text_color())
                })
                .map(argb);
            let raster_scale = self
                .raster
                .borrow()
                .as_ref()
                .and_then(|layout| layout.0.downcast_ref::<NativeText>())
                .map_or(scale, |raster| raster.scale);
            let tx = (content.x * scale).round();
            let ty = ((content.y + self.raster_origin_y.get()) * scale).round();
            let scale_mismatch = (scale - raster_scale).abs() > f32::EPSILON;
            let needs_save = tx != 0.0 || ty != 0.0 || scale_mismatch;
            if needs_save {
                canvas.save();
                if tx != 0.0 || ty != 0.0 {
                    canvas.translate(tx, ty);
                }
                if scale_mismatch {
                    canvas.scale(scale / raster_scale, scale / raster_scale);
                }
            }
            if let Err(error) = drawing
                .lock()
                .expect("text drawing")
                .draw_tinted(canvas, tint)
            {
                log::warn!("OHOS text draw: {error}");
            }
            if needs_save {
                canvas.restore();
            }
        }
        self.draw_selection(canvas, true);
        if shifted {
            canvas.restore();
        }
    }
}
