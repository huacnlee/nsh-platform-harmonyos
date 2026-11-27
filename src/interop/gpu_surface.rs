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
    collections::VecDeque,
    ffi::c_void,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, LazyLock, Mutex, RwLock,
        atomic::{AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use nsh_core::JsonValue;
use rustc_hash::FxHashMap;

use crate::extensions::ExtensionError;

/// Maximum interval between event-driven frame renders to treat a GPU surface
/// as actively streaming (keeping the 120 Hz vote steady without IPC churn).
const STREAM_BURST_WINDOW: Duration = Duration::from_millis(50);

/// Hold duration after the last event-driven render before dropping the
/// high-refresh vote and parking the render thread indefinitely.
const STREAM_IDLE_HOLD: Duration = Duration::from_millis(200);

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

type EventCallback = Arc<dyn Fn(Arc<str>, JsonValue) + Send + Sync>;
type SizeCallback = Arc<dyn Fn(f32, f32) + Send + Sync>;
type WakeCallback = Arc<dyn Fn() + Send + Sync>;

struct HostInner {
    surface_id: Arc<str>,
    scale_bits: Arc<AtomicU32>,
    wake: WakeCallback,
    dispatch_event: Option<EventCallback>,
    set_intrinsic_size: Option<SizeCallback>,
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
    pub(crate) fn new(
        surface_id: Arc<str>,
        scale_bits: Arc<AtomicU32>,
        wake: WakeCallback,
        dispatch_event: Option<EventCallback>,
        set_intrinsic_size: Option<SizeCallback>,
    ) -> Self {
        Self {
            inner: Arc::new(HostInner {
                surface_id,
                scale_bits,
                wake,
                dispatch_event,
                set_intrinsic_size,
            }),
        }
    }

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
        (self.inner.wake)();
    }

    /// Dispatches an NSH `NativeEvent` (`name`, `data`) back to the mounted NSH
    /// component node from any thread.
    pub fn dispatch_event(&self, name: impl Into<Arc<str>>, data: JsonValue) {
        if let Some(callback) = &self.inner.dispatch_event {
            callback(name.into(), data);
        }
    }

    /// Reports the component's intrinsic size in logical points (`vp`) back to
    /// the NSH layout engine.
    pub fn set_intrinsic_size(&self, width: f32, height: f32) {
        if width.is_finite()
            && height.is_finite()
            && (0.0..=16384.0).contains(&width)
            && (0.0..=16384.0).contains(&height)
            && let Some(callback) = &self.inner.set_intrinsic_size
        {
            callback(width, height);
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

pub(crate) type RendererFactory =
    Arc<dyn Fn(GpuSurfaceHost) -> Box<dyn GpuSurfaceRenderer> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct RegisteredGpuSurface {
    pub(crate) options: GpuSurfaceOptions,
    pub(crate) factory: RendererFactory,
}

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

/// Process-wide (or test-isolated) registry of pure-Rust GPU surface factories.
#[derive(Default)]
pub(crate) struct GpuSurfaceRegistry {
    entries: RwLock<FxHashMap<Arc<str>, RegisteredGpuSurface>>,
}

impl GpuSurfaceRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register<F, R>(
        &self,
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
        self.entries
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

    pub(crate) fn unregister(&self, name: &str) -> bool {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .is_some()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<RegisteredGpuSurface> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }
}

static GLOBAL_GPU_SURFACES: LazyLock<GpuSurfaceRegistry> = LazyLock::new(GpuSurfaceRegistry::new);

/// Registers a pure-Rust HarmonyOS GPU `XComponent` surface component under
/// `name` with [`GpuSurfaceOptions::default()`].
///
/// Typically invoked once inside an `nshm` crate's `#[native_module]`
/// registration function on `#[cfg(target_env = "ohos")]`. When NSH mounts
/// `native(name)`, `nsh-platform-harmonyos` creates the ArkUI `XComponent`
/// directly in Rust and drives the returned [`GpuSurfaceRenderer`] on a
/// dedicated render thread.
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
    GLOBAL_GPU_SURFACES.register(name, options, factory)
}

/// Unregisters a previously registered pure-Rust GPU surface component.
///
/// Returns `true` if a factory was registered under `name`. Existing mounted
/// instances continue running until unmounted.
pub fn unregister_gpu_surface(name: &str) -> bool {
    GLOBAL_GPU_SURFACES.unregister(name)
}

/// Returns `true` if a pure-Rust GPU surface factory is currently registered
/// under `name`.
pub fn has_gpu_surface(name: &str) -> bool {
    GLOBAL_GPU_SURFACES.contains(name)
}

#[cfg(target_env = "ohos")]
pub(crate) fn lookup_gpu_surface(name: &str) -> Option<RegisteredGpuSurface> {
    GLOBAL_GPU_SURFACES.get(name)
}

/// Abstraction over a retained native window handle (`NativeWindow` on OHOS,
/// drop-tracked mock handle in host unit tests).
pub(crate) trait SurfaceWindowHandle: Send + 'static {
    fn raw_ptr(&self) -> *mut c_void;
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

enum SurfaceCommand<W> {
    Created {
        window: W,
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

struct WorkerState<W> {
    commands: VecDeque<SurfaceCommand<W>>,
    frame_requested: bool,
    vsync_timestamp_ns: Option<u64>,
    host_active: bool,
    shutdown: bool,
    exited: bool,
}

impl<W> WorkerState<W> {
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

struct WorkerShared<W> {
    state: Mutex<WorkerState<W>>,
    cv: Condvar,
}

impl<W> WorkerShared<W> {
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

struct RateGuard<F: FnMut(bool)> {
    active: bool,
    callback: F,
}

impl<F: FnMut(bool)> RateGuard<F> {
    fn new(callback: F) -> Self {
        Self {
            active: false,
            callback,
        }
    }

    fn set(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            (self.callback)(active);
        }
    }
}

impl<F: FnMut(bool)> Drop for RateGuard<F> {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            (self.callback)(false);
        }
    }
}

/// Ensures any waiting `Destroy` barriers are released even if the render
/// thread exits unexpectedly or a renderer callback panics.
struct ExitGuard<W> {
    shared: Arc<WorkerShared<W>>,
}

impl<W> Drop for ExitGuard<W> {
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

/// Dedicated single-thread render loop owning a [`GpuSurfaceRenderer`] and its
/// retained native window handle.
pub(crate) struct GpuSurfaceWorker<W: SurfaceWindowHandle> {
    shared: Arc<WorkerShared<W>>,
    thread: Option<JoinHandle<()>>,
}

impl<W: SurfaceWindowHandle> GpuSurfaceWorker<W> {
    pub(crate) fn spawn(
        surface_id: Arc<str>,
        scale_bits: Arc<AtomicU32>,
        host_active: bool,
        factory: RendererFactory,
        dispatch_event: Option<EventCallback>,
        set_intrinsic_size: Option<SizeCallback>,
        rate_callback: impl FnMut(bool) + Send + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState::new(host_active)),
            cv: Condvar::new(),
        });
        let wake_shared = shared.clone();
        let wake: WakeCallback = Arc::new(move || wake_shared.wake_frame());
        let host = GpuSurfaceHost::new(
            surface_id.clone(),
            scale_bits,
            wake,
            dispatch_event,
            set_intrinsic_size,
        );
        let worker_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("nsh-gpu-{}", surface_id))
            .spawn(move || {
                run_render_thread(worker_shared, host, factory, rate_callback);
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn surface_created(&self, window: W, width_px: u32, height_px: u32, scale: f32) {
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

    pub(crate) fn surface_resized(&self, width_px: u32, height_px: u32, scale: f32) {
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

    pub(crate) fn props(&self, props: Vec<GpuSurfaceProperty>) {
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

    pub(crate) fn command(&self, name: String, args: JsonValue) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state
            .commands
            .push_back(SurfaceCommand::Command(name, args));
        self.shared.cv.notify_one();
    }

    pub(crate) fn on_vsync(&self, timestamp_ns: u64) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown || state.exited {
            return;
        }
        state.vsync_timestamp_ns = Some(timestamp_ns);
        self.shared.cv.notify_one();
    }

    pub(crate) fn set_host_active(&self, active: bool) {
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
    pub(crate) fn surface_destroyed_sync(&self) {
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
    pub(crate) fn shutdown(&mut self) {
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

impl<W: SurfaceWindowHandle> Drop for GpuSurfaceWorker<W> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_render_thread<W: SurfaceWindowHandle>(
    shared: Arc<WorkerShared<W>>,
    host: GpuSurfaceHost,
    factory: RendererFactory,
    rate_callback: impl FnMut(bool) + Send + 'static,
) {
    let _exit_guard = ExitGuard {
        shared: shared.clone(),
    };
    let mut rate = RateGuard::new(rate_callback);
    let mut renderer = match catch_unwind(AssertUnwindSafe(|| factory(host))) {
        Ok(renderer) => renderer,
        Err(_) => {
            log::error!("OHOS GPU surface factory panicked");
            return;
        }
    };

    let epoch = Instant::now();
    let mut active_window: Option<W> = None;
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
                    let ptr = window.raw_ptr();
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

#[cfg(target_env = "ohos")]
mod ohos {
    use std::{
        cell::{Cell, RefCell},
        ffi::c_void,
        ptr::NonNull,
        rc::Rc,
        sync::{
            Arc, Weak,
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        },
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
    use ohos_xcomponent_binding::NativeXComponent;

    use super::{
        EventCallback, GpuSurfaceOptions, GpuSurfaceProperty, GpuSurfaceWorker,
        RegisteredGpuSurface, SizeCallback, SurfaceWindowHandle,
    };
    use crate::{
        app::{
            frame_rate::{FrameRate, normalize_display_hz},
            runtime::{Dispatcher, Message},
        },
        render::node::{NativeNode, Result},
    };

    static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(1);

    impl SurfaceWindowHandle for NativeWindow {
        fn raw_ptr(&self) -> *mut c_void {
            self.raw().cast()
        }
    }

    /// Mounted pure-Rust HarmonyOS `XComponent` GPU surface instance.
    pub(crate) struct GpuSurfaceInstance {
        pub(crate) native: NativeNode,
        xcomponent: NativeXComponent,
        worker: RefCell<Option<GpuSurfaceWorker<NativeWindow>>>,
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
        ) -> Result<Rc<Self>> {
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
                    node.set_i32_attribute(
                        A::AccessibilityMode,
                        AccessibilityMode::Disabled.into(),
                    )?;
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

            let event_active = active_flag.clone();
            let event_control = control.clone();
            let dispatch_event: EventCallback = Arc::new(move |name, data| {
                if event_active.load(Ordering::Acquire) {
                    event_control.dispatch_event(
                        node_id,
                        nsh_core::EventKind::NativeEvent,
                        nsh_core::EventPayload::Native { name, data },
                    );
                }
            });

            let size_active = active_flag.clone();
            let size_control = control;
            let set_intrinsic_size: SizeCallback = Arc::new(move |width, height| {
                if size_active.load(Ordering::Acquire) {
                    size_control.set_component_size(
                        node_id,
                        nsh_core::Size::new(nsh_core::Points(width), nsh_core::Points(height)),
                    );
                }
            });

            let host_active = dispatcher
                .upgrade()
                .is_none_or(|route| route.active.load(Ordering::Acquire));

            let mut vote = frame_rate.vote();
            let vote_flag = rate_active.clone();
            let vote_dispatcher = dispatcher.clone();
            let worker = GpuSurfaceWorker::spawn(
                surface_id,
                scale_bits.clone(),
                host_active,
                registered.factory,
                Some(dispatch_event),
                Some(set_intrinsic_size),
                move |active| {
                    vote_flag.store(active, Ordering::Release);
                    if vote.update(active)
                        && let Some(route) = vote_dispatcher.upgrade()
                    {
                        route.send(Message::CompositeDemand);
                    }
                },
            )
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
            xc_raw: &ohos_xcomponent_binding::XComponentRaw,
            window_raw: ohos_xcomponent_binding::WindowRaw,
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

        pub(crate) fn set_frame(&self, width: f32, height: f32, scale: f32) -> Result<()> {
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

        pub(crate) fn dispose(&self) -> Result<()> {
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
}

#[cfg(target_env = "ohos")]
pub(crate) use ohos::GpuSurfaceInstance;

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use serde_json::json;

    use super::*;

    struct MockWindow {
        ptr: usize,
        dropped: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl SurfaceWindowHandle for MockWindow {
        fn raw_ptr(&self) -> *mut c_void {
            self.ptr as *mut c_void
        }
    }

    impl Drop for MockWindow {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push(format!("window_drop:{:#x}", self.ptr));
        }
    }

    struct RecordingRenderer {
        host: GpuSurfaceHost,
        thread_id: std::thread::ThreadId,
        events: Arc<Mutex<Vec<String>>>,
        continuous_remaining: usize,
        frames_seen: Arc<AtomicUsize>,
        frame_cv: Arc<(Mutex<usize>, Condvar)>,
    }

    impl RecordingRenderer {
        fn assert_same_thread(&self) {
            assert_eq!(
                std::thread::current().id(),
                self.thread_id,
                "GpuSurfaceRenderer must preserve single-thread affinity"
            );
        }
    }

    impl GpuSurfaceRenderer for RecordingRenderer {
        fn on_surface_created(
            &mut self,
            native_window: *mut c_void,
            width_px: u32,
            height_px: u32,
            scale: f32,
        ) {
            self.assert_same_thread();
            self.events.lock().unwrap().push(format!(
                "created:{:#x}:{width_px}x{height_px}@{scale:.2}",
                native_window as usize
            ));
        }

        fn on_surface_resized(&mut self, width_px: u32, height_px: u32, scale: f32) {
            self.assert_same_thread();
            self.events
                .lock()
                .unwrap()
                .push(format!("resized:{width_px}x{height_px}@{scale:.2}"));
        }

        fn on_props(&mut self, props: &[GpuSurfaceProperty]) {
            self.assert_same_thread();
            for prop in props {
                self.events.lock().unwrap().push(format!(
                    "prop:{}={:?}",
                    prop.name,
                    prop.value.as_ref()
                ));
            }
        }

        fn on_command(&mut self, name: &str, args: &JsonValue) {
            self.assert_same_thread();
            self.events
                .lock()
                .unwrap()
                .push(format!("cmd:{name}:{args}"));
        }

        fn on_frame(&mut self, _timestamp_ns: u64) -> bool {
            self.assert_same_thread();
            let count = self.frames_seen.fetch_add(1, Ordering::SeqCst) + 1;
            self.events.lock().unwrap().push(format!("frame:{count}"));
            let (lock, cv) = &*self.frame_cv;
            let mut seen = lock.lock().unwrap();
            *seen = count;
            cv.notify_all();
            if self.continuous_remaining > 0 {
                self.continuous_remaining -= 1;
                true
            } else {
                false
            }
        }

        fn on_surface_destroyed(&mut self) {
            self.assert_same_thread();
            let _ = self.host.surface_id();
            self.events.lock().unwrap().push("destroyed".into());
        }
    }

    impl Drop for RecordingRenderer {
        fn drop(&mut self) {
            self.assert_same_thread();
            self.events.lock().unwrap().push("renderer_drop".into());
        }
    }

    fn wait_frames(frame_cv: &(Mutex<usize>, Condvar), expected: usize) {
        let mut seen = frame_cv.0.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while *seen < expected {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(
                !rem.is_zero(),
                "timed out waiting for frame {expected} (saw {})",
                *seen
            );
            let (next, _) = frame_cv.1.wait_timeout(seen, rem).unwrap();
            seen = next;
        }
    }

    #[test]
    fn registry_validates_names_and_supports_register_lookup_unregister() {
        let registry = GpuSurfaceRegistry::new();
        assert!(!registry.contains("nsh-ghostty.surface"));
        assert!(
            registry
                .register("", GpuSurfaceOptions::default(), |_| DummyRenderer)
                .is_err()
        );
        assert!(
            registry
                .register("NshReserved", GpuSurfaceOptions::default(), |_| {
                    DummyRenderer
                })
                .is_err()
        );

        let opts = GpuSurfaceOptions::new()
            .with_hit_test_passthrough(false)
            .with_clip(false)
            .with_preferred_frame_rate(120);
        registry
            .register("nsh-ghostty.surface", opts, |_| DummyRenderer)
            .unwrap();
        assert!(registry.contains("nsh-ghostty.surface"));
        let entry = registry.get("nsh-ghostty.surface").unwrap();
        assert_eq!(entry.options, opts);

        assert!(registry.unregister("nsh-ghostty.surface"));
        assert!(!registry.unregister("nsh-ghostty.surface"));
        assert!(!registry.contains("nsh-ghostty.surface"));
    }

    struct DummyRenderer;
    impl GpuSurfaceRenderer for DummyRenderer {
        fn on_surface_created(&mut self, _: *mut c_void, _: u32, _: u32, _: f32) {}
        fn on_frame(&mut self, _: u64) -> bool {
            false
        }
    }

    #[test]
    fn worker_preserves_thread_affinity_lifecycle_order_and_teardown_before_window_drop() {
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let dropped = Arc::new(AtomicUsize::new(0));
        let frames_seen = Arc::new(AtomicUsize::new(0));
        let frame_cv = Arc::new((Mutex::new(0usize), Condvar::new()));
        let rate_transitions = Arc::new(Mutex::new(Vec::<bool>::new()));

        let ev_factory = events.clone();
        let frames_factory = frames_seen.clone();
        let cv_factory = frame_cv.clone();
        let factory: RendererFactory = Arc::new(move |host| {
            Box::new(RecordingRenderer {
                host,
                thread_id: std::thread::current().id(),
                events: ev_factory.clone(),
                continuous_remaining: 1,
                frames_seen: frames_factory.clone(),
                frame_cv: cv_factory.clone(),
            })
        });

        let rates_cb = rate_transitions.clone();
        let mut worker = GpuSurfaceWorker::<MockWindow>::spawn(
            Arc::from("test-1"),
            Arc::new(AtomicU32::new(2.0f32.to_bits())),
            true,
            factory,
            None,
            None,
            move |active| {
                rates_cb.lock().unwrap().push(active);
            },
        )
        .unwrap();

        // Props arriving before OnSurfaceCreated are delivered first without rendering.
        worker.props(vec![GpuSurfaceProperty {
            name: Arc::from("sessionId"),
            value: Some(json!(42)),
        }]);
        worker.command("setPostProcess".into(), json!("crt"));

        worker.surface_created(
            MockWindow {
                ptr: 0x1000,
                dropped: dropped.clone(),
                events: events.clone(),
            },
            1600,
            1000,
            2.0,
        );
        wait_frames(&frame_cv, 1);

        // First frame returned true (continuous), so VSync drives frame 2 and high refresh is on.
        worker.on_vsync(16_666_667);
        wait_frames(&frame_cv, 2);

        worker.surface_resized(1920, 1200, 2.0);
        wait_frames(&frame_cv, 3);

        // Synchronous surface destroy must complete on_surface_destroyed BEFORE window drop.
        worker.surface_destroyed_sync();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        // Recreate surface on the exact same render thread, then shutdown directly.
        worker.surface_created(
            MockWindow {
                ptr: 0x2000,
                dropped: dropped.clone(),
                events: events.clone(),
            },
            800,
            600,
            1.5,
        );
        wait_frames(&frame_cv, 4);
        worker.shutdown();
        assert_eq!(dropped.load(Ordering::SeqCst), 2);

        let log = events.lock().unwrap().clone();
        let pos = |needle: &str| {
            log.iter()
                .position(|entry| entry == needle)
                .unwrap_or_else(|| panic!("missing {needle} in {log:?}"))
        };
        assert!(pos("prop:sessionId=Some(Number(42))") < pos("created:0x1000:1600x1000@2.00"));
        assert!(pos("cmd:setPostProcess:\"crt\"") < pos("created:0x1000:1600x1000@2.00"));
        assert!(pos("created:0x1000:1600x1000@2.00") < pos("frame:1"));
        assert!(pos("resized:1920x1200@2.00") < pos("frame:3"));
        assert!(pos("destroyed") < pos("window_drop:0x1000"));
        assert!(pos("window_drop:0x1000") < pos("created:0x2000:800x600@1.50"));

        // Final shutdown order: destroyed -> renderer_drop -> window_drop:0x2000.
        let last_destroyed = log.iter().rposition(|e| e == "destroyed").unwrap();
        let renderer_drop = pos("renderer_drop");
        let second_win_drop = pos("window_drop:0x2000");
        assert!(last_destroyed < renderer_drop, "{log:?}");
        assert!(renderer_drop < second_win_drop, "{log:?}");

        let rates = rate_transitions.lock().unwrap().clone();
        assert!(rates.first().copied() == Some(true), "{rates:?}");
        assert!(rates.last().copied() == Some(false), "{rates:?}");
    }

    #[test]
    fn property_helpers_and_host_waker_work_cleanly() {
        let p_str = GpuSurfaceProperty {
            name: Arc::from("shader"),
            value: Some(json!("crt")),
        };
        assert!(!p_str.is_removed());
        assert_eq!(p_str.as_str(), Some("crt"));

        let p_num = GpuSurfaceProperty {
            name: Arc::from("sessionId"),
            value: Some(json!(7.0)),
        };
        assert_eq!(p_num.as_u64(), Some(7));
        assert_eq!(p_num.as_f64(), Some(7.0));

        let p_removed = GpuSurfaceProperty {
            name: Arc::from("shader"),
            value: None,
        };
        assert!(p_removed.is_removed());
        assert_eq!(p_removed.as_str(), None);

        let woke = Arc::new(AtomicUsize::new(0));
        let woke_cb = woke.clone();
        let sent = Arc::new(Mutex::new(Vec::<(String, JsonValue)>::new()));
        let sent_cb = sent.clone();
        let host = GpuSurfaceHost::new(
            Arc::from("nsh-gpu-9"),
            Arc::new(AtomicU32::new(2.0f32.to_bits())),
            Arc::new(move || {
                woke_cb.fetch_add(1, Ordering::SeqCst);
            }),
            Some(Arc::new(move |name, data| {
                sent_cb.lock().unwrap().push((name.to_string(), data));
            })),
            None,
        );
        assert_eq!(host.surface_id(), "nsh-gpu-9");
        assert_eq!(host.scale(), 2.0);
        host.request_frame();
        host.dispatch_event("ready", json!({"adapter": "Maleoon 916"}));
        assert_eq!(woke.load(Ordering::SeqCst), 1);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![("ready".to_owned(), json!({"adapter": "Maleoon 916"}))]
        );
    }
}
