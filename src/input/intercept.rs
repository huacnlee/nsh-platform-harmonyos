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
pub(crate) fn typing_text(
    key: Option<nsh_core::keymap::Key>,
    unicode: u32,
    modifiers: nsh_core::keymap::Modifiers,
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
    if let Some(text) = key_text
        && !text.is_empty()
        && text != " "
        && !text.chars().any(char::is_control)
    {
        return Some(text.to_owned());
    }
    if unicode != 0
        && let Some(ch) = char::from_u32(unicode)
        && !ch.is_control()
        && ch != ' '
    {
        return Some(ch.to_string());
    }
    if let Some(nsh_core::keymap::Key::Char(ch)) = key
        && !ch.is_control()
        && ch != ' '
    {
        let ch = if modifiers.shift && ch.is_ascii_alphabetic() {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        return Some(ch.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{SurfaceHit, viewport_px_to_root_vp};

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
            typing_text(Some(Key::Char('a')), 0, Modifiers::default(), None).as_deref(),
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
                Some("á")
            )
            .as_deref(),
            Some("á")
        );
        assert_eq!(
            typing_text(None, '中' as u32, Modifiers::default(), None).as_deref(),
            Some("中")
        );
        assert_eq!(
            typing_text(
                Some(Key::Char(' ')),
                ' ' as u32,
                Modifiers::default(),
                Some(" ")
            ),
            None
        );
        assert_eq!(
            typing_text(Some(Key::Enter), 0, Modifiers::default(), Some("\n")),
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
                Some("a")
            ),
            None
        );
    }
}
