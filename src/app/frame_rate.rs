//! Single owner of the window-associated `NativeVSync` expected frame-rate
//! range. Both UI frame scheduling and shader animations submit demands here.
//! Demand changes only record the next range. The frame source publishes it
//! after an actual NativeVSync callback, and releases it when that source stops.
//! Idle boundaries survive coalescing; the applied cache records only success.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

#[cfg(target_env = "ohos")]
use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange, Vsync};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RateRange {
    pub(crate) min: i32,
    pub(crate) max: i32,
    pub(crate) expected: i32,
}

impl RateRange {
    /// Preserve the backend's API 20 DisplaySync bound at the UI API boundary.
    /// This does not change the panel's capability or the NativeVSync vote.
    pub(crate) fn for_display_sync(self) -> Self {
        Self {
            min: self.min.min(144),
            max: self.max.min(144),
            expected: self.expected.min(144),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UiDemand {
    #[default]
    Idle,
    Uncapped,
    Capped(i32),
}

impl UiDemand {
    /// An absent consumer is distinct from a running animation without a cap.
    pub(crate) fn from_animation(active: bool, rate: Option<f32>) -> Self {
        if !active {
            Self::Idle
        } else if let Some(rate) = rate.filter(|rate| rate.is_finite() && *rate > 0.0) {
            Self::Capped((rate.round() as i32).max(1))
        } else {
            Self::Uncapped
        }
    }

    /// A cap applies only while every running consumer permits it.
    pub(crate) fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Idle, demand) | (demand, Self::Idle) => demand,
            (Self::Uncapped, _) | (_, Self::Uncapped) => Self::Uncapped,
            (Self::Capped(left), Self::Capped(right)) => Self::Capped(left.max(right)),
        }
    }
}

/// Scheduling split between sustained `NativeVSync` high-refresh voting and
/// ArkTS `DisplaySync` UI frame sampling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameSchedule {
    pub(crate) rate_demand: UiDemand,
    pub(crate) rate_active: bool,
    pub(crate) ui_active: bool,
}

/// Distinguishes sustained animation/rAF/shader demand (which votes for high
/// refresh via `NativeVSync`) from discrete one-shot UI updates (`frame_wanted`,
/// which only sample `DisplaySync` at the idle baseline without toggling
/// `OH_NativeVSync_SetExpectedFrameRateRange`).
pub(crate) fn classify_frame_schedule(
    is_active: bool,
    engine_active: bool,
    native_demand: UiDemand,
    has_shader_demand: bool,
    frame_wanted: bool,
) -> FrameSchedule {
    let rate_demand = if engine_active {
        UiDemand::Uncapped
    } else {
        native_demand
    };
    let rate_active = is_active && (rate_demand != UiDemand::Idle || has_shader_demand);
    let ui_active = rate_active || (is_active && frame_wanted);
    FrameSchedule {
        rate_demand,
        rate_active,
        ui_active,
    }
}

/// Diagnostic counters for frame demand sources and native rate setter IPCs
/// (read-on-demand only; never logged per frame).
#[derive(Default)]
struct RateCounters {
    mount_requests: AtomicU64,
    engine_requests: AtomicU64,
    animation_frames: AtomicU64,
    shader_votes_added: AtomicU64,
    publish_calls: AtomicU64,
    release_calls: AtomicU64,
    setter_calls: AtomicU64,
    setter_failures: AtomicU64,
}

/// Point-in-time snapshot of [`FrameRate`] demand and IPC counters.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RateMetrics {
    pub(crate) mount_requests: u64,
    pub(crate) engine_requests: u64,
    pub(crate) animation_frames: u64,
    pub(crate) shader_votes_added: u64,
    pub(crate) publish_calls: u64,
    pub(crate) release_calls: u64,
    pub(crate) setter_calls: u64,
    pub(crate) setter_failures: u64,
}

