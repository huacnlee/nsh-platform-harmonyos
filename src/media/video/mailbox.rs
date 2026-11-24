//! Coalescing, non-dropping control mailbox for `Video` lifecycle and playback state.
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use nsh_core::VideoProps;

pub(crate) trait RawWindow {
    fn raw_addr(&self) -> usize;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WindowChange<W> {
    Set(Option<W>),
    GoneIfRaw(usize),
}

pub(crate) struct ControlBatch<W> {
    pub(crate) closed: bool,
    pub(crate) context_retired: bool,
    pub(crate) window: Option<WindowChange<W>>,
    pub(crate) context_ready: Option<bool>,
    pub(crate) config: Option<(Box<VideoProps>, u64)>,
    pub(crate) toggles: u32,
    pub(crate) seek: Option<f64>,
}

impl<W> ControlBatch<W> {
    pub(crate) fn is_empty(&self) -> bool {
        !self.closed
            && !self.context_retired
            && self.window.is_none()
            && self.context_ready.is_none()
            && self.config.is_none()
            && self.toggles == 0
            && self.seek.is_none()
    }
}

struct State<W> {
    last_source: Option<Arc<str>>,
    config: Option<(Box<VideoProps>, u64)>,
    context_retired: bool,
    context_ready: Option<bool>,
    window: Option<WindowChange<W>>,
    seek: Option<f64>,
    pending_toggles: u32,
    woken: bool,
    closed: bool,
}

impl<W> Default for State<W> {
    fn default() -> Self {
        Self {
            last_source: None,
            config: None,
            context_retired: false,
            context_ready: None,
            window: None,
            seek: None,
            pending_toggles: 0,
            woken: false,
            closed: false,
        }
    }
}

impl<W> State<W> {
    fn has_pending(&self) -> bool {
        self.closed
            || self.woken
            || self.context_retired
            || self.window.is_some()
            || self.context_ready.is_some()
            || self.config.is_some()
            || self.pending_toggles > 0
            || self.seek.is_some()
    }
}

pub(crate) struct VideoMailbox<W> {
    active: AtomicBool,
    source_epoch: AtomicU64,
    state: Mutex<State<W>>,
    ready: Condvar,
}

impl<W: RawWindow> VideoMailbox<W> {
    pub(crate) fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
            source_epoch: AtomicU64::new(0),
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn current_epoch(&self) -> u64 {
        self.source_epoch.load(Ordering::Acquire)
    }

