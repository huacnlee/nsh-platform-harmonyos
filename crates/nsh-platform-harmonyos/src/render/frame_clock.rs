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

pub(crate) struct Active<T> {
    positions: Vec<Option<usize>>,
    entries: Vec<(u32, T)>,
}

impl<T> Default for Active<T> {
    fn default() -> Self {
        Self {
            positions: Vec::new(),
            entries: Vec::new(),
        }
    }
}

impl<T> Active<T> {
    pub(crate) fn get(&self, id: u32) -> Option<&T> {
        self.positions
            .get(id as usize)
            .copied()
            .flatten()
            .map(|at| &self.entries[at].1)
    }

    pub(crate) fn insert(&mut self, id: u32, value: T) {
        if let Some(Some(at)) = self.positions.get(id as usize) {
            self.entries[*at].1 = value;
            return;
        }
        self.positions
            .resize(self.positions.len().max(id as usize + 1), None);
        self.positions[id as usize] = Some(self.entries.len());
        self.entries.push((id, value));
    }

    pub(crate) fn remove(&mut self, id: u32) {
        let Some(at) = self.positions.get_mut(id as usize).and_then(Option::take) else {
            return;
        };
        self.entries.swap_remove(at);
        if let Some((moved, _)) = self.entries.get(at) {
            self.positions[*moved as usize] = Some(at);
        }
        while self.positions.last().is_some_and(Option::is_none) {
            self.positions.pop();
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
        active.insert(100_000, 1);
        active.insert(3, 2);
        active.insert(100_000, 3);
        assert_eq!(active.iter().count(), 2);
        active.remove(100_000);
        assert_eq!(active.positions.len(), 4);
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