impl RateCounters {
    fn snapshot(&self) -> RateMetrics {
        RateMetrics {
            mount_requests: self.mount_requests.load(Ordering::Relaxed),
            engine_requests: self.engine_requests.load(Ordering::Relaxed),
            animation_frames: self.animation_frames.load(Ordering::Relaxed),
            shader_votes_added: self.shader_votes_added.load(Ordering::Relaxed),
            publish_calls: self.publish_calls.load(Ordering::Relaxed),
            release_calls: self.release_calls.load(Ordering::Relaxed),
            setter_calls: self.setter_calls.load(Ordering::Relaxed),
            setter_failures: self.setter_failures.load(Ordering::Relaxed),
        }
    }
}

pub(crate) trait RateSink: Send + Sync {
    fn set_range(&self, range: RateRange) -> i32;
    fn read_display_hz(&self) -> u32;
}

#[cfg(target_env = "ohos")]
struct OhosSink {
    vsync: Arc<Vsync>,
    host: Arc<crate::app::session::Services>,
}

#[cfg(target_env = "ohos")]
impl RateSink for OhosSink {
    fn set_range(&self, range: RateRange) -> i32 {
        self.vsync
            .set_expected_frame_rate_range(OH_NativeVSync_ExpectedRateRange {
                min: range.min,
                max: range.max,
                expected: range.expected,
            })
    }

    fn read_display_hz(&self) -> u32 {
        self.host.max_refresh_rate()
    }
}

struct State {
    shader_votes: usize,
    ui_demand: UiDemand,
    hz: u32,
    applied: Option<RateRange>,
    pending: PendingRates,
}

impl State {
    /// Arbitrates the shared window-associated `NativeVSync` rate range (A08).
    ///
    /// Intentional policy: while any GPU shader animation holds a vote
    /// (`shader_votes > 0`), the shared `NativeVSync` handle must run at full
    /// panel `hz` so active shaders are never down-throttled by a concurrent
    /// `UiDemand::Capped` UI animation; as soon as the last shader vote drops,
    /// any active UI cap resumes on the next published frame. If strict
    /// per-animation UI rate limiting is ever required alongside an active
    /// shader, throttle that consumer per-deadline inside `Animator` rather than
    /// lowering the shared VSync rate range here.
    fn desired(&self) -> RateRange {
        let hz = self.hz as i32;
        if self.shader_votes > 0 {
            RateRange {
                min: hz,
                max: hz,
                expected: hz,
            }
        } else {
            match self.ui_demand {
                UiDemand::Capped(cap) => {
                    let capped = cap.clamp(1, hz);
                    RateRange {
                        min: 0,
                        max: capped,
                        expected: capped,
                    }
                }
                UiDemand::Uncapped => RateRange {
                    min: hz,
                    max: hz,
                    expected: hz,
                },
                UiDemand::Idle => self.idle(),
            }
        }
    }

    fn idle(&self) -> RateRange {
        let hz = self.hz as i32;
        RateRange {
            min: 0,
            max: hz,
            expected: hz.min(60),
        }
    }
}

/// One release boundary followed by the latest frame range. A release removes
/// older, unsent frame work, but a later frame cannot erase that release.
#[derive(Default)]
struct PendingRates {
    release: Option<RateRange>,
    frame: Option<RateRange>,
}

#[derive(Clone, Copy)]
enum RateUpdate {
    Release(RateRange),
    Frame(RateRange),
}

impl RateUpdate {
    fn range(self) -> RateRange {
        match self {
            Self::Release(range) | Self::Frame(range) => range,
        }
    }
}

impl PendingRates {
    fn release(&mut self, range: RateRange) {
        self.release = Some(range);
        self.frame = None;
    }

    fn take(&mut self) -> Option<RateUpdate> {
        self.release
            .take()
            .map(RateUpdate::Release)
            .or_else(|| self.frame.take().map(RateUpdate::Frame))
    }

    fn retry(&mut self, update: RateUpdate) {
        match update {
            // A failed release must still precede the following active range.
            RateUpdate::Release(range) => {
                self.release.get_or_insert(range);
            }
            // A newer release or frame supersedes this failed frame.
            RateUpdate::Frame(range) if self.release.is_none() && self.frame.is_none() => {
                self.frame = Some(range);
            }
            RateUpdate::Frame(_) => {}
        }
    }
}

