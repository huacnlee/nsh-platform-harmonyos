//! Controlled editor values, focus acknowledgements, and Unicode-safe edit
//! planning shared between the OHOS runtime and host unit tests.

use nsh_core::text_edit::{byte_to_utf16, char_len, clip_chars, pasted, utf16_to_byte};

#[derive(Default)]
pub(crate) struct State {
    pub(crate) text: String,
    pub(crate) count: u32,
    pub(crate) focus_gen: u64,
    pub(crate) edit_gen: u64,
    focused: bool,
    composing: bool,
    deferred: Option<(String, u32)>,
}

impl State {
    pub(crate) fn controlled(&mut self, text: String, count: u32) -> bool {
        if count != self.count {
            return false;
        }
        if self.composing {
            self.deferred = Some((text, count));
            return false;
        }
        if self.text == text {
            return false;
        }
        self.text = text;
        self.edit_gen = self.edit_gen.wrapping_add(1);
        true
    }

    pub(crate) fn composition(&mut self, composing: bool) -> bool {
        self.composing = composing;
        if !composing && let Some((text, count)) = self.deferred.take() {
            return self.controlled(text, count);
        }
        false
    }

    pub(crate) fn changed(&mut self, text: String) -> Option<u32> {
        if self.text == text {
            return None;
        }
        let count = self.count.checked_add(1)?;
        self.text = text;
        self.count = count;
        self.edit_gen = self.edit_gen.wrapping_add(1);
        Some(count)
    }

    pub(crate) fn focus(&mut self, focused: bool) -> bool {
        let changed = self.focused != focused;
        if changed {
            self.focused = focused;
            self.focus_gen = self.focus_gen.wrapping_add(1);
        }
        changed
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused
    }
}

/// Result of planning a Unicode-aware insertion or replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedEdit {
    pub(crate) text: String,
    pub(crate) clipped: String,
    pub(crate) caret: u32,
    pub(crate) changed: bool,
}

/// Plans replacing `[start_utf16, end_utf16]` in `current` with `inserted`
/// using `nsh_core::text_edit`'s Unicode scalar counting and grapheme-cluster
/// boundary clipping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_insert(
    current: &str,
    start_utf16: u32,
    end_utf16: u32,
    inserted: &str,
    max_length: Option<u32>,
    multiline: bool,
    normalize_paste: bool,
    digits_only: bool,
) -> PlannedEdit {
    let raw_start = utf16_to_byte(current, start_utf16);
    let raw_end = utf16_to_byte(current, end_utf16);
    let (start_b, end_b) = if raw_start <= raw_end {
        (raw_start, raw_end)
    } else {
        (raw_end, raw_start)
    };
    let normalized = if normalize_paste {
        pasted(inserted, multiline)
    } else {
        inserted.to_owned()
    };
    let filtered = if digits_only {
        normalized
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>()
    } else {
        normalized
    };
    let prefix = &current[..start_b];
    let suffix = &current[end_b..];
    let clipped = match max_length {
        Some(max) => {
            let kept = char_len(prefix) + char_len(suffix);
            let room = (max as usize).saturating_sub(kept);
            clip_chars(&filtered, room)
        }
        None => filtered.as_str(),
    };
    let mut text = String::with_capacity(prefix.len() + clipped.len() + suffix.len());
    text.push_str(prefix);
    text.push_str(clipped);
    text.push_str(suffix);
    let caret = byte_to_utf16(&text, start_b + clipped.len());
    let changed = text != current;
    PlannedEdit {
        text,
        clipped: clipped.to_owned(),
        caret,
        changed,
    }
}

/// Plans clamping a native candidate replacement (`previous -> candidate`) to
/// `max_length` Unicode scalars on grapheme boundaries, preserving existing
/// prefix/suffix text around the edited span.
pub(crate) fn plan_replace(
    previous: &str,
    candidate: &str,
    max_length: Option<u32>,
    multiline: bool,
    digits_only: bool,
) -> PlannedEdit {
    let Some(max) = max_length else {
        let caret = byte_to_utf16(candidate, candidate.len());
        return PlannedEdit {
            text: candidate.to_owned(),
            clipped: String::new(),
            caret,
            changed: candidate != previous,
        };
    };
    if char_len(candidate) <= max as usize {
        let caret = byte_to_utf16(candidate, candidate.len());
        return PlannedEdit {
            text: candidate.to_owned(),
            clipped: String::new(),
            caret,
            changed: candidate != previous,
        };
    }
    // Find longest common scalar prefix and suffix between `previous` and `candidate`.
    let mut prefix_b = 0usize;
    for (a, b) in previous.chars().zip(candidate.chars()) {
        if a != b {
            break;
        }
        prefix_b += a.len_utf8();
    }
    let prev_rest = &previous[prefix_b..];
    let cand_rest = &candidate[prefix_b..];
    let mut suffix_b = 0usize;
    for (a, b) in prev_rest.chars().rev().zip(cand_rest.chars().rev()) {
        if a != b {
            break;
        }
        suffix_b += a.len_utf8();
    }
    let start_utf16 = byte_to_utf16(previous, prefix_b);
    let end_utf16 = byte_to_utf16(previous, previous.len() - suffix_b);
    let inserted = &candidate[prefix_b..candidate.len() - suffix_b];
    plan_insert(
        previous,
        start_utf16,
        end_utf16,
        inserted,
        Some(max),
        multiline,
        false,
        digits_only,
    )
}

