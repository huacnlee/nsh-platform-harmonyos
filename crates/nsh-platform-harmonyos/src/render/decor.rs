//! Cached background paints and outlines; geometry comes from the shared core.
use std::{cell::RefCell, sync::Arc};

use nsh_core::{
    BorderStyle, Gradient, GradientKind, GradientSize, Points, Size as CoreSize, ViewStyle,
};
use ohos_arkui_binding::types::view::BlendMode as ArkUiBlendMode;
use ohos_drawing_binding::{
    Brush, Canvas, ClipOperation, CornerPosition, Filter, MaskFilter, Matrix, Path, PathDirection,
    PathEffect, PathFillType, Pen, Rect, RoundRect, ShaderEffect, TileMode,
};

use super::{
    gradient::NativeStops,
    paint_node::{NativeRoundRect, PaintCanvas},
    shadow_geometry::{effective_background, uniform_gradient_color, visible_shadow},
};
use crate::{
    render::{geometry::Size, node::Node},
    text::typography::argb,
};

#[derive(Default)]
pub(crate) struct Decoration {
    backgrounds: RefCell<Option<Backgrounds>>,
    shadows: RefCell<[Option<Shadows>; 2]>,
}

enum ShadowShape {
    FillPadding,
    NestedHole(NativeRoundRect),
    AnalyticRing {
        enclosing: NativeRoundRect,
        hole: NativeRoundRect,
    },
    OuterCrescent {
        shadow: NativeRoundRect,
        enclosing: NativeRoundRect,
    },
    BlurredPath(Path),
}

struct ShadowPaint {
    inset: bool,
    shape: ShadowShape,
    brush: Brush,
}

struct Shadows {
    effects: Option<Arc<nsh_core::Effects>>,
    size: Size,
    radii: [f32; 4],
    borders: [f32; 4],
    border: Option<NativeRoundRect>,
    padding: Option<NativeRoundRect>,
    paints: Vec<ShadowPaint>,
}

struct Backgrounds {
    effects: Option<Arc<nsh_core::Effects>>,
    size: Size,
    radii: [f32; 4],
    remaining: usize,
    round_rect: Option<NativeRoundRect>,
    paints: Vec<Brush>,
}

pub(crate) fn warn_repeating_conic(owner: &Node, style: &ViewStyle) {
    for g in style
        .effects
        .as_ref()
        .into_iter()
        .flat_map(|e| &e.background_images)
    {
        if matches!(g.kind, GradientKind::Conic { .. }) && g.repeating && !g.stops.is_empty() {
            let frame = owner.frame.get();
            let stops = g.resolved_stops(CoreSize::new(Points(frame.width), Points(frame.height)));
            let span = stops.last().unwrap().1.0 - stops[0].1.0;
            if span > 1e-5 && (1.0 / span + 3.0) * stops.len() as f32 > 4096.0 {
                log::warn!("OHOS repeating conic gradient: limiting repeated stops to 4096");
            }
        }
    }
}

pub(crate) fn arkui_blend_mode(blend: nsh_core::BlendMode) -> ArkUiBlendMode {
    use nsh_core::BlendMode as M;
    match blend {
        M::Normal => ArkUiBlendMode::None,
        M::Multiply => ArkUiBlendMode::Multiply,
        M::Screen => ArkUiBlendMode::Screen,
        M::Overlay => ArkUiBlendMode::Overlay,
        M::Darken => ArkUiBlendMode::Darken,
        M::Lighten => ArkUiBlendMode::Lighten,
        M::ColorDodge => ArkUiBlendMode::ColorDodge,
        M::ColorBurn => ArkUiBlendMode::ColorBurn,
        M::HardLight => ArkUiBlendMode::HardLight,
        M::SoftLight => ArkUiBlendMode::SoftLight,
        M::Difference => ArkUiBlendMode::Difference,
        M::Exclusion => ArkUiBlendMode::Exclusion,
        M::Hue => ArkUiBlendMode::Hue,
        M::Saturation => ArkUiBlendMode::Saturation,
        M::Color => ArkUiBlendMode::Color,
        M::Luminosity => ArkUiBlendMode::Luminosity,
        M::PlusLighter => ArkUiBlendMode::Plus,
        // Scene composition owns this mode. Hidden/detached triggers
        // can keep the style while their native exports retire.
        M::PlusDarker => ArkUiBlendMode::None,
    }
}

