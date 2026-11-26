//! Unified pan gesture specification, activation state machine, and coordinate
//! conversions shared between ArkUI's native `PanGesture` and the touch router.

use nsh_core::{
    EventKind, EventMask, GestureValues, NodeId, PanActivation, PanDirection, Point, Points, Size,
};

/// Directional mask for ArkUI's `createPanGesture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanDirectionSpec {
    All,
    Horizontal,
    Vertical,
    Left,
    Right,
    Up,
    Down,
}

/// Configuration for an ArkUI `PanGesture` derived from NSH props/events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PanSpec {
    pub(crate) direction: PanDirectionSpec,
    pub(crate) distance_px: f64,
}

/// Complete native gesture recognizer specification for a view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GestureSpec {
    pub(crate) pan: Option<PanSpec>,
    pub(crate) pinch: bool,
    pub(crate) rotation: bool,
}

/// Computes which native ArkUI gesture recognizers a view needs from its
/// animated gesture values, subscribed event mask, `panActivation`, pointer
/// blocking state, and display density (`scale`).
pub(crate) fn gesture_spec(
    values: GestureValues,
    events: EventMask,
    activation: Option<PanActivation>,
    blocked: bool,
    scale: f32,
) -> GestureSpec {
    if blocked {
        return GestureSpec {
            pan: None,
            pinch: false,
            rotation: false,
        };
    }
    let scale = f64::from(scale.max(1.0));
    let pan = nsh_core::input::Candidate::pan(&values, events, activation).map(|candidate| {
        let direction = match activation.and_then(|a| a.way()) {
            Some(PanDirection::Left) => PanDirectionSpec::Left,
            Some(PanDirection::Right) => PanDirectionSpec::Right,
            Some(PanDirection::Up) => PanDirectionSpec::Up,
            Some(PanDirection::Down) => PanDirectionSpec::Down,
            None | Some(_) => match activation.and_then(|a| a.axis()) {
                Some(true) => PanDirectionSpec::Horizontal,
                Some(false) => PanDirectionSpec::Vertical,
                None => match candidate {
                    nsh_core::input::Candidate::Pan {
                        x: true, y: false, ..
                    } => PanDirectionSpec::Horizontal,
                    nsh_core::input::Candidate::Pan {
                        x: false, y: true, ..
                    } => PanDirectionSpec::Vertical,
                    _ => PanDirectionSpec::All,
                },
            },
        };
        let min_active_vp = activation.and_then(|a| {
            [a.active_offset_x, a.active_offset_y]
                .into_iter()
                .flatten()
                .map(|p| p.0.abs())
                .filter(|v| v.is_finite() && *v > 0.0)
                .reduce(f32::min)
        });
        let distance_px = match min_active_vp {
            Some(vp) => f64::from(vp) * scale,
            None if activation.is_some() => 0.0,
            None => f64::from(nsh_core::input::PAN_SLOP.0) * scale,
        };
        PanSpec {
            direction,
            distance_px,
        }
    });
    let pinch = values.pinch.is_some()
        || events.contains(EventKind::PinchStart)
        || events.contains(EventKind::PinchEnd);
    let rotation = values.rotation.is_some()
        || events.contains(EventKind::RotationStart)
        || events.contains(EventKind::RotationEnd);
    GestureSpec {
        pan,
        pinch,
        rotation,
    }
}

/// Three-state decision of a `PanActivation` candidate within a single pointer
/// sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanDecision {
    Waiting,
    Activated,
    Failed,
}

/// Three-state lifecycle of an active ArkUI `PanGesture` callback sequence on a
/// single view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PanPhaseState {
    #[default]
    Pending,
    Active,
    Rejected,
}

#[derive(Debug, Clone, Copy)]
struct CandidateEntry {
    id: NodeId,
    activation: PanActivation,
    state: PanDecision,
}

#[derive(Debug, Clone)]
struct TrackedPointer {
    pointer_id: u32,
    start_surface: (f32, f32),
    last_delta: (f32, f32),
    candidates: Vec<CandidateEntry>,
}

/// Tracks `PanActivation` state across pointer sequences so ArkUI's native
/// `PanGesture`, its gesture interrupter, and the touch responder all share the
/// exact same `starts_at` / `decide_move` result.
#[derive(Debug, Default)]
pub(crate) struct PanTracker {
    pointers: Vec<TrackedPointer>,
}

impl PanTracker {
    pub(crate) fn clear(&mut self) {
        self.pointers.clear();
    }

