//! Shadow routing, uniform-gradient folding and conservative physical paint bounds.
use nsh_core::{BoxShadow, Color, Gradient, ViewStyle};

/// The native shadow caster is the border box, without CSS spread.
pub(crate) fn visible_shadow(layer: &BoxShadow, inset: bool) -> bool {
    layer.inset == inset && layer.is_visible()
}

pub(crate) fn native_box_shadow(style: &ViewStyle, scale: f32) -> Option<BoxShadow> {
    let mut outer = style
        .effects
        .as_ref()?
        .box_shadows
        .iter()
        .filter(|s| !s.inset && s.is_visible());
    let shadow = *outer.next()?;
    // Rosen's RSShadow::IsValid requires radius > 0 after ConvertRadiusToSigma,
    // and CustomShadow with isFilled=false clips out the RRect interior natively.
    (outer.next().is_none()
        && shadow.spread.0 == 0.0
        && native_radius(shadow.blur.0 * scale * 0.5) > 0.0)
        .then_some(shadow)
}

pub(crate) fn has_non_native_outer_shadow(style: &ViewStyle, scale: f32) -> bool {
    native_box_shadow(style, scale).is_none()
        && style
            .effects
            .as_ref()
            .is_some_and(|e| e.box_shadows.iter().any(|s| !s.inset && s.is_visible()))
}

pub(crate) fn uniform_gradient_color(gradient: &Gradient) -> Option<Color> {
    let first = gradient.stops.first()?.color;
    gradient
        .stops
        .iter()
        .all(|stop| stop.color == first)
        .then_some(first)
}

