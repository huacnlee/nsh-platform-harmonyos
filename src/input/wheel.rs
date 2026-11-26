//! Surface-wide wheel and touchpad-pinch arbitration built on `nsh_core::input::WheelLatch`,
//! `wheel_listener`, and `wheel_along`.

use nsh_core::{
    NodeId, PanActivation, PinchPhase, WheelDelta,
    input::{TOUCHPAD_IDLE, WHEEL_LATCH, WheelLatch, WheelStep, wheel_along, wheel_listener},
    keymap::Modifiers,
};

/// Normalized axis action mirroring ArkUI's `UIAxisEventAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxisAction {
    None,
    Begin,
    Update,
    End,
    Cancel,
}

/// Deduplication key for a single native system axis event bubbling across
/// multiple ArkUI nodes in one surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AxisEventKey {
    pub(crate) time_ns: i64,
    pub(crate) source_type: i32,
    pub(crate) tool_type: i32,
    pub(crate) action: i32,
    pub(crate) vx_bits: u64,
    pub(crate) vy_bits: u64,
    pub(crate) pinch_bits: u64,
}

/// Snapshot of a single view in the innermost-first hit chain under the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AxisNode {
    pub(crate) id: NodeId,
    pub(crate) listens_wheel: bool,
    pub(crate) listens_pinch: bool,
    pub(crate) pan_activation: Option<PanActivation>,
    pub(crate) is_native_scroll: bool,
    pub(crate) native_scroll_scrolling: bool,
    pub(crate) native_scroll_zoomable: bool,
}

/// Inputs extracted from a single ArkUI `OnAxis` callback.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AxisInput {
    pub(crate) key: AxisEventKey,
    pub(crate) time_ms: f64,
    pub(crate) action: AxisAction,
    pub(crate) is_mouse: bool,
    pub(crate) vx: f64,
    pub(crate) vy: f64,
    pub(crate) pinch_scale: Option<f32>,
    pub(crate) modifiers: Modifiers,
    pub(crate) scale: f32,
}

/// Event to dispatch for an arbitrated axis input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AxisDispatch {
    Wheel {
        target: NodeId,
        delta: WheelDelta,
    },
    Pinch {
        target: NodeId,
        delta: f32,
        phase: PinchPhase,
    },
}

/// Arbitration result for an ArkUI `OnAxis` callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AxisOutcome {
    pub(crate) dispatch: Option<AxisDispatch>,
    /// Value to pass to `OH_ArkUI_AxisEvent_SetPropagation(propagate)`.
    /// Always explicitly applied because HarmonyOS defaults propagation to `false`.
    pub(crate) propagate: bool,
}

#[derive(Debug, Clone, Copy)]
struct NativeScrollLatch {
    horizontal: bool,
    last_ms: f64,
    in_gesture: bool,
}

/// Per-surface router that decides a unique wheel/pinch target and propagation
/// flag once per system axis event.
#[derive(Debug)]
pub(crate) struct AxisRouter {
    last_event: Option<(AxisEventKey, bool)>,
    wheel_latch: WheelLatch<NodeId>,
    pinch_latch: WheelLatch<NodeId>,
    pinch_scale: f32,
    last_touchpad_wheel_ms: Option<f64>,
    native_scroll: Option<NativeScrollLatch>,
}

impl Default for AxisRouter {
    fn default() -> Self {
        Self {
            last_event: None,
            wheel_latch: WheelLatch::default(),
            pinch_latch: WheelLatch::default(),
            pinch_scale: 1.0,
            last_touchpad_wheel_ms: None,
            native_scroll: None,
        }
    }
}

impl AxisRouter {
    /// Resets active scroll/pinch latches (e.g. on a fresh touch down or surface reset).
    pub(crate) fn clear(&mut self) {
        self.wheel_latch.clear();
        self.pinch_latch.clear();
        self.pinch_scale = 1.0;
        self.last_touchpad_wheel_ms = None;
        self.native_scroll = None;
        self.last_event = None;
    }

