//! Pure hit-test interception helpers shared with host-side unit tests.

/// Outcome of querying an embedded surface during `OnTouchIntercept`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceHit {
    /// The surface could not be queried safely (re-entrant borrow, invalid scale,
    /// or native query error); keep default hit testing so input is not lost.
    Indeterminate,
    /// App content, a fullscreen video, or the dev error overlay occupies this point.
    Hit,
    /// No hit-testable app content occupies this point (or no runtime/root is mounted).
    Miss,
}

impl SurfaceHit {
    /// Whether `OnTouchIntercept` should set `HitTest::BlockDescendants` on the
    /// surface viewport so the touch reaches siblings underneath.
    pub(crate) fn should_block_descendants(self) -> bool {
        matches!(self, Self::Miss)
    }
}

/// Converts a viewport-local physical-pixel coordinate into root logical `vp`.
pub(crate) fn viewport_px_to_root_vp(point_px: (f32, f32), scale: f32) -> Option<(f32, f32)> {
    if !point_px.0.is_finite() || !point_px.1.is_finite() || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    Some((point_px.0 / scale, point_px.1 / scale))
}

/// Returns the text a key press would type for `captureKeys="text"`, or `None`
/// for Space, non-character keys, or Control/Alt/Command-modified chords.
///
/// When `caps_lock` is `Some(on)`, ASCII letters from OHOS's uppercase keycap
/// unicode values are normalized via `shift ^ on`. When `caps_lock` is `None`
/// (SDK did not expose lock state), the OS-provided logical character is
/// preserved rather than forced to lowercase.
pub(crate) fn typing_text(
    key: Option<nsh_core::keymap::Key>,
    unicode: u32,
    modifiers: nsh_core::keymap::Modifiers,
    caps_lock: Option<bool>,
    key_text: Option<&str>,
) -> Option<String> {
    if modifiers.ctrl || modifiers.alt || modifiers.cmd {
        return None;
    }
    match key {
        Some(nsh_core::keymap::Key::Char(' ')) => return None,
        Some(nsh_core::keymap::Key::Char(_)) | None => {}
        Some(_) => return None,
    }
    if unicode != 0
        && let Some(ch) = char::from_u32(unicode)
        && !ch.is_control()
        && ch != ' '
    {
        // ArkUI's `OH_ArkUI_KeyEvent_GetUnicode` reports uppercase ASCII
        // letters ('A'..'Z') for letter keycodes even when Shift and CapsLock
        // are both off; normalize matching ASCII letter case when lock state is
        // known, and keep the OS character as-is when lock state is unavailable.
        let ch = match (key, caps_lock) {
            (Some(nsh_core::keymap::Key::Char(base)), Some(caps))
                if base.is_ascii_alphabetic() && ch.eq_ignore_ascii_case(&base) =>
            {
                if modifiers.shift ^ caps {
                    base.to_ascii_uppercase()
                } else {
                    base.to_ascii_lowercase()
                }
            }
            _ => ch,
        };
        return Some(ch.to_string());
    }
    let uppercase = modifiers.shift ^ caps_lock.unwrap_or(false);
    if let Some(text) = key_text
        && !text.is_empty()
        && !text.starts_with("KEYCODE_")
        && text != " "
        && !text.chars().any(char::is_control)
        && (!text.is_ascii() || text.len() == 1)
    {
        // ArkUI reports uppercase keycap labels (e.g. "K") in `key_text` even
        // when Shift is not held; fall through to `Key::Char` + shift ^ caps_lock
        // for ASCII letters when `unicode` is unavailable.
        let is_ascii_letter_label = matches!(
            key,
            Some(nsh_core::keymap::Key::Char(ch))
                if ch.is_ascii_alphabetic()
                    && text.len() == 1
                    && text.as_bytes()[0].eq_ignore_ascii_case(&(ch as u8))
        );
        if !is_ascii_letter_label {
            return Some(text.to_owned());
        }
    }
    if let Some(nsh_core::keymap::Key::Char(ch)) = key
        && !ch.is_control()
        && ch != ' '
    {
        let ch = if ch.is_ascii_alphabetic() {
            if uppercase {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            }
        } else {
            ch
        };
        return Some(ch.to_string());
    }
    None
}

/// Returns whether a test surface is ready for initial screen-coordinate input.
///
/// On HarmonyOS desktop (2in1), `FirstBatch` often finishes before SceneBoard
/// finishes activating the floating window (`WINDOW_ACTIVE`); waiting until the
/// window has been visible and focused at least once prevents early taps from
/// being dropped before the window can accept focus, while keeping bounds
/// queries working later when a system dialog temporarily takes focus.
#[cfg(any(feature = "dev", test))]
pub(crate) fn desktop_surface_ready_for_input(
    is_desktop: bool,
    window_state: Option<nsh_core::desktop::WindowState>,
    activated_once: bool,
) -> bool {
    if !is_desktop || activated_once {
        return true;
    }
    window_state.is_some_and(|state| state.visible && state.focused)
}

