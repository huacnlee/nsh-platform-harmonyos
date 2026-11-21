//! Applying style attributes and restoring temporary native presentation.

use std::rc::Rc;

use nsh_core::{ViewKind, ViewStyle};
use ohos_arkui_binding::{
    common::attribute::ArkUINodeAttributeNumber as Number,
    component::attribute::ArkUICommonAttribute,
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};

use super::{NativeNode, Node, Result};
use crate::{render::geometry::Size, text::typography::argb};

impl Node {
    pub(crate) fn density_changed(&self) -> Result<()> {
        self.decoration.clear_paint_cache();
        self.invalidate_composites();
        self.update_layout_bounds()?;
        self.restore_style()?;
        self.animator.apply_node(self)?;
        self.native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        self.invalidate_paint()?;
        Ok(())
    }

    /// The corner radii its native node was given (top-left first), which
    /// platform-owned children that fill it clip to.
    pub(crate) fn border_radii(&self) -> [f32; 4] {
        self.applied.get().border_radius
    }
    pub(crate) fn clips_children(&self) -> bool {
        self.style.borrow().overflow_hidden
            || matches!(
                self.kind,
                ViewKind::Scroll | ViewKind::Image | ViewKind::Stack | ViewKind::Input
            )
            || self.modal.borrow().as_ref().is_some_and(|modal| {
                modal.presentation() != nsh_core::ModalPresentation::FullScreen
            })
    }
    pub(crate) fn header_item_visible(&self) -> bool {
        if self.kind != ViewKind::HeaderItem {
            return true;
        }
        let Some(parent) = self.parent_node.borrow().upgrade() else {
            return false;
        };
        let Some(screen) = parent.screen.borrow().as_ref().cloned() else {
            return false;
        };
        let c = screen.config.borrow();
        c.header_visible
            && (c.left == Some(self.id)
                || c.right == Some(self.id)
                || c.title_view == Some(self.id))
    }
    pub(super) fn can_defer_style(&self, style: &ViewStyle) -> bool {
        self.batch_created.get()
            && self.id != nsh_core::ROOT_ID
            && !matches!(self.kind, ViewKind::Screen | ViewKind::Modal)
            && !self.scene_root.get()
            && !self.has_selection()
            && !self.wrapper_owns_semantics()
            && self.wrapper.borrow().is_none()
            && self.image.borrow().is_none()
            && self.live.borrow().is_none()
            && !self.effects_plan_for(style).requires_texture
    }
    fn ensure_outlined_paint(&self, style: &ViewStyle) -> Result<()> {
        if style.outline.width.0 > 0.0 && self.outlined.borrow().is_none() {
            if self.drawing.borrow().is_some() {
                self.promote_direct_paint()?;
            }
            let allow_direct =
                self.drawing.borrow().is_none() && self.native.borrow().children().is_empty();
            let weak = self.weak.clone();
            let paint = crate::render::paint_node::PaintNode::new_attached(
                &self.native,
                i32::MAX,
                allow_direct,
                move |canvas| {
                    if let Some(node) = weak.upgrade()
                        && !node.scene_active()
                    {
                        let outset = (node.applied.get().ink_outset * node.scale.get()).ceil();
                        canvas.save();
                        canvas.translate(outset, outset);
                        let frame = node.frame.get();
                        node.decoration.outline(
                            canvas,
                            &node.style.borrow(),
                            Size::new(frame.width, frame.height),
                            node.scale.get(),
                        );
                        canvas.restore();
                    }
                },
            )?;
            *self.outlined.borrow_mut() = Some(paint);
        }
        Ok(())
    }
    pub(crate) fn style(self: &Rc<Self>, style: &ViewStyle) -> Result<()> {
        self.invalidate_composites();
        let cursor_changed = self.style.borrow().cursor != style.cursor;
        let mut target = Box::new(style.clone());
        if self.live.borrow().is_some() || target.transition.is_some() || target.animation.is_some()
        {
            let old = self.style.borrow().clone();
            self.live
                .borrow_mut()
                .get_or_insert_with(Default::default)
                .style(
                    &mut target,
                    *old,
                    self.platform.get().reduce_motion,
                    self.styled.get(),
                );
            self.animator.track(self);
        }
        let (paint_dirty, had_effects, hidden_changed) = {
            let prev = self.style.borrow();
            let dirty = prev.effects != target.effects
                || prev.outline != target.outline
                || ((prev.border_radius != target.border_radius
                    || prev.corner_radius != target.corner_radius
                    || prev.borders.widths != target.borders.widths)
                    && (prev.effects.is_some()
                        || target.effects.is_some()
                        || prev.outline.is_visible()
                        || target.outline.is_visible()));
            (dirty, prev.effects.is_some(), prev.hidden != target.hidden)
        };
        let has_effects = target.effects.is_some();
        self.styled.set(true);
        *self.style.borrow_mut() = target;
        if hidden_changed {
            if let Some(parent) = self.parent_node.borrow().upgrade()
                && let Some(scroll) = parent.scroll.borrow().as_ref()
            {
                scroll.sync_inferred_size()?;
            }
            if let Some(scene) = self.scene.borrow().as_ref()
                && (scene.has_inactive_requirements() || scene.is_ancestor_of_active_root(self))
            {
                scene.invalidate();
            }
        }
        if cursor_changed {
            self.set_events(self.interaction.mask())?;
            self.cursor.update(self, self.interaction.hovered())?;
        }
        if self.can_defer_style(&self.style.borrow()) {
            if had_effects
                && !has_effects
                && let Some(scene) = self.scene.borrow().as_ref()
            {
                scene.require(self);
            }
            self.style_deferred.set(true);
            if paint_dirty {
                self.paint_deferred.set(true);
            }
            return Ok(());
        }
        if (had_effects || has_effects || !self.shader_layers.borrow().is_empty())
            && let Some(scene) = self.scene.borrow().as_ref()
        {
            scene.require(self);
        }
        self.ensure_wrapper()?;
        self.restore_style()?;
        if paint_dirty {
            self.invalidate_paint()?;
        }
        Ok(())
    }
    pub(crate) fn applied_box_colors(&self) -> (u32, [u32; 4]) {
        let applied = self.applied.get();
        (applied.background_color, applied.border_color)
    }

