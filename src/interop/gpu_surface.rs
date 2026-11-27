//! Pure-Rust native GPU `XComponent` surface extension point for HarmonyOS
//! `nshm` crates (terminals, 3D views, charts, video/map renderers).
//!
//! `nshm` crates register a [`GpuSurfaceRenderer`] factory under a component
//! name (e.g. `"nsh-ghostty.surface"`). When the NSH tree mounts that native
//! component on HarmonyOS, `nsh-platform-harmonyos` instantiates an ArkUI
//! `XComponent` directly in Rust—with zero C glue, zero ArkTS `BuilderNode`,
//! zero `dlsym`, and zero post-build patching—and drives the renderer on a
//! dedicated GPU thread that preserves OpenGL ES / EGL / `wgpu-gles` thread
//! affinity and integrates with [`crate::app::frame_rate::FrameRate`].

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Condvar, LazyLock, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use nsh_core::{JsonValue, NodeId};
use nsh_engine::EngineControl;
use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::XComponent},
    types::{
        advanced::LengthMetricUnit,
        attribute::ArkUINodeAttributeType as A,
        view::{AccessibilityMode, HitTestMode},
    },
};
use ohos_native_window_binding::NativeWindow;
use ohos_xcomponent_binding::{NativeXComponent, WindowRaw, XComponentRaw};
use rustc_hash::FxHashMap;

use crate::{
    app::{
        frame_rate::{FrameRate, Vote, normalize_display_hz},
        runtime::{Dispatcher, Message},
    },
    extensions::ExtensionError,
    render::node::{NativeNode, Result as NodeResult},
};

/// Maximum interval between event-driven frame renders to treat a GPU surface
/// as actively streaming (keeping the 120 Hz vote steady without IPC churn).
const STREAM_BURST_WINDOW: Duration = Duration::from_millis(50);

/// Hold duration after the last event-driven render before dropping the
/// high-refresh vote and parking the render thread indefinitely.
const STREAM_IDLE_HOLD: Duration = Duration::from_millis(200);

static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(1);

/// One changed property delivered directly from the NSH engine to a
/// [`GpuSurfaceRenderer`] without crossing ArkTS or JSON strings.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuSurfaceProperty {
    /// Property name (e.g. `"sessionId"`, `"frameRev"`, `"postProcessShader"`).
    pub name: Arc<str>,
    /// New property value, or `None` when the property was removed.
    pub value: Option<JsonValue>,
}

impl GpuSurfaceProperty {
    /// Returns `true` if this property was removed from the component.
    #[inline]
    pub fn is_removed(&self) -> bool {
        self.value.is_none()
    }

    /// Returns the property value as a string slice when present and a JSON string.
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        self.value.as_ref().and_then(JsonValue::as_str)
    }

    /// Returns the property value as a `u64` when present and representable.
    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        self.value.as_ref().and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(|n| n as u64)
            })
        })
    }

    /// Returns the property value as an `i64` when present and representable.
    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        self.value.as_ref().and_then(JsonValue::as_i64)
    }

    /// Returns the property value as an `f64` when present and numeric.
    #[inline]
    pub fn as_f64(&self) -> Option<f64> {
        self.value.as_ref().and_then(JsonValue::as_f64)
    }

    /// Returns the property value as a `bool` when present and boolean.
    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        self.value.as_ref().and_then(JsonValue::as_bool)
    }
}

/// Configuration for a pure-Rust HarmonyOS `XComponent` GPU surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct GpuSurfaceOptions {
    /// When `true` (the default), sets `HitTestMode::None` and disables
    /// accessibility on the underlying `XComponent` so touch, mouse, wheel,
    /// and keyboard events pass cleanly to the owning NSH view.
    pub hit_test_passthrough: bool,
    /// When `true` (the default), clips the `XComponent` to its layout bounds.
    pub clip: bool,
    /// Preferred refresh rate in Hz when the surface is active (`0` uses the
    /// panel's maximum refresh rate, typically 120 Hz).
    pub preferred_frame_rate: u32,
}

impl Default for GpuSurfaceOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuSurfaceOptions {
    /// Returns the default GPU surface options (`hit_test_passthrough: true`,
    /// `clip: true`, `preferred_frame_rate: 0` = panel max).
    pub const fn new() -> Self {
        Self {
            hit_test_passthrough: true,
            clip: true,
            preferred_frame_rate: 0,
        }
    }

