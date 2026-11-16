//! Requirement eligibility, root-narrowing, and telemetry for [`super::scene::Scene`].
//!
//! Native preparation runs only when inputs invalidate the scene, never on a timer.

use std::time::{Duration, Instant};

use nsh_core::{Color, NodeId, ViewStyle};
use rustc_hash::FxHashSet;

/// Unsupported inputs are reconsidered when their requirement changes. Transient
/// errors only suppress duplicate diagnostics; they never block a new invalidation.
#[derive(Default, Debug)]
pub(crate) struct SceneRequirements {
    ineligible: FxHashSet<NodeId>,
    unsupported_reported: FxHashSet<NodeId>,
    failure_reported: bool,
}

impl SceneRequirements {
    pub(crate) fn is_eligible(&self, id: NodeId) -> bool {
        !self.ineligible.contains(&id)
    }

    pub(crate) fn on_requirement_updated(&mut self, id: NodeId, required: bool) {
        self.ineligible.remove(&id);
        if !required {
            self.unsupported_reported.remove(&id);
        }
    }

    pub(crate) fn retain_live(&mut self, mut is_live: impl FnMut(NodeId) -> bool) {
        self.ineligible.retain(|&id| is_live(id));
        self.unsupported_reported.retain(|&id| is_live(id));
    }

    pub(crate) fn on_success(&mut self) {
        self.failure_reported = false;
    }

    pub(crate) fn mark_unsupported(&mut self, ids: &[NodeId]) -> bool {
        let mut should_report = false;
        for &id in ids {
            self.ineligible.insert(id);
            should_report |= self.unsupported_reported.insert(id);
        }
        should_report
    }

    pub(crate) fn on_failure(&mut self) -> bool {
        !std::mem::replace(&mut self.failure_reported, true)
    }
}

/// Returns `true` when `bounds` extends beyond the node's own `[0, 0, width, height]` box.
pub(crate) fn bounds_overflow_box(bounds: [f32; 4], width: f32, height: f32) -> bool {
    let [l, t, r, b] = bounds;
    if !l.is_finite() || !t.is_finite() || !r.is_finite() || !b.is_finite() {
        return true;
    }
    const EPS: f32 = 1e-3;
    l < -EPS || t < -EPS || r > width + EPS || b > height + EPS
}

pub(crate) fn is_opaque_color(color: Color) -> bool {
    color.0 & 0xff == 0xff
}