impl Decoration {
    pub(crate) fn clear_paint_cache(&self) {
        self.backgrounds.borrow_mut().take();
        *self.shadows.borrow_mut() = [None, None];
    }

    pub(crate) fn background(
        &self,
        canvas: &PaintCanvas,
        style: &ViewStyle,
        size: Size,
        scale: f32,
        remaining_gradients: usize,
    ) {
        let Some(effects) = style.effects.as_ref() else {
            self.backgrounds.borrow_mut().take();
            *self.shadows.borrow_mut() = [None, None];
            return;
        };
        if remaining_gradients == 0 && effects.box_shadows.is_empty() {
            self.backgrounds.borrow_mut().take();
            *self.shadows.borrow_mut() = [None, None];
            return;
        }
        let images =
            &effects.background_images[..remaining_gradients.min(effects.background_images.len())];
        if images.is_empty() {
            self.backgrounds.borrow_mut().take();
            self.shadows(canvas, style, size, scale, true);
            return;
        }
        let radii = style
            .corner_radius
            .map(|radius| radius.unwrap_or(style.border_radius).0)
            .to_array();
        let mut backgrounds = self.backgrounds.borrow_mut();
        if backgrounds.as_ref().is_none_or(|c| {
            c.size != size
                || c.radii != radii
                || c.remaining != images.len()
                || match (&c.effects, &style.effects) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                }
        }) {
            *backgrounds = Some(Backgrounds {
                effects: style.effects.clone(),
                size,
                radii,
                remaining: images.len(),
                round_rect: border_round_rect(style, size, 0.0, 0.0, 0.0),
                paints: images.iter().rev().map(|g| gradient(g, size)).collect(),
            });
        }
        let cache = backgrounds.as_ref().unwrap();
        if let Some(round_rect) = &cache.round_rect {
            let (sx, sy) = pixel_scale(size, scale);
            canvas.save();
            canvas.scale(sx, sy);
            for brush in &cache.paints {
                canvas.attach_brush(brush);
                canvas.draw_round_rect(round_rect);
                canvas.detach_brush();
            }
            canvas.restore();
        }
        drop(backgrounds);
        self.shadows(canvas, style, size, scale, true);
    }

    pub(crate) fn shadows(
        &self,
        canvas: &PaintCanvas,
        style: &ViewStyle,
        size: Size,
        scale: f32,
        inset: bool,
    ) {
        let layers = style
            .effects
            .as_ref()
            .map_or(&[][..], |effects| effects.box_shadows.as_slice());
        let visible_layer = |layer: &nsh_core::BoxShadow| visible_shadow(layer, inset);
        if !layers.iter().any(visible_layer) {
            self.shadows.borrow_mut()[usize::from(inset)] = None;
            return;
        }
        let radii = style
            .corner_radius
            .map(|radius| radius.unwrap_or(style.border_radius).0)
            .to_array();
        let borders = style.borders.widths.to_array().map(|v| v.0);
        let mut caches = self.shadows.borrow_mut();
        let cache = &mut caches[usize::from(inset)];
        if cache.as_ref().is_none_or(|cache| {
            cache.size != size
                || cache.radii != radii
                || cache.borders != borders
                || !match (&cache.effects, &style.effects) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        }) {
            let border = border_round_rect(style, size, 0.0, 0.0, 0.0);
            let padding = padding_round_rect(style, size, 0.0, 0.0, 0.0);
            let paints = layers
                .iter()
                .rev()
                .filter(|layer| visible_layer(layer))
                .filter_map(|layer| {
                    let mut brush = Brush::new();
                    brush.set_anti_alias(true);
                    brush.set_color(argb(layer.color));
                    if layer.blur.0 > 0.0 {
                        let mask = MaskFilter::blur(layer.blur.0 * 0.5, true);
                        let mut filter = Filter::new();
                        filter.set_mask_filter(mask.as_ref());
                        brush.set_filter(Some(&filter));
                    }
                    let shape = if layer.inset {
                        let pad = padding.as_ref()?;
                        if layer.blur.0 <= 0.0 {
                            match padding_round_rect(
                                style,
                                size,
                                layer.spread.0,
                                layer.offset_x.0,
                                layer.offset_y.0,
                            ) {
                                Some(hole)
                                    if layer.offset_x.0 == 0.0
                                        && layer.offset_y.0 == 0.0
                                        && layer.spread.0 >= 0.0 =>
                                {
                                    ShadowShape::NestedHole(hole)
                                }
                                Some(hole) => ShadowShape::AnalyticRing {
                                    enclosing: pad.enclosing(&hole, 2.0),
                                    hole,
                                },
                                None => ShadowShape::FillPadding,
                            }
                        } else if padding_round_rect(
                            style,
                            size,
                            layer.spread.0,
                            layer.offset_x.0,
                            layer.offset_y.0,
                        )
                        .is_some()
                        {
                            let mut hole = padding_outline(style, size, layer.spread.0);
                            if layer.offset_x.0 != 0.0 || layer.offset_y.0 != 0.0 {
                                hole.transform(&Matrix::from_affine(
                                    1.0,
                                    0.0,
                                    0.0,
                                    1.0,
                                    layer.offset_x.0,
                                    layer.offset_y.0,
                                ));
                            }
                            let reach = layer.offset_x.0.abs()
                                + layer.offset_y.0.abs()
                                + layer.spread.0.abs()
                                + layer.blur.0.max(0.0) * 2.0
                                + 4.0;
                            let mut ring = Path::new();
                            ring.set_fill_type(PathFillType::EvenOdd);
                            ring.add_rect(
                                -reach,
                                -reach,
                                size.width + reach,
                                size.height + reach,
                                PathDirection::Cw,
                            );
                            ring.add_path(&hole, None);
                            ShadowShape::BlurredPath(ring)
                        } else {
                            ShadowShape::FillPadding
                        }
                    } else if layer.blur.0 <= 0.0 {
                        let bdr = border.as_ref()?;
                        let shadow = border_round_rect(
                            style,
                            size,
                            layer.spread.0,
                            layer.offset_x.0,
                            layer.offset_y.0,
                        )?;
                        let enclosing = bdr.enclosing(&shadow, 2.0);
                        ShadowShape::OuterCrescent { shadow, enclosing }
                    } else {
                        border_round_rect(
                            style,
                            size,
                            layer.spread.0,
                            layer.offset_x.0,
                            layer.offset_y.0,
                        )?;
                        let mut path = outline(style, size, layer.spread.0);
                        if layer.offset_x.0 != 0.0 || layer.offset_y.0 != 0.0 {
                            path.transform(&Matrix::from_affine(
                                1.0,
                                0.0,
                                0.0,
                                1.0,
                                layer.offset_x.0,
                                layer.offset_y.0,
                            ));
                        }
                        ShadowShape::BlurredPath(path)
                    };
                    Some(ShadowPaint {
                        inset: layer.inset,
                        shape,
                        brush,
                    })
                })
                .collect();
            *cache = Some(Shadows {
                effects: style.effects.clone(),
                size,
                radii,
                borders,
                border,
                padding,
                paints,
            });
        }
        let cache = cache.as_ref().unwrap();
        let clip_rrect = if inset {
            cache.padding.as_ref()
        } else {
            cache.border.as_ref()
        };
        let Some(clip_rrect) = clip_rrect else {
            return;
        };
        let (sx, sy) = pixel_scale(size, scale);
        canvas.save();
        canvas.scale(sx, sy);
        if inset {
            canvas.clip_round_rect(clip_rrect, ClipOperation::Intersect, true);
        }
        for paint in cache.paints.iter().filter(|paint| paint.inset == inset) {
            match &paint.shape {
                ShadowShape::FillPadding => {
                    if let Some(padding) = &cache.padding {
                        canvas.attach_brush(&paint.brush);
                        canvas.draw_round_rect(padding);
                        canvas.detach_brush();
                    }
                }
                ShadowShape::NestedHole(hole) => {
                    if let Some(padding) = &cache.padding {
                        canvas.attach_brush(&paint.brush);
                        let _ = canvas.draw_nested_round_rect(padding, hole);
                        canvas.detach_brush();
                    }
                }
                ShadowShape::AnalyticRing { enclosing, hole } => {
                    canvas.attach_brush(&paint.brush);
                    let _ = canvas.draw_nested_round_rect(enclosing, hole);
                    canvas.detach_brush();
                }
                ShadowShape::OuterCrescent { shadow, enclosing } => {
                    canvas.save();
                    canvas.clip_round_rect(shadow, ClipOperation::Intersect, true);
                    canvas.attach_brush(&paint.brush);
                    let _ = canvas.draw_nested_round_rect(enclosing, clip_rrect);
                    canvas.detach_brush();
                    canvas.restore();
                }
                ShadowShape::BlurredPath(path) => {
                    if !inset {
                        canvas.save();
                        canvas.clip_round_rect(clip_rrect, ClipOperation::Difference, true);
                    }
                    canvas.attach_brush(&paint.brush);
                    canvas.draw_path(path);
                    canvas.detach_brush();
                    if !inset {
                        canvas.restore();
                    }
                }
            }
        }
        canvas.restore();
    }

    pub(crate) fn outline(&self, canvas: &Canvas, style: &ViewStyle, size: Size, scale: f32) {
        let o = style.outline;
        if o.width.0 <= 0.0 || o.color.is_transparent() {
            return;
        }
        let Some(round_rect) = border_drawing_round_rect(style, size, o.offset.0 + o.width.0 * 0.5)
        else {
            return;
        };
        let width = o.width.0;
        let mut pen = Pen::new();
        pen.set_anti_alias(true);
        pen.set_color(argb(o.color));
        pen.set_width(width);
        let dash = match o.style {
            BorderStyle::Solid => None,
            BorderStyle::Dashed => PathEffect::dash(&[width * 3.0, width * 3.0], 0.0),
            BorderStyle::Dotted => PathEffect::dash(&[width, width], 0.0),
        };
        pen.set_path_effect(dash.as_ref());
        let (sx, sy) = pixel_scale(size, scale);
        canvas.save();
        canvas.scale(sx, sy);
        canvas.attach_pen(&pen);
        canvas.draw_round_rect(&round_rect);
        canvas.detach_pen();
        canvas.restore();
    }
}