/// Selection offsets use UTF-16; never split a surrogate pair in a native command.
pub(crate) fn selection(text: &str, start: u32, end: u32) -> (i32, i32) {
    fn boundary(text: &str, wanted: u32) -> i32 {
        let mut units = 0_u32;
        for character in text.chars() {
            let next = units.saturating_add(character.len_utf16() as u32);
            if next > wanted {
                break;
            }
            units = next;
        }
        units.min(i32::MAX as u32) as i32
    }
    let start = boundary(text, start);
    (start, boundary(text, end).max(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controlled_values_require_the_current_native_count() {
        let mut state = State::default();
        assert_eq!(state.changed("native".into()), Some(1));
        assert!(!state.controlled("stale".into(), 0));
        assert_eq!(state.text, "native");
        assert!(state.controlled("accepted".into(), 1));
        assert_eq!(state.count, 1);
        assert_eq!(state.changed("accepted".into()), None);
    }

    #[test]
    fn composition_holds_controlled_values_and_rejects_stale_deferred_edits() {
        let mut state = State::default();
        state.changed("preedit".into());
        state.composition(true);
        assert!(!state.controlled("PREEDIT".into(), 1));
        assert_eq!(state.text, "preedit");
        assert!(state.composition(false));
        assert_eq!(state.text, "PREEDIT");
        state.composition(true);
        assert!(!state.controlled("old".into(), 1));
        state.changed("committed".into());
        assert!(!state.composition(false));
        assert_eq!(state.text, "committed");
        state.composition(true);
        state.controlled("obsolete".into(), 2);
        state.controlled("committed".into(), 2);
        assert!(!state.composition(false));
        assert_eq!(state.text, "committed");
    }

    #[test]
    fn focus_is_deduplicated() {
        let mut state = State::default();
        assert!(state.focus(true));
        assert!(!state.focus(true));
        assert!(state.focus(false));
        assert!(!state.focus(false));
    }

    #[test]
    fn selection_preserves_non_bmp_utf16_boundaries() {
        assert_eq!(selection("a😀b", 1, 3), (1, 3));
        assert_eq!(selection("a😀b", 2, 4), (1, 4));
        assert_eq!(selection("a😀b", 99, 100), (4, 4));
    }

    #[test]
    fn plan_insert_counts_unicode_scalars_and_preserves_grapheme_clusters() {
        // Empty box with maxLength=1 accepts a full astral emoji (2 UTF-16 code units).
        let emoji = plan_insert("", 0, 0, "😀", Some(1), false, false, false);
        assert_eq!(emoji.text, "😀");
        assert_eq!(emoji.clipped, "😀");
        assert_eq!(emoji.caret, 2);
        assert!(emoji.changed);

        // Empty box with maxLength=1 accepts a full CJK Extension B ideograph.
        let ext_b = plan_insert("", 0, 0, "𠀀", Some(1), false, false, false);
        assert_eq!(ext_b.text, "𠀀");
        assert_eq!(ext_b.caret, 2);

        // Multi-scalar clusters (combining mark, skin-tone emoji, ZWJ family) are
        // dropped as a whole rather than split when they do not fit.
        let combining = plan_insert("", 0, 0, "e\u{0301}", Some(1), false, false, false);
        assert_eq!(combining.text, "");
        assert!(!combining.changed);

        let skin_tone = plan_insert("", 0, 0, "👍🏽", Some(1), false, false, false);
        assert_eq!(skin_tone.text, "");
        assert!(!skin_tone.changed);

        let zwj = plan_insert("a", 1, 1, "👨‍👩‍👧‍👦", Some(3), false, false, false);
        assert_eq!(zwj.text, "a");
        assert!(!zwj.changed);

        // Replacing an existing emoji selection computes room from remaining text.
        let replaced = plan_insert("a😀b", 1, 3, "🚀✨", Some(3), false, false, false);
        assert_eq!(replaced.text, "a🚀b");
        assert_eq!(replaced.clipped, "🚀");
        assert_eq!(replaced.caret, 3);

        // Single-line paste normalizes CRLF/LF into spaces before clipping.
        let single_paste = plan_insert("", 0, 0, "hi\r\nthere\n", Some(8), false, true, false);
        assert_eq!(single_paste.text, "hi there");
        assert_eq!(single_paste.caret, 8);

        // Multiline paste preserves `\n`.
        let multi_paste = plan_insert("", 0, 0, "hi\r\nthere", Some(8), true, true, false);
        assert_eq!(multi_paste.text, "hi\nthere");
    }

    #[test]
    fn plan_replace_clips_native_ime_commit_at_grapheme_boundary() {
        let planned = plan_replace("a", "a你好世界", Some(3), false, false);
        assert_eq!(planned.text, "a你好");
        assert_eq!(planned.clipped, "你好");
        assert_eq!(planned.caret, 3);
        assert!(planned.changed);

        // Inserting into the middle preserves the trailing text.
        let mid = plan_replace("ab", "a😀🚀b", Some(3), false, false);
        assert_eq!(mid.text, "a😀b");
        assert_eq!(mid.caret, 3);

        // Already within scalar limit even if UTF-16 code units exceed maxLength.
        let ok_astral = plan_replace("", "😀", Some(1), false, false);
        assert_eq!(ok_astral.text, "😀");
        assert_eq!(ok_astral.caret, 2);
    }
}