/// Checks whether a finite `sample_bounds` AABB lies completely inside `style`'s
/// opaque background shape over `[0, 0, width, height]`.
///
/// By convexity of rounded rectangles, an axis-aligned rectangle lies inside the
/// rounded rectangle iff all 4 of its corners lie inside it. Continuous corners
/// apply a conservative `1.6x` corner-tangent inset so squircle trim never leaks
/// pixels from behind the ancestor.
pub(crate) fn opaque_background_covers_bounds(
    style: &ViewStyle,
    scene_opacity: f32,
    width: f32,
    height: f32,
    sample_bounds: [f32; 4],
) -> bool {
    if scene_opacity < 1.0 || !is_opaque_color(style.background_color) {
        return false;
    }
    if width <= 0.0 || height <= 0.0 {
        return false;
    }
    let [x0, y0, x1, y1] = sample_bounds;
    if !x0.is_finite()
        || !y0.is_finite()
        || !x1.is_finite()
        || !y1.is_finite()
        || x0 > x1
        || y0 > y1
    {
        return false;
    }
    if x0 < 0.0 || y0 < 0.0 || x1 > width || y1 > height {
        return false;
    }
    let base = style.border_radius.0.max(0.0);
    let max_half = (width.min(height) * 0.5).max(0.0);
    let factor = if style.continuous_corners { 1.6 } else { 1.0 };
    let corner =
        |opt: Option<nsh_core::Points>| (opt.map_or(base, |p| p.0).max(0.0) * factor).min(max_half);
    let tl = corner(style.corner_radius.top_left);
    let tr = corner(style.corner_radius.top_right);
    let bl = corner(style.corner_radius.bottom_left);
    let br = corner(style.corner_radius.bottom_right);

    let point_in_rrect = |x: f32, y: f32| -> bool {
        if x < tl && y < tl {
            let dx = tl - x;
            let dy = tl - y;
            if dx * dx + dy * dy > tl * tl {
                return false;
            }
        }
        if x > width - tr && y < tr {
            let dx = x - (width - tr);
            let dy = tr - y;
            if dx * dx + dy * dy > tr * tr {
                return false;
            }
        }
        if x < bl && y > height - bl {
            let dx = bl - x;
            let dy = y - (height - bl);
            if dx * dx + dy * dy > bl * bl {
                return false;
            }
        }
        if x > width - br && y > height - br {
            let dx = x - (width - br);
            let dy = y - (height - br);
            if dx * dx + dy * dy > br * br {
                return false;
            }
        }
        true
    };

    point_in_rrect(x0, y0)
        && point_in_rrect(x1, y0)
        && point_in_rrect(x1, y1)
        && point_in_rrect(x0, y1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SceneStatsSnapshot {
    pub(crate) active_planes: usize,
    pub(crate) readback_bytes: u64,
    pub(crate) upload_bytes: u64,
    pub(crate) window_frames: u32,
    pub(crate) window_readback_bytes: u64,
    pub(crate) window_upload_bytes: u64,
}

/// Throttles `[NSH] scene-stats` counter logs to once per second while non-zero (A14),
/// emitting immediately on the first active frame after an idle gap so single-frame
/// static Scene captures are also observable on device.
#[derive(Default, Debug)]
pub(crate) struct SceneStats {
    last_logged: Option<Instant>,
    window_frames: u32,
    window_readback_bytes: u64,
    window_upload_bytes: u64,
}

impl SceneStats {
    pub(crate) fn record(
        &mut self,
        now: Instant,
        active_planes: usize,
        readback_bytes: u64,
        upload_bytes: u64,
    ) -> Option<SceneStatsSnapshot> {
        if active_planes == 0 && readback_bytes == 0 && upload_bytes == 0 {
            return None;
        }
        self.window_frames = self.window_frames.saturating_add(1);
        self.window_readback_bytes = self.window_readback_bytes.saturating_add(readback_bytes);
        self.window_upload_bytes = self.window_upload_bytes.saturating_add(upload_bytes);
        let due = self
            .last_logged
            .is_none_or(|prev| now.duration_since(prev) >= Duration::from_secs(1));
        if !due {
            return None;
        }
        self.last_logged = Some(now);
        let snapshot = SceneStatsSnapshot {
            active_planes,
            readback_bytes,
            upload_bytes,
            window_frames: self.window_frames,
            window_readback_bytes: self.window_readback_bytes,
            window_upload_bytes: self.window_upload_bytes,
        };
        self.window_frames = 0;
        self.window_readback_bytes = 0;
        self.window_upload_bytes = 0;
        Some(snapshot)
    }

    pub(crate) fn reset_idle(&mut self) {
        self.last_logged = None;
        self.window_frames = 0;
        self.window_readback_bytes = 0;
        self.window_upload_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{Corners, Points};

    use super::*;

    #[test]
    fn transient_errors_do_not_disable_inputs_or_reset_on_unrelated_updates() {
        let mut requirements = SceneRequirements::default();
        assert!(requirements.on_failure());
        for _ in 0..8 {
            assert!(requirements.is_eligible(7));
            assert!(!requirements.on_failure());
        }
        requirements.on_requirement_updated(99, false);
        assert!(!requirements.on_failure());
        assert!(requirements.is_eligible(7));
        requirements.on_success();
        assert!(requirements.on_failure());
    }

    #[test]
    fn unsupported_marks_requirement_ineligible_and_reports_once_until_updated() {
        let mut retry = SceneRequirements::default();
        assert!(retry.is_eligible(7));
        assert!(retry.mark_unsupported(&[7]));
        assert!(!retry.is_eligible(7));
        // Duplicate unsupported check for the same node does not spam diagnostics.
        assert!(!retry.mark_unsupported(&[7]));

        // Style update re-enables eligibility; removing requirement clears reported state.
        retry.on_requirement_updated(7, false);
        assert!(retry.is_eligible(7));
        assert!(retry.mark_unsupported(&[7]));
    }

    #[test]
    fn non_overflowing_mask_and_opaque_rounded_card_narrow_scene_root() {
        assert!(!bounds_overflow_box([0.0, 0.0, 120.0, 60.0], 120.0, 60.0));
        assert!(bounds_overflow_box([-4.0, 0.0, 120.0, 60.0], 120.0, 60.0));

        let opaque_card = ViewStyle {
            background_color: Color(0x1e1e1eff),
            border_radius: Points(16.0),
            corner_radius: Corners::all(None),
            ..ViewStyle::default()
        };
        // Pill well inside card's rounded corners is completely covered -> card is minimal root.
        assert!(opaque_background_covers_bounds(
            &opaque_card,
            1.0,
            340.0,
            200.0,
            [12.0, 8.0, 136.0, 68.0],
        ));
        // Sample box poking into the exterior of the 16pt top-left corner arc `(1, 1)` is NOT covered.
        assert!(!opaque_background_covers_bounds(
            &opaque_card,
            1.0,
            340.0,
            200.0,
            [1.0, 1.0, 100.0, 50.0],
        ));
        // Translucent card (`alpha < 255`) or mid-fade (`scene_opacity < 1.0`) cannot stop backdrop walk.
        let translucent_card = ViewStyle {
            background_color: Color(0x1e1e1e80),
            ..opaque_card.clone()
        };
        assert!(!opaque_background_covers_bounds(
            &translucent_card,
            1.0,
            340.0,
            200.0,
            [24.0, 24.0, 100.0, 60.0],
        ));
        assert!(!opaque_background_covers_bounds(
            &opaque_card,
            0.8,
            340.0,
            200.0,
            [24.0, 24.0, 100.0, 60.0],
        ));
    }

    #[test]
    fn scene_stats_logs_first_active_frame_then_throttles_to_one_second() {
        let t0 = Instant::now();
        let mut stats = SceneStats::default();
        assert_eq!(stats.record(t0, 0, 0, 0), None);

        let first = stats.record(t0, 2, 4096, 2048).unwrap();
        assert_eq!(first.active_planes, 2);
        assert_eq!(first.readback_bytes, 4096);
        assert_eq!(first.upload_bytes, 2048);
        assert_eq!(first.window_frames, 1);

        // Suppressed within 1s, while accumulating window totals.
        assert!(
            stats
                .record(t0 + Duration::from_millis(200), 2, 4096, 0)
                .is_none()
        );
        assert!(
            stats
                .record(t0 + Duration::from_millis(999), 2, 4096, 1024)
                .is_none()
        );

        let next = stats
            .record(t0 + Duration::from_millis(1000), 3, 8192, 512)
            .unwrap();
        assert_eq!(next.active_planes, 3);
        assert_eq!(next.window_frames, 3);
        assert_eq!(next.window_readback_bytes, 4096 + 4096 + 8192);
        assert_eq!(next.window_upload_bytes, 1024 + 512);
    }
}