    /// Arbitrates a single ArkUI axis event against the innermost-first `chain`
    /// of hit views. Subsequent deliveries of the exact same `AxisEventKey`
    /// return the cached propagation decision without re-emitting an event.
    pub(crate) fn route(
        &mut self,
        input: AxisInput,
        chain: &[AxisNode],
        is_alive: impl Fn(&NodeId) -> bool,
    ) -> AxisOutcome {
        if let Some((last_key, propagate)) = self.last_event
            && last_key == input.key
        {
            return AxisOutcome {
                dispatch: None,
                propagate,
            };
        }

        let outcome = self.route_fresh(input, chain, is_alive);
        self.last_event = Some((input.key, outcome.propagate));
        outcome
    }

    fn route_fresh(
        &mut self,
        input: AxisInput,
        chain: &[AxisNode],
        is_alive: impl Fn(&NodeId) -> bool,
    ) -> AxisOutcome {
        // 1. Native touchpad pinch gesture.
        if let Some(pinch_scale) = input.pinch_scale {
            let (step, phase, delta) = match input.action {
                AxisAction::Begin => {
                    self.pinch_scale = pinch_scale;
                    (WheelStep::Begin, PinchPhase::Start, 0.0)
                }
                AxisAction::End | AxisAction::Cancel => {
                    let prev = std::mem::replace(&mut self.pinch_scale, 1.0);
                    let delta = if prev > 0.0 {
                        pinch_scale / prev - 1.0
                    } else {
                        0.0
                    };
                    (WheelStep::End, PinchPhase::End, delta)
                }
                _ => {
                    let prev = std::mem::replace(&mut self.pinch_scale, pinch_scale);
                    let delta = if prev > 0.0 {
                        pinch_scale / prev - 1.0
                    } else {
                        0.0
                    };
                    let step = if self.pinch_latch.in_gesture() {
                        WheelStep::Continue
                    } else {
                        WheelStep::Begin
                    };
                    (step, PinchPhase::Move, delta)
                }
            };
            let moved = delta != 0.0;
            let picked = self.pinch_latch.route(step, moved, false, is_alive, || {
                wheel_listener(
                    chain.iter().copied(),
                    |n| n.listens_pinch,
                    |n| n.native_scroll_zoomable,
                )
                .map(|(n, takes)| (n.id, takes))
            });
            return match picked {
                Some((target, takes)) => AxisOutcome {
                    dispatch: Some(AxisDispatch::Pinch {
                        target,
                        delta,
                        phase,
                    }),
                    propagate: !takes,
                },
                None => AxisOutcome {
                    dispatch: None,
                    propagate: true,
                },
            };
        }

        // 2. Wheel or Ctrl+wheel.
        let delta_opt = crate::services::desktop::wheel_delta_from_axis(
            input.vx,
            input.vy,
            input.is_mouse,
            input.scale,
            input.modifiers.shift && !input.modifiers.ctrl,
        );
        let moved = delta_opt.is_some();
        let horizontal = delta_opt.is_some_and(|delta| match delta {
            WheelDelta::Lines(p) => p.x.abs() > p.y.abs(),
            WheelDelta::Points(p) => p.x.0.abs() > p.y.0.abs(),
        });

        let step = if input.is_mouse {
            // HarmonyOS wraps mouse wheel notches in Begin/Update/End; map mouse
            // events to `Discrete` so `WheelLatch` holds ownership across ticks.
            WheelStep::Discrete {
                horizontal,
                precise: false,
                time: input.time_ms,
            }
        } else {
            match input.action {
                AxisAction::Begin => {
                    self.last_touchpad_wheel_ms = Some(input.time_ms);
                    WheelStep::Begin
                }
                AxisAction::End | AxisAction::Cancel => {
                    self.last_touchpad_wheel_ms = None;
                    WheelStep::End
                }
                AxisAction::Update | AxisAction::None => {
                    if self.wheel_latch.in_gesture()
                        && self
                            .last_touchpad_wheel_ms
                            .is_some_and(|last| input.time_ms - last > TOUCHPAD_IDLE)
                    {
                        self.wheel_latch.clear();
                    }
                    self.last_touchpad_wheel_ms = Some(input.time_ms);
                    if self.wheel_latch.in_gesture() {
                        WheelStep::Continue
                    } else {
                        WheelStep::Discrete {
                            horizontal,
                            precise: true,
                            time: input.time_ms,
                        }
                    }
                }
            }
        };

        // Check whether a native scroll view already owns this scroll sequence.
        let native_scrolling = self.is_native_scrolling(&input, chain, horizontal);

        // First try `onWheel` listeners (including Ctrl+wheel: upstream gives
        // `onWheel` priority over `onPinch` when a view handles wheel events).
        let wheel_picked = self
            .wheel_latch
            .route(step, moved, native_scrolling, &is_alive, || {
                wheel_listener(
                    chain.iter().copied(),
                    |n| {
                        n.listens_wheel
                            && wheel_along(n.pan_activation, horizontal, input.modifiers.ctrl)
                    },
                    |n| n.is_native_scroll,
                )
                .map(|(n, takes)| (n.id, takes))
            });

        let is_ctrl_mouse_zoom =
            input.is_mouse && input.modifiers.ctrl && input.vy.is_finite() && input.vy != 0.0;

        if let Some((target, takes)) = wheel_picked {
            if !takes {
                self.update_native_scroll_latch(&input, chain, horizontal);
            } else {
                self.native_scroll = None;
            }
            if let Some(delta) = delta_opt {
                return AxisOutcome {
                    dispatch: Some(AxisDispatch::Wheel { target, delta }),
                    propagate: !takes,
                };
            }
            return AxisOutcome {
                dispatch: None,
                propagate: !takes,
            };
        }

        // Fallback: Ctrl+wheel zooms via `onPinch` when no `onWheel` listener took it.
        if is_ctrl_mouse_zoom {
            let zoom_delta =
                crate::services::desktop::ctrl_wheel_pinch_delta((input.vy as f32) / 15.0);
            if zoom_delta != 0.0 {
                let pinch_step = WheelStep::Discrete {
                    horizontal: false,
                    precise: false,
                    time: input.time_ms,
                };
                if let Some((target, takes)) =
                    self.pinch_latch
                        .route(pinch_step, true, false, &is_alive, || {
                            wheel_listener(
                                chain.iter().copied(),
                                |n| n.listens_pinch,
                                |n| n.native_scroll_zoomable,
                            )
                            .map(|(n, takes)| (n.id, takes))
                        })
                {
                    return AxisOutcome {
                        dispatch: Some(AxisDispatch::Pinch {
                            target,
                            delta: zoom_delta,
                            phase: PinchPhase::Move,
                        }),
                        propagate: !takes,
                    };
                }
            }
        }

        self.update_native_scroll_latch(&input, chain, horizontal);
        AxisOutcome {
            dispatch: None,
            propagate: true,
        }
    }