    /// Registers a new pointer down at `start_surface` (vp, relative to the
    /// surface root) with `surface_size` (vp) and innermost-first candidates.
    pub(crate) fn down(
        &mut self,
        pointer_id: u32,
        start_surface: (f32, f32),
        surface_size: (f32, f32),
        candidates: impl IntoIterator<Item = (NodeId, PanActivation)>,
    ) {
        self.pointers.retain(|p| p.pointer_id != pointer_id);
        let at = Point::new(Points(start_surface.0), Points(start_surface.1));
        let size = Size::new(Points(surface_size.0), Points(surface_size.1));
        let entries = candidates
            .into_iter()
            .map(|(id, activation)| {
                let state = if activation.starts_at(at, size) {
                    PanDecision::Waiting
                } else {
                    PanDecision::Failed
                };
                CandidateEntry {
                    id,
                    activation,
                    state,
                }
            })
            .collect();
        self.pointers.push(TrackedPointer {
            pointer_id,
            start_surface,
            last_delta: (0.0, 0.0),
            candidates: entries,
        });
    }

    /// Advances `pointer_id` to `current_surface` (vp, surface-relative) using
    /// `PanActivation::decide_move` along the line segment from the previous
    /// move. Returns the first newly activated candidate id (if any).
    pub(crate) fn move_pointer(
        &mut self,
        pointer_id: u32,
        current_surface: (f32, f32),
    ) -> Option<NodeId> {
        let pointer = self
            .pointers
            .iter_mut()
            .find(|p| p.pointer_id == pointer_id)?;
        let from = Point::new(Points(pointer.last_delta.0), Points(pointer.last_delta.1));
        let dx = current_surface.0 - pointer.start_surface.0;
        let dy = current_surface.1 - pointer.start_surface.1;
        let to = Point::new(Points(dx), Points(dy));
        pointer.last_delta = (dx, dy);

        let mut newly_activated = None;
        for entry in &mut pointer.candidates {
            if entry.state != PanDecision::Waiting {
                continue;
            }
            match entry.activation.decide_move(from, to) {
                Some(true) => {
                    entry.state = PanDecision::Activated;
                    if newly_activated.is_none() {
                        newly_activated = Some(entry.id);
                    }
                }
                Some(false) => {
                    entry.state = PanDecision::Failed;
                }
                None => {}
            }
        }
        newly_activated
    }

    pub(crate) fn up_or_cancel(&mut self, pointer_id: u32) {
        self.pointers.retain(|p| p.pointer_id != pointer_id);
    }

    /// Returns the decision and surface-relative start origin (vp) for `node_id`
    /// on the most recent tracked pointer carrying that candidate.
    pub(crate) fn decision(&self, node_id: NodeId) -> Option<(PanDecision, (f32, f32))> {
        self.pointers.iter().rev().find_map(|p| {
            p.candidates
                .iter()
                .find(|c| c.id == node_id)
                .map(|c| (c.state, p.start_surface))
        })
    }

    /// Returns the surface-relative down origin (vp) of the primary/latest
    /// tracked pointer, if any.
    pub(crate) fn latest_start(&self) -> Option<(f32, f32)> {
        self.pointers.last().map(|p| p.start_surface)
    }
}

/// Converts ArkUI gesture input window coordinates and pan offset (both in
/// physical px per OpenHarmony `native_gesture.h` / `ui_input_event.h`) into a
/// surface-relative origin in `vp`.
pub(crate) fn gesture_origin_surface_vp(
    input_window_px: (f32, f32),
    pan_offset_px: (f32, f32),
    surface_origin_px: (f32, f32),
    scale: f32,
) -> (f32, f32) {
    let s = scale.max(1.0);
    (
        (input_window_px.0 - pan_offset_px.0 - surface_origin_px.0) / s,
        (input_window_px.1 - pan_offset_px.1 - surface_origin_px.1) / s,
    )
}

/// Converts ArkUI gesture local coordinates and pan offset (both physical px)
/// into the local down point in `vp` for `clipPath` hit testing.
pub(crate) fn gesture_local_down_vp(
    input_local_px: (f32, f32),
    pan_offset_px: (f32, f32),
    scale: f32,
) -> (f32, f32) {
    let s = scale.max(1.0);
    (
        (input_local_px.0 - pan_offset_px.0) / s,
        (input_local_px.1 - pan_offset_px.1) / s,
    )
}

/// Converts a physical-px pan offset into logical `vp`.
pub(crate) fn gesture_translation_vp(offset_px: (f32, f32), scale: f32) -> (f32, f32) {
    let s = scale.max(1.0);
    (offset_px.0 / s, offset_px.1 / s)
}