/// Computes ArkUI `NODE_RESPONSE_REGION` (`[x_vp, y_vp, width_pct, height_pct]`)
/// from a node's frame, `hitSlop`, and optional `clipPath`.
///
/// Returns `None` when neither `hitSlop` nor `clipPath` customizes the node's
/// default hit region.
pub(crate) fn response_region_percent(
    frame_width: f32,
    frame_height: f32,
    slop: nsh_core::EdgeInsets,
    clip_path: Option<nsh_core::ClipInset>,
) -> Option<[f32; 4]> {
    let has_area = frame_width > 0.0 && frame_height > 0.0;
    if !has_area {
        return clip_path.map(|_| [0.0, 0.0, 0.0, 0.0]);
    }
    if clip_path.is_none() && slop == nsh_core::EdgeInsets::default() {
        return None;
    }
    let mut x0 = -slop.left.0;
    let mut y0 = -slop.top.0;
    let mut x1 = frame_width + slop.right.0;
    let mut y1 = frame_height + slop.bottom.0;
    if let Some(clip) = clip_path {
        let (rect, _) = clip.resolve(nsh_core::geometry::size(
            nsh_core::Points(frame_width),
            nsh_core::Points(frame_height),
        ));
        x0 = x0.max(rect.origin.x.0);
        y0 = y0.max(rect.origin.y.0);
        x1 = x1.min(rect.origin.x.0 + rect.size.width.0);
        y1 = y1.min(rect.origin.y.0 + rect.size.height.0);
    }
    let rw = (x1 - x0).max(0.0);
    let rh = (y1 - y0).max(0.0);
    Some([
        x0,
        y0,
        (rw * 100.0) / frame_width,
        (rh * 100.0) / frame_height,
    ])
}

#[cfg(test)]
mod tests {
    use super::{SurfaceHit, response_region_percent, viewport_px_to_root_vp};

    #[test]
    fn only_confirmed_miss_blocks_descendants() {
        assert!(SurfaceHit::Miss.should_block_descendants());
        assert!(!SurfaceHit::Hit.should_block_descendants());
        assert!(!SurfaceHit::Indeterminate.should_block_descendants());
    }

    #[test]
    fn converts_viewport_px_to_root_vp_and_rejects_invalid_inputs() {
        assert_eq!(
            viewport_px_to_root_vp((300.0, 540.0), 2.0),
            Some((150.0, 270.0))
        );
        assert_eq!(viewport_px_to_root_vp((0.0, 0.0), 3.0), Some((0.0, 0.0)));
        assert_eq!(viewport_px_to_root_vp((100.0, 200.0), 0.0), None);
        assert_eq!(viewport_px_to_root_vp((100.0, 200.0), -1.0), None);
        assert_eq!(viewport_px_to_root_vp((100.0, 200.0), f32::NAN), None);
        assert_eq!(viewport_px_to_root_vp((f32::NAN, 200.0), 2.0), None);
        assert_eq!(viewport_px_to_root_vp((100.0, f32::INFINITY), 2.0), None);
    }