    /// Publishes `props` and any new `source_epoch` atomically under one lock so
    /// an epoch bump can never be observed without its corresponding configuration.
    pub(crate) fn configure(&self, props: Box<VideoProps>) -> Result<u64, &'static str> {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Ok(self.source_epoch.load(Ordering::Acquire));
        }
        let mut epoch = self.source_epoch.load(Ordering::Acquire);
        let source_changed = state.last_source != props.source;
        if source_changed {
            epoch = epoch
                .checked_add(1)
                .ok_or("Video source generation exhausted")?;
            state.last_source = props.source.clone();
            self.source_epoch.store(epoch, Ordering::Release);
            state.seek = None;
            state.pending_toggles = 0;
        } else if state
            .config
            .as_ref()
            .is_some_and(|(prior, _)| prior.paused != props.paused)
        {
            state.pending_toggles = 0;
        }
        state.config = Some((props, epoch));
        drop(state);
        self.ready.notify_one();
        Ok(epoch)
    }

    pub(crate) fn set_context(&self, ready: bool, window: Option<W>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        if !ready {
            state.context_retired = true;
            state.context_ready = Some(false);
            state.window = Some(WindowChange::Set(None));
        } else {
            state.window = Some(WindowChange::Set(window));
            state.context_ready = Some(true);
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn set_window(&self, window: Option<W>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.window = Some(WindowChange::Set(window));
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn window_gone(&self, raw: usize) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        match &state.window {
            Some(WindowChange::Set(Some(current))) if current.raw_addr() == raw => {
                state.window = Some(WindowChange::Set(None));
            }
            Some(WindowChange::Set(_)) => {
                // Newer window (or already None) supersedes a stale destroy notice.
            }
            Some(WindowChange::GoneIfRaw(_)) | None => {
                state.window = Some(WindowChange::GoneIfRaw(raw));
            }
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn seek(&self, seconds: f64) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.seek = Some(seconds);
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn toggle(&self) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        if state.pending_toggles >= 16 {
            state.pending_toggles = 16 + ((state.pending_toggles + 1) & 1);
        } else {
            state.pending_toggles += 1;
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn wake(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            state.woken = true;
        }
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn close(&self) {
        self.active.store(false, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.window = None;
        state.config = None;
        drop(state);
        self.ready.notify_all();
    }

    pub(crate) fn wait_and_take(&self, timeout: Duration) -> ControlBatch<W> {
        let mut state = self.state.lock().unwrap();
        if !state.has_pending() && !timeout.is_zero() {
            let (next, _) = self.ready.wait_timeout(state, timeout).unwrap();
            state = next;
        }
        state.woken = false;
        ControlBatch {
            closed: state.closed,
            context_retired: std::mem::replace(&mut state.context_retired, false),
            window: state.window.take(),
            context_ready: state.context_ready.take(),
            config: state.config.take(),
            toggles: std::mem::replace(&mut state.pending_toggles, 0),
            seek: state.seek.take(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FakeWindow(usize);

    impl RawWindow for FakeWindow {
        fn raw_addr(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn flooded_updates_preserve_latest_config_epoch_and_seek() {
        let mailbox = VideoMailbox::<FakeWindow>::new();
        for idx in 0..500 {
            let props = VideoProps {
                source: Some(Arc::from(format!("https://example.com/{idx}.mp4"))),
                volume: (idx as f32 % 10.0) / 10.0,
                ..VideoProps::default()
            };
            mailbox.configure(Box::new(props)).unwrap();
            mailbox.seek(idx as f64);
        }
        let batch = mailbox.wait_and_take(Duration::ZERO);
        let (props, epoch) = batch.config.expect("latest config retained");
        assert_eq!(epoch, 500);
        assert_eq!(mailbox.current_epoch(), 500);
        assert_eq!(props.source.as_deref(), Some("https://example.com/499.mp4"));
        assert_eq!(batch.seek, Some(499.0));
    }

    #[test]
    fn stale_window_gone_does_not_clobber_newer_window() {
        let mailbox = VideoMailbox::<FakeWindow>::new();
        mailbox.set_window(Some(FakeWindow(10)));
        mailbox.set_window(Some(FakeWindow(20)));
        mailbox.window_gone(10);
        let batch = mailbox.wait_and_take(Duration::ZERO);
        assert_eq!(batch.window, Some(WindowChange::Set(Some(FakeWindow(20)))));

        mailbox.set_window(Some(FakeWindow(20)));
        mailbox.window_gone(20);
        let batch = mailbox.wait_and_take(Duration::ZERO);
        assert_eq!(batch.window, Some(WindowChange::Set(None)));
    }

    #[test]
    fn background_then_foreground_retains_retirement_and_new_window() {
        let mailbox = VideoMailbox::<FakeWindow>::new();
        mailbox.set_context(false, None);
        mailbox.set_context(true, Some(FakeWindow(42)));
        let batch = mailbox.wait_and_take(Duration::ZERO);
        assert!(batch.context_retired);
        assert_eq!(batch.context_ready, Some(true));
        assert_eq!(batch.window, Some(WindowChange::Set(Some(FakeWindow(42)))));
    }

    #[test]
    fn close_marks_inactive_and_wakes_waiter_immediately() {
        let mailbox = Arc::new(VideoMailbox::<FakeWindow>::new());
        let worker_box = mailbox.clone();
        let handle = std::thread::spawn(move || worker_box.wait_and_take(Duration::from_secs(30)));
        mailbox.close();
        let batch = handle.join().unwrap();
        assert!(batch.closed);
        assert!(!mailbox.is_active());
    }
}