    /// Configures whether pointer/touch events pass through to the parent NSH view.
    pub const fn with_hit_test_passthrough(mut self, passthrough: bool) -> Self {
        self.hit_test_passthrough = passthrough;
        self
    }

    /// Configures whether the `XComponent` clips to its layout bounds.
    pub const fn with_clip(mut self, clip: bool) -> Self {
        self.clip = clip;
        self
    }

    /// Sets the preferred active refresh rate in Hz (`0` uses the panel maximum).
    pub const fn with_preferred_frame_rate(mut self, hz: u32) -> Self {
        self.preferred_frame_rate = hz;
        self
    }
}

struct HostInner {
    surface_id: Arc<str>,
    node_id: NodeId,
    control: EngineControl,
    active: Arc<AtomicBool>,
    scale_bits: Arc<AtomicU32>,
    shared: Arc<WorkerShared>,
}

/// Thread-safe handle handed to a [`GpuSurfaceRenderer`] factory.
///
/// Clone and send it freely to background producers (such as a PTY reader or
/// decoding worker) to wake the dedicated GPU render thread via
/// [`Self::request_frame`] or emit native component events back to NSH.
#[derive(Clone)]
pub struct GpuSurfaceHost {
    inner: Arc<HostInner>,
}

impl fmt::Debug for GpuSurfaceHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuSurfaceHost")
            .field("surface_id", &self.inner.surface_id)
            .field("scale", &self.scale())
            .finish()
    }
}

impl GpuSurfaceHost {
    /// Returns the unique ArkUI `XComponent` surface identifier for this instance.
    #[inline]
    pub fn surface_id(&self) -> &str {
        &self.inner.surface_id
    }

    /// Returns the current display pixel ratio (scale factor) for this surface.
    #[inline]
    pub fn scale(&self) -> f32 {
        f32::from_bits(self.inner.scale_bits.load(Ordering::Relaxed))
    }

    /// Wakes the dedicated GPU render thread immediately so [`GpuSurfaceRenderer::on_frame`]
    /// runs promptly. Multiple calls coalesce cheaply.
    #[inline]
    pub fn request_frame(&self) {
        self.inner.shared.wake_frame();
    }

    /// Dispatches an NSH `NativeEvent` (`name`, `data`) back to the mounted NSH
    /// component node from any thread.
    pub fn dispatch_event(&self, name: impl Into<Arc<str>>, data: JsonValue) {
        if self.inner.active.load(Ordering::Acquire) {
            self.inner.control.dispatch_event(
                self.inner.node_id,
                nsh_core::EventKind::NativeEvent,
                nsh_core::EventPayload::Native {
                    name: name.into(),
                    data,
                },
            );
        }
    }

    /// Reports the component's intrinsic size in logical points (`vp`) back to
    /// the NSH layout engine.
    pub fn set_intrinsic_size(&self, width: f32, height: f32) {
        if width.is_finite()
            && height.is_finite()
            && (0.0..=16384.0).contains(&width)
            && (0.0..=16384.0).contains(&height)
            && self.inner.active.load(Ordering::Acquire)
        {
            self.inner.control.set_component_size(
                self.inner.node_id,
                nsh_core::Size::new(nsh_core::Points(width), nsh_core::Points(height)),
            );
        }
    }
}