    #[test]
    fn typing_text_accepts_printable_keys_and_rejects_space_or_shortcuts() {
        use super::typing_text;
        use nsh_core::keymap::{Key, Modifiers};

        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers::default(),
                Some(false),
                None
            )
            .as_deref(),
            Some("a")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Some(false),
                None
            )
            .as_deref(),
            Some("A")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers {
                    alt_graph: true,
                    ..Modifiers::default()
                },
                Some(false),
                Some("á")
            )
            .as_deref(),
            Some("á")
        );
        assert_eq!(
            typing_text(None, '中' as u32, Modifiers::default(), Some(false), None).as_deref(),
            Some("中")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char(' ')),
                ' ' as u32,
                Modifiers::default(),
                Some(false),
                Some(" ")
            ),
            None
        );
        assert_eq!(
            typing_text(
                Some(Key::Enter),
                0,
                Modifiers::default(),
                Some(false),
                Some("\n")
            ),
            None
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
                Some(false),
                Some("a")
            ),
            None
        );
        // ArkUI delivers hardware key names such as KEYCODE_CTRL_LEFT / KEYCODE_A
        // in key_text; never insert those identifiers verbatim.
        assert_eq!(
            typing_text(
                None,
                0,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_CTRL_LEFT")
            ),
            None
        );
        assert_eq!(
            typing_text(
                None,
                0,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_SHIFT_LEFT")
            ),
            None
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_A")
            )
            .as_deref(),
            Some("a")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Some(false),
                Some("KEYCODE_A")
            )
            .as_deref(),
            Some("A")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('a')),
                'ä' as u32,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_A")
            )
            .as_deref(),
            Some("ä")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char(' ')),
                0,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_SPACE")
            ),
            None
        );
        // ArkUI reports uppercase keycap labels and uppercase ASCII unicode
        // (e.g. 'K' / "K") even when Shift and CapsLock are off; normalize via
        // shift ^ caps_lock when lock state is known.
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                0,
                Modifiers::default(),
                Some(false),
                Some("K")
            )
            .as_deref(),
            Some("k")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                'K' as u32,
                Modifiers::default(),
                Some(false),
                Some("KEYCODE_K")
            )
            .as_deref(),
            Some("k")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                'K' as u32,
                Modifiers::default(),
                Some(true),
                Some("KEYCODE_K")
            )
            .as_deref(),
            Some("K")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                'K' as u32,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Some(true),
                Some("KEYCODE_K")
            )
            .as_deref(),
            Some("k")
        );
        // When SDK does not report Caps Lock (`None`), preserve system unicode case.
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                'K' as u32,
                Modifiers::default(),
                None,
                Some("KEYCODE_K")
            )
            .as_deref(),
            Some("K")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                Some(false),
                Some("K")
            )
            .as_deref(),
            Some("K")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('k')),
                'k' as u32,
                Modifiers::default(),
                Some(false),
                Some("K")
            )
            .as_deref(),
            Some("k")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char('1')),
                0,
                Modifiers::default(),
                Some(false),
                Some("!")
            )
            .as_deref(),
            Some("!")
        );
        assert_eq!(
            typing_text(None, 0, Modifiers::default(), Some(false), Some("VolumeUp")),
            None
        );
    }

    #[test]
    fn desktop_surface_readiness_waits_for_initial_window_activation_only() {
        use super::desktop_surface_ready_for_input;
        use nsh_core::desktop::WindowState;

        let mut unfocused = WindowState::default();
        unfocused.visible = true;
        unfocused.focused = false;
        let mut hidden = WindowState::default();
        hidden.visible = false;
        hidden.focused = true;
        let mut active = WindowState::default();
        active.visible = true;
        active.focused = true;

        assert!(desktop_surface_ready_for_input(false, None, false));
        assert!(desktop_surface_ready_for_input(
            false,
            Some(unfocused),
            false
        ));
        assert!(!desktop_surface_ready_for_input(true, None, false));
        assert!(!desktop_surface_ready_for_input(
            true,
            Some(unfocused),
            false
        ));
        assert!(!desktop_surface_ready_for_input(true, Some(hidden), false));
        assert!(desktop_surface_ready_for_input(true, Some(active), false));
        // Once activated at least once, temporarily losing focus (e.g. system
        // AlertDialog / ActionSheet) must not block surface geometry queries.
        assert!(desktop_surface_ready_for_input(true, Some(unfocused), true));
        assert!(desktop_surface_ready_for_input(true, None, true));
    }

    #[test]
    fn response_region_percent_combines_hit_slop_and_clip_path() {
        use nsh_core::{ClipInset, EdgeInsets, Points, geometry::Percentage};

        assert_eq!(
            response_region_percent(200.0, 100.0, EdgeInsets::default(), None),
            None
        );
        assert_eq!(
            response_region_percent(0.0, 0.0, EdgeInsets::all(Points(8.0)), None),
            None
        );
        let slop_region =
            response_region_percent(100.0, 50.0, EdgeInsets::all(Points(10.0)), None).unwrap();
        assert!((slop_region[0] - (-10.0)).abs() < 1e-3);
        assert!((slop_region[1] - (-10.0)).abs() < 1e-3);
        assert!((slop_region[2] - 120.0).abs() < 1e-3);
        assert!((slop_region[3] - 140.0).abs() < 1e-3);
        let right_only = ClipInset::new().left(Percentage(0.65));
        let region =
            response_region_percent(260.0, 68.0, EdgeInsets::default(), Some(right_only)).unwrap();
        assert!((region[0] - 169.0).abs() < 1e-3);
        assert!((region[1] - 0.0).abs() < 1e-3);
        assert!((region[2] - 35.0).abs() < 1e-3);
        assert!((region[3] - 100.0).abs() < 1e-3);

        let inset_box = ClipInset::new()
            .top(Points(24.0))
            .right(Points(40.0))
            .bottom(Points(24.0))
            .left(Points(40.0));
        assert_eq!(
            response_region_percent(200.0, 100.0, EdgeInsets::all(Points(12.0)), Some(inset_box)),
            Some([40.0, 24.0, 60.0, 52.0])
        );

        let collapsed = ClipInset::new().left(Percentage(1.0));
        let empty =
            response_region_percent(200.0, 100.0, EdgeInsets::default(), Some(collapsed)).unwrap();
        assert_eq!(empty[2], 0.0);
        assert_eq!(
            response_region_percent(0.0, 0.0, EdgeInsets::default(), Some(inset_box)),
            Some([0.0, 0.0, 0.0, 0.0])
        );
    }
}
