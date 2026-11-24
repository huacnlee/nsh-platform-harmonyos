//! CSS premultiplied stops for Drawing's straight-alpha gradient shaders.
//! Canvas gradients intentionally use their own straight-alpha interpolation.

use nsh_core::Color;

type Stop = (f32, Color);

pub(crate) struct NativeStops {
    pub(crate) colors: Vec<u32>,
    pub(crate) positions: Vec<f32>,
}

impl NativeStops {
    pub(crate) fn new(stops: &[Stop]) -> Self {
        let mut out = Self {
            colors: Vec::with_capacity(stops.len()),
            positions: Vec::with_capacity(stops.len()),
        };
        if stops.len() == 1 {
            out.push(stops[0]);
        }
        for pair in stops.windows(2) {
            let [mut a, mut b] = [pair[0], pair[1]];
            if b.0 > a.0 {
                // A zero-alpha stop has a separate RGB limit on each side.
                // Keep both at the same position for red -> transparent -> blue.
                if alpha(a.1) == 0 {
                    a.1 = Color(b.1.0 & !255);
                }
                if alpha(b.1) == 0 {
                    b.1 = Color(a.1.0 & !255);
                }
                out.push(a);
                out.interval(&Segment { a, b }, a, b);
            } else {
                // Preserve the order of coincident stops: they are hard edges.
                out.push(a);
                out.push(b);
            }
        }
        out
    }

    fn push(&mut self, (position, color): Stop) {
        let argb = color.0.rotate_right(8);
        if self.positions.last() == Some(&position) && self.colors.last() == Some(&argb) {
            return;
        }
        self.positions.push(position);
        self.colors.push(argb);
    }

    fn interval(&mut self, source: &Segment, a: Stop, b: Stop) {
        let aa = alpha(a.1);
        let ab = alpha(b.1);
        let delta = [8, 16, 24]
            .map(|shift| channel(a.1, shift).abs_diff(channel(b.1, shift)))
            .into_iter()
            .max()
            .unwrap();
        // Straight vs premultiplied interpolation differs by
        // t*(1-t)*delta_rgb*delta_alpha/255. Allow half a channel step;
        // rounding each generated RGB endpoint costs at most another half.
        // Integer alpha splits introduce no alpha quantization. Adjacent
        // alpha values always pass, bounding this to 256 stops / depth 8.
        if delta * aa.abs_diff(ab) <= 510 {
            self.push(b);
            return;
        }
        let middle_alpha = (aa + ab) / 2;
        let t = (f64::from(middle_alpha) - f64::from(alpha(source.a.1)))
            / (f64::from(alpha(source.b.1)) - f64::from(alpha(source.a.1)));
        let position =
            (f64::from(source.a.0) + (f64::from(source.b.0) - f64::from(source.a.0)) * t) as f32;
        if position <= a.0 || position >= b.0 {
            // No representable native position remains in this interval.
            self.push(b);
            return;
        }
        let middle = (position, mix(source.a.1, source.b.1, t));
        self.interval(source, a, middle);
        self.interval(source, middle, b);
    }
}

struct Segment {
    a: Stop,
    b: Stop,
}

fn channel(color: Color, shift: u32) -> u32 {
    (color.0 >> shift) & 255
}

fn alpha(color: Color) -> u32 {
    channel(color, 0)
}

/// Same premultiplied sRGB interpolation as nsh-core's CSS stop clipping.
fn mix(a: Color, b: Color, t: f64) -> Color {
    let aa = f64::from(alpha(a));
    let ab = f64::from(alpha(b));
    let alpha = aa + (ab - aa) * t;
    if alpha <= 0.0 {
        return Color(0);
    }
    let mut out = alpha.round().clamp(0.0, 255.0) as u32;
    for shift in [8, 16, 24] {
        let x = f64::from(channel(a, shift)) * aa;
        let y = f64::from(channel(b, shift)) * ab;
        out |= (((x + (y - x) * t) / alpha).round().clamp(0.0, 255.0) as u32) << shift;
    }
    Color(out)
}