fn src_over(fg: Color, bg: Color) -> Color {
    let fa = fg.0 & 0xff;
    if fa == 0 {
        return bg;
    }
    if fa == 0xff || (bg.0 & 0xff) == 0 {
        return fg;
    }
    let af = fa as f64 / 255.0;
    let ab = (bg.0 & 0xff) as f64 / 255.0;
    let out_a = af + ab * (1.0 - af);
    if out_a <= 0.0 {
        return Color::TRANSPARENT;
    }
    let blend = |shift: u32| -> u8 {
        let cf = ((fg.0 >> shift) & 0xff) as f64;
        let cb = ((bg.0 >> shift) & 0xff) as f64;
        (((cf * af + cb * ab * (1.0 - af)) / out_a)
            .round()
            .clamp(0.0, 255.0)) as u8
    };
    Color::rgba(
        blend(24),
        blend(16),
        blend(8),
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// Folds bottom-most constant-color `backgroundImage` ramps into `base` and
/// returns `(native_background_color, remaining_top_gradient_count)`.
pub(crate) fn effective_background(base: Color, style: &ViewStyle) -> (Color, usize) {
    let Some(effects) = style.effects.as_ref() else {
        return (base, 0);
    };
    let images = effects.background_images.as_slice();
    let mut color = base;
    let mut remaining = images.len();
    while remaining > 0 {
        let Some(uniform) = uniform_gradient_color(&images[remaining - 1]) else {
            break;
        };
        color = src_over(uniform, color);
        remaining -= 1;
    }
    (color, remaining)
}

/// ArkUI/Rosen converts a Skia radius to sigma with radius / sqrt(3) + 0.5.
pub(crate) fn native_radius(sigma: f32) -> f32 {
    if sigma <= 0.0 {
        0.0
    } else {
        ((sigma - 0.5) * 3.0_f32.sqrt()).max(0.0)
    }
}

/// Physical pixels, rounded outwards so all blurred ink belongs to this node.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PaintBounds {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}
impl PaintBounds {
    pub(crate) fn shadows(style: &ViewStyle, width: f32, height: f32, scale: f32) -> Self {
        if width <= 0.0 || height <= 0.0 {
            return Self::default();
        }
        // Match the snapped border box used by Decoration::shadows.
        let sx = (width * scale).round().max(1.0) / width;
        let sy = (height * scale).round().max(1.0) / height;
        let mut rect = [
            0.0_f32,
            0.0,
            (width * scale).round(),
            (height * scale).round(),
        ];
        let mut visible = false;
        for s in style
            .effects
            .iter()
            .flat_map(|e| &e.box_shadows)
            .filter(|s| !s.inset && s.is_visible())
        {
            visible = true;
            // Three sigma plus an antialias pixel; signed spread shrinks the caster.
            let reach = s.spread.0 + s.blur.0.max(0.0) * 1.5;
            rect[0] = rect[0].min((s.offset_x.0 - reach) * sx - 1.0);
            rect[1] = rect[1].min((s.offset_y.0 - reach) * sy - 1.0);
            rect[2] = rect[2].max((width + s.offset_x.0 + reach) * sx + 1.0);
            rect[3] = rect[3].max((height + s.offset_y.0 + reach) * sy + 1.0);
        }
        if !visible {
            return Self::default();
        }
        let x = rect[0].floor() as i32;
        let y = rect[1].floor() as i32;
        Self {
            x,
            y,
            width: (rect[2].ceil() as i32).saturating_sub(x),
            height: (rect[3].ceil() as i32).saturating_sub(y),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nsh_core::{Color, Effects, Points};

    use super::*;

    fn style(layers: Vec<BoxShadow>) -> ViewStyle {
        ViewStyle {
            background_color: Color(0xffffffff),
            effects: Some(Arc::new(Effects {
                box_shadows: layers,
                ..Default::default()
            })),
            ..Default::default()
        }
    }
    fn shadow() -> BoxShadow {
        BoxShadow {
            color: Color::BLACK,
            blur: Points(8.0),
            ..Default::default()
        }
    }
    #[test]
    fn native_route_preserves_css_features() {
        let mut s = style(vec![shadow()]);
        assert!(native_box_shadow(&s, 3.25).is_some());
        Arc::make_mut(s.effects.as_mut().unwrap())
            .box_shadows
            .push(BoxShadow {
                inset: true,
                ..shadow()
            });
        assert!(native_box_shadow(&s, 3.25).is_some());
        s.continuous_corners = true;
        assert!(native_box_shadow(&s, 3.25).is_some());
        s.background_color = Color::TRANSPARENT;
        assert!(native_box_shadow(&s, 3.25).is_some());
        // 1vp blur at 3.25x density has sigma 1.625 > 0.5 -> native radius > 0.
        assert!(
            native_box_shadow(
                &style(vec![BoxShadow {
                    blur: Points(1.0),
                    ..shadow()
                }]),
                3.25
            )
            .is_some()
        );
        // At 1x density, 1vp blur has sigma 0.5 -> native radius == 0 -> non-native.
        assert!(
            native_box_shadow(
                &style(vec![BoxShadow {
                    blur: Points(1.0),
                    ..shadow()
                }]),
                1.0
            )
            .is_none()
        );
        for layers in [
            vec![shadow(), shadow()],
            vec![BoxShadow {
                spread: Points(-1.0),
                ..shadow()
            }],
            vec![BoxShadow {
                blur: Points(0.0),
                offset_y: Points(1.0),
                ..shadow()
            }],
        ] {
            let candidate = style(layers);
            assert!(native_box_shadow(&candidate, 3.25).is_none());
            assert!(has_non_native_outer_shadow(&candidate, 3.25));
        }
    }
    #[test]
    fn uniform_gradients_fold_into_background_color_in_stack_order() {
        use nsh_core::{ColorStop, GradientKind, geometry::Radians};
        let uniform = |color| Gradient {
            kind: GradientKind::Linear {
                angle: Radians(std::f32::consts::PI),
            },
            stops: vec![
                ColorStop {
                    color,
                    position: None,
                },
                ColorStop {
                    color,
                    position: None,
                },
            ],
            repeating: false,
        };
        let ramp = Gradient {
            kind: GradientKind::Linear {
                angle: Radians(std::f32::consts::PI),
            },
            stops: vec![
                ColorStop {
                    color: Color(0xff0000ff),
                    position: None,
                },
                ColorStop {
                    color: Color(0x0000ffff),
                    position: None,
                },
            ],
            repeating: false,
        };
        let mut s = ViewStyle {
            background_color: Color(0x112233ff),
            effects: Some(Arc::new(Effects {
                background_images: vec![uniform(Color(0x33353bff))],
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(
            effective_background(s.background_color, &s),
            (Color(0x33353bff), 0)
        );
        // Top ramp over bottom uniform layer folds only the bottom layer.
        Arc::make_mut(s.effects.as_mut().unwrap()).background_images =
            vec![ramp, uniform(Color(0xffffffff))];
        assert_eq!(
            effective_background(s.background_color, &s),
            (Color(0xffffffff), 1)
        );
    }
    #[test]
    fn bounds_cover_offsets_spread_blur_and_density() {
        let s = style(vec![BoxShadow {
            offset_x: Points(-9.0),
            offset_y: Points(7.0),
            spread: Points(2.0),
            ..shadow()
        }]);
        assert_eq!(
            PaintBounds::shadows(&s, 56.0, 56.0, 3.25),
            PaintBounds {
                x: -76,
                y: -24,
                width: 276,
                height: 276
            }
        );
        assert_eq!(
            PaintBounds::shadows(&s, 0.0, 56.0, 3.25),
            PaintBounds::default()
        );
        let inset = style(vec![BoxShadow {
            inset: true,
            ..shadow()
        }]);
        assert_eq!(
            PaintBounds::shadows(&inset, 56.0, 56.0, 3.25),
            PaintBounds::default()
        );
    }
    #[test]
    fn zero_blur_inset_ring_is_visible() {
        let layer = BoxShadow {
            inset: true,
            blur: Points(0.0),
            spread: Points(2.0),
            color: Color(0x3388ffff),
            ..Default::default()
        };
        assert!(visible_shadow(&layer, true));
        assert!(!visible_shadow(&layer, false));
        assert!(!visible_shadow(
            &BoxShadow {
                color: Color::TRANSPARENT,
                ..layer
            },
            true
        ));
    }
    #[test]
    fn native_blur_matches_css_sigma_at_each_density() {
        for scale in [1.0, 2.0, 3.25] {
            let sigma = 8.0 * 0.5 * scale;
            assert!((native_radius(sigma) / 3.0_f32.sqrt() + 0.5 - sigma).abs() < 0.00001);
        }
        assert_eq!(native_radius(0.0), 0.0);
    }
}