pub(crate) struct FrameRate {
    sink: Arc<dyn RateSink>,
    state: Arc<Mutex<State>>,
    updates: RateUpdates,
    counters: Arc<RateCounters>,
}

/// NativeVSync's range setter calls RS SyncFrameRateRange with TF_SYNC. Keep
/// that IPC off both the UI and compositor threads. The channel carries only a
/// bounded wake; State owns the two pending publication slots, not live demand.
struct RateUpdates(mpsc::SyncSender<()>);

impl RateUpdates {
    fn spawn(
        sink: Arc<dyn RateSink>,
        state: Arc<Mutex<State>>,
        counters: Arc<RateCounters>,
    ) -> std::io::Result<(Self, std::thread::JoinHandle<()>)> {
        let (send, receive) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("nsh-frame-rate".into())
            .spawn(move || {
                while receive.recv().is_ok() {
                    loop {
                        let (update, applied) = {
                            let mut state = state.lock().unwrap();
                            (state.pending.take(), state.applied)
                        };
                        let Some(update) = update else { break };
                        let range = update.range();
                        if applied == Some(range) {
                            continue;
                        }
                        // No arbitration lock is held while RS processes its IPC.
                        counters.setter_calls.fetch_add(1, Ordering::Relaxed);
                        let status = Self::apply_range(&*sink, range);
                        if status == 0 {
                            state.lock().unwrap().applied = Some(range);
                        } else {
                            counters.setter_failures.fetch_add(1, Ordering::Relaxed);
                            state.lock().unwrap().pending.retry(update);
                            log::warn!("OHOS VSync rate vote failed: {status}");
                            // Only another publication wakes a retry. There is
                            // no polling clock or failed-IPC spin loop.
                            break;
                        }
                    }
                }
            })?;
        Ok((Self(send), worker))
    }

    fn apply_range(sink: &dyn RateSink, range: RateRange) -> i32 {
        sink.set_range(range)
    }

    fn wake(&self) -> i32 {
        match self.0.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => 0,
            Err(mpsc::TrySendError::Disconnected(())) => -1,
        }
    }
}

impl FrameRate {
    #[cfg(target_env = "ohos")]
    pub(crate) fn new(
        vsync: Arc<Vsync>,
        host: Arc<crate::app::session::Services>,
    ) -> std::io::Result<Arc<Self>> {
        let sink: Arc<dyn RateSink> = Arc::new(OhosSink {
            vsync: vsync.clone(),
            host,
        });
        let hz = normalize_display_hz(sink.read_display_hz());
        let state = Arc::new(Mutex::new(State {
            shader_votes: 0,
            ui_demand: UiDemand::Idle,
            hz,
            applied: None,
            pending: PendingRates::default(),
        }));
        let counters = Arc::new(RateCounters::default());
        let (updates, _) = RateUpdates::spawn(sink.clone(), state.clone(), counters.clone())?;
        Ok(Arc::new(Self {
            sink,
            state,
            updates,
            counters,
        }))
    }