/// Renderer trait implemented by pure-Rust HarmonyOS GPU surface `nshm` crates.
///
/// # Single-Thread Affinity Guarantee
/// The factory closure and every method of `GpuSurfaceRenderer`
/// (`on_surface_created`, `on_surface_resized`, `on_props`, `on_command`,
/// `on_frame`, `on_surface_destroyed`, and `Drop`) execute on the **same**
/// dedicated GPU render thread for the lifetime of a mounted surface instance.
/// OpenGL ES / EGL / `wgpu-gles` contexts stay thread-affine without requiring
/// `Send` on renderer internals, and the ArkUI UI thread stays completely free
/// of GPU work.
///
/// # Safe Teardown Ordering
/// [`Self::on_surface_destroyed`] is guaranteed to complete on the render
/// thread **before** `nsh-platform-harmonyos` releases its retained
/// `OHNativeWindow` reference or disposes the `XComponent`, preventing EGL or
/// Vulkan use-after-free during teardown or surface recreation.
pub trait GpuSurfaceRenderer: 'static {
    /// Called on the render thread once the `XComponent` provides a live
    /// `OHNativeWindow*` with its physical pixel dimensions and display scale.
    ///
    /// `native_window` is a valid, retained non-null `OHNativeWindow*` that
    /// remains alive until [`Self::on_surface_destroyed`] returns.
    fn on_surface_created(
        &mut self,
        native_window: *mut c_void,
        width_px: u32,
        height_px: u32,
        scale: f32,
    );

    /// Called on the render thread when the surface's physical pixel dimensions
    /// or display pixel ratio change.
    fn on_surface_resized(&mut self, width_px: u32, height_px: u32, scale: f32) {
        let _ = (width_px, height_px, scale);
    }

    /// Called on the render thread with updated NSH native component properties.
    ///
    /// Initial properties may be delivered before [`Self::on_surface_created`]
    /// so renderers can capture configuration (e.g. session IDs, shader presets)
    /// ahead of the first frame.
    fn on_props(&mut self, props: &[GpuSurfaceProperty]) {
        let _ = props;
    }

    /// Called on the render thread when the NSH app sends an imperative
    /// native component command.
    fn on_command(&mut self, name: &str, args: &JsonValue) {
        let _ = (name, args);
    }

    /// Called on the render thread when a frame should be rendered.
    ///
    /// `timestamp_ns` is a monotonic timestamp in nanoseconds.
    ///
    /// Return `true` if the renderer expects continuous VSync-paced frames
    /// (keeping the high-refresh `FrameRate` vote at e.g. 120 Hz), or `false`
    /// when idle so the platform releases the high-refresh vote and parks the
    /// render thread until woken by [`GpuSurfaceHost::request_frame`], props,
    /// commands, or resize events.
    fn on_frame(&mut self, timestamp_ns: u64) -> bool;

    /// Called on the render thread before the underlying `OHNativeWindow` and
    /// `XComponent` are dropped. Release any EGL / `wgpu` surface bound to the
    /// `native_window` here.
    fn on_surface_destroyed(&mut self) {}
}

type RendererFactory = Arc<dyn Fn(GpuSurfaceHost) -> Box<dyn GpuSurfaceRenderer> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct RegisteredGpuSurface {
    pub(crate) options: GpuSurfaceOptions,
    pub(crate) factory: RendererFactory,
}

static GLOBAL_GPU_SURFACES: LazyLock<RwLock<FxHashMap<Arc<str>, RegisteredGpuSurface>>> =
    LazyLock::new(RwLock::default);

fn validate_surface_name(name: &str) -> Result<(), ExtensionError> {
    if name.trim().is_empty() {
        return Err(ExtensionError::Failed(
            "GPU surface component name must not be empty".into(),
        ));
    }
    if name.starts_with("Nsh") {
        return Err(ExtensionError::Failed(
            "Component names starting with 'Nsh' are reserved".into(),
        ));
    }
    Ok(())
}

/// Registers a pure-Rust HarmonyOS GPU `XComponent` surface component under
/// `name` with [`GpuSurfaceOptions::default()`].
///
/// Typically invoked once inside an `nshm` crate's `#[native_module]`
/// registration function. When NSH mounts `native(name)`,
/// `nsh-platform-harmonyos` creates the ArkUI `XComponent` directly in Rust
/// and drives the returned [`GpuSurfaceRenderer`] on a dedicated render thread.
pub fn register_gpu_surface<F, R>(name: impl Into<String>, factory: F) -> Result<(), ExtensionError>
where
    F: Fn(GpuSurfaceHost) -> R + Send + Sync + 'static,
    R: GpuSurfaceRenderer + 'static,
{
    register_gpu_surface_with_options(name, GpuSurfaceOptions::default(), factory)
}

/// Registers a pure-Rust HarmonyOS GPU `XComponent` surface component under
/// `name` with explicit [`GpuSurfaceOptions`].
pub fn register_gpu_surface_with_options<F, R>(
    name: impl Into<String>,
    options: GpuSurfaceOptions,
    factory: F,
) -> Result<(), ExtensionError>
where
    F: Fn(GpuSurfaceHost) -> R + Send + Sync + 'static,
    R: GpuSurfaceRenderer + 'static,
{
    let name = name.into();
    validate_surface_name(&name)?;
    let boxed: RendererFactory = Arc::new(move |host| Box::new(factory(host)));
    GLOBAL_GPU_SURFACES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            Arc::from(name),
            RegisteredGpuSurface {
                options,
                factory: boxed,
            },
        );
    Ok(())
}

