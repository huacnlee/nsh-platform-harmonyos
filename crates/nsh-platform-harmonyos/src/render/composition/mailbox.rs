//! Synchronized scene-plan mailbox and barrier-preserving command coalescing
//! for the compositor worker.

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlanOp<P> {
    None,
    Set(P),
    Clear,
}

struct PlanState<P> {
    op: PlanOp<P>,
    notified: bool,
}

/// Synchronizes `Set(plan)`, `Clear`, and worker wake notifications under one
/// lock so a redundant `Task::Plan` wakeup never erases a newer `Set(plan)`.
pub(crate) struct SceneMailbox<P> {
    inner: Mutex<PlanState<P>>,
}

impl<P> Default for SceneMailbox<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P> SceneMailbox<P> {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(PlanState {
                op: PlanOp::None,
                notified: false,
            }),
        }
    }

    /// Publishes `Set(plan)` and returns `true` iff the caller must enqueue a
    /// wakeup task for the worker.
    pub(crate) fn set(&self, plan: P) -> bool {
        let mut state = self.inner.lock().unwrap();
        state.op = PlanOp::Set(plan);
        if state.notified {
            false
        } else {
            state.notified = true;
            true
        }
    }

    /// Publishes `Clear` and returns `true` iff the caller must enqueue a
    /// wakeup task for the worker.
    pub(crate) fn clear(&self) -> bool {
        let mut state = self.inner.lock().unwrap();
        state.op = PlanOp::Clear;
        if state.notified {
            false
        } else {
            state.notified = true;
            true
        }
    }

    /// Atomically claims the latest pending operation and resets `notified`.
    pub(crate) fn take(&self) -> PlanOp<P> {
        let mut state = self.inner.lock().unwrap();
        state.notified = false;
        std::mem::replace(&mut state.op, PlanOp::None)
    }
}

/// Coalescing rules for tasks within a single barrier-free tail segment.
pub(crate) trait Coalesce: Sized {
    /// Returns `true` if `self` is an ordering barrier across which updates
    /// must never be merged or reordered.
    fn is_barrier(&self) -> bool;

    /// Attempts to merge `incoming` into `self` in place. When merged, `incoming`
    /// is consumed (`take()`) and `true` is returned.
    fn try_merge(&mut self, incoming: &mut Option<Self>) -> bool;
}

struct QueueState<T> {
    items: VecDeque<T>,
    /// Index of the first item after the most recent barrier task. Only
    /// `items[barrier_watermark..]` may participate in in-place coalescing.
    barrier_watermark: usize,
    senders: usize,
    closed: bool,
}

struct QueueInner<T> {
    state: Mutex<QueueState<T>>,
    ready: Condvar,
}

pub(crate) struct CommandSender<T> {
    inner: Arc<QueueInner<T>>,
}

impl<T> Clone for CommandSender<T> {
    fn clone(&self) -> Self {
        self.inner.state.lock().unwrap().senders += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for CommandSender<T> {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.senders = state.senders.saturating_sub(1);
        if state.senders == 0 {
            state.closed = true;
            self.inner.ready.notify_all();
        }
    }
}

pub(crate) struct CommandReceiver<T> {
    inner: Arc<QueueInner<T>>,
}

impl<T> Drop for CommandReceiver<T> {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap();
        state.closed = true;
        self.inner.ready.notify_all();
    }
}