    /// Applies the initial baseline range and returns the raw status code so
    /// surface startup can fail fast if the native VSync handle rejects it.
    pub(crate) fn apply_initial(&self) -> i32 {
        // Called once before either UI or shader frame demand can be published.
        let desired = self.state.lock().unwrap().idle();
        self.counters.setter_calls.fetch_add(1, Ordering::Relaxed);
        let status = RateUpdates::apply_range(&*self.sink, desired);
        if status == 0 {
            self.state.lock().unwrap().applied = Some(desired);
        } else {
            self.counters
                .setter_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        status
    }

    pub(crate) fn record_mount_request(&self) {
        self.counters.mount_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_engine_request(&self) {
        self.counters
            .engine_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_animation_frame(&self) {
        self.counters
            .animation_frames
            .fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn metrics(&self) -> RateMetrics {
        self.counters.snapshot()
    }

    pub(crate) fn set_ui_demand(&self, demand: UiDemand) {
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        state.ui_demand = demand;
    }

    /// Called by the active frame source after a real NativeVSync callback:
    /// RS has refreshed this connection's request time before this vote arrives.
    /// The source serializes this with release and its active-state changes.
    pub(crate) fn publish_frame(&self) {
        self.counters.publish_calls.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        state.pending.frame = Some(state.desired());
        self.wake();
    }

    /// Releases the source's vote without discarding consumer demands needed
    /// after a background/foreground transition. A later publish cannot erase
    /// this Idle boundary while its IPC is pending.
    pub(crate) fn release(&self) {
        self.counters.release_calls.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        let idle = state.idle();
        state.pending.release(idle);
        self.wake();
    }

    /// The UI source uses this same arbitration for its system frame divider.
    /// NativeVSync publishes it asynchronously after an actual native callback.
    pub(crate) fn desired_range(&self) -> RateRange {
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        state.desired()
    }

    pub(crate) fn has_shader_demand(&self) -> bool {
        self.state.lock().unwrap().shader_votes > 0
    }

    pub(crate) fn vote(self: &Arc<Self>) -> Vote {
        Vote {
            rate: self.clone(),
            active: false,
        }
    }

    fn update_vote(&self, delta: isize) {
        let mut state = self.state.lock().unwrap();
        if delta > 0 {
            self.counters
                .shader_votes_added
                .fetch_add(delta as u64, Ordering::Relaxed);
            state.shader_votes = state.shader_votes.saturating_add(delta as usize);
        } else if delta < 0 {
            state.shader_votes = state.shader_votes.saturating_sub((-delta) as usize);
        }
    }

    fn maybe_refresh_hz(&self, state: &mut State) {
        state.hz = normalize_display_hz(self.sink.read_display_hz());
    }

    fn wake(&self) {
        if self.updates.wake() != 0 {
            log::error!("OHOS VSync rate publisher stopped");
        }
    }
}

pub(crate) struct Vote {
    rate: Arc<FrameRate>,
    active: bool,
}

impl Vote {
    pub(crate) fn update(&mut self, active: bool) -> bool {
        if self.active == active {
            return false;
        }
        self.active = active;
        self.rate.update_vote(if active { 1 } else { -1 });
        true
    }
}

impl Drop for Vote {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.rate.update_vote(-1);
        }
    }
}

/// Normalizes the raw OHOS display refresh rate, defaulting to a conservative
/// 60 Hz when the display query reports `0` (unavailable).
pub(crate) fn normalize_display_hz(raw: u32) -> u32 {
    match raw {
        0 => 60,
        hz => hz.clamp(30, 240),
    }
}

#[cfg(all(test, not(target_env = "ohos")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    const IDLE: RateRange = RateRange {
        min: 0,
        max: 120,
        expected: 60,
    };
    const FULL: RateRange = RateRange {
        min: 120,
        max: 120,
        expected: 120,
    };
    const CAPPED: RateRange = RateRange {
        min: 0,
        max: 30,
        expected: 30,
    };

    struct MockSink {
        hz: AtomicU32,
        calls: Mutex<Vec<RateRange>>,
    }

    impl MockSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                hz: AtomicU32::new(120),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl RateSink for MockSink {
        fn set_range(&self, range: RateRange) -> i32 {
            self.calls.lock().unwrap().push(range);
            0
        }

        fn read_display_hz(&self) -> u32 {
            self.hz.load(Ordering::Relaxed)
        }
    }

    struct BlockingSink {
        initial: AtomicBool,
        calls: Mutex<Vec<RateRange>>,
        entered: mpsc::Sender<RateRange>,
        responses: Mutex<mpsc::Receiver<i32>>,
    }