    fn is_native_scrolling(
        &mut self,
        input: &AxisInput,
        chain: &[AxisNode],
        horizontal: bool,
    ) -> bool {
        if chain.iter().any(|n| n.native_scroll_scrolling) {
            return true;
        }
        let Some(latch) = self.native_scroll else {
            return false;
        };
        if input.is_mouse {
            if latch.horizontal == horizontal && input.time_ms - latch.last_ms <= WHEEL_LATCH {
                return true;
            }
            self.native_scroll = None;
            false
        } else {
            if latch.in_gesture && input.time_ms - latch.last_ms <= TOUCHPAD_IDLE {
                return true;
            }
            if !latch.in_gesture && input.time_ms - latch.last_ms <= WHEEL_LATCH {
                return true;
            }
            self.native_scroll = None;
            false
        }
    }

    fn update_native_scroll_latch(
        &mut self,
        input: &AxisInput,
        chain: &[AxisNode],
        horizontal: bool,
    ) {
        if !input.is_mouse && matches!(input.action, AxisAction::End | AxisAction::Cancel) {
            self.native_scroll = None;
            return;
        }
        if chain.iter().any(|n| n.is_native_scroll) {
            self.native_scroll = Some(NativeScrollLatch {
                horizontal,
                last_ms: input.time_ms,
                in_gesture: !input.is_mouse
                    && matches!(input.action, AxisAction::Begin | AxisAction::Update),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::Points;

    use super::*;

    fn key(t: i64, vx: f64, vy: f64) -> AxisEventKey {
        AxisEventKey {
            time_ns: t,
            source_type: 1,
            tool_type: 1,
            action: 2,
            vx_bits: vx.to_bits(),
            vy_bits: vy.to_bits(),
            pinch_bits: 0,
        }
    }

    fn node(id: NodeId) -> AxisNode {
        AxisNode {
            id,
            listens_wheel: false,
            listens_pinch: false,
            pan_activation: None,
            is_native_scroll: false,
            native_scroll_scrolling: false,
            native_scroll_zoomable: false,
        }
    }

    #[test]
    fn horizontal_timeline_inside_vertical_scroll_only_takes_horizontal_wheels() {
        let mut router = AxisRouter::default();
        let mut timeline = node(10);
        timeline.listens_wheel = true;
        timeline.pan_activation = Some(PanActivation::horizontal(Points(6.0), Points(8.0)));
        let mut page = node(2);
        page.is_native_scroll = true;
        let chain = [timeline, page];

        // Vertical wheel passes through to parent scroll (`propagate == true`, no Wheel dispatched).
        let vert = router.route(
            AxisInput {
                key: key(1, 0.0, 30.0),
                time_ms: 10.0,
                action: AxisAction::Update,
                is_mouse: true,
                vx: 0.0,
                vy: 30.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &chain,
            |_| true,
        );
        assert_eq!(vert.dispatch, None);
        assert!(vert.propagate);

        // Horizontal wheel (later notch) is taken by the timeline (`propagate == false`).
        let horiz = router.route(
            AxisInput {
                key: key(2, 30.0, 0.0),
                time_ms: 300.0,
                action: AxisAction::Update,
                is_mouse: true,
                vx: 30.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &chain,
            |_| true,
        );
        assert!(matches!(
            horiz.dispatch,
            Some(AxisDispatch::Wheel { target: 10, .. })
        ));
        assert!(!horiz.propagate);
    }

    #[test]
    fn ctrl_wheel_prefers_wheel_listener_and_falls_back_to_pinch() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let mut router = AxisRouter::default();

        // Both onWheel and onPinch present: onWheel gets Ctrl+wheel first.
        let mut both = node(5);
        both.listens_wheel = true;
        both.listens_pinch = true;
        let res = router.route(
            AxisInput {
                key: key(1, 0.0, -15.0),
                time_ms: 10.0,
                action: AxisAction::Update,
                is_mouse: true,
                vx: 0.0,
                vy: -15.0,
                pinch_scale: None,
                modifiers: ctrl,
                scale: 2.0,
            },
            &[both],
            |_| true,
        );
        assert!(matches!(
            res.dispatch,
            Some(AxisDispatch::Wheel { target: 5, .. })
        ));
        assert!(!res.propagate);

        // Only onPinch present: Ctrl+wheel synthesizes Pinch zoom.
        let mut pinch_only = node(6);
        pinch_only.listens_pinch = true;
        let res2 = router.route(
            AxisInput {
                key: key(2, 0.0, -15.0),
                time_ms: 400.0,
                action: AxisAction::Update,
                is_mouse: true,
                vx: 0.0,
                vy: -15.0,
                pinch_scale: None,
                modifiers: ctrl,
                scale: 2.0,
            },
            &[pinch_only],
            |_| true,
        );
        assert!(matches!(
            res2.dispatch,
            Some(AxisDispatch::Pinch {
                target: 6,
                phase: PinchPhase::Move,
                delta,
            }) if delta > 0.0
        ));
        assert!(!res2.propagate);
    }

    #[test]
    fn touchpad_gesture_stays_latched_when_crossing_sibling_and_releases_when_destroyed() {
        let mut router = AxisRouter::default();
        let mut a = node(10);
        a.listens_wheel = true;
        let mut b = node(20);
        b.listens_wheel = true;

        // Begin on A.
        let start = router.route(
            AxisInput {
                key: key(1, 10.0, 0.0),
                time_ms: 10.0,
                action: AxisAction::Begin,
                is_mouse: false,
                vx: 10.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[a],
            |_| true,
        );
        assert!(matches!(
            start.dispatch,
            Some(AxisDispatch::Wheel { target: 10, .. })
        ));
        assert!(!start.propagate);

        // Pointer drifts onto B mid-gesture: A still owns the gesture.
        let mid = router.route(
            AxisInput {
                key: key(2, 12.0, 0.0),
                time_ms: 26.0,
                action: AxisAction::Update,
                is_mouse: false,
                vx: 12.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[b],
            |_| true,
        );
        assert!(matches!(
            mid.dispatch,
            Some(AxisDispatch::Wheel { target: 10, .. })
        ));
        assert!(!mid.propagate);

        // Same system event bubbling to an ancestor is deduplicated without re-dispatch.
        let dup = router.route(
            AxisInput {
                key: key(2, 12.0, 0.0),
                time_ms: 26.0,
                action: AxisAction::Update,
                is_mouse: false,
                vx: 12.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[b],
            |_| true,
        );
        assert_eq!(dup.dispatch, None);
        assert!(!dup.propagate);

        // A is destroyed: gesture releases cleanly, never emits to dead A, and
        // any listener now under the pointer only observes (`propagate == true`).
        let after_destroy = router.route(
            AxisInput {
                key: key(3, 12.0, 0.0),
                time_ms: 42.0,
                action: AxisAction::Update,
                is_mouse: false,
                vx: 12.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[b],
            |&id| id != 10,
        );
        assert!(matches!(
            after_destroy.dispatch,
            Some(AxisDispatch::Wheel { target: 20, .. })
        ));
        assert!(after_destroy.propagate);

        // Over a non-listener after A is gone, nothing is dispatched and propagation stays true.
        let non_listener = node(30);
        let after_empty = router.route(
            AxisInput {
                key: key(4, 12.0, 0.0),
                time_ms: 58.0,
                action: AxisAction::Update,
                is_mouse: false,
                vx: 12.0,
                vy: 0.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[non_listener],
            |&id| id != 10,
        );
        assert_eq!(after_empty.dispatch, None);
        assert!(after_empty.propagate);
    }

    #[test]
    fn active_native_scroll_prevents_inner_wheel_listener_from_hijacking() {
        let mut router = AxisRouter::default();
        let mut scroll = node(2);
        scroll.is_native_scroll = true;
        scroll.native_scroll_scrolling = true;
        let mut canvas = node(9);
        canvas.listens_wheel = true;

        let res = router.route(
            AxisInput {
                key: key(1, 0.0, 30.0),
                time_ms: 10.0,
                action: AxisAction::Update,
                is_mouse: true,
                vx: 0.0,
                vy: 30.0,
                pinch_scale: None,
                modifiers: Modifiers::default(),
                scale: 2.0,
            },
            &[canvas, scroll],
            |_| true,
        );
        // Canvas only observes; propagation stays true so Scroll keeps scrolling.
        assert!(matches!(
            res.dispatch,
            Some(AxisDispatch::Wheel { target: 9, .. })
        ));
        assert!(res.propagate);
    }
}