pub(crate) fn command_channel<T: Coalesce>() -> (CommandSender<T>, CommandReceiver<T>) {
    let inner = Arc::new(QueueInner {
        state: Mutex::new(QueueState {
            items: VecDeque::new(),
            barrier_watermark: 0,
            senders: 1,
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        CommandSender {
            inner: inner.clone(),
        },
        CommandReceiver { inner },
    )
}

impl<T: Coalesce> CommandSender<T> {
    pub(crate) fn send(&self, item: T) -> Result<(), T> {
        let mut state = self.inner.state.lock().unwrap();
        if state.closed {
            return Err(item);
        }
        let was_empty = state.items.is_empty();
        if item.is_barrier() {
            state.items.push_back(item);
            state.barrier_watermark = state.items.len();
        } else {
            let watermark = state.barrier_watermark;
            let mut incoming = Some(item);
            for existing in state.items.range_mut(watermark..) {
                if existing.try_merge(&mut incoming) {
                    return Ok(());
                }
            }
            state
                .items
                .push_back(incoming.expect("unmerged item remains Some"));
        }
        if was_empty {
            self.inner.ready.notify_one();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn queued_len(&self) -> usize {
        self.inner.state.lock().unwrap().items.len()
    }
}

impl<T> CommandReceiver<T> {
    pub(crate) fn recv(&self) -> Result<T, ()> {
        let mut state = self.inner.state.lock().unwrap();
        loop {
            if let Some(item) = state.items.pop_front() {
                state.barrier_watermark = state.barrier_watermark.saturating_sub(1);
                return Ok(item);
            }
            if state.closed {
                return Err(());
            }
            state = self.inner.ready.wait(state).unwrap();
        }
    }

    pub(crate) fn try_recv(&self) -> Option<T> {
        let mut state = self.inner.state.lock().unwrap();
        let item = state.items.pop_front()?;
        state.barrier_watermark = state.barrier_watermark.saturating_sub(1);
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_mailbox_preserves_latest_set_across_clear_and_redundant_wakes() {
        let mailbox = SceneMailbox::new();

        // UI: clear_plan() -> enqueues Plan_1; then plan("B") -> no second Plan needed,
        // or even if an extra wakeup already existed in the queue, "B" is preserved.
        assert!(mailbox.clear());
        assert!(!mailbox.set("B"));

        // Plan_1 consumes Set("B").
        assert_eq!(mailbox.take(), PlanOp::Set("B"));
        // Spurious extra Plan_2 wakeup is a pure no-op (PlanOp::None), NOT Clear!
        assert_eq!(mailbox.take(), PlanOp::None);

        // Set("A") -> Clear -> Set("B") before worker wakes:
        assert!(mailbox.set("A"));
        assert!(!mailbox.clear());
        assert!(!mailbox.set("B"));
        assert_eq!(mailbox.take(), PlanOp::Set("B"));

        // Set("A") -> Clear before worker wakes:
        assert!(mailbox.set("A"));
        assert!(!mailbox.clear());
        assert_eq!(mailbox.take(), PlanOp::Clear);
        assert_eq!(mailbox.take(), PlanOp::None);

        // Producer submits during consumer execution (after take()):
        assert!(mailbox.set("X"));
        assert_eq!(mailbox.take(), PlanOp::Set("X"));
        assert!(mailbox.clear());
        assert!(!mailbox.clear());
        assert_eq!(mailbox.take(), PlanOp::Clear);
    }

    #[derive(Debug, PartialEq, Eq)]
    enum MockTask {
        Slot(u32, u16, i32),
        Frame,
        Capture(u32),
        Drain,
    }

    impl Coalesce for MockTask {
        fn is_barrier(&self) -> bool {
            matches!(self, Self::Capture(_) | Self::Drain)
        }

        fn try_merge(&mut self, incoming: &mut Option<Self>) -> bool {
            match (self, incoming.as_ref()) {
                (Self::Frame, Some(Self::Frame)) => {
                    incoming.take();
                    true
                }
                (Self::Slot(k1, s1, v1), Some(Self::Slot(k2, s2, _))) if k1 == k2 && s1 == s2 => {
                    if let Some(Self::Slot(_, _, next)) = incoming.take() {
                        *v1 = next;
                    }
                    true
                }
                _ => false,
            }
        }
    }

    #[test]
    fn command_queue_coalesces_segments_without_crossing_barriers() {
        let (tx, rx) = command_channel();

        for i in 0..500 {
            tx.send(MockTask::Slot(1, 0, i)).unwrap();
            tx.send(MockTask::Frame).unwrap();
        }
        // 1000 rapid updates collapse into just 2 queued entries.
        assert_eq!(tx.queued_len(), 2);

        // Barrier preserves pre-capture state and prevents post-capture overwrite.
        tx.send(MockTask::Capture(99)).unwrap();
        for i in 500..1000 {
            tx.send(MockTask::Slot(1, 0, i)).unwrap();
        }
        tx.send(MockTask::Drain).unwrap();
        assert_eq!(tx.queued_len(), 5);

        assert_eq!(rx.recv(), Ok(MockTask::Slot(1, 0, 499)));
        assert_eq!(rx.recv(), Ok(MockTask::Frame));
        assert_eq!(rx.recv(), Ok(MockTask::Capture(99)));
        assert_eq!(rx.recv(), Ok(MockTask::Slot(1, 0, 999)));
        assert_eq!(rx.recv(), Ok(MockTask::Drain));
        drop(tx);
        assert_eq!(rx.recv(), Err(()));
    }
}