    impl RateSink for BlockingSink {
        fn set_range(&self, range: RateRange) -> i32 {
            self.calls.lock().unwrap().push(range);
            // Startup is synchronous, before the worker receives publications.
            if self.initial.swap(false, Ordering::Relaxed) {
                return 0;
            }
            self.entered.send(range).unwrap();
            self.responses
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .expect("test must complete each native setter")
        }

        fn read_display_hz(&self) -> u32 {
            120
        }
    }

    fn blocking_sink() -> (
        Arc<BlockingSink>,
        mpsc::Receiver<RateRange>,
        mpsc::Sender<i32>,
    ) {
        let (entered, calls) = mpsc::channel();
        let (reply, responses) = mpsc::channel();
        (
            Arc::new(BlockingSink {
                initial: AtomicBool::new(true),
                calls: Mutex::new(Vec::new()),
                entered,
                responses: Mutex::new(responses),
            }),
            calls,
            reply,
        )
    }

    fn start(sink: Arc<dyn RateSink>) -> (Arc<FrameRate>, JoinHandle<()>) {
        let state = Arc::new(Mutex::new(State {
            shader_votes: 0,
            ui_demand: UiDemand::Idle,
            hz: normalize_display_hz(sink.read_display_hz()),
            applied: None,
            pending: PendingRates::default(),
        }));
        let counters = Arc::new(RateCounters::default());
        let (updates, worker) =
            RateUpdates::spawn(sink.clone(), state.clone(), counters.clone()).unwrap();
        let rate = Arc::new(FrameRate {
            sink,
            state,
            updates,
            counters,
        });
        assert_eq!(rate.apply_initial(), 0);
        (rate, worker)
    }

    fn next(calls: &mpsc::Receiver<RateRange>) -> RateRange {
        calls.recv_timeout(Duration::from_secs(2)).unwrap()
    }

    fn finish(rate: Arc<FrameRate>, worker: JoinHandle<()>) -> Option<RateRange> {
        let state = rate.state.clone();
        drop(rate);
        // Closing the actual publication channel drains its queued work before
        // joining. No sleep or synchronous test-only setter bypass is needed.
        worker.join().unwrap();
        state.lock().unwrap().applied
    }