fn sample(stops: &[Stop], t: f32, before: bool) -> Color {
    let i = stops.partition_point(|s| if before { s.0 < t } else { s.0 <= t });
    if i == 0 {
        return stops[0].1;
    }
    if i == stops.len() {
        return stops[i - 1].1;
    }
    let (a, b) = (stops[i - 1], stops[i]);
    mix(a.1, b.1, f64::from(t - a.0) / f64::from(b.0 - a.0))
}

/// Clip a CSS conic ramp to one turn and rotate it into Drawing's sweep space.
/// Both sides of a hard edge survive, including one at the native sweep seam.
pub(crate) fn sweep_stops(stops: &[Stop], angle: f32) -> Vec<Stop> {
    if stops.is_empty() {
        return Vec::new();
    }
    let shift = (angle / std::f32::consts::TAU).rem_euclid(1.0);
    let mut out = Vec::with_capacity(stops.len() + 4);
    if shift == 0.0 {
        out.push((0.0, sample(stops, 0.0, false)));
        out.extend(stops.iter().copied().filter(|s| s.0 > 0.0 && s.0 < 1.0));
        out.push((1.0, sample(stops, 1.0, true)));
        return out;
    }
    let cut = 1.0 - shift;
    out.push((0.0, sample(stops, cut, false)));
    out.extend(
        stops
            .iter()
            .filter(|s| s.0 > cut && s.0 < 1.0)
            .map(|s| (s.0 - cut, s.1)),
    );
    out.push((shift, sample(stops, 1.0, true)));
    out.push((shift, sample(stops, 0.0, false)));
    out.extend(
        stops
            .iter()
            .filter(|s| s.0 > 0.0 && s.0 < cut)
            .map(|s| (s.0 + shift, s.1)),
    );
    out.push((1.0, sample(stops, cut, true)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color = Color(0xff0000ff);
    const BLUE: Color = Color(0x0000ffff);
    const CLEAR: Color = Color(0);

    // Evaluate Drawing's straight RGBA interpolation, then source-over. The
    // reference instead interpolates the original premultiplied endpoints.
    fn rgba(color: Color) -> [f64; 4] {
        [24, 16, 8, 0].map(|s| f64::from(channel(color, s)))
    }

    fn over(color: [f64; 4], background: f64) -> [f64; 3] {
        let alpha = color[3] / 255.0;
        [0, 1, 2].map(|i| color[i] * alpha + background * (1.0 - alpha))
    }

    fn native_at(stops: &NativeStops, t: f64) -> [f64; 4] {
        let i = stops.positions.partition_point(|p| f64::from(*p) <= t);
        let color = |i: usize| rgba(Color(stops.colors[i].rotate_left(8)));
        if i == 0 {
            return color(0);
        }
        if i == stops.positions.len() {
            return color(i - 1);
        }
        let a = color(i - 1);
        let b = color(i);
        let x = f64::from(stops.positions[i - 1]);
        let f = (t - x) / (f64::from(stops.positions[i]) - x);
        std::array::from_fn(|j| a[j] + (b[j] - a[j]) * f)
    }

    fn check_segment(a: Color, b: Color, samples: u32) {
        let native = NativeStops::new(&[(0.0, a), (1.0, b)]);
        assert!(native.positions.len() <= 256);
        for step in 0..=samples {
            let t = f64::from(step) / f64::from(samples);
            let actual = native_at(&native, t);
            for background in [0.0, 255.0] {
                let start = over(rgba(a), background);
                let end = over(rgba(b), background);
                let rendered = over(actual, background);
                for i in 0..3 {
                    let reference = start[i] + (end[i] - start[i]) * t;
                    assert!(
                        (rendered[i] - reference).abs() <= 1.001,
                        "{a:?} -> {b:?}, t={t}, actual={}, reference={reference}",
                        rendered[i]
                    );
                }
            }
        }
    }

    #[test]
    fn diffview_fade_keeps_the_row_color_without_extra_stops() {
        let row = Color(0xfbebecff);
        let stops = NativeStops::new(&[(0.0, CLEAR), (1.0, row)]);
        assert_eq!(stops.colors, [0x00fbebec, 0xfffbebec]);
        assert_eq!(stops.positions, [0.0, 1.0]);
        for i in 0..=100 {
            let color = native_at(&stops, f64::from(i) / 100.0);
            for (component, background) in color[..3].iter().zip([251.0, 235.0, 236.0]) {
                let actual = component * color[3] / 255.0 + background * (1.0 - color[3] / 255.0);
                assert!((actual - background).abs() < 1e-10);
            }
        }
    }

    #[test]
    fn transparent_middle_keeps_both_color_limits() {
        let stops = NativeStops::new(&[(0.0, RED), (0.5, CLEAR), (1.0, BLUE)]);
        assert_eq!(stops.positions, [0.0, 0.5, 0.5, 1.0]);
        assert_eq!(
            stops.colors,
            [0xffff0000, 0x00ff0000, 0x000000ff, 0xff0000ff]
        );
        assert_eq!(native_at(&stops, 0.25), [255.0, 0.0, 0.0, 127.5]);
        assert_eq!(native_at(&stops, 0.75), [0.0, 0.0, 255.0, 127.5]);
    }

    #[test]
    fn opaque_and_constant_alpha_ramps_keep_their_original_stops() {
        for alpha in [0, 1, 127, 255] {
            let stops = NativeStops::new(&[
                (0.0, Color(0xff000000 | alpha)),
                (1.0, Color(0x0000ff00 | alpha)),
            ]);
            assert_eq!(stops.positions, [0.0, 1.0]);
        }
    }

    #[test]
    fn hard_edges_and_degenerate_ramps_keep_stop_order() {
        let stops = NativeStops::new(&[(0.0, RED), (0.5, RED), (0.5, BLUE), (1.0, BLUE)]);
        assert_eq!(stops.positions, [0.0, 0.5, 0.5, 1.0]);
        assert_eq!(native_at(&stops, 0.5), rgba(BLUE));
        let stops = NativeStops::new(&[(0.0, RED), (0.0, BLUE)]);
        assert_eq!(stops.colors, [0xffff0000, 0xff0000ff]);
        assert!(NativeStops::new(&[]).colors.is_empty());
        assert_eq!(NativeStops::new(&[(0.0, RED)]).colors, [0xffff0000]);
    }

    #[test]
    fn all_alpha_pairs_stay_within_one_channel_step() {
        for a in 0..=255 {
            for b in 0..=255 {
                check_segment(Color(0xff130000 | a), Color(0x00e8ff00 | b), 16);
            }
        }
    }

    #[test]
    fn varied_colors_and_alpha_follow_the_premultiplied_reference() {
        let mut random = 0x6a09_e667_u32;
        for _ in 0..4096 {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let a = Color(random);
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            check_segment(a, Color(random), 128);
        }
    }

    #[test]
    fn conic_cut_uses_premultiplied_color_and_keeps_the_wrap_edge() {
        let stops = sweep_stops(&[(0.0, RED), (1.0, CLEAR)], std::f32::consts::FRAC_PI_2);
        assert_eq!(
            stops,
            [
                (0.0, Color(0xff000040)),
                (0.25, CLEAR),
                (0.25, RED),
                (1.0, Color(0xff000040))
            ]
        );
        let native = NativeStops::new(&stops);
        for i in 0..=100 {
            assert_eq!(
                native_at(&native, f64::from(i) / 100.0)[..3],
                [255.0, 0.0, 0.0]
            );
        }
    }

    #[test]
    fn conic_hard_edge_at_the_cut_keeps_distinct_seam_colors() {
        let stops = sweep_stops(
            &[(0.0, RED), (0.5, RED), (0.5, BLUE), (1.0, BLUE)],
            std::f32::consts::PI,
        );
        assert_eq!(stops, [(0.0, BLUE), (0.5, BLUE), (0.5, RED), (1.0, RED)]);
    }

    #[test]
    fn conic_clipping_and_repeated_stops_preserve_boundary_limits() {
        let stops = [
            (-0.5, RED),
            (0.0, RED),
            (0.0, BLUE),
            (0.5, BLUE),
            (0.5, RED),
            (1.0, RED),
            (1.0, BLUE),
            (1.5, BLUE),
        ];
        assert_eq!(
            sweep_stops(&stops, 0.0),
            [(0.0, BLUE), (0.5, BLUE), (0.5, RED), (1.0, RED)]
        );
    }
}
