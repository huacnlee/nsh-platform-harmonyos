//! Native playback controls remain visible while paused and expire while playing.
#[derive(Clone, Copy)]
pub(super) struct Controls {
    pub(super) visible: bool,
    hide_at: Option<f64>,
}
impl Default for Controls {
    fn default() -> Self {
        Self {
            visible: true,
            hide_at: None,
        }
    }
}
impl Controls {
    pub(super) fn show(&mut self, playing: bool, now: f64) {
        self.visible = true;
        self.hide_at = playing.then_some(now + 3000.0);
    }
    pub(super) fn toggle(&mut self, playing: bool, now: f64) {
        if self.visible {
            self.visible = false;
            self.hide_at = None;
        } else {
            self.show(playing, now);
        }
    }
    pub(super) fn deadline(&self) -> Option<f64> {
        self.hide_at
    }
    pub(super) fn expire(&mut self, deadline: f64, now: f64) -> bool {
        if self.hide_at == Some(deadline) && now >= deadline {
            self.hide_at = None;
            self.visible = false;
            return true;
        }
        false
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paused_controls_stay_visible_and_playing_controls_expire() {
        let mut controls = Controls::default();
        controls.show(false, 0.0);
        assert_eq!(controls.deadline(), None);
        assert!(controls.visible);
        controls.show(true, 60_000.0);
        assert!(!controls.expire(63_000.0, 62_999.0));
        assert!(controls.expire(63_000.0, 63_000.0));
        assert!(!controls.visible);
    }
    #[test]
    fn tapping_reveals_for_a_fresh_interval_and_can_hide_immediately() {
        let mut controls = Controls::default();
        controls.toggle(true, 0.0);
        assert!(!controls.visible);
        controls.toggle(true, 1000.0);
        assert!(controls.visible);
        assert!(!controls.expire(4000.0, 3999.0));
        assert!(controls.expire(4000.0, 4000.0));
        controls.toggle(false, 5000.0);
        assert!(controls.visible);
        assert!(!controls.expire(4000.0, 10000.0));
    }
    #[test]
    fn posted_expiration_cannot_hide_refreshed_or_paused_controls() {
        let mut controls = Controls::default();
        controls.show(true, 0.0);
        controls.show(true, 2000.0);
        assert!(!controls.expire(3000.0, 4000.0));
        assert!(controls.visible);
        assert_eq!(controls.deadline(), Some(5000.0));
        controls.show(false, 4500.0);
        assert!(!controls.expire(5000.0, 6000.0));
        assert!(controls.visible);
    }
}
