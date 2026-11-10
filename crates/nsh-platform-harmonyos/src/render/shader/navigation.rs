//! One navigation generation shared by the UI owner and its completion message.
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
};

use nsh_core::NodeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) owner: NodeId,
    pub(crate) generation: u64,
}

pub(crate) struct Navigation {
    pub(crate) key: Key,
    pub(crate) backwards: bool,
    complete: Cell<bool>,
}
impl Navigation {
    pub(crate) fn new(owner: NodeId, backwards: bool) -> Option<Self> {
        // Queued replies can outlive an app reload on the same host dispatcher.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        Some(Self {
            key: Key { owner, generation },
            backwards,
            complete: Cell::new(false),
        })
    }
    pub(crate) fn complete(&self, key: Key) -> bool {
        if self.key != key {
            return false;
        }
        self.complete.set(true);
        true
    }
    pub(crate) fn running(&self) -> bool {
        !self.complete.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_completion_cannot_finish_a_reused_owner() {
        let previous = Navigation::new(7, false).unwrap();
        let current = Navigation::new(7, true).unwrap();
        assert!(current.backwards);
        assert!(!current.complete(previous.key));
        assert!(current.running());
        assert!(current.complete(current.key));
        assert!(!current.running());
    }
}
