//! Cold-start first-frame buffering gate and atomic buffering-to-live handoff.
//!
//! When a fresh `NshSurface` loads its first app before `ContentSlot` attaches
//! to ArkUI, the engine starts immediately with the window's dimensions and
//! initial `safeArea` while [`MountGate`] buffers the initial `MountBatch`es.
//! The UI thread waits up to a bounded cold-start budget for either the first
//! `MountBatch` or the end of the initial boot turn (`FlushGuard`), then
//! atomically switches the gate to `Live` and synchronously mounts the buffered
//! batch(es) into `NodeContent` before ArkUI builds Frame 1.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use nsh_core::MountBatch;

/// Absolute cold-start deadline shared across resource read, engine boot, and
/// initial first-frame mount wait so the slow path never gets a second budget.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColdDeadline {
    started_at: Instant,
    deadline: Instant,
}

impl ColdDeadline {
    pub(crate) fn new(budget: Duration) -> Self {
        let started_at = Instant::now();
        Self {
            started_at,
            deadline: started_at + budget,
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// Outcome of waiting on [`MountGate::wait_first_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitReady {
    /// The engine submitted its first `MountBatch` within the budget.
    FirstBatch,
    /// The initial boot pass settled (or the gate closed) without a batch
    /// (for example an async mount or a fatal startup error).
    BootFlushed,
    /// The cold-start budget elapsed; attach live so future batches stream in.
    TimedOut,
}

/// Buffered engine outputs captured while running before a live `Dispatcher`
/// and `Mounter` are attached.
#[derive(Default)]
pub(crate) struct BufferedOutput {
    pub(crate) batches: VecDeque<MountBatch>,
    pub(crate) errors: Vec<String>,
    pub(crate) frame_requested: bool,
    pub(crate) pending_chunks: Vec<String>,
    #[cfg(feature = "dev")]
    pub(crate) hot_failed: bool,
}

enum Mode<D> {
    Buffering(BufferedOutput),
    Live(D),
    Closed,
}

struct GateState<D> {
    mode: Mode<D>,
    first_batch_seen: bool,
    boot_settled: bool,
}

/// Thread-safe handoff gate between `"nsh-engine"` (`MountSink`) and the UI
/// thread (`Runtime`).
///
/// While in `Buffering` mode, batches, errors, frame requests and deferred chunk
/// requests accumulate in strict FIFO order. Calling [`Self::attach`] atomically
/// switches the gate to `Live(D)` under the mutex and returns the buffered
/// items so the UI thread applies them synchronously before processing any live
/// TSFN messages posted afterwards.
pub(crate) struct MountGate<D> {
    state: Mutex<GateState<D>>,
    ready: Condvar,
}

impl<D> MountGate<D> {
    pub(crate) fn new_buffering() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                mode: Mode::Buffering(BufferedOutput::default()),
                first_batch_seen: false,
                boot_settled: false,
            }),
            ready: Condvar::new(),
        })
    }

    pub(crate) fn new_live(target: D) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                mode: Mode::Live(target),
                first_batch_seen: false,
                boot_settled: false,
            }),
            ready: Condvar::new(),
        })
    }

    pub(crate) fn on_submit(&self, batch: MountBatch, send_live: impl FnOnce(&D, MountBatch)) {
        let mut wake = false;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.first_batch_seen {
                state.first_batch_seen = true;
                wake = true;
            }
            match &mut state.mode {
                Mode::Buffering(buffered) => buffered.batches.push_back(batch),
                Mode::Live(target) => send_live(target, batch),
                Mode::Closed => {}
            }
        }
        if wake {
            self.ready.notify_all();
        }
    }

    pub(crate) fn on_error(&self, message: String, send_live: impl FnOnce(&D, String)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut state.mode {
            Mode::Buffering(buffered) => buffered.errors.push(message),
            Mode::Live(target) => send_live(target, message),
            Mode::Closed => {}
        }
    }

    pub(crate) fn on_request_frame(&self, send_live: impl FnOnce(&D)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut state.mode {
            Mode::Buffering(buffered) => buffered.frame_requested = true,
            Mode::Live(target) => send_live(target),
            Mode::Closed => {}
        }
    }

    #[cfg(feature = "dev")]
    pub(crate) fn on_hot_failed(&self, send_live: impl FnOnce(&D)) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut state.mode {
            Mode::Buffering(buffered) => buffered.hot_failed = true,
            Mode::Live(target) => send_live(target),
            Mode::Closed => {}
        }
    }

    pub(crate) fn on_load_chunk(
        &self,
        id: &str,
        load_buffering: impl FnOnce(&str) -> bool,
        load_live: impl FnOnce(&D, &str) -> bool,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut state.mode {
            Mode::Buffering(buffered) => {
                if load_buffering(id) {
                    true
                } else {
                    buffered.pending_chunks.push(id.to_owned());
                    true
                }
            }
            Mode::Live(target) => load_live(target, id),
            Mode::Closed => false,
        }
    }

    /// Signals that the engine finished its initial boot pass (either via
    /// `EngineControl::flush` or early engine termination). Guarantees any
    /// bounded waiter wakes immediately even if the bundle threw a fatal error
    /// or deferred `mount()` behind an async call.
    pub(crate) fn on_boot_flushed(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.boot_settled = true;
        }
        self.ready.notify_all();
    }

    /// Waits up to `timeout` for the engine's first `MountBatch` or the end of
    /// its initial synchronous boot pass.
    #[allow(dead_code)]
    pub(crate) fn wait_first_frame(&self, timeout: Duration) -> WaitReady {
        self.wait_first_frame_until(ColdDeadline::new(timeout))
    }

    /// Waits until `deadline` for the engine's first `MountBatch` or the end of
    /// its initial synchronous boot pass. Returns immediately when `deadline`
    /// has already elapsed without blocking, while still observing a batch or
    /// flush that is already ready.
    pub(crate) fn wait_first_frame_until(&self, deadline: ColdDeadline) -> WaitReady {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.first_batch_seen {
                return WaitReady::FirstBatch;
            }
            if state.boot_settled || matches!(state.mode, Mode::Closed) {
                return WaitReady::BootFlushed;
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return WaitReady::TimedOut;
            }
            let (guard, result) = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            if result.timed_out()
                && !state.first_batch_seen
                && !state.boot_settled
                && !matches!(state.mode, Mode::Closed)
            {
                return WaitReady::TimedOut;
            }
        }
    }

    /// Atomically transitions from `Buffering` to `Live(target)` and returns
    /// everything buffered so far.
    pub(crate) fn attach(&self, target: D) -> Option<BufferedOutput> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(state.mode, Mode::Buffering(_)) {
            return None;
        }
        let Mode::Buffering(buffered) = std::mem::replace(&mut state.mode, Mode::Live(target))
        else {
            unreachable!()
        };
        Some(buffered)
    }

    /// Closes the gate before attachment, waking any waiter.
    pub(crate) fn close(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.mode = Mode::Closed;
        }
        self.ready.notify_all();
    }
}

