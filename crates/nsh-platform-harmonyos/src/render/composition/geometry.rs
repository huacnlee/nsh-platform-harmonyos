//! Finite capture bounds in surface units, including projected native overflow.
use nsh_core::{Filter, Matrix, Points, Size, ViewStyle};

/// Bound allocations uniformly; logical boxes never change with resolution.
pub(crate) fn target_size(width: f32, height: f32, scale: f32) -> (u32, u32, f32) {
    if !width.is_finite()
        || !height.is_finite()
        || !scale.is_finite()
        || width <= 0.0
        || height <= 0.0
        || scale <= 0.0
    {
        return (0, 0, 0.0);
    }
    let raw_w = (f64::from(width) * f64::from(scale)).ceil().max(1.0);
    let raw_h = (f64::from(height) * f64::from(scale)).ceil().max(1.0);
    let ratio = (4096.0 / raw_w.max(raw_h)).min(1.0);
    let w = (raw_w * ratio).round().clamp(1.0, 4096.0) as u32;
    let h = (raw_h * ratio).round().clamp(1.0, 4096.0) as u32;
    (w, h, scale * ratio as f32)
}

/// Logical coverage of a picture, independent of its pixel resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Coverage {
    pub(crate) box_size: Size<Points>,
    pub(crate) insets: [f32; 4],
}
impl Coverage {
    pub(crate) fn shader(box_size: Size<Points>, overflow: f32) -> Self {
        Self {
            box_size,
            insets: [overflow.max(0.0); 4],
        }
    }
    pub(crate) fn size(self) -> (f32, f32) {
        (
            self.box_size.width.0 + self.insets[0] + self.insets[2],
            self.box_size.height.0 + self.insets[1] + self.insets[3],
        )
    }
    pub(crate) fn pixels(self, scale: f32) -> (u32, u32, f32) {
        let (width, height) = self.size();
        target_size(width, height, scale)
    }
    /// Where the source picture lies in a destination pixel grid.
    pub(crate) fn destination(self, source: Self, size: (u32, u32)) -> [i32; 4] {
        let (width, height) = self.size();
        let sx = size.0 as f32 / width;
        let sy = size.1 as f32 / height;
        let left = self.insets[0] - source.insets[0];
        let top = self.insets[1] - source.insets[1];
        let (sw, sh) = source.size();
        [
            (left * sx).round() as i32,
            (top * sy).round() as i32,
            ((left + sw) * sx).round() as i32,
            ((top + sh) * sy).round() as i32,
        ]
    }
}

pub(crate) fn blur_sigma(radius: f32, scale: f32) -> f32 {
    radius.max(0.0) * scale * 0.5
}