/// Unregisters a previously registered pure-Rust GPU surface component.
///
/// Returns `true` if a factory was registered under `name`. Existing mounted
/// instances continue running until unmounted.
pub fn unregister_gpu_surface(name: &str) -> bool {
    GLOBAL_GPU_SURFACES
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(name)
        .is_some()
}

/// Returns `true` if a pure-Rust GPU surface factory is currently registered
/// under `name`.
pub fn has_gpu_surface(name: &str) -> bool {
    GLOBAL_GPU_SURFACES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(name)
}

pub(crate) fn lookup_gpu_surface(name: &str) -> Option<RegisteredGpuSurface> {
    GLOBAL_GPU_SURFACES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

struct SyncBarrier {
    done: Mutex<bool>,
    cv: Condvar,
}

impl SyncBarrier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn signal(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        *done = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.cv.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

enum SurfaceCommand {
    Created {
        window: NativeWindow,
        width_px: u32,
        height_px: u32,
        scale: f32,
    },
    Resized {
        width_px: u32,
        height_px: u32,
        scale: f32,
    },
    Props(Vec<GpuSurfaceProperty>),
    Command(String, JsonValue),
    Destroy(Arc<SyncBarrier>),
}

struct WorkerState {
    commands: VecDeque<SurfaceCommand>,
    frame_requested: bool,
    vsync_timestamp_ns: Option<u64>,
    host_active: bool,
    shutdown: bool,
    exited: bool,
}

impl WorkerState {
    fn new(host_active: bool) -> Self {
        Self {
            commands: VecDeque::new(),
            frame_requested: false,
            vsync_timestamp_ns: None,
            host_active,
            shutdown: false,
            exited: false,
        }
    }
}

struct WorkerShared {
    state: Mutex<WorkerState>,
    cv: Condvar,
}

impl WorkerShared {
    fn wake_frame(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state.frame_requested = true;
        self.cv.notify_one();
    }
}

struct StreamBurst {
    last_frame: Option<Instant>,
    streaming: bool,
}

impl StreamBurst {
    fn new() -> Self {
        Self {
            last_frame: None,
            streaming: false,
        }
    }

    fn record_frame(&mut self, now: Instant) {
        if let Some(prev) = self.last_frame.replace(now)
            && now.saturating_duration_since(prev) <= STREAM_BURST_WINDOW
        {
            self.streaming = true;
        }
    }

    fn remaining_hold(&mut self, now: Instant) -> Option<Duration> {
        if !self.streaming {
            return None;
        }
        let Some(last) = self.last_frame else {
            self.streaming = false;
            return None;
        };
        let elapsed = now.saturating_duration_since(last);
        if elapsed >= STREAM_IDLE_HOLD {
            self.streaming = false;
            self.last_frame = None;
            None
        } else {
            Some(STREAM_IDLE_HOLD - elapsed)
        }
    }

    fn clear(&mut self) {
        self.last_frame = None;
        self.streaming = false;
    }
}

struct RateGuard {
    active: bool,
    vote: Vote,
    rate_active: Arc<AtomicBool>,
    dispatcher: Weak<Dispatcher>,
}

impl RateGuard {
    fn new(vote: Vote, rate_active: Arc<AtomicBool>, dispatcher: Weak<Dispatcher>) -> Self {
        Self {
            active: false,
            vote,
            rate_active,
            dispatcher,
        }
    }

    fn set(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.apply(active);
        }
    }

    fn apply(&mut self, active: bool) {
        self.rate_active.store(active, Ordering::Release);
        if self.vote.update(active)
            && let Some(route) = self.dispatcher.upgrade()
        {
            route.send(Message::CompositeDemand);
        }
    }
}

impl Drop for RateGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.apply(false);
        }
    }
}

/// Ensures any waiting `Destroy` barriers are released even if the render
/// thread exits unexpectedly or a renderer callback panics.
struct ExitGuard {
    shared: Arc<WorkerShared>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let pending = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.exited = true;
            std::mem::take(&mut state.commands)
        };
        for cmd in pending {
            if let SurfaceCommand::Destroy(barrier) = cmd {
                barrier.signal();
            }
        }
    }
}

struct WorkerLaunch {
    surface_id: Arc<str>,
    node_id: NodeId,
    control: EngineControl,
    active: Arc<AtomicBool>,
    scale_bits: Arc<AtomicU32>,
    host_active: bool,
    factory: RendererFactory,
    rate: RateGuard,
}