/// Ensures [`MountGate::on_boot_flushed`] runs exactly once whether
/// `EngineControl::flush` completes normally or the engine exits early and
/// drops the flush closure.
pub(crate) struct FlushGuard<D>(Option<Arc<MountGate<D>>>);

impl<D> FlushGuard<D> {
    pub(crate) fn new(gate: Arc<MountGate<D>>) -> Self {
        Self(Some(gate))
    }

    pub(crate) fn finish(mut self) {
        if let Some(gate) = self.0.take() {
            gate.on_boot_flushed();
        }
    }
}

impl<D> Drop for FlushGuard<D> {
    fn drop(&mut self) {
        if let Some(gate) = self.0.take() {
            gate.on_boot_flushed();
        }
    }
}

struct ReadSlotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// Single-value handoff supporting a bounded synchronous cold-start wait with
/// seamless async fallback if the blocking read exceeds the cold-start budget.
pub(crate) struct ColdReadSlot<T> {
    state: Mutex<ReadSlotState<T>>,
    ready: Condvar,
}

impl<T> ColdReadSlot<T> {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReadSlotState {
                value: None,
                waker: None,
            }),
            ready: Condvar::new(),
        })
    }

    pub(crate) fn complete(&self, value: T) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.value = Some(value);
            state.waker.take()
        };
        self.ready.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> Option<T> {
        self.wait_until(ColdDeadline::new(timeout))
    }

    pub(crate) fn wait_until(&self, deadline: ColdDeadline) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(value) = state.value.take() {
                return Some(value);
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return None;
            }
            let (guard, result) = self
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            if let Some(value) = state.value.take() {
                return Some(value);
            }
            if result.timed_out() {
                return None;
            }
        }
    }

    pub(crate) fn into_future(self: Arc<Self>) -> ColdReadFuture<T> {
        ColdReadFuture(self)
    }
}