    pub(super) fn sync_visual_node(&self, visual: &NativeNode) {
        let ptr = Rc::as_ptr(visual) as usize;
        let mut applied = self.applied.get();
        if applied.visual_ptr != ptr {
            applied.visual_ptr = ptr;
            applied.visibility = 0;
            applied.transform = None;
            applied.transform_center = [f32::NAN; 3];
            applied.blur = 0.0;
            applied.grayscale = 0.0;
            applied.sepia = 0.0;
            applied.invert = 0.0;
            applied.contrast = 1.0;
            applied.blend = nsh_core::BlendMode::Normal;
            applied.z_index = 0;
            self.applied.set(applied);
            self.interaction.invalidate_opacity();
        }
    }
    fn unhidden_for_effects(&self, style: &ViewStyle) -> bool {
        if !self.active.get()
            || self.retained.get()
            || self.is_settled_popped_screen()
            || style.hidden
        {
            return false;
        }
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(current) = at {
            if !current.active.get()
                || current.retained.get()
                || current.is_settled_popped_screen()
                || current.style.borrow().hidden
            {
                return false;
            }
            at = current.parent_node.borrow().upgrade();
        }
        true
    }
    pub(crate) fn effects_plan_for(
        &self,
        style: &ViewStyle,
    ) -> crate::render::effects::EffectsPlan {
        let surface_active = self.shaders.surface_active();
        let hosts_retainable = self.shaders.hosts_retainable();
        let unhidden = hosts_retainable && self.unhidden_for_effects(style);
        let shaders = self.shader_layers.borrow();
        let has_shaders = (!shaders.is_empty() || !self.modal_screen_layers.borrow().is_empty())
            && hosts_retainable
            && (unhidden || (surface_active && self.shaders.navigation_key(self.id).is_some()));
        let shader_samples_backdrop = has_shaders
            && self.id != nsh_core::ROOT_ID
            && shaders.iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            });
        drop(shaders);
        let composite_active = self
            .wrapper
            .borrow()
            .as_ref()
            .is_some_and(|wrapper| wrapper.composite.borrow().as_ref().is_some());
        crate::render::effects::EffectsPlan::compute(
            style,
            crate::render::effects::EffectsContext {
                has_shaders,
                shader_samples_backdrop,
                navigation_capture: surface_active && self.navigation_capture.get(),
                scene_active: self.scene_active(),
                composite_active,
                has_native_paint: self.needs_scene_texture_snapshot(style),
                supports_image_color_filter: self.supports_image_color_filter(style),
            },
        )
    }
    pub(crate) fn effects_plan(&self) -> crate::render::effects::EffectsPlan {
        self.effects_plan_for(&self.style.borrow())
    }
    pub(crate) fn desired_z_index(&self) -> i32 {
        if self.pinned.get() != (0.0, 0.0) {
            1_000_000
        } else if self.kind == ViewKind::HeaderItem {
            2
        } else {
            0
        }
    }
    pub(crate) fn apply_z_index(&self) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let z_index = self.desired_z_index();
        let mut applied = self.applied.get();
        if applied.z_index != z_index {
            visual
                .borrow()
                .set_i32_attribute(Attribute::ZIndex, z_index)?;
            applied.z_index = z_index;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(super) fn apply_effects(&self, style: &ViewStyle) -> Result<()> {
        use crate::render::effects::NativeColorFilter;
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let plan = self.effects_plan_for(style);
        let mut applied = self.applied.get();
        let scale = self.scale.get();
        let blur = crate::render::composition::geometry::native_blur_value(plan.native_blur, scale);
        if applied.blur != blur {
            visual.borrow().set_f32_attribute(Attribute::Blur, blur)?;
            applied.blur = blur;
        }
        let backdrop_blur = crate::render::composition::geometry::native_blur_value(
            plan.native_backdrop_blur,
            scale,
        );
        if applied.backdrop_blur != backdrop_blur {
            self.native
                .borrow()
                .set_f32_attribute(Attribute::BackdropBlur, backdrop_blur)?;
            applied.backdrop_blur = backdrop_blur;
        }
        let (grayscale, sepia, invert, contrast) = match plan.native_color_filter {
            NativeColorFilter::Grayscale(v) => (v, 0.0, 0.0, 1.0),
            NativeColorFilter::Sepia(v) => (0.0, v, 0.0, 1.0),
            NativeColorFilter::Invert(v) => (0.0, 0.0, v, 1.0),
            NativeColorFilter::Contrast(v) => (0.0, 0.0, 0.0, v),
            NativeColorFilter::None | NativeColorFilter::ImageMatrix(_) => (0.0, 0.0, 0.0, 1.0),
        };
        if applied.grayscale != grayscale {
            if grayscale == 0.0 {
                visual.borrow().reset_attribute(Attribute::GrayScale)?;
            } else {
                visual
                    .borrow()
                    .set_f32_attribute(Attribute::GrayScale, grayscale)?;
            }
            applied.grayscale = grayscale;
        }
        if applied.sepia != sepia {
            if sepia == 0.0 {
                visual.borrow().reset_attribute(Attribute::Sepia)?;
            } else {
                visual.borrow().set_f32_attribute(Attribute::Sepia, sepia)?;
            }
            applied.sepia = sepia;
        }
        if applied.invert != invert {
            if invert == 0.0 {
                visual.borrow().reset_attribute(Attribute::Invert)?;
            } else {
                visual
                    .borrow()
                    .set_f32_attribute(Attribute::Invert, invert)?;
            }
            applied.invert = invert;
        }
        if applied.contrast != contrast {
            if contrast == 1.0 {
                visual.borrow().reset_attribute(Attribute::Contrast)?;
            } else {
                visual
                    .borrow()
                    .set_f32_attribute(Attribute::Contrast, contrast)?;
            }
            applied.contrast = contrast;
        }
        if applied.blend != plan.native_blend {
            let mode = crate::render::decor::arkui_blend_mode(plan.native_blend);
            visual
                .borrow()
                .set_attribute(Attribute::BlendMode, vec![i32::from(mode), 1].into())?;
            applied.blend = plan.native_blend;
        }
        self.applied.set(applied);
        Ok(())
    }
    pub(crate) fn reset_visual_baseline(&self, node: &NativeNode) -> Result<()> {
        let native = node.borrow();
        native.reset_attribute(Attribute::LayoutRect)?;
        native.reset_attribute(Attribute::Position)?;
        native.reset_attribute(Attribute::Transform)?;
        native.reset_attribute(Attribute::TransformCenter)?;
        native.reset_attribute(Attribute::Blur)?;
        native.reset_attribute(Attribute::GrayScale)?;
        native.reset_attribute(Attribute::Sepia)?;
        native.reset_attribute(Attribute::Invert)?;
        native.reset_attribute(Attribute::Contrast)?;
        native.reset_attribute(Attribute::BlendMode)?;
        native.reset_attribute(Attribute::ZIndex)?;
        native.opacity(1.0)?;
        native.set_i32_attribute(Attribute::Visibility, 0)?;
        Ok(())
    }
    pub(crate) fn apply_visual_style(&self, style: &ViewStyle) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        self.apply_effects(style)?;
        self.apply_z_index()?;
        self.transform(style, |_| None)?;
        self.apply_opacity(style.opacity)?;
        self.animator.apply_node(self)?;
        Ok(())
    }
    pub(crate) fn visual_opacity_for(&self, style_opacity: f32) -> f32 {
        crate::render::effects::resolve_visual_opacity(
            style_opacity,
            self.animated_opacity.get(),
            self.presentation.get().0,
            self.extra.get().0,
            self.navigation_presentation.get().0,
        )
    }
    pub(super) fn apply_opacity(&self, style_opacity: f32) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let scene_active = self.scene_active();
        if scene_active {
            self.invalidate_scene();
        }
        let scene_exported = scene_active
            && self
                .wrapper
                .borrow()
                .as_ref()
                .is_some_and(|wrapper| wrapper.composite.borrow().is_some());
        let filter_opacity = if scene_active {
            1.0
        } else {
            self.effects_plan().native_filter_opacity
        };
        self.interaction.opacity(
            self,
            crate::render::effects::resolve_native_node_opacity(
                self.native_visibility_hidden()
                    || (self.culled_by_scroll.get() && !self.navigation_capture.get()),
                scene_exported,
                self.visual_opacity_for(style_opacity) * filter_opacity,
            ),
        )
    }
    pub(super) fn apply_background_color_for(
        &self,
        base_color: nsh_core::Color,
        style: &ViewStyle,
    ) -> Result<()> {
        let (color, _) = crate::render::shadow_geometry::effective_background(base_color, style);
        let default_surface_bg = self.kind == ViewKind::Screen
            || self.modal.borrow().as_ref().is_some_and(|modal| {
                modal.presentation() != nsh_core::ModalPresentation::FullScreen
            });
        let bg = if default_surface_bg && color.is_transparent() {
            let dark = self
                .appearance
                .get()
                .resolve(self.platform.get().color_scheme)
                == nsh_engine::ColorScheme::Dark;
            self.theme.borrow().color("background", dark)
        } else {
            argb(color)
        };
        let mut applied = self.applied.get();
        if applied.background_color != bg {
            self.native.borrow().background_color(bg)?;
            applied.background_color = bg;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(super) fn apply_borders(&self, style: &ViewStyle) -> Result<()> {
        let corners = style
            .corner_radius
            .map(|r| r.unwrap_or(style.border_radius).0);
        let mut radius = [
            corners.top_left,
            corners.top_right,
            corners.bottom_left,
            corners.bottom_right,
        ];
        if let Some(modal) = self.modal.borrow().as_ref()
            && radius == [0.0; 4]
        {
            radius = match modal.presentation() {
                nsh_core::ModalPresentation::FullScreen => [0.0; 4],
                nsh_core::ModalPresentation::PageSheet => [16.0, 16.0, 0.0, 0.0],
                nsh_core::ModalPresentation::FormSheet => [16.0; 4],
            };
        }
        let frame = self.frame.get();
        if frame.width > 0.0 && frame.height > 0.0 {
            let short = frame.width.min(frame.height);
            let half = short * 0.5;
            let snapped_half = ((short * self.scale.get()).round() * 0.5) / self.scale.get();
            for r in &mut radius {
                if *r >= half - 1e-4 && *r < snapped_half {
                    *r = snapped_half;
                }
            }
        }
        let widths = style.borders.widths.to_array().map(|v| v.0);
        let mut colors = style.borders.colors.map(argb);
        // Rosen's `RSBorder::ApplyFourLine` fast path requires uniform color
        // across all 4 slots (`colors_.size() == 1`); otherwise `DrawBorderBase`
        // allocates a full `canvas.SaveLayer` offscreen render pass even when
        // 3 of the 4 border widths are zero (e.g. single-side dividers/bars).
        if let Some(first_active) = (0..4).find(|&i| widths[i] > 0.0).map(|i| colors[i])
            && (0..4).all(|i| widths[i] <= 0.0 || colors[i] == first_active)
        {
            colors = [first_active; 4];
        }
        let border_style = match style.borders.style {
            nsh_core::BorderStyle::Solid => 0,
            nsh_core::BorderStyle::Dashed => 1,
            nsh_core::BorderStyle::Dotted => 2,
        };
        let mut applied = self.applied.get();
        let node = self.native.borrow();
        if applied.border_radius != radius {
            let values = radius.map(Number::Float);
            node.set_number_slice_attribute(Attribute::BorderRadius, &values)?;
            applied.border_radius = radius;
        }
        if applied.border_width != widths {
            let values = widths.map(Number::Float);
            node.set_number_slice_attribute(Attribute::BorderWidth, &values)?;
            applied.border_width = widths;
        }
        if applied.border_color != colors {
            let values = colors.map(Number::Uint);
            node.set_number_slice_attribute(Attribute::BorderColor, &values)?;
            applied.border_color = colors;
        }
        if applied.border_style != border_style {
            node.set_i32_attribute(Attribute::BorderStyle, border_style)?;
            applied.border_style = border_style;
        }
        self.applied.set(applied);
        Ok(())
    }
    /// Outlines and box shadows own bounded retained paint nodes.
    pub(super) fn apply_ink_outset(&self, style: &ViewStyle) -> Result<()> {
        let mut outset = 0.0f32;
        if style.outline.is_visible() {
            outset = outset.max(style.outline.width.0 + style.outline.offset.0);
        }
        let outset = outset.max(0.0).ceil();
        let mut applied = self.applied.get();
        if applied.ink_outset != outset {
            applied.ink_outset = outset;
            self.applied.set(applied);
            self.invalidate_ink(outset.max(1.0))?;
            self.invalidate_paint()?;
        }
        Ok(())
    }
    /// A view whose paint reaches outside its bounds moved: ArkUI repaints
    /// only the view's own region, so its parent repaints the area the paint
    /// left (inside the parent's bounds).
    pub(super) fn invalidate_ink(&self, outset: f32) -> Result<()> {
        if outset > 0.0
            && let Some(parent) = self.parent_node.borrow().upgrade()
        {
            parent
                .child_container()
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    pub(super) fn apply_shadow(&self, style: &ViewStyle) -> Result<()> {
        use crate::render::shadow_geometry::{native_box_shadow, native_radius};
        let scale = self.scale.get();
        let next = native_box_shadow(style, scale).map(|shadow| {
            (
                native_radius(shadow.blur.0 * scale * 0.5),
                shadow.offset_x.0 * scale,
                shadow.offset_y.0 * scale,
                argb(shadow.color),
            )
        });
        let mut applied = self.applied.get();
        if applied.shadow != next {
            let node = self.native.borrow();
            if let Some((radius, ox, oy, color)) = next {
                node.set_number_slice_attribute(
                    Attribute::CustomShadow,
                    &[
                        Number::Float(radius),
                        Number::Int(0),
                        Number::Float(ox),
                        Number::Float(oy),
                        Number::Int(0),
                        Number::Uint(color),
                        Number::Uint(0),
                    ],
                )?;
            } else {
                node.reset_attribute(Attribute::CustomShadow)?;
            }
            applied.shadow = next;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(crate) fn apply_clip(&self) -> Result<()> {
        let clip = self.clips_children();
        let mut applied = self.applied.get();
        if applied.clip != clip {
            self.native
                .borrow()
                .set_bool_attribute(Attribute::Clip, clip)?;
            applied.clip = clip;
            self.applied.set(applied);
            let frame = self.frame.get();
            if frame.width <= 0.0 || frame.height <= 0.0 {
                self.update_layout_bounds()?;
                self.native
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        }
        Ok(())
    }
    pub(super) fn apply_style(&self, style: &ViewStyle) -> Result<()> {
        use crate::render::shadow_geometry::{effective_background, has_non_native_outer_shadow};
        self.style_deferred.set(false);
        self.ensure_outlined_paint(style)?;
        let (_, remaining_gradients) = effective_background(style.background_color, style);
        let needs_drawing = remaining_gradients > 0
            || has_non_native_outer_shadow(style, self.scale.get())
            || style.effects.as_ref().is_some_and(|effects| {
                effects
                    .box_shadows
                    .iter()
                    .any(|shadow| shadow.inset && shadow.is_visible())
            });
        if needs_drawing {
            self.enable_drawing()?;
        }
        if let Some(paint) = self.drawing.borrow().as_ref() {
            let bounds = self.compute_drawing_bounds(style);
            if self.drawing_bounds.replace(bounds) != bounds {
                paint.bounds(bounds)?;
            }
        }
        crate::render::decor::warn_repeating_conic(self, style);
        if let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.update(self)?;
        }
        if let Some(image) = self.image.borrow().as_ref() {
            let filter_matrix = match self.effects_plan_for(style).native_color_filter {
                crate::render::effects::NativeColorFilter::ImageMatrix(matrix) => Some(matrix),
                _ => None,
            };
            image.style(style, filter_matrix)?;
        }
        self.apply_background_color_for(style.background_color, style)?;
        self.apply_borders(style)?;
        self.apply_shadow(style)?;
        self.apply_ink_outset(style)?;
        self.apply_clip()?;
        self.apply_visual_style(style)
    }
    pub(crate) fn reset_text_tint(&self) -> Result<()> {
        if self.tint.take().is_some() {
            self.invalidate_paint()?;
        }
        Ok(())
    }

    pub(crate) fn restore_style(&self) -> Result<()> {
        self.apply_style(&self.style.borrow())
    }
}