fn pixel_scale(size: Size, scale: f32) -> (f32, f32) {
    let sx = if size.width > 0.0 {
        (size.width * scale).round().max(1.0) / size.width
    } else {
        scale
    };
    let sy = if size.height > 0.0 {
        (size.height * scale).round().max(1.0) / size.height
    } else {
        scale
    };
    (sx, sy)
}

fn padding_round_rect(
    style: &ViewStyle,
    size: Size,
    spread: f32,
    offset_x: f32,
    offset_y: f32,
) -> Option<NativeRoundRect> {
    let [top, right, bottom, left] = style
        .borders
        .widths
        .to_array()
        .map(|width| width.0 + spread);
    if size.width <= left + right || size.height <= top + bottom {
        return None;
    }
    let radii = nsh_core::canvas::fit_radii(
        size.into(),
        style
            .corner_radius
            .map(|radius| radius.unwrap_or(style.border_radius)),
    )
    .to_array()
    .map(|r| r.0);
    let insets = [(left, top), (right, top), (right, bottom), (left, bottom)];
    let corners = std::array::from_fn(|i| {
        (
            (radii[i] - insets[i].0).max(0.0),
            (radii[i] - insets[i].1).max(0.0),
        )
    });
    Some(NativeRoundRect::new(
        left + offset_x,
        top + offset_y,
        size.width - right + offset_x,
        size.height - bottom + offset_y,
        corners,
    ))
}