/// Dedicated single-thread render loop owning a [`GpuSurfaceRenderer`] and its
/// retained [`NativeWindow`] handle.
struct GpuSurfaceWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl GpuSurfaceWorker {
    fn spawn(launch: WorkerLaunch) -> std::io::Result<Self> {
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState::new(launch.host_active)),
            cv: Condvar::new(),
        });
        let host = GpuSurfaceHost {
            inner: Arc::new(HostInner {
                surface_id: launch.surface_id.clone(),
                node_id: launch.node_id,
                control: launch.control,
                active: launch.active,
                scale_bits: launch.scale_bits,
                shared: shared.clone(),
            }),
        };
        let worker_shared = shared.clone();
        let factory = launch.factory;
        let rate = launch.rate;
        let thread = std::thread::Builder::new()
            .name(format!("nsh-gpu-{}", launch.surface_id))
            .spawn(move || {
                run_render_thread(worker_shared, host, factory, rate);
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn surface_created(&self, window: NativeWindow, width_px: u32, height_px: u32, scale: f32) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state.commands.push_back(SurfaceCommand::Created {
            window,
            width_px: width_px.max(1),
            height_px: height_px.max(1),
            scale,
        });
        self.shared.cv.notify_one();
    }

    fn surface_resized(&self, width_px: u32, height_px: u32, scale: f32) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        // Coalesce consecutive pending resize events so rapid window dragging
        // delivers only the latest dimensions.
        if let Some(SurfaceCommand::Resized {
            width_px: w,
            height_px: h,
            scale: s,
        }) = state.commands.back_mut()
        {
            *w = width_px.max(1);
            *h = height_px.max(1);
            *s = scale;
        } else {
            state.commands.push_back(SurfaceCommand::Resized {
                width_px: width_px.max(1),
                height_px: height_px.max(1),
                scale,
            });
        }
        self.shared.cv.notify_one();
    }

    fn props(&self, props: Vec<GpuSurfaceProperty>) {
        if props.is_empty() {
            return;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        if let Some(SurfaceCommand::Props(existing)) = state.commands.back_mut() {
            existing.extend(props);
        } else {
            state.commands.push_back(SurfaceCommand::Props(props));
        }
        self.shared.cv.notify_one();
    }

    fn command(&self, name: String, args: JsonValue) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state
            .commands
            .push_back(SurfaceCommand::Command(name, args));
        self.shared.cv.notify_one();
    }

    fn on_vsync(&self, timestamp_ns: u64) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state.vsync_timestamp_ns = Some(timestamp_ns);
        self.shared.cv.notify_one();
    }

    fn set_host_active(&self, active: bool) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited || state.host_active == active {
            return;
        }
        state.host_active = active;
        self.shared.cv.notify_one();
    }

    /// Synchronously destroys the currently attached native window on the render
    /// thread so `on_surface_destroyed()` completes before ArkUI tears down the
    /// underlying `OHNativeWindow`.
    fn surface_destroyed_sync(&self) {
        let barrier = SyncBarrier::new();
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.shutdown || state.exited {
                return;
            }
            state
                .commands
                .push_back(SurfaceCommand::Destroy(barrier.clone()));
            self.shared.cv.notify_one();
        }
        barrier.wait();
    }

    /// Shuts down the render thread and joins it, guaranteeing
    /// `on_surface_destroyed()` -> `drop(renderer)` -> `drop(window)` all
    /// complete before returning.
    fn shutdown(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
            self.shared.cv.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for GpuSurfaceWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_render_thread(
    shared: Arc<WorkerShared>,
    host: GpuSurfaceHost,
    factory: RendererFactory,
    mut rate: RateGuard,
) {
    let _exit_guard = ExitGuard {
        shared: shared.clone(),
    };
    let mut renderer = match catch_unwind(AssertUnwindSafe(|| factory(host))) {
        Ok(renderer) => renderer,
        Err(_) => {
            log::error!("OHOS GPU surface factory panicked");
            return;
        }
    };

    let epoch = Instant::now();
    let mut active_window: Option<NativeWindow> = None;
    let mut continuous = false;
    let mut burst = StreamBurst::new();

    loop {
        let (commands, immediate_frame, vsync_ts, host_active, shutdown) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let has_work = state.shutdown
                    || !state.commands.is_empty()
                    || (active_window.is_some()
                        && (state.frame_requested
                            || (continuous
                                && state.host_active
                                && state.vsync_timestamp_ns.is_some())));
                if has_work {
                    break;
                }
                if !state.host_active {
                    rate.set(false);
                } else if !continuous {
                    let now = Instant::now();
                    if let Some(rem) = burst.remaining_hold(now) {
                        let (next_state, _) = shared
                            .cv
                            .wait_timeout(state, rem)
                            .unwrap_or_else(|e| e.into_inner());
                        state = next_state;
                        if burst.remaining_hold(Instant::now()).is_none() && !continuous {
                            rate.set(false);
                        }
                        continue;
                    }
                    rate.set(false);
                }
                state = shared.cv.wait(state).unwrap_or_else(|e| e.into_inner());
            }

            let commands = std::mem::take(&mut state.commands);
            let immediate_frame = std::mem::take(&mut state.frame_requested);
            let vsync_ts = state.vsync_timestamp_ns.take();
            let host_active = state.host_active;
            let shutdown = state.shutdown;
            (commands, immediate_frame, vsync_ts, host_active, shutdown)
        };

        let mut need_frame = immediate_frame;
        for cmd in commands {
            match cmd {
                SurfaceCommand::Created {
                    window,
                    width_px,
                    height_px,
                    scale,
                } => {
                    if let Some(old) = active_window.take() {
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            renderer.on_surface_destroyed();
                        }));
                        drop(old);
                    }
                    let ptr = window.raw().cast();
                    active_window = Some(window);
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        renderer.on_surface_created(ptr, width_px, height_px, scale);
                    }));
                    need_frame = true;
                }
                SurfaceCommand::Resized {
                    width_px,
                    height_px,
                    scale,
                } => {
                    if active_window.is_some() {
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            renderer.on_surface_resized(width_px, height_px, scale);
                        }));
                        need_frame = true;
                    }
                }
                SurfaceCommand::Props(props) => {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        renderer.on_props(&props);
                    }));
                    if active_window.is_some() {
                        need_frame = true;
                    }
                }
                SurfaceCommand::Command(name, args) => {
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        renderer.on_command(&name, &args);
                    }));
                    if active_window.is_some() {
                        need_frame = true;
                    }
                }
                SurfaceCommand::Destroy(barrier) => {
                    if let Some(old) = active_window.take() {
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            renderer.on_surface_destroyed();
                        }));
                        drop(old);
                    }
                    continuous = false;
                    burst.clear();
                    rate.set(false);
                    need_frame = false;
                    barrier.signal();
                }
            }
        }

        if shutdown {
            break;
        }

        let should_render = active_window.is_some()
            && (need_frame || (continuous && host_active && vsync_ts.is_some()));
        if should_render {
            let now = Instant::now();
            let timestamp_ns = vsync_ts.unwrap_or_else(|| epoch.elapsed().as_nanos() as u64);
            let wants_more = catch_unwind(AssertUnwindSafe(|| renderer.on_frame(timestamp_ns)))
                .unwrap_or_else(|_| {
                    log::error!("OHOS GPU surface on_frame panicked");
                    false
                });
            continuous = wants_more;
            burst.record_frame(now);
            let high_refresh = host_active && (continuous || burst.streaming);
            rate.set(high_refresh);
        } else if !host_active {
            rate.set(false);
        }
    }

    // Strict teardown ordering: on_surface_destroyed() -> drop(renderer) -> drop(window).
    if active_window.is_some() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            renderer.on_surface_destroyed();
        }));
    }
    let _ = catch_unwind(AssertUnwindSafe(move || {
        drop(renderer);
    }));
    drop(active_window);
    rate.set(false);
}