pub(crate) struct ColdReadFuture<T>(Arc<ColdReadSlot<T>>);

impl<T> Future for ColdReadFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = state.value.take() {
            Poll::Ready(value)
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    fn empty_batch() -> MountBatch {
        MountBatch::default()
    }

    #[test]
    fn buffering_preserves_order_and_switches_atomically_to_live() {
        let gate = MountGate::<Arc<StdMutex<Vec<&'static str>>>>::new_buffering();
        let live = Arc::new(StdMutex::new(Vec::new()));

        gate.on_submit(empty_batch(), |_, _| panic!("must buffer"));
        gate.on_error("warn".into(), |_, _| panic!("must buffer"));
        gate.on_request_frame(|_| panic!("must buffer"));
        assert!(gate.on_load_chunk("lazy.js", |_| false, |_, _| false));
        gate.on_submit(empty_batch(), |_, _| panic!("must buffer"));

        assert_eq!(
            gate.wait_first_frame(Duration::from_millis(10)),
            WaitReady::FirstBatch
        );

        let buffered = gate.attach(live.clone()).expect("buffered output");
        assert_eq!(buffered.batches.len(), 2);
        assert_eq!(buffered.errors, vec!["warn".to_owned()]);
        assert!(buffered.frame_requested);
        assert_eq!(buffered.pending_chunks, vec!["lazy.js".to_owned()]);
        assert!(gate.attach(live.clone()).is_none());

        gate.on_submit(empty_batch(), |target, _| {
            target.lock().unwrap().push("batch")
        });
        gate.on_error("err".into(), |target, _| {
            target.lock().unwrap().push("error")
        });
        gate.on_request_frame(|target| target.lock().unwrap().push("frame"));
        assert!(gate.on_load_chunk(
            "more.js",
            |_| panic!("must be live"),
            |target, _| {
                target.lock().unwrap().push("chunk");
                true
            }
        ));
        assert_eq!(
            *live.lock().unwrap(),
            vec!["batch", "error", "frame", "chunk"]
        );
    }