/// Converts physical-px/s pan velocity into logical `vp/s`.
pub(crate) fn gesture_velocity_vp(velocity_px: (f32, f32), scale: f32) -> (f32, f32) {
    let s = scale.max(1.0);
    (velocity_px.0 / s, velocity_px.1 / s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_only_pan_listener_creates_recognizer_with_all_direction() {
        let mut mask = EventMask::default();
        mask.set(EventKind::PanStart, true);
        let spec = gesture_spec(GestureValues::default(), mask, None, false, 2.0);
        let pan = spec.pan.expect("onPanStart alone must create a PanGesture");
        assert_eq!(pan.direction, PanDirectionSpec::All);
        assert!((pan.distance_px - 12.0).abs() < 1e-6);
        assert!(!spec.pinch);
        assert!(!spec.rotation);
    }

    #[test]
    fn blocked_view_creates_no_recognizers() {
        let mut mask = EventMask::default();
        mask.set(EventKind::PanStart, true);
        mask.set(EventKind::PinchStart, true);
        mask.set(EventKind::RotationStart, true);
        let spec = gesture_spec(GestureValues::default(), mask, None, true, 3.0);
        assert_eq!(spec.pan, None);
        assert!(!spec.pinch);
        assert!(!spec.rotation);
    }

    #[test]
    fn directional_activation_scales_distance_to_px() {
        let mut mask = EventMask::default();
        mask.set(EventKind::PanEnd, true);
        let act = PanActivation::horizontal(Points(10.0), Points(8.0));
        let spec = gesture_spec(GestureValues::default(), mask, Some(act), false, 3.0);
        let pan = spec.pan.expect("pan spec");
        assert_eq!(pan.direction, PanDirectionSpec::Horizontal);
        assert!((pan.distance_px - 30.0).abs() < 1e-6);
    }

    #[test]
    fn tracker_fails_when_crossing_fail_axis_before_active_axis() {
        let mut tracker = PanTracker::default();
        let act = PanActivation::horizontal(Points(10.0), Points(8.0));
        tracker.down(1, (100.0, 100.0), (400.0, 800.0), [(7, act)]);
        assert_eq!(
            tracker.decision(7),
            Some((PanDecision::Waiting, (100.0, 100.0)))
        );

        // Move vertically past fail_offset_y (8 vp) before reaching active_offset_x (10 vp).
        assert_eq!(tracker.move_pointer(1, (103.0, 109.0)), None);
        assert_eq!(
            tracker.decision(7),
            Some((PanDecision::Failed, (100.0, 100.0)))
        );

        // Later moving far horizontally must NOT resurrect a failed candidate.
        assert_eq!(tracker.move_pointer(1, (140.0, 109.0)), None);
        assert_eq!(
            tracker.decision(7),
            Some((PanDecision::Failed, (100.0, 100.0)))
        );
    }

    #[test]
    fn tracker_fails_edge_activation_immediately_outside_edge() {
        let mut tracker = PanTracker::default();
        let mut act = PanActivation::horizontal(Points(10.0), Points(8.0));
        act.edge = Some(nsh_core::PanEdge::Left);
        // Down at x = 80 vp is outside the left edge zone (PanActivation::EDGE_WIDTH = 24 vp).
        tracker.down(1, (80.0, 200.0), (400.0, 800.0), [(9, act)]);
        assert_eq!(
            tracker.decision(9),
            Some((PanDecision::Failed, (80.0, 200.0)))
        );

        // Down at x = 10 vp is inside the left edge zone and activates on rightward drag.
        tracker.down(2, (10.0, 200.0), (400.0, 800.0), [(9, act)]);
        assert_eq!(
            tracker.decision(9),
            Some((PanDecision::Waiting, (10.0, 200.0)))
        );
        assert_eq!(tracker.move_pointer(2, (22.0, 201.0)), Some(9));
        assert_eq!(
            tracker.decision(9),
            Some((PanDecision::Activated, (10.0, 200.0)))
        );
    }

    #[test]
    fn coordinate_helpers_divide_all_px_inputs_consistently_for_offset_subwindows() {
        // Density 2.5x: surface sits at (250 px, 100 px) = (100 vp, 40 vp).
        // Pointer went down at surface-relative (20 vp, 30 vp) = window (300 px, 175 px),
        // then moved by (+50 px, -25 px) = (+20 vp, -10 vp) to window (350 px, 150 px).
        let origin = gesture_origin_surface_vp((350.0, 150.0), (50.0, -25.0), (250.0, 100.0), 2.5);
        assert!((origin.0 - 20.0).abs() < 1e-5);
        assert!((origin.1 - 30.0).abs() < 1e-5);

        let local_down = gesture_local_down_vp((80.0, 60.0), (50.0, -25.0), 2.5);
        assert!((local_down.0 - 12.0).abs() < 1e-5);
        assert!((local_down.1 - 34.0).abs() < 1e-5);

        let tr = gesture_translation_vp((50.0, -25.0), 2.5);
        assert!((tr.0 - 20.0).abs() < 1e-5);
        assert!((tr.1 + 10.0).abs() < 1e-5);

        let vel = gesture_velocity_vp((500.0, -250.0), 2.5);
        assert!((vel.0 - 200.0).abs() < 1e-5);
        assert!((vel.1 + 100.0).abs() < 1e-5);
    }
}