fn expand_corner_radius(radius: f32, spread: f32) -> f32 {
    if radius <= 0.0 {
        0.0
    } else {
        (radius + spread).max(0.0)
    }
}

fn border_round_rect(
    style: &ViewStyle,
    size: Size,
    spread: f32,
    offset_x: f32,
    offset_y: f32,
) -> Option<NativeRoundRect> {
    let w = (size.width + spread * 2.0).max(0.0);
    let h = (size.height + spread * 2.0).max(0.0);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let radii = nsh_core::canvas::fit_radii(
        CoreSize::new(Points(w), Points(h)),
        style.corner_radius.map(|r| {
            Points(expand_corner_radius(
                r.unwrap_or(style.border_radius).0,
                spread,
            ))
        }),
    )
    .to_array()
    .map(|r| (r.0, r.0));
    Some(NativeRoundRect::new(
        -spread + offset_x,
        -spread + offset_y,
        w - spread + offset_x,
        h - spread + offset_y,
        radii,
    ))
}

pub(crate) fn border_drawing_round_rect(
    style: &ViewStyle,
    size: Size,
    spread: f32,
) -> Option<RoundRect> {
    let w = (size.width + spread * 2.0).max(0.0);
    let h = (size.height + spread * 2.0).max(0.0);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let radii = nsh_core::canvas::fit_radii(
        CoreSize::new(Points(w), Points(h)),
        style.corner_radius.map(|r| {
            Points(expand_corner_radius(
                r.unwrap_or(style.border_radius).0,
                spread,
            ))
        }),
    )
    .to_array()
    .map(|r| r.0);
    let rect = Rect::new(-spread, -spread, w - spread, h - spread);
    let mut round_rect = RoundRect::new(&rect, 0.0, 0.0);
    for (corner, radius) in [
        CornerPosition::TopLeft,
        CornerPosition::TopRight,
        CornerPosition::BottomRight,
        CornerPosition::BottomLeft,
    ]
    .into_iter()
    .zip(radii)
    {
        round_rect.set_corner(corner, radius, radius);
    }
    Some(round_rect)
}

