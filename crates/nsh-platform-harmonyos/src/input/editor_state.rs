//! Controlled editor values and focus acknowledgements, independent of SDK objects.

#[derive(Default)]
pub(crate) struct State {
    pub(crate) text: String,
    pub(crate) count: u32,
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
        Some(count)
    }

    pub(crate) fn focus(&mut self, focused: bool) -> bool {
        let changed = self.focused != focused;
        self.focused = focused;
        changed
    }
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
}