/// Mounted pure-Rust HarmonyOS `XComponent` GPU surface instance.
pub(crate) struct GpuSurfaceInstance {
    pub(crate) native: NativeNode,
    xcomponent: NativeXComponent,
    worker: RefCell<Option<GpuSurfaceWorker>>,
    scale_bits: Arc<AtomicU32>,
    frame_vp: Rc<Cell<(f32, f32)>>,
    active_flag: Arc<AtomicBool>,
    rate_active: Arc<AtomicBool>,
    applied_rate_active: Cell<Option<bool>>,
    target_hz: i32,
    idle_hz: i32,
    disposed: Cell<bool>,
}

impl GpuSurfaceInstance {
    pub(crate) fn new(
        registered: RegisteredGpuSurface,
        node_id: NodeId,
        control: EngineControl,
        scale: f32,
        frame_rate: Arc<FrameRate>,
        dispatcher: Weak<Dispatcher>,
        panel_hz: u32,
    ) -> NodeResult<Rc<Self>> {
        let options: GpuSurfaceOptions = registered.options;
        let panel_hz = normalize_display_hz(panel_hz) as i32;
        let target_hz = if options.preferred_frame_rate > 0 {
            (options.preferred_frame_rate as i32).clamp(30, panel_hz)
        } else {
            panel_hz
        };
        let idle_hz = target_hz.min(60);

        let seq = NEXT_SURFACE_ID.fetch_add(1, Ordering::Relaxed);
        let surface_id: Arc<str> = Arc::from(format!("nsh-gpu-{seq}"));
        let native = XComponent::new()?.into_node();
        {
            let node = native.borrow();
            node.set_length_metric_unit(LengthMetricUnit::Vp)?;
            node.set_string_attribute(A::XComponentId, &surface_id)?;
            if options.hit_test_passthrough {
                node.set_i32_attribute(A::HitTestBehavior, HitTestMode::None.into())?;
                node.set_i32_attribute(A::AccessibilityMode, AccessibilityMode::Disabled.into())?;
            }
            node.set_bool_attribute(A::Clip, options.clip)?;
            node.background_color(0)?;
        }
        let xcomponent = native.borrow().native_xcomponent().ok_or_else(|| {
            napi_ohos::Error::from_reason("GPU surface XComponent is unavailable")
        })?;

        let scale_bits = Arc::new(AtomicU32::new(scale.clamp(0.5, 4.0).to_bits()));
        let frame_vp = Rc::new(Cell::new((0.0f32, 0.0f32)));
        let active_flag = Arc::new(AtomicBool::new(true));
        let rate_active = Arc::new(AtomicBool::new(false));
        let host_active = dispatcher
            .upgrade()
            .is_none_or(|route| route.active.load(Ordering::Acquire));

        let worker = GpuSurfaceWorker::spawn(WorkerLaunch {
            surface_id,
            node_id,
            control,
            active: active_flag.clone(),
            scale_bits: scale_bits.clone(),
            host_active,
            factory: registered.factory,
            rate: RateGuard::new(frame_rate.vote(), rate_active.clone(), dispatcher),
        })
        .map_err(|error| {
            napi_ohos::Error::from_reason(format!(
                "failed to spawn OHOS GPU surface worker: {error}"
            ))
        })?;

        let instance = Rc::new(Self {
            native,
            xcomponent,
            worker: RefCell::new(Some(worker)),
            scale_bits,
            frame_vp,
            active_flag,
            rate_active,
            applied_rate_active: Cell::new(None),
            target_hz,
            idle_hz,
            disposed: Cell::new(false),
        });

        let created_weak = Rc::downgrade(&instance);
        instance
            .xcomponent
            .on_surface_created(move |xc_raw, window_raw| {
                let Some(inst) = created_weak.upgrade() else {
                    return Ok(());
                };
                let raw = NonNull::new(window_raw.0).ok_or_else(|| {
                    napi_ohos::Error::from_reason("GPU surface native window is null")
                })?;
                // SAFETY: ArkUI lends a live OHNativeWindow* for this callback;
                // `retain_from_ptr` acquires our own reference and we guarantee
                // `on_surface_destroyed` drops the window before XComponent disposal.
                let retained = unsafe { NativeWindow::retain_from_ptr(raw) }
                    .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
                let scale = inst.current_scale();
                let (w, h) = inst.resolve_pixel_size(&xc_raw, window_raw, scale);
                let _ = inst
                    .xcomponent
                    .set_frame_rate(0, inst.target_hz, inst.target_hz);
                inst.applied_rate_active.set(Some(false));
                if let Some(worker) = inst.worker.borrow().as_ref() {
                    worker.surface_created(retained, w, h, scale);
                }
                Ok(())
            });

        let changed_weak = Rc::downgrade(&instance);
        instance
            .xcomponent
            .on_surface_changed(move |xc_raw, window_raw| {
                let Some(inst) = changed_weak.upgrade() else {
                    return Ok(());
                };
                let scale = inst.current_scale();
                let (w, h) = inst.resolve_pixel_size(&xc_raw, window_raw, scale);
                if let Some(worker) = inst.worker.borrow().as_ref() {
                    worker.surface_resized(w, h, scale);
                }
                Ok(())
            });

        let destroyed_weak = Rc::downgrade(&instance);
        instance.xcomponent.on_surface_destroyed(move |_, _| {
            if let Some(inst) = destroyed_weak.upgrade()
                && let Some(worker) = inst.worker.borrow().as_ref()
            {
                worker.surface_destroyed_sync();
            }
            Ok(())
        });

        instance.xcomponent.register_callback()?;
        Ok(instance)
    }