fn padding_outline(style: &ViewStyle, size: Size, spread: f32) -> Path {
    let [top, right, bottom, left] = style
        .borders
        .widths
        .to_array()
        .map(|width| width.0 + spread);
    let mut path = Path::new();
    if size.width <= left + right || size.height <= top + bottom {
        return path;
    }
    let radii = nsh_core::canvas::fit_radii(
        size.into(),
        style
            .corner_radius
            .map(|radius| radius.unwrap_or(style.border_radius)),
    )
    .to_array()
    .map(|r| r.0);
    let corners = [(left, top), (right, top), (right, bottom), (left, bottom)];
    let rect = Rect::new(left, top, size.width - right, size.height - bottom);
    let mut round_rect = RoundRect::new(&rect, 0.0, 0.0);
    let positions = [
        CornerPosition::TopLeft,
        CornerPosition::TopRight,
        CornerPosition::BottomRight,
        CornerPosition::BottomLeft,
    ];
    for (i, pos) in positions.into_iter().enumerate() {
        round_rect.set_corner(
            pos,
            (radii[i] - corners[i].0).max(0.0),
            (radii[i] - corners[i].1).max(0.0),
        );
    }
    path.add_round_rect(&round_rect, PathDirection::Cw);
    path
}

pub(crate) fn outline(style: &ViewStyle, size: Size, spread: f32) -> Path {
    let mut path = Path::new();
    if let Some(round_rect) = border_drawing_round_rect(style, size, spread) {
        path.add_round_rect(&round_rect, PathDirection::Cw);
    }
    path
}