    #[test]
    fn idle_graph_does_not_uncap_css_animation() {
        let sink = MockSink::new();
        let (rate, worker) = start(sink.clone());
        let graph = UiDemand::from_animation(false, None);
        let slower_css = UiDemand::from_animation(true, Some(15.0));
        let css = UiDemand::from_animation(true, Some(30.0));
        let finished_motion = UiDemand::from_animation(false, Some(120.0));
        let demand = UiDemand::Idle
            .merge(graph)
            .merge(slower_css)
            .merge(css)
            .merge(finished_motion);
        assert_eq!(demand, UiDemand::Capped(30));
        assert_eq!(css.merge(slower_css).merge(graph), demand);
        rate.set_ui_demand(demand);
        rate.publish_frame();
        assert_eq!(finish(rate, worker), Some(CAPPED));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, CAPPED]);
    }

    #[test]
    fn uncapped_motion_overrides_then_restores_capped_consumers() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        let capped = UiDemand::from_animation(true, Some(30.0))
            .merge(UiDemand::from_animation(true, Some(15.0)));
        rate.set_ui_demand(capped);
        rate.publish_frame();
        assert_eq!(next(&calls), CAPPED);
        reply.send(0).unwrap();

        // Modal, navigation, image fades, scrolling and selection report a
        // running consumer without a cap; none may inherit another's 30 Hz.
        let motion = UiDemand::from_animation(true, None);
        assert_eq!(motion.merge(capped), capped.merge(motion));
        rate.set_ui_demand(capped.merge(motion));
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        reply.send(0).unwrap();

        let finished = UiDemand::from_animation(false, None);
        rate.set_ui_demand(capped.merge(finished));
        rate.publish_frame();
        assert_eq!(next(&calls), CAPPED);
        reply.send(0).unwrap();
        rate.set_ui_demand(finished);
        rate.release();
        assert_eq!(next(&calls), IDLE);
        reply.send(0).unwrap();
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(
            *sink.calls.lock().unwrap(),
            [IDLE, CAPPED, FULL, CAPPED, IDLE]
        );
    }

    #[test]
    fn demand_changes_wait_for_a_real_frame_publication() {
        let sink = MockSink::new();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        let mut shader = rate.vote();
        assert!(shader.update(true));
        assert!(!shader.update(true));
        drop(shader);
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE]);

        let sink = MockSink::new();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(finish(rate, worker), Some(FULL));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL]);
    }

    #[test]
    fn blocked_setter_preserves_release_before_latest_active_range() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        assert!(rate.state.try_lock().is_ok());

        // All changes happen while RS is blocked. Obsolete frame ranges may
        // coalesce, but the real release separating active periods must survive.
        rate.release();
        rate.set_ui_demand(UiDemand::Capped(30));
        rate.publish_frame();
        rate.set_ui_demand(UiDemand::Capped(15));
        rate.publish_frame();
        rate.release();
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        reply.send(0).unwrap();
        assert_eq!(next(&calls), IDLE);
        reply.send(0).unwrap();
        assert_eq!(next(&calls), FULL);
        reply.send(0).unwrap();

        assert_eq!(finish(rate, worker), Some(FULL));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL, IDLE, FULL]);
    }

    #[test]
    fn repeated_active_frames_do_not_repeat_a_successful_setter() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        for _ in 0..1000 {
            rate.publish_frame();
        }
        reply.send(0).unwrap();
        assert_eq!(finish(rate, worker), Some(FULL));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL]);
    }

    #[test]
    fn final_release_discards_unsent_active_work_and_keeps_demands() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        rate.release();
        rate.publish_frame();
        rate.release();
        assert_eq!(rate.state.lock().unwrap().ui_demand, UiDemand::Uncapped);
        reply.send(0).unwrap();
        assert_eq!(next(&calls), IDLE);
        reply.send(0).unwrap();
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL, IDLE]);
    }

    #[test]
    fn failed_frame_does_not_poison_cache_and_next_publication_retries() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Capped(30));
        rate.publish_frame();
        assert_eq!(next(&calls), CAPPED);
        reply.send(-1).unwrap();
        rate.publish_frame();
        assert_eq!(next(&calls), CAPPED);
        assert_eq!(rate.state.lock().unwrap().applied, Some(IDLE));
        reply.send(0).unwrap();
        assert_eq!(finish(rate, worker), Some(CAPPED));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, CAPPED, CAPPED]);
    }

    #[test]
    fn failed_release_still_precedes_the_next_active_publication() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        rate.release();
        rate.publish_frame();
        reply.send(0).unwrap();
        assert_eq!(next(&calls), IDLE);
        reply.send(-1).unwrap();
        rate.publish_frame();
        assert_eq!(next(&calls), IDLE);
        assert_eq!(rate.state.lock().unwrap().applied, Some(FULL));
        reply.send(0).unwrap();
        assert_eq!(next(&calls), FULL);
        reply.send(0).unwrap();
        assert_eq!(finish(rate, worker), Some(FULL));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL, IDLE, IDLE, FULL]);
    }

    #[test]
    fn release_supersedes_a_failed_active_publication() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        rate.release();
        reply.send(-1).unwrap();
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL]);
    }

    #[test]
    fn shader_and_capped_ui_arbitrate_only_at_frame_publication() {
        let (sink, calls, reply) = blocking_sink();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Capped(30));
        let mut shader = rate.vote();
        assert!(!rate.has_shader_demand());
        assert!(shader.update(true));
        assert!(rate.has_shader_demand());
        rate.publish_frame();
        assert_eq!(next(&calls), FULL);
        reply.send(0).unwrap();

        assert!(shader.update(false));
        assert!(!shader.update(false));
        assert!(!rate.has_shader_demand());
        rate.publish_frame();
        assert_eq!(next(&calls), CAPPED);
        reply.send(0).unwrap();

        rate.set_ui_demand(UiDemand::Idle);
        rate.release();
        assert_eq!(next(&calls), IDLE);
        reply.send(0).unwrap();
        drop(shader);
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, FULL, CAPPED, IDLE]);
    }

    #[test]
    fn published_uncapped_range_uses_the_current_panel_rate() {
        let sink = MockSink::new();
        let (rate, worker) = start(sink.clone());
        rate.set_ui_demand(UiDemand::Uncapped);
        sink.hz.store(144, Ordering::Relaxed);
        rate.publish_frame();
        let expected = RateRange {
            min: 144,
            max: 144,
            expected: 144,
        };
        assert_eq!(finish(rate, worker), Some(expected));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE, expected]);
    }

    #[test]
    fn legacy_display_sync_bound_does_not_change_native_arbitration() {
        for hz in [30, 60, 120, 144] {
            let range = RateRange {
                min: 0,
                max: hz,
                expected: hz,
            };
            assert_eq!(range.for_display_sync(), range);
        }
        let native = RateRange {
            min: 240,
            max: 240,
            expected: 240,
        };
        assert_eq!(
            native.for_display_sync(),
            RateRange {
                min: 144,
                max: 144,
                expected: 144
            }
        );
        assert_eq!(native.max, 240);
        let idle = RateRange {
            min: 0,
            max: 240,
            expected: 60,
        };
        assert_eq!(
            idle.for_display_sync(),
            RateRange {
                min: 0,
                max: 144,
                expected: 60
            }
        );
    }

    #[test]
    fn unavailable_display_rate_defaults_to_60_hz() {
        assert_eq!(normalize_display_hz(0), 60);
        assert_eq!(normalize_display_hz(10), 30);
        assert_eq!(normalize_display_hz(120), 120);
        assert_eq!(normalize_display_hz(360), 240);
    }

    #[test]
    fn discrete_frame_wanted_samples_ui_without_voting_high_refresh_rate() {
        let sink = MockSink::new();
        let (rate, worker) = start(sink.clone());

        // Simulate 20 periodic mount batches (e.g. 60 ms store snapshots) with
        // no running animation, rAF, or active shader.
        for _ in 0..20 {
            rate.record_mount_request();
            let sched = classify_frame_schedule(true, false, UiDemand::Idle, false, true);
            assert_eq!(sched.rate_demand, UiDemand::Idle);
            assert!(!sched.rate_active);
            assert!(sched.ui_active);
            rate.set_ui_demand(sched.rate_demand);
            assert_eq!(rate.desired_range(), IDLE);
        }

        // Continuous animation activates both NativeVSync voting and UI sampling.
        let anim = classify_frame_schedule(true, false, UiDemand::Capped(30), false, false);
        assert_eq!(anim.rate_demand, UiDemand::Capped(30));
        assert!(anim.rate_active);
        assert!(anim.ui_active);

        // Continuous engine rAF activates Uncapped high-refresh voting.
        let raf = classify_frame_schedule(true, true, UiDemand::Idle, false, false);
        assert_eq!(raf.rate_demand, UiDemand::Uncapped);
        assert!(raf.rate_active);
        assert!(raf.ui_active);

        // Active shader activates high-refresh voting even without UI animation.
        let shader = classify_frame_schedule(true, false, UiDemand::Idle, true, false);
        assert!(shader.rate_active);
        assert!(shader.ui_active);

        // Background disables both rate voting and UI sampling.
        let bg = classify_frame_schedule(false, true, UiDemand::Uncapped, true, true);
        assert!(!bg.rate_active);
        assert!(!bg.ui_active);

        let metrics = rate.metrics();
        assert_eq!(metrics.mount_requests, 20);
        assert_eq!(metrics.publish_calls, 0);
        assert_eq!(metrics.release_calls, 0);
        // Only the single initial baseline setter call occurred.
        assert_eq!(metrics.setter_calls, 1);
        assert_eq!(finish(rate, worker), Some(IDLE));
        assert_eq!(*sink.calls.lock().unwrap(), [IDLE]);
    }
}