    #[test]
    fn wait_first_frame_wakes_when_engine_thread_submits() {
        let gate = MountGate::<()>::new_buffering();
        let producer = gate.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.on_submit(empty_batch(), |_, _| {});
        });

        assert_eq!(
            gate.wait_first_frame(Duration::from_secs(1)),
            WaitReady::FirstBatch
        );
        handle.join().unwrap();
    }

    #[test]
    fn flush_guard_wakes_waiter_immediately_when_no_batch_is_submitted() {
        let gate = MountGate::<()>::new_buffering();
        let guard = FlushGuard::new(gate.clone());
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            drop(guard);
        });

        assert_eq!(
            gate.wait_first_frame(Duration::from_secs(1)),
            WaitReady::BootFlushed
        );
        handle.join().unwrap();
    }

    #[test]
    fn wait_first_frame_times_out_cleanly_and_still_attaches_live() {
        let gate = MountGate::<Arc<StdMutex<usize>>>::new_buffering();
        assert_eq!(
            gate.wait_first_frame(Duration::from_millis(10)),
            WaitReady::TimedOut
        );

        let delivered = Arc::new(StdMutex::new(0usize));
        let buffered = gate
            .attach(delivered.clone())
            .expect("attach after timeout");
        assert!(buffered.batches.is_empty());

        gate.on_submit(empty_batch(), |count, _| *count.lock().unwrap() += 1);
        assert_eq!(*delivered.lock().unwrap(), 1);
    }

    #[test]
    fn close_wakes_waiter_and_drops_subsequent_batches() {
        let gate = MountGate::<()>::new_buffering();
        let closer = gate.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            closer.close();
        });

        assert_eq!(
            gate.wait_first_frame(Duration::from_secs(1)),
            WaitReady::BootFlushed
        );
        handle.join().unwrap();
        gate.on_submit(empty_batch(), |_, _| panic!("closed gate must drop"));
        assert!(gate.attach(()).is_none());
    }

    #[test]
    fn cold_read_slot_supports_fast_sync_wait_and_async_fallback() {
        let fast = ColdReadSlot::<u32>::new();
        let producer = fast.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            producer.complete(42);
        });
        assert_eq!(fast.wait_timeout(Duration::from_secs(1)), Some(42));
        handle.join().unwrap();

        let slow = ColdReadSlot::<u32>::new();
        assert_eq!(slow.wait_timeout(Duration::from_millis(5)), None);
        let producer = slow.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            producer.complete(99);
        });
        let value = poll_ready(slow.into_future());
        assert_eq!(value, 99);
        handle.join().unwrap();
    }

    #[test]
    fn shared_cold_deadline_never_grants_a_second_budget_on_slow_path() {
        // 1. Resource consumes the whole budget -> deadline expires -> first-frame
        //    wait must return TimedOut immediately without blocking again.
        let deadline = ColdDeadline::new(Duration::from_millis(15));
        let slot = ColdReadSlot::<u32>::new();
        assert_eq!(slot.wait_until(deadline), None);
        assert!(deadline.is_expired());

        let gate = MountGate::<()>::new_buffering();
        let wait_start = Instant::now();
        assert_eq!(gate.wait_first_frame_until(deadline), WaitReady::TimedOut);
        assert!(
            wait_start.elapsed() < Duration::from_millis(10),
            "expired deadline must not block again"
        );

        // 2. If a batch was already submitted before an expired deadline check,
        //    wait_first_frame_until still reports FirstBatch without blocking.
        gate.on_submit(empty_batch(), |_, _| {});
        assert_eq!(gate.wait_first_frame_until(deadline), WaitReady::FirstBatch);

        // 3. Fast resource leaves remaining budget for first-frame wait.
        let fast_deadline = ColdDeadline::new(Duration::from_millis(80));
        let fast_slot = ColdReadSlot::<u32>::new();
        fast_slot.complete(7);
        assert_eq!(fast_slot.wait_until(fast_deadline), Some(7));
        assert!(fast_deadline.remaining() > Duration::ZERO);
        let fast_gate = MountGate::<()>::new_buffering();
        let producer = fast_gate.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.on_submit(empty_batch(), |_, _| {});
        });
        assert_eq!(
            fast_gate.wait_first_frame_until(fast_deadline),
            WaitReady::FirstBatch
        );
        handle.join().unwrap();
    }

    fn poll_ready<F: Future>(future: F) -> F::Output {
        struct Notify(Mutex<bool>, Condvar);
        impl std::task::Wake for Notify {
            fn wake(self: Arc<Self>) {
                *self.0.lock().unwrap() = true;
                self.1.notify_all();
            }
        }
        let notify = Arc::new(Notify(Mutex::new(false), Condvar::new()));
        let waker = Waker::from(notify.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => {
                    let mut flag = notify.0.lock().unwrap();
                    while !*flag {
                        flag = notify.1.wait(flag).unwrap();
                    }
                    *flag = false;
                }
            }
        }
    }
}