/// Paints a plain `<View>` / `<Text>` box directly onto `Scene`'s bitmap canvas,
/// avoiding offscreen `BuilderNode(RENDER_TYPE_TEXTURE)` + `SurfaceImage` swapchains.
pub(crate) fn draw_direct_box(
    canvas: &Canvas,
    style: &ViewStyle,
    bg_argb: u32,
    border_colors: [u32; 4],
    size: Size,
    scale: f32,
) {
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    let Some(border_rrect) = border_drawing_round_rect(style, size, 0.0) else {
        return;
    };
    let (sx, sy) = pixel_scale(size, scale);
    let (_, remaining_gradients) = effective_background(style.background_color, style);

    canvas.save();
    canvas.scale(sx, sy);

    // 1. Outer box shadows
    if let Some(effects) = style.effects.as_ref()
        && effects.box_shadows.iter().any(|s| visible_shadow(s, false))
    {
        let clip_border = outline(style, size, 0.0);
        canvas.save();
        canvas.clip_path(&clip_border, ClipOperation::Difference, true);
        for layer in effects
            .box_shadows
            .iter()
            .rev()
            .filter(|s| visible_shadow(s, false))
        {
            let mut brush = Brush::new();
            brush.set_anti_alias(true);
            brush.set_color(argb(layer.color));
            if layer.blur.0 > 0.0 {
                let mask = MaskFilter::blur(layer.blur.0 * 0.5, true);
                let mut filter = Filter::new();
                filter.set_mask_filter(mask.as_ref());
                brush.set_filter(Some(&filter));
            }
            let mut path = outline(style, size, layer.spread.0);
            path.transform(&Matrix::from_affine(
                1.0,
                0.0,
                0.0,
                1.0,
                layer.offset_x.0,
                layer.offset_y.0,
            ));
            canvas.attach_brush(&brush);
            canvas.draw_path(&path);
            canvas.detach_brush();
        }
        canvas.restore();
    }

    // 2. Effective background color
    if (bg_argb >> 24) != 0 {
        let mut brush = Brush::new();
        brush.set_anti_alias(true);
        brush.set_color(bg_argb);
        canvas.attach_brush(&brush);
        canvas.draw_round_rect(&border_rrect);
        canvas.detach_brush();
    }

    // 3. Remaining background gradients
    if let Some(effects) = style.effects.as_ref() {
        let images =
            &effects.background_images[..remaining_gradients.min(effects.background_images.len())];
        for g in images.iter().rev() {
            let brush = gradient(g, size);
            canvas.attach_brush(&brush);
            canvas.draw_round_rect(&border_rrect);
            canvas.detach_brush();
        }

        // 4. Inset box shadows
        if effects.box_shadows.iter().any(|s| visible_shadow(s, true)) {
            let pad_clip = padding_outline(style, size, 0.0);
            canvas.save();
            canvas.clip_path(&pad_clip, ClipOperation::Intersect, true);
            for layer in effects
                .box_shadows
                .iter()
                .rev()
                .filter(|s| visible_shadow(s, true))
            {
                let mut brush = Brush::new();
                brush.set_anti_alias(true);
                brush.set_color(argb(layer.color));
                if layer.blur.0 > 0.0 {
                    let mask = MaskFilter::blur(layer.blur.0 * 0.5, true);
                    let mut filter = Filter::new();
                    filter.set_mask_filter(mask.as_ref());
                    brush.set_filter(Some(&filter));
                }
                let mut hole = padding_outline(style, size, layer.spread.0);
                hole.transform(&Matrix::from_affine(
                    1.0,
                    0.0,
                    0.0,
                    1.0,
                    layer.offset_x.0,
                    layer.offset_y.0,
                ));
                let reach = layer.offset_x.0.abs()
                    + layer.offset_y.0.abs()
                    + layer.spread.0.abs()
                    + layer.blur.0.max(0.0) * 2.0
                    + 4.0;
                let mut ring = Path::new();
                ring.set_fill_type(PathFillType::EvenOdd);
                ring.add_rect(
                    -reach,
                    -reach,
                    size.width + reach,
                    size.height + reach,
                    PathDirection::Cw,
                );
                ring.add_path(&hole, None);
                canvas.attach_brush(&brush);
                canvas.draw_path(&ring);
                canvas.detach_brush();
            }
            canvas.restore();
        }
    }

    // 5. Uniform border
    let bw = style.borders.widths.top.0;
    let color = border_colors[0];
    if bw > 0.0 && (color >> 24) != 0 {
        match style.borders.style {
            BorderStyle::Solid => {
                let mut ring = outline(style, size, 0.0);
                ring.set_fill_type(PathFillType::EvenOdd);
                ring.add_path(&padding_outline(style, size, 0.0), None);
                let mut brush = Brush::new();
                brush.set_anti_alias(true);
                brush.set_color(color);
                canvas.attach_brush(&brush);
                canvas.draw_path(&ring);
                canvas.detach_brush();
            }
            BorderStyle::Dashed | BorderStyle::Dotted => {
                if let Some(stroke_rrect) = border_drawing_round_rect(style, size, -bw * 0.5) {
                    let clip_border = outline(style, size, 0.0);
                    let mut pen = Pen::new();
                    pen.set_anti_alias(true);
                    pen.set_color(color);
                    pen.set_width(bw);
                    let dash = match style.borders.style {
                        BorderStyle::Dashed => PathEffect::dash(&[bw * 3.0, bw * 3.0], 0.0),
                        BorderStyle::Dotted => PathEffect::dash(&[bw, bw], 0.0),
                        BorderStyle::Solid => None,
                    };
                    pen.set_path_effect(dash.as_ref());
                    canvas.save();
                    canvas.clip_path(&clip_border, ClipOperation::Intersect, true);
                    canvas.attach_pen(&pen);
                    canvas.draw_round_rect(&stroke_rrect);
                    canvas.detach_pen();
                    canvas.restore();
                }
            }
        }
    }

    canvas.restore();
}