    fn current_scale(&self) -> f32 {
        f32::from_bits(self.scale_bits.load(Ordering::Relaxed)).clamp(0.5, 4.0)
    }

    fn resolve_pixel_size(
        &self,
        xc_raw: &XComponentRaw,
        window_raw: WindowRaw,
        scale: f32,
    ) -> (u32, u32) {
        if let Ok(size) = xc_raw.size(window_raw)
            && size.width > 0
            && size.height > 0
        {
            return (
                size.width.min(u64::from(u32::MAX)) as u32,
                size.height.min(u64::from(u32::MAX)) as u32,
            );
        }
        let (fw, fh) = self.frame_vp.get();
        (
            (fw * scale).round().max(1.0) as u32,
            (fh * scale).round().max(1.0) as u32,
        )
    }

    pub(crate) fn set_frame(&self, width: f32, height: f32, scale: f32) -> NodeResult<()> {
        self.frame_vp.set((width, height));
        self.scale_bits
            .store(scale.clamp(0.5, 4.0).to_bits(), Ordering::Relaxed);
        let node = self.native.borrow();
        node.width(width)?;
        node.height(height)?;
        Ok(())
    }

    pub(crate) fn density_changed(&self, scale: f32) {
        let clamped = scale.clamp(0.5, 4.0);
        let old = self.scale_bits.swap(clamped.to_bits(), Ordering::Relaxed);
        if old != clamped.to_bits()
            && let Some(worker) = self.worker.borrow().as_ref()
        {
            let (fw, fh) = self.frame_vp.get();
            let w = (fw * clamped).round().max(1.0) as u32;
            let h = (fh * clamped).round().max(1.0) as u32;
            worker.surface_resized(w, h, clamped);
        }
    }

