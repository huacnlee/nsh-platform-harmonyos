//! Maps native VSync timestamps into the animator's elapsed-time domain. The
//! first frame anchors the domains; callback delivery latency cannot stretch
//! subsequent animation frames. Input between frames uses the elapsed clock.
#[derive(Default)]
pub(crate) struct FrameClock {
    anchor: Option<(u64, f64)>,
    last: f64,
}
impl FrameClock {
    pub(crate) fn sample(&mut self, timestamp: Option<u64>, elapsed: f64) -> f64 {
        let now = if let Some(timestamp) = timestamp.filter(|stamp| *stamp > 0) {
            let (origin, start) = *self.anchor.get_or_insert((timestamp, elapsed));
            start + timestamp.saturating_sub(origin) as f64 / 1_000_000.0
        } else {
            elapsed
        };
        self.last = now.max(self.last);
        self.last
    }
}

/// Native property animations advance on the portion of the timeline that can
/// reach the display. The UI and shader frame timestamps keep their own clocks.
#[derive(Default)]
pub(crate) struct GraphClock {
    holds: usize,
    held_at: Option<f64>,
    excluded: f64,
}
impl GraphClock {
    pub(crate) fn sample(&mut self, now: f64) -> f64 {
        // A stalled graphics producer must not stop animation callbacks on the
        // surface. Only discount the initial handoff, up to 160 ms.
        const MAX_HANDOFF_MS: f64 = 160.0;
        if let Some(start) = self.held_at {
            let hidden = (now - start).clamp(0.0, MAX_HANDOFF_MS);
            if self.holds > 0 {
                return now - hidden - self.excluded;
            }
            self.excluded += hidden;
            self.held_at = None;
        }
        now - self.excluded
    }
    pub(crate) fn hold(&mut self, now: f64) {
        self.held_at.get_or_insert(now);
        self.holds += 1;
    }
    pub(crate) fn release(&mut self) {
        self.holds = self.holds.saturating_sub(1);
    }
}

pub(crate) struct Active<T> {
    positions: rustc_hash::FxHashMap<u32, usize>,
    entries: Vec<(u32, T)>,
}

impl<T> Default for Active<T> {
    fn default() -> Self {
        Self {
            positions: Default::default(),
            entries: Vec::new(),
        }
    }
}

impl<T> Active<T> {
    pub(crate) fn get(&self, id: u32) -> Option<&T> {
        self.positions.get(&id).map(|&at| &self.entries[at].1)
    }

    pub(crate) fn insert(&mut self, id: u32, value: T) {
        if let Some(at) = self.positions.get(&id) {
            self.entries[*at].1 = value;
            return;
        }
        self.positions.insert(id, self.entries.len());
        self.entries.push((id, value));
    }

    pub(crate) fn remove(&mut self, id: u32) {
        let Some(at) = self.positions.remove(&id) else {
            return;
        };
        self.entries.swap_remove(at);
        if let Some((moved, _)) = self.entries.get(at) {
            self.positions.insert(*moved, at);
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|(_, value)| value)
    }

    pub(crate) fn try_retain<E>(
        &mut self,
        mut keep: impl FnMut(&T) -> Result<bool, E>,
    ) -> Result<(), E> {
        let mut at = 0;
        while at < self.entries.len() {
            let (id, value) = &self.entries[at];
            if keep(value)? {
                at += 1;
            } else {
                self.remove(*id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_root_handoff_resumes_from_the_last_visible_animation_frame() {
        let mut clock = GraphClock::default();
        assert_eq!(clock.sample(100.0), 100.0);
        clock.hold(100.0);
        assert_eq!(clock.sample(108.0), 100.0);
        // A replacement may start before the retired presentation is dropped.
        clock.hold(108.0);
        clock.release();
        assert_eq!(clock.sample(132.0), 100.0);
        clock.release();
        assert_eq!(clock.sample(148.0), 100.0);
        assert_eq!(clock.sample(156.0), 108.0);
        clock.hold(156.0);
        clock.release();
        assert_eq!(clock.sample(172.0), 108.0);
        assert_eq!(clock.sample(180.0), 116.0);
    }
    #[test]
    fn a_stalled_root_does_not_block_animation_completion() {
        let mut clock = GraphClock::default();
        clock.hold(10.0);
        assert_eq!(clock.sample(260.0), 100.0);
        clock.release();
        assert_eq!(clock.sample(268.0), 108.0);
    }
    #[test]
    fn delayed_delivery_uses_the_frame_time_and_never_rewinds() {
        let mut clock = FrameClock::default();
        assert_eq!(clock.sample(Some(1_000_000_000), 50.0), 50.0);
        assert_eq!(clock.sample(Some(1_008_000_000), 90.0), 58.0);
        assert_eq!(clock.sample(Some(1_016_000_000), 91.0), 66.0);
        assert_eq!(clock.sample(Some(1_008_000_000), 92.0), 66.0);
    }
    #[test]
    fn visits_only_active_entries_and_repairs_recycled_indices() {
        let mut active = Active::default();
        active.insert(u32::MAX, 1);
        active.insert(3, 2);
        active.insert(u32::MAX, 3);
        assert_eq!(active.positions.len(), 2);
        assert_eq!(active.get(u32::MAX), Some(&3));
        assert_eq!(active.iter().count(), 2);
        active.remove(u32::MAX);
        assert_eq!(active.positions.len(), 1);
        assert_eq!(active.get(3), Some(&2));
        active.insert(4, 4);
        active.try_retain::<()>(|value| Ok(*value == 4)).unwrap();
        assert_eq!(active.get(3), None);
        assert_eq!(active.get(4), Some(&4));
        active.remove(4);
        assert!(active.positions.is_empty());
        assert_eq!(active.iter().count(), 0);
    }
    #[test]
    fn retain_error_keeps_the_failing_entry_and_remaining_work() {
        let mut active = Active::default();
        active.insert(0, 0);
        active.insert(1, 1);
        assert!(
            active
                .try_retain(|value| if *value == 1 { Err(()) } else { Ok(false) })
                .is_err()
        );
        assert_eq!(active.get(0), None);
        assert_eq!(active.get(1), Some(&1));
    }
}
