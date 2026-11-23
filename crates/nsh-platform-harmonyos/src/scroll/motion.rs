//! Scale and offset motion evaluated by the surface's native frame clock.

/// Aggregate the SDK's main/cross Scroll states (idle=0, drag=1, fling=2).
/// A finger still controls the scroll while either axis reports a drag.
pub(crate) fn combined_phase(phases: [i32; 2]) -> i32 {
    if phases.contains(&1) {
        1
    } else if phases.contains(&2) {
        2
    } else {
        0
    }
}

/// JumpToPosition ends a fling immediately; AnimateTo also ends it when the
/// effective-axis target matches its current offset (ArkUI NearEqual, in px).
/// A different animated target may keep an observed fling, but setter success
/// alone must never turn an idle axis into an active driver.
pub(crate) fn command_phase(
    phase: i32,
    animated: bool,
    current: Option<f32>,
    target: f32,
    scale: f32,
) -> i32 {
    let unchanged = current.is_some_and(|current| (target * scale - current * scale).abs() < 0.001);
    if phase == 2 && (!animated || unchanged) {
        0
    } else {
        phase
    }
}

/// Tracks reports already delivered to the engine. Applied scroll commands must
/// be acknowledged even when their final position is unchanged.
#[derive(Default)]
pub(crate) struct OffsetReport {
    last: std::cell::Cell<Option<(ZoomFrame, u32)>>,
}
impl OffsetReport {
    pub(crate) fn update(&self, offset: (f32, f32), scale: f32, applied: u32) -> bool {
        let next = Some((ZoomFrame { offset, scale }, applied));
        self.last.replace(next) != next
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct ZoomFrame {
    pub(crate) scale: f32,
    pub(crate) offset: (f32, f32),
}

#[derive(Clone, Copy)]
pub(crate) struct ZoomMotion {
    pub(crate) from: ZoomFrame,
    pub(crate) to: ZoomFrame,
    pub(crate) start: Option<f64>,
}

impl ZoomMotion {
    pub(crate) fn sample(&mut self, now: f64) -> (ZoomFrame, bool) {
        let start = *self.start.get_or_insert(now);
        let t = ((now - start) / 250.0).clamp(0.0, 1.0) as f32;
        let eased = 1.0 - (1.0 - t).powi(3);
        let lerp = |a, b| a + (b - a) * eased;
        (
            ZoomFrame {
                scale: lerp(self.from.scale, self.to.scale),
                offset: (
                    lerp(self.from.offset.0, self.to.offset.0),
                    lerp(self.from.offset.1, self.to.offset.1),
                ),
            },
            t < 1.0,
        )
    }
}

/// Maximum native ArkUI scroll offset along one axis when `content_inset` is
/// applied as ArkUI `Padding = [top, right, bottom, left]`.
pub(crate) fn native_max_offset(
    length: f32,
    scale: f32,
    viewport: f32,
    lead: f32,
    trail: f32,
) -> f32 {
    (length * scale + lead + trail - viewport).max(0.0)
}

/// Converts an NSH/UIKit-convention offset (`-lead` at rest) to ArkUI's
/// 0-based native scroll offset, clamped to `[0, native_max_offset]`.
pub(crate) fn nsh_to_native_offset(
    offset: f32,
    length: f32,
    scale: f32,
    viewport: f32,
    lead: f32,
    trail: f32,
) -> f32 {
    let max = native_max_offset(length, scale, viewport, lead, trail);
    (offset + lead).clamp(0.0, max)
}

/// Converts ArkUI's 0-based native scroll offset to NSH/UIKit-convention
/// offset (`-lead` at rest).
pub(crate) fn native_to_nsh_offset(native: f32, lead: f32) -> f32 {
    native - lead
}

/// Keeps a content point under the given viewport position while scale changes,
/// returning a 0-based native ArkUI scroll offset.
#[allow(clippy::too_many_arguments)]
pub(crate) fn focal_native_offset(
    content: f32,
    viewport_point: f32,
    scale: f32,
    length: f32,
    viewport: f32,
    lead: f32,
    trail: f32,
    centering: f32,
) -> f32 {
    let max = native_max_offset(length, scale, viewport, lead, trail);
    (content * scale + lead + centering - viewport_point).clamp(0.0, max)
}

/// Keeps a content point under the given viewport position while scale changes.
#[cfg(test)]
pub(crate) fn focal_offset(
    content: f32,
    viewport_point: f32,
    scale: f32,
    length: f32,
    viewport: f32,
) -> f32 {
    focal_native_offset(
        content,
        viewport_point,
        scale,
        length,
        viewport,
        0.0,
        0.0,
        0.0,
    )
}

/// Rubber-band zoom past a bound; release animates back to the mounted limits.
pub(crate) fn resisted_scale(scale: f32, min: f32, max: f32, bounce: bool) -> f32 {
    let bound = scale.clamp(min, max);
    if bounce {
        (bound + (scale - bound) * 0.25).clamp(min * 0.5, max * 1.5)
    } else {
        bound
    }
}

/// Infers a `Scroll` node's scrollable content extent from its viewport and
/// mounted non-hidden child frames until `MountItem::ScrollContent` arrives
/// (for example when `nsh-core`'s paced mounting held creation past the initial
/// `cull_frames_in` layout pass).
pub(crate) fn inferred_content_size(
    viewport: crate::render::geometry::Size,
    children: impl IntoIterator<Item = crate::render::geometry::Rect>,
) -> crate::render::geometry::Size {
    let mut width = viewport.width.max(0.0);
    let mut height = viewport.height.max(0.0);
    for frame in children {
        if frame.width.is_finite() && frame.x.is_finite() {
            width = width.max(frame.x + frame.width);
        }
        if frame.height.is_finite() && frame.y.is_finite() {
            height = height.max(frame.y + frame.height);
        }
    }
    crate::render::geometry::Size::new(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_native_axis_keeps_the_scroll_active_until_both_stop() {
        for cross_first in [false, true] {
            let observed = [
                [0, 0],
                [1, 0],
                [1, 1],
                [2, 1],
                [2, 0],
                [2, 2],
                [0, 2],
                [0, 0],
            ];
            let phases: Vec<_> = observed
                .into_iter()
                .map(|[main, cross]| {
                    combined_phase(if cross_first {
                        [cross, main]
                    } else {
                        [main, cross]
                    })
                })
                .collect();
            assert_eq!(phases, [0, 1, 1, 1, 2, 2, 2, 0]);
        }
    }

    #[test]
    fn commands_release_a_replaced_fling_only_when_it_cannot_continue() {
        assert_eq!(command_phase(2, false, None, 100.0, 2.0), 0);
        assert_eq!(command_phase(2, true, Some(100.0), 100.0, 2.0), 0);
        assert_eq!(command_phase(2, true, Some(100.0), 200.0, 2.0), 2);
        assert_eq!(command_phase(2, true, None, 100.0, 2.0), 2);
        // A zero-distance command must also acknowledge the SDK's subpixel
        // no-op, with the tolerance applied to pixels rather than VP.
        assert_eq!(command_phase(2, true, Some(0.0), 0.0004, 2.0), 0);
        assert_eq!(command_phase(2, true, Some(0.0), 0.0004, 3.0), 2);
        // Neither a successful setter nor an immediate correction ends a
        // finger's drag or manufactures an animation on an idle axis.
        for phase in [0, 1] {
            for animated in [false, true] {
                assert_eq!(command_phase(phase, animated, Some(0.0), 100.0, 2.0), phase);
            }
        }
    }

    #[test]
    fn replacing_one_axis_with_the_other_keeps_a_single_momentum_interval() {
        for old_axis in [0, 1] {
            let new_axis = 1 - old_axis;
            let mut phases = [0; 2];
            phases[old_axis] = 2;
            let previous = combined_phase(phases);
            // Actual synchronous native Start, then the old axis's no-op.
            phases[new_axis] = 2;
            phases[old_axis] = command_phase(phases[old_axis], true, Some(40.0), 40.0, 2.0);
            assert_eq!(combined_phase(phases), previous);
            phases[new_axis] = 0; // actual native Stop
            assert_eq!(combined_phase(phases), 0);
            // Both axes stopped by one immediate command publish idle once.
            assert_eq!(
                combined_phase([2, 2].map(|p| command_phase(p, false, None, 0.0, 2.0))),
                0
            );
        }
    }

    #[test]
    fn native_duplicates_are_suppressed_without_losing_command_acknowledgements() {
        let report = OffsetReport::default();
        assert!(report.update((0.0, 0.0), 1.0, 0));
        assert!(report.update((40.0, 0.0), 1.0, 1)); // immediate programmatic result
        assert!(!report.update((40.0, 0.0), 1.0, 1)); // delayed OnDidScroll
        assert!(!report.update((40.0, 0.0), 1.0, 1)); // OnScrollStop
        assert!(report.update((40.0, 0.0), 1.0, 2)); // clamped command still acknowledged
        assert!(!report.update((40.0, 0.0), 1.0, 2));
        assert!(report.update((39.0, 0.0), 1.0, 2)); // finger movement still delivered
        assert!(report.update((0.0, 0.0), 0.5, 2));
        assert!(report.update((0.0, 0.0), 0.75, 2)); // zoom below fit need not move
        assert!(!report.update((0.0, 0.0), 0.75, 2));
    }

    #[test]
    fn zoom_interpolates_scale_and_offsets_and_finishes_exactly() {
        let mut motion = ZoomMotion {
            from: ZoomFrame {
                scale: 1.0,
                offset: (0.0, 10.0),
            },
            to: ZoomFrame {
                scale: 3.0,
                offset: (200.0, 250.0),
            },
            start: None,
        };
        let (frame, running) = motion.sample(20.0);
        assert!(running);
        assert_eq!(frame.scale, 1.0);
        assert_eq!(frame.offset, (0.0, 10.0));
        let (frame, running) = motion.sample(145.0);
        assert!(running);
        assert_eq!(frame.scale, 2.75);
        assert_eq!(frame.offset, (175.0, 220.0));
        let (frame, running) = motion.sample(270.0);
        assert!(!running);
        assert_eq!(frame.scale, 3.0);
        assert_eq!(frame.offset, (200.0, 250.0));
    }

    #[test]
    fn focal_point_stays_under_the_fingers_until_content_bounds() {
        assert_eq!(focal_offset(90.0, 30.0, 2.0, 400.0, 200.0), 150.0);
        assert_eq!(focal_offset(10.0, 80.0, 1.0, 400.0, 200.0), 0.0);
        assert_eq!(focal_offset(390.0, 30.0, 2.0, 400.0, 200.0), 600.0);
    }

    #[test]
    fn content_inset_roundtrips_between_nsh_and_native_offsets() {
        // Content 400, viewport 200, lead 40, trail 60 => native max = 400 + 100 - 200 = 300.
        assert_eq!(native_max_offset(400.0, 1.0, 200.0, 40.0, 60.0), 300.0);
        // Resting NSH offset (-40) maps to native 0, and maps back to -40.
        let native_start = nsh_to_native_offset(-40.0, 400.0, 1.0, 200.0, 40.0, 60.0);
        assert_eq!(native_start, 0.0);
        assert_eq!(native_to_nsh_offset(native_start, 40.0), -40.0);
        // NSH offset 0 aligns content top with viewport top => native 40.
        let native_zero = nsh_to_native_offset(0.0, 400.0, 1.0, 200.0, 40.0, 60.0);
        assert_eq!(native_zero, 40.0);
        assert_eq!(native_to_nsh_offset(native_zero, 40.0), 0.0);
        // Overshooting end clamps to native 300 => NSH 260 (400 + 60 - 200).
        let native_end = nsh_to_native_offset(999.0, 400.0, 1.0, 200.0, 40.0, 60.0);
        assert_eq!(native_end, 300.0);
        assert_eq!(native_to_nsh_offset(native_end, 40.0), 260.0);
        // Focal offset accounts for leading inset and centering.
        assert_eq!(
            focal_native_offset(90.0, 30.0, 2.0, 400.0, 200.0, 20.0, 10.0, 0.0),
            170.0
        );
    }

    #[test]
    fn bounce_has_resistance_and_is_bounded() {
        assert_eq!(resisted_scale(5.0, 1.0, 3.0, false), 3.0);
        assert_eq!(resisted_scale(5.0, 1.0, 3.0, true), 3.5);
        assert_eq!(resisted_scale(0.0, 1.0, 3.0, true), 0.75);
        assert_eq!(resisted_scale(f32::MAX, 1.0, 3.0, true), 4.5);
    }

    #[test]
    fn inferred_content_size_covers_viewport_and_mounted_children() {
        use crate::render::geometry::{Rect, Size};
        assert_eq!(
            inferred_content_size(Size::new(152.0, 476.0), []),
            Size::new(152.0, 476.0)
        );
        assert_eq!(
            inferred_content_size(
                Size::new(152.0, 476.0),
                [
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 576.0,
                        height: 476.0,
                    },
                    Rect {
                        x: 12.0,
                        y: 400.0,
                        width: 100.0,
                        height: 120.0,
                    },
                ],
            ),
            Size::new(576.0, 520.0)
        );
    }
}