    pub(crate) fn props(&self, props: Vec<(Arc<str>, Option<JsonValue>)>) {
        if self.disposed.get() {
            return;
        }
        if let Some(worker) = self.worker.borrow().as_ref() {
            let converted = props
                .into_iter()
                .map(|(name, value)| GpuSurfaceProperty { name, value })
                .collect();
            worker.props(converted);
        }
    }

    pub(crate) fn command(&self, name: &str, args: &JsonValue) {
        if self.disposed.get() {
            return;
        }
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.command(name.to_owned(), args.clone());
        }
    }

    pub(crate) fn on_vsync(&self, timestamp_ns: u64) {
        if self.disposed.get() {
            return;
        }
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.on_vsync(timestamp_ns);
        }
    }

    pub(crate) fn set_host_active(&self, active: bool) {
        if self.disposed.get() {
            return;
        }
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.set_host_active(active);
        }
    }

    /// Synchronizes `OH_NativeXComponent_SetExpectedFrameRateRange` on the
    /// UI thread whenever the render thread's high-refresh vote transitions.
    pub(crate) fn sync_xcomponent_rate(&self) {
        if self.disposed.get() {
            return;
        }
        let active = self.rate_active.load(Ordering::Acquire);
        if self.applied_rate_active.get() == Some(active) {
            return;
        }
        let result = if active {
            self.xcomponent
                .set_frame_rate(self.target_hz, self.target_hz, self.target_hz)
        } else {
            self.xcomponent
                .set_frame_rate(0, self.target_hz, self.idle_hz)
        };
        if result.is_ok() {
            self.applied_rate_active.set(Some(active));
        }
    }

    pub(crate) fn deactivate(&self) {
        self.active_flag.store(false, Ordering::Release);
        if let Some(worker) = self.worker.borrow().as_ref() {
            worker.set_host_active(false);
        }
    }

    pub(crate) fn dispose(&self) -> NodeResult<()> {
        self.deactivate();
        if self.disposed.replace(true) {
            return Ok(());
        }
        self.xcomponent.unregister_callbacks();
        if let Some(mut worker) = self.worker.borrow_mut().take() {
            worker.shutdown();
        }
        if self.native.borrow().is_owned() {
            self.native.borrow_mut().dispose()?;
        }
        Ok(())
    }
}

impl Drop for GpuSurfaceInstance {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}