pub(crate) fn requires_scene(style: &ViewStyle) -> bool {
    style.blend_mode == nsh_core::BlendMode::PlusDarker
        || style.effects.as_ref().is_some_and(|effects| {
            !effects.mask_images.is_empty()
                || effects.backdrop_filters.len() > 1
                || effects
                    .backdrop_filters
                    .iter()
                    .any(|filter| !matches!(filter, Filter::Blur(_)))
        })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds(pub(crate) [f32; 4]);
impl Bounds {
    pub(crate) fn new(width: f32, height: f32) -> Self {
        Self([0.0, 0.0, width, height])
    }
    pub(crate) fn union(&mut self, other: Self) {
        self.0 = [
            self.0[0].min(other.0[0]),
            self.0[1].min(other.0[1]),
            self.0[2].max(other.0[2]),
            self.0[3].max(other.0[3]),
        ];
    }
    fn extend(&mut self, dx: f32, dy: f32, pad: f32) {
        let [a, b, c, d] = self.0;
        self.union(Self([
            a + dx - pad,
            b + dy - pad,
            c + dx + pad,
            d + dy + pad,
        ]));
    }
    pub(crate) fn shaders(
        mut self,
        fill_overflow: f32,
        layers: &[(
            nsh_core::NodeId,
            std::sync::Arc<nsh_core::shader::ShaderLayer>,
        )],
    ) -> Self {
        let overflow = layers
            .iter()
            .map(|(_, layer)| layer.overflow.0.max(0.0))
            .fold(fill_overflow.max(0.0), f32::max);
        self.extend(0.0, 0.0, overflow);
        self
    }
    pub(crate) fn own(mut self, style: &ViewStyle) -> Self {
        let border = self;
        if style.outline.is_visible() {
            self.extend(
                0.0,
                0.0,
                (style.outline.width.0 + style.outline.offset.0).max(0.0),
            );
        }
        if let Some(effects) = style.effects.as_ref() {
            for shadow in effects
                .box_shadows
                .iter()
                .filter(|s| !s.inset && s.is_visible())
            {
                let mut bounds = border;
                bounds.extend(
                    shadow.offset_x.0,
                    shadow.offset_y.0,
                    shadow.spread.0 + shadow.blur.0.max(0.0) * 1.5,
                );
                self.union(bounds);
            }
        }
        self
    }
    pub(crate) fn filters(mut self, filters: &[Filter]) -> Self {
        for filter in filters {
            match *filter {
                Filter::Blur(radius) => self.extend(0.0, 0.0, blur_sigma(radius.0, 1.0) * 3.0),
                Filter::DropShadow {
                    offset_x,
                    offset_y,
                    blur,
                    ..
                } => self.extend(offset_x.0, offset_y.0, blur_sigma(blur.0, 1.0) * 3.0),
                _ => {}
            }
        }
        self
    }
    pub(crate) fn transformed(self, matrix: Matrix) -> Option<Self> {
        let m = matrix.0;
        let [a, b, c, d] = self.0;
        let mut result = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for (x, y) in [(a, b), (c, b), (c, d), (a, d)] {
            let (x, y) = (x as f64, y as f64);
            let w = x * m[3] + y * m[7] + m[15];
            if !w.is_finite() || w <= 1e-6 {
                return None;
            }
            let (x, y) = (
                (x * m[0] + y * m[4] + m[12]) / w,
                (x * m[1] + y * m[5] + m[13]) / w,
            );
            if !x.is_finite() || !y.is_finite() || x.abs() > 1e7 || y.abs() > 1e7 {
                return None;
            }
            result[0] = result[0].min(x as f32);
            result[1] = result[1].min(y as f32);
            result[2] = result[2].max(x as f32);
            result[3] = result[3].max(y as f32);
        }
        Some(Self(result))
    }
    pub(crate) fn insets(self, width: f32, height: f32, scale: f32) -> [u32; 4] {
        [
            (-self.0[0]).max(0.0),
            (-self.0[1]).max(0.0),
            (self.0[2] - width).max(0.0),
            (self.0[3] - height).max(0.0),
        ]
        .map(|v| (v * scale).ceil() as u32)
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{Color, Points, TransformKind};

    use super::*;
    #[test]
    fn shader_capture_uses_maximum_nonnegative_overflow() {
        use nsh_core::shader::{Shader, ShaderLayer};
        let shader =
            Shader::compile("fn shade(uv: vec2f) -> vec4f { return vec4f(uv, 0.0, 1.0); }")
                .unwrap();
        let layer = |overflow| {
            std::sync::Arc::new(
                ShaderLayer::new(shader.clone(), &[], &mut Vec::new())
                    .with_overflow(Points(overflow)),
            )
        };
        let layers = [(2, layer(6.0)), (4, layer(12.0)), (6, layer(-5.0))];
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(0.0, &layers).0,
            [-12.0, -12.0, 112.0, 62.0]
        );
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(0.0, &[]).0,
            [0.0, 0.0, 100.0, 50.0]
        );
        // Fills live in Shaders, separately from the effect stack. Native
        // texture export and the CSS scene must keep their overflow too.
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(10.0, &[]).0,
            [-10.0, -10.0, 110.0, 60.0]
        );
        assert_eq!(
            Bounds::new(100.0, 50.0).shaders(20.0, &layers).0,
            [-20.0, -20.0, 120.0, 70.0]
        );
    }
    #[test]
    fn oversized_targets_keep_the_box_and_aspect_ratio() {
        let coverage = Coverage::shader(Size::new(Points(3000.0), Points(1500.0)), 100.0);
        let (w, h, scale) = coverage.pixels(3.0);
        assert_eq!((w, h), (4096, 2176));
        assert!((scale - 1.28).abs() < 0.0001);
        assert_eq!(w as f32 / h as f32, 3200.0 / 1700.0);
        let words = nsh_core::shader::builtins(
            coverage.box_size,
            0.0,
            scale,
            nsh_core::geometry::Percentage(0.0),
            Points(100.0),
        );
        assert_eq!(
            [f32::from_bits(words[0]), f32::from_bits(words[1])],
            [3000.0, 1500.0]
        );
        assert_eq!(f32::from_bits(words[3]), scale);
        let (w, h, scale) = target_size(1.0, 10000.0, 1.0);
        assert_eq!((w, h), (1, 4096));
        assert!((scale - 0.4096).abs() < 0.0001);
        assert_eq!(target_size(0.0, 100.0, 3.0), (0, 0, 0.0));
        assert_eq!(target_size(100.0, 100.0, f32::INFINITY), (0, 0, 0.0));
    }
    #[test]
    fn asymmetric_capture_reframes_around_the_view_box() {
        let box_size = Size::new(Points(100.0), Points(50.0));
        let capture = Coverage {
            box_size,
            insets: [3.0, 7.0, 21.0, 11.0],
        };
        let shader = Coverage::shader(box_size, 10.0);
        // Source origin at (-3, -7), shader origin at (-10, -10), at 2 px/pt.
        assert_eq!(shader.destination(capture, (240, 140)), [14, 6, 262, 142]);
        assert_eq!(capture.destination(shader, (248, 136)), [-14, -6, 226, 134]);
        let first = nsh_core::shader::builtins(
            capture.box_size,
            0.0,
            2.0,
            nsh_core::geometry::Percentage(0.0),
            Points(10.0),
        );
        let second = nsh_core::shader::builtins(
            shader.box_size,
            0.0,
            2.0,
            nsh_core::geometry::Percentage(0.0),
            Points(10.0),
        );
        assert_eq!(first, second);
        assert_eq!(f32::from_bits(second[0]), 100.0);
        assert_eq!(f32::from_bits(second[1]), 50.0);
    }
    #[test]
    fn scene_requirements_follow_the_current_style() {
        let mut style = ViewStyle {
            blend_mode: nsh_core::BlendMode::PlusDarker,
            ..ViewStyle::default()
        };
        assert!(requires_scene(&style));
        style.blend_mode = nsh_core::BlendMode::Normal;
        assert!(!requires_scene(&style));
        let mut effects = nsh_core::Effects {
            backdrop_filters: vec![Filter::Blur(Points(4.0)), Filter::Blur(Points(8.0))],
            ..Default::default()
        };
        style.effects = Some(std::sync::Arc::new(effects.clone()));
        assert!(requires_scene(&style));
        effects.backdrop_filters.pop();
        style.effects = Some(std::sync::Arc::new(effects));
        assert!(!requires_scene(&style));
    }
    #[test]
    fn css_blur_radius_becomes_half_a_physical_sigma() {
        assert_eq!(blur_sigma(8.0, 3.0), 12.0);
        assert_eq!(blur_sigma(-8.0, 3.0), 0.0);
        assert_eq!(
            Bounds::new(10.0, 10.0)
                .filters(&[Filter::Blur(Points(8.0))])
                .0,
            [-12.0, -12.0, 22.0, 22.0]
        );
    }
    #[test]
    fn nested_shadow_overflow_and_negative_translation() {
        let child = Bounds::new(40.0, 20.0).filters(&[Filter::DropShadow {
            offset_x: Points(8.0),
            offset_y: Points(4.0),
            blur: Points(2.0),
            color: Color::BLACK,
        }]);
        let child = child
            .transformed(Matrix::of(TransformKind::TranslateX, -15.0))
            .unwrap();
        let mut parent = Bounds::new(30.0, 10.0);
        parent.union(child);
        assert_eq!(parent.insets(30.0, 10.0, 2.0), [30, 0, 12, 34]);
    }
    #[test]
    fn perspective_crossing_camera_has_no_finite_capture() {
        let mut matrix = Matrix::IDENTITY;
        matrix.0[3] = -0.1;
        assert!(Bounds::new(20.0, 10.0).transformed(matrix).is_none());
    }
}