pub(crate) fn drawing_blend(mode: nsh_core::BlendMode) -> ohos_drawing_binding::BlendMode {
    use nsh_core::BlendMode as M;
    use ohos_drawing_binding::BlendMode as B;
    match mode {
        M::Normal | M::PlusDarker => B::SrcOver,
        M::Multiply => B::Multiply,
        M::Screen => B::Screen,
        M::Overlay => B::Overlay,
        M::Darken => B::Darken,
        M::Lighten => B::Lighten,
        M::ColorDodge => B::ColorDodge,
        M::ColorBurn => B::ColorBurn,
        M::HardLight => B::HardLight,
        M::SoftLight => B::SoftLight,
        M::Difference => B::Difference,
        M::Exclusion => B::Exclusion,
        M::Hue => B::Hue,
        M::Saturation => B::Saturation,
        M::Color => B::Color,
        M::Luminosity => B::Luminosity,
        M::PlusLighter => B::Plus,
    }
}

pub(crate) fn gradient(g: &Gradient, size: Size) -> Brush {
    let mut brush = Brush::new();
    brush.set_anti_alias(true);
    if g.stops.is_empty() {
        return brush;
    }
    if let Some(uniform) = uniform_gradient_color(g) {
        brush.set_color(argb(uniform));
        return brush;
    }
    let stops = g.resolved_stops(size.into());
    let first = stops[0].1.0;
    let last = stops.last().unwrap().1.0;
    let span = (last - first).max(1e-6);
    brush.set_color(argb(stops.last().unwrap().0));
    let stops = if let GradientKind::Conic { from, .. } = g.kind {
        let mut stops: Vec<_> = stops.into_iter().map(|(c, p)| (p.0, c)).collect();
        if g.repeating && span > 1e-5 {
            let original = stops.clone();
            stops.clear();
            let start = (-last / span).floor() as i32;
            let end = ((1.0 - first) / span).ceil() as i32;
            // A maliciously tiny period must not allocate unbounded stops.
            if (end - start) as usize * original.len() <= 4096 {
                for k in start..=end {
                    stops.extend(original.iter().map(|(p, c)| (*p + k as f32 * span, *c)));
                }
            } else {
                stops = vec![(0.0, original[0].1), (1.0, original.last().unwrap().1)];
            }
        }
        super::gradient::sweep_stops(&stops, from.0 - std::f32::consts::FRAC_PI_2)
    } else {
        stops
            .into_iter()
            .map(|(c, p)| ((p.0 - first) / span, c))
            .collect()
    };
    let NativeStops { colors, positions } = NativeStops::new(&stops);
    let tile = if g.repeating {
        TileMode::Repeat
    } else {
        TileMode::Clamp
    };
    let (w, h) = (size.width, size.height);
    let shader = match g.kind {
        GradientKind::Radial { circle, size, x, y } => {
            let (cx, cy) = (x.0 * w, y.0 * h);
            let (rx, ry) = radii(circle, size, cx, cy, w, h);
            let matrix = Matrix::from_affine(rx.max(0.01), 0.0, 0.0, ry.max(0.01), cx, cy);
            ShaderEffect::two_point_conical_gradient(
                (0.0, 0.0),
                first.max(0.0),
                (0.0, 0.0),
                last.max(0.0001),
                &colors,
                &positions,
                tile,
                Some(&matrix),
            )
        }
        GradientKind::Conic { x, y, .. } => {
            ShaderEffect::conic_gradient((x.0 * w, y.0 * h), &colors, &positions, TileMode::Clamp)
        }
        kind => {
            let angle = kind
                .linear_angle(size.into())
                .map_or(std::f32::consts::PI, |r| r.0);
            let (dx, dy) = (angle.sin(), -angle.cos());
            let length = (w * dx).abs() + (h * dy).abs();
            let point = |t: f32| {
                (
                    w * 0.5 + dx * length * (t - 0.5),
                    h * 0.5 + dy * length * (t - 0.5),
                )
            };
            ShaderEffect::linear_gradient(
                point(first),
                point(last.max(first + 1e-6)),
                &colors,
                &positions,
                tile,
            )
        }
    };
    if shader.is_some() {
        brush.set_color(0xffffffff);
        brush.set_shader_effect(shader.as_ref());
    }
    brush
}

fn radii(circle: bool, size: GradientSize, cx: f32, cy: f32, w: f32, h: f32) -> (f32, f32) {
    let near = (cx.abs().min((w - cx).abs()), cy.abs().min((h - cy).abs()));
    let far = (cx.abs().max((w - cx).abs()), cy.abs().max((h - cy).abs()));
    if circle {
        let r = match size {
            GradientSize::ClosestSide => near.0.min(near.1),
            GradientSize::FarthestSide => far.0.max(far.1),
            GradientSize::ClosestCorner => near.0.hypot(near.1),
            GradientSize::FarthestCorner => far.0.hypot(far.1),
        };
        (r, r)
    } else {
        match size {
            GradientSize::ClosestSide => near,
            GradientSize::FarthestSide => far,
            GradientSize::ClosestCorner => (
                near.0 * std::f32::consts::SQRT_2,
                near.1 * std::f32::consts::SQRT_2,
            ),
            GradientSize::FarthestCorner => (
                far.0 * std::f32::consts::SQRT_2,
                far.1 * std::f32::consts::SQRT_2,
            ),
        }
    }
}
