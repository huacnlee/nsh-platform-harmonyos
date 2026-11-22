//! One engine generation. Closing it invalidates all outstanding batches and replies.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Weak},
};

use napi_ohos::{
    Env, Status,
    bindgen_prelude::Function,
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use nsh_core::MountBatch;
use nsh_engine::{Engine, EngineConfig, Environment, MountSink, Os};
use ohos_vsync_binding::Vsync;

use crate::{
    app::{environment::NshEnvironment, options::Config},
    render::{
        geometry::Size,
        mount::Mounter,
        node::{NativeNode, Result},
    },
    text::typography::OhosTextEngine,
};

type ErrorCallback = ThreadsafeFunction<String, (), String, Status, false>;
#[derive(Clone, Default)]
pub(crate) struct Errors(Rc<RefCell<ErrorState>>);
#[derive(Default)]
struct ErrorState {
    last: String,
    callback: Option<ErrorCallback>,
    #[cfg(feature = "dev")]
    overlay: Option<std::rc::Weak<crate::app::dev::Overlay>>,
    #[cfg(feature = "dev")]
    testing: Option<std::sync::Weak<nsh_testing::agent::Model>>,
}
impl Errors {
    pub(crate) fn report(&self, message: String) {
        log::error!("OHOS surface: {message}");
        let mut errors = self.0.borrow_mut();
        #[cfg(feature = "dev")]
        if let Some(model) = errors.testing.as_ref().and_then(std::sync::Weak::upgrade) {
            model.error(message.clone());
        }
        errors.last = message.clone();
        #[cfg(feature = "dev")]
        if let Some(overlay) = errors.overlay.as_ref().and_then(std::rc::Weak::upgrade) {
            overlay.show(&message);
        }
        if let Some(callback) = &errors.callback {
            callback.call(message, ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().last.clear();
        #[cfg(feature = "dev")]
        if let Some(overlay) = self
            .0
            .borrow()
            .overlay
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
        {
            overlay.hide();
        }
    }
    #[cfg(feature = "dev")]
    pub(crate) fn testing(&self, model: Option<&Arc<nsh_testing::agent::Model>>) {
        self.0.borrow_mut().testing = model.map(Arc::downgrade);
    }
    #[cfg(feature = "dev")]
    pub(crate) fn overlay(&self, overlay: Option<&Rc<crate::app::dev::Overlay>>) {
        self.0.borrow_mut().overlay = overlay.map(Rc::downgrade);
    }
    pub(crate) fn last(&self) -> String {
        self.0.borrow().last.clone()
    }
    pub(crate) fn set_handler(&self, handler: Function<'_, String, ()>) -> napi_ohos::Result<()> {
        self.0.borrow_mut().callback = Some(
            handler
                .build_threadsafe_function::<String>()
                .callee_handled::<false>()
                .build()?,
        );
        Ok(())
    }
    pub(crate) fn dispose(&self) {
        let mut state = self.0.borrow_mut();
        state.callback = None;
        #[cfg(feature = "dev")]
        {
            state.overlay = None;
            state.testing = None;
        }
    }
}

pub(crate) enum Message {
    Arkui,
    ScrollReports,
    #[cfg(feature = "dev")]
    HotFailed,
    Chunk(std::result::Result<nsh_engine::Chunk, (String, String)>),
    Back,
    /// Native controls changed Back priority outside an engine commit.
    UpdateBack,
    DismissKeyboard,
    ReconcileNavigation,
    FlushScrollLayout(nsh_core::NodeId),
    InputPaste {
        id: nsh_core::NodeId,
        text: String,
        html: Option<String>,
        files: Option<String>,
        image: Option<String>,
        report_only: bool,
    },
    DialogResult {
        id: u32,
        index: i32,
    },
    HostEnvironment(NshEnvironment),
    HostFontsChanged,
    HostWindow(
        [crate::app::session::AvoidArea; 3],
        crate::app::session::AvoidArea,
    ),
    HostDesktop(crate::services::desktop::DesktopEvent),
    DesktopMenuAction {
        id: u32,
        role: Option<nsh_core::desktop::MenuRole>,
    },
    DesktopSelectionMenu {
        target: nsh_core::NodeId,
        x: f32,
        y: f32,
    },
    KeyScroll {
        window: nsh_core::NodeId,
        key_move: nsh_core::scroll::KeyMove,
        horizontal: bool,
    },
    HostEvent(nsh_core::EventKind, nsh_core::EventPayload),
    TextSelection(
        nsh_core::NodeId,
        crate::text::selection::controller::Command,
    ),
    Composite {
        id: nsh_core::NodeId,
        generation: u32,
    },
    Scene,
    Navigation(crate::render::shader::navigation::Event),
    /// The shader thread left frames (`render::shader`).
    Shader,
    #[cfg(feature = "api-23")]
    AccessibilityEvent(nsh_core::NodeId, ohos_accessibility_binding::EventType),
    HostClosed,
    Close,
    Image(crate::media::image::Ready),
    ImageProgress(crate::media::image::Progress),
    Batch(MountBatch),
    Error(String),
    RequestFrame,
    EngineFrameRequested,
    EngineFrameCompleted,
    Reclaim(u64),
    CompositeDemand,
    VideoRetired(u64),
    VideoUi(u64, crate::media::video::UiState),
    UiTask(u64, Option<Box<dyn std::any::Any + Send>>),
}

type UiTaskFn = Box<dyn FnOnce(Box<dyn std::any::Any + Send>)>;
type UiTaskEntry = (Weak<Dispatcher>, UiTaskFn);

static NEXT_UI_TASK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    static UI_TASKS: RefCell<rustc_hash::FxHashMap<u64, UiTaskEntry>> = RefCell::default();
}

fn prune_dead_tasks() {
    UI_TASKS.with(|tasks| {
        tasks.borrow_mut().retain(|_, (dispatcher, _)| {
            dispatcher
                .upgrade()
                .is_some_and(|d| d.live.load(std::sync::atomic::Ordering::Acquire))
        });
    });
}

struct UiReplyInner {
    id: u64,
    dispatcher: Weak<Dispatcher>,
    sent: std::sync::atomic::AtomicBool,
}

impl Drop for UiReplyInner {
    fn drop(&mut self) {
        if !self.sent.swap(true, std::sync::atomic::Ordering::AcqRel)
            && let Some(dispatcher) = self.dispatcher.upgrade()
            && dispatcher.live.load(std::sync::atomic::Ordering::Acquire)
        {
            dispatcher.send(Message::UiTask(self.id, None));
        }
    }
}

pub(crate) struct UiReply<T> {
    inner: Option<Arc<UiReplyInner>>,
    _marker: std::marker::PhantomData<fn(T)>,
}

impl<T> Clone for UiReply<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: Send + 'static> UiReply<T> {
    pub(crate) fn send(&self, value: T) {
        let Some(inner) = &self.inner else {
            return;
        };
        if !inner.sent.swap(true, std::sync::atomic::Ordering::AcqRel)
            && let Some(dispatcher) = inner.dispatcher.upgrade()
            && dispatcher.live.load(std::sync::atomic::Ordering::Acquire)
        {
            dispatcher.send(Message::UiTask(inner.id, Some(Box::new(value))));
        }
    }
}

pub(crate) struct Dispatcher {
    pub(crate) owner: std::sync::Weak<crate::app::surface::Queue>,
    queue: ThreadsafeFunction<Message, (), (), Status, false>,
    pub(crate) wants_back: std::sync::atomic::AtomicBool,
    /// Keeps mounted status-bar changes hidden during native video fullscreen.
    pub(crate) video_fullscreen: std::sync::atomic::AtomicBool,
    pub(crate) active: std::sync::atomic::AtomicBool,
    pub(crate) live: std::sync::atomic::AtomicBool,
    pub(crate) manages_status_bar: bool,
    pub(crate) status_bar_configured: std::sync::atomic::AtomicBool,
    pub(crate) status_bar: std::sync::Mutex<Option<(nsh_core::StatusBarConfig, bool)>>,
    pub(crate) keyboard: std::sync::Mutex<crate::input::keyboard::Snapshot>,
}
impl Dispatcher {
    pub(crate) fn request_frame(&self) {
        self.send(Message::RequestFrame);
    }
    pub(crate) fn image(&self, ready: crate::media::image::Ready) {
        self.send(Message::Image(ready));
    }
    pub(crate) fn image_progress(&self, progress: crate::media::image::Progress) {
        self.send(Message::ImageProgress(progress));
    }
    pub(crate) fn send(&self, message: Message) {
        let status = self
            .queue
            .call(message, ThreadsafeFunctionCallMode::NonBlocking);
        if status != Status::Ok {
            log::error!("OHOS UI dispatch failed: {status:?}");
        }
    }
    /// Registers a single-shot UI-thread continuation and returns a `Send` handle
    /// that can complete it from any worker thread. Must be called on the UI thread.
    pub(crate) fn reply<T: Send + 'static>(
        this: &Weak<Self>,
        done: impl FnOnce(T) + 'static,
    ) -> UiReply<T> {
        let Some(dispatcher) = this.upgrade() else {
            return UiReply {
                inner: None,
                _marker: std::marker::PhantomData,
            };
        };
        if !dispatcher.live.load(std::sync::atomic::Ordering::Acquire) {
            return UiReply {
                inner: None,
                _marker: std::marker::PhantomData,
            };
        }
        let id = NEXT_UI_TASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        UI_TASKS.with(|tasks| {
            tasks.borrow_mut().insert(
                id,
                (
                    this.clone(),
                    Box::new(move |value| {
                        if let Ok(typed) = value.downcast::<T>() {
                            done(*typed);
                        }
                    }),
                ),
            );
        });
        UiReply {
            inner: Some(Arc::new(UiReplyInner {
                id,
                dispatcher: this.clone(),
                sent: std::sync::atomic::AtomicBool::new(false),
            })),
            _marker: std::marker::PhantomData,
        }
    }
    pub(crate) fn after(
        this: &Weak<Self>,
        delay: std::time::Duration,
        work: Box<dyn FnOnce()>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let reply = Self::reply(this, move |()| work());
        reply.inner.as_ref()?;
        Some(napi_ohos::bindgen_prelude::spawn(async move {
            tokio::time::sleep(delay).await;
            reply.send(());
        }))
    }
}
struct Sink(
    Arc<Dispatcher>,
    Option<Arc<crate::app::chunks::ResourceChunks>>,
);
impl MountSink for Sink {
    fn load_chunk(&self, id: &str) -> bool {
        self.1
            .as_ref()
            .is_some_and(|chunks| chunks.load(id, Arc::downgrade(&self.0)))
    }
    fn submit(&self, mut batch: MountBatch) {
        // Native paragraphs belong to the engine. As on Linux, the UI receives
        // completed drawing data, not the text engine's layout machinery.
        for item in &mut batch.items {
            if let nsh_core::MountItem::Text { layout, .. } = item
                && let Some(text) = layout
                    .0
                    .downcast_ref::<crate::text::typography::EngineText>()
            {
                *layout = nsh_core::TextLayout(text.native.clone());
            }
        }
        self.0.send(Message::Batch(batch));
    }
    fn report_error(&self, message: &str) {
        self.0.send(Message::Error(message.into()));
    }
    fn request_frame(&self) {
        self.0.send(Message::EngineFrameRequested);
    }
    #[cfg(feature = "dev")]
    fn hot_update_failed(&self) {
        self.0.send(Message::HotFailed);
    }
}

struct State {
    #[cfg(feature = "dev")]
    testing: Option<Arc<crate::app::testing::Session>>,
    #[cfg(feature = "dev")]
    test_closed: Arc<std::sync::atomic::AtomicBool>,
    parent: std::rc::Weak<RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    engine: Option<Engine>,
    mounter: Option<Mounter>,
    dispatcher: Weak<Dispatcher>,
    rate_driver: Option<Arc<super::frame_source::RateDriver>>,
    phase: Option<super::frame_source::Phase>,
    frame_rate: Arc<crate::app::frame_rate::FrameRate>,
    environment: Environment,
    errors: Errors,
    engine_wanted: bool,
    engine_in_flight: usize,
    native_demand: super::frame_rate::UiDemand,
    laid_out: bool,
    attached: bool,
    visible: bool,
    frame_wanted: bool,
    retiring_dispatcher: Option<Arc<Dispatcher>>,
    #[cfg(feature = "dev")]
    reload: Option<(u64, std::sync::Weak<crate::app::surface::Queue>)>,
    #[cfg(feature = "dev")]
    hot_version: Option<(u64, u64)>,
}
impl State {
    fn receive(&mut self, message: Message) -> Result<()> {
        if matches!(message, Message::Close | Message::HostClosed) {
            return self.close();
        }
        // Native retirement barriers remain valid after the engine closes.
        if let Message::Reclaim(epoch) = message {
            if let Some(mounter) = &mut self.mounter {
                mounter.reclaim(epoch);
            }
            if let Some(parent) = self.parent.upgrade()
                && let Ok(node) = parent.try_borrow()
                && node.is_owned()
            {
                let _ =
                    node.mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedRender);
            }
            return Ok(());
        }
        if let Message::VideoRetired(token) = message {
            crate::media::video::retired(token);
            return Ok(());
        }
        if let Message::VideoUi(token, state) = message {
            crate::media::video::update(token, state);
            return Ok(());
        }
        if self.engine.is_none() {
            if let Message::UiTask(id, _) = message {
                UI_TASKS.with(|tasks| {
                    tasks.borrow_mut().remove(&id);
                });
            }
            if let Some(mounter) = &self.mounter {
                let collected = mounter.shaders.collect();
                if let Some(worker) = mounter.compositor.borrow().as_ref() {
                    worker.collect();
                }
                if mounter
                    .compositor
                    .borrow()
                    .as_ref()
                    .is_none_or(|worker| worker.drained())
                {
                    self.retiring_dispatcher.take();
                }
                return collected;
            }
            return Ok(());
        }
        match message {
            Message::Scene => self.mounter.as_ref().unwrap().scene_ready()?,
            Message::Navigation(event) => self.mounter.as_ref().unwrap().navigation_event(event)?,
            Message::Shader => self.mounter.as_ref().unwrap().shaders.collect()?,
            Message::Arkui => self.mounter.as_ref().unwrap().flush_arkui()?,
            Message::ScrollReports => self.mounter.as_ref().unwrap().flush_scroll_reports()?,
            Message::TextSelection(id, command) => {
                self.mounter.as_ref().unwrap().text_selection(id, command)?
            }
            Message::Composite { id, generation } => self
                .mounter
                .as_ref()
                .unwrap()
                .composite_ready(id, generation)?,
            #[cfg(feature = "dev")]
            Message::HotFailed => {
                if let Some((token, queue)) = &self.reload
                    && let Some(queue) = queue.upgrade()
                    && let Some(version) = self.hot_version
                {
                    queue.call(
                        crate::app::surface::Message::DevReload {
                            token: *token,
                            version,
                        },
                        ThreadsafeFunctionCallMode::NonBlocking,
                    );
                }
            }
            Message::Chunk(result) => {
                let control = self.engine.as_ref().unwrap().control();
                match result {
                    Ok(chunk) => control.chunk_loaded(chunk),
                    Err((id, error)) => control.chunk_failed(id, error),
                }
            }
            Message::Back => {
                let mounter = self.mounter.as_mut().unwrap();
                mounter.back()?;
                mounter.flush_arkui()?;
                self.request_frame()?;
            }
            Message::UpdateBack => {}
            Message::DismissKeyboard => {
                self.mounter.as_ref().unwrap().dismiss_keyboard()?;
            }
            Message::FlushScrollLayout(id) => {
                self.mounter.as_ref().unwrap().flush_scroll_layout(id)?
            }
            Message::InputPaste {
                id,
                text,
                html,
                files,
                image,
                report_only,
            } => self.mounter.as_ref().unwrap().input_paste(
                id,
                text,
                html,
                files,
                image,
                report_only,
            )?,
            Message::ReconcileNavigation => {
                let mounter = self.mounter.as_mut().unwrap();
                mounter.reconcile_navigation()?;
                mounter.flush_arkui()?;
                self.request_frame()?;
            }
            Message::DialogResult { id, index } => {
                self.mounter.as_mut().unwrap().dialog_result(id, index)?;
            }
            Message::HostEnvironment(changes) => self.update_environment(changes)?,
            Message::HostFontsChanged => {
                self.engine.as_ref().unwrap().control().fonts_changed();
                self.mounter.as_ref().unwrap().invalidate_canvases()?;
                self.request_frame()?;
            }
            Message::HostWindow(visual, keyboard) if self.laid_out => {
                let changes = self
                    .mounter
                    .as_ref()
                    .unwrap()
                    .window_environment(visual, keyboard)?;
                self.update_environment(changes)?;
            }
            Message::HostWindow(..) => {}
            Message::HostDesktop(event) => {
                use crate::services::desktop::DesktopEvent;
                match event {
                    DesktopEvent::WindowState(state) => {
                        if self.environment.os.is_desktop() {
                            let became_focused = state.focused
                                && !self
                                    .environment
                                    .window_state
                                    .is_some_and(|prev| prev.focused);
                            if self.environment.window_state != Some(state) {
                                self.environment.window_state = Some(state);
                                self.engine
                                    .as_ref()
                                    .unwrap()
                                    .control()
                                    .set_environment(self.environment);
                                let mounter = self.mounter.as_ref().unwrap();
                                mounter.environment(self.environment)?;
                                mounter.on_main_window_state(state);
                            }
                            if became_focused
                                && self.laid_out
                                && let Some(mounter) = &self.mounter
                            {
                                mounter.request_desktop_focus();
                            }
                        }
                    }
                    DesktopEvent::TitleBar(edges) => {
                        if self.environment.os.is_desktop() && self.environment.title_bar != edges {
                            self.environment.title_bar = edges;
                            self.engine
                                .as_ref()
                                .unwrap()
                                .control()
                                .set_environment(self.environment);
                            let mounter = self.mounter.as_ref().unwrap();
                            mounter.environment(self.environment)?;
                            mounter.on_main_window_title_bar(edges);
                        }
                    }
                    DesktopEvent::Displays(displays) => {
                        if self.environment.os.is_desktop() {
                            self.engine
                                .as_ref()
                                .unwrap()
                                .control()
                                .set_displays(displays);
                        }
                    }
                    other => {
                        self.mounter.as_mut().unwrap().desktop_event(other)?;
                    }
                }
            }
            Message::DesktopMenuAction { id, role } => {
                self.mounter
                    .as_mut()
                    .unwrap()
                    .desktop_menu_action(id, role)?;
            }
            Message::DesktopSelectionMenu { target, x, y } => {
                self.mounter
                    .as_mut()
                    .unwrap()
                    .desktop_selection_menu(target, x, y)?;
            }
            Message::KeyScroll {
                window,
                key_move,
                horizontal,
            } => {
                self.mounter
                    .as_ref()
                    .unwrap()
                    .key_scroll(window, key_move, horizontal)?;
            }
            Message::HostEvent(kind, payload) => self
                .engine
                .as_ref()
                .unwrap()
                .control()
                .dispatch_event(nsh_core::ROOT_ID, kind, payload),
            #[cfg(feature = "api-23")]
            Message::AccessibilityEvent(id, event) => {
                self.mounter
                    .as_ref()
                    .unwrap()
                    .accessibility_event(id, event)?;
            }
            Message::Image(ready) => self.mounter.as_mut().unwrap().image_ready(ready)?,
            Message::ImageProgress(progress) => {
                self.mounter.as_ref().unwrap().image_progress(progress)
            }
            Message::UiTask(id, payload) => {
                let entry = UI_TASKS.with(|tasks| tasks.borrow_mut().remove(&id));
                if let (Some((dispatcher, work)), Some(value)) = (entry, payload)
                    && dispatcher
                        .upgrade()
                        .is_some_and(|d| d.live.load(std::sync::atomic::Ordering::Acquire))
                {
                    work(value);
                }
            }
            Message::Batch(batch) => {
                #[cfg(feature = "dev")]
                if let Some(testing) = &self.testing {
                    // Scene queries also run on this UI thread, after the
                    // native batch below finishes. No batch clone is needed.
                    testing.model.apply(&batch);
                }
                let mounter = self
                    .mounter
                    .as_mut()
                    .expect("running surface has a mounter");
                let first_batch = mounter.batch_count() == 0;
                let result = mounter.apply(batch);
                if first_batch && self.environment.os.is_desktop() && result.is_ok() {
                    mounter.request_desktop_focus();
                }
                self.retire()?;
                if let Err(error) = result {
                    #[cfg(feature = "dev")]
                    if let Some(testing) = &self.testing {
                        testing.model.error(error.to_string());
                    }
                    if matches!(error, crate::render::node::Error::Tree(_)) {
                        // Root creation is the only fatal mount item. Stale
                        // references and detach failures are logged by the mounter.
                        if let Err(cleanup) = self.close() {
                            log::error!("OHOS failed-generation cleanup: {cleanup}");
                        }
                        return Err(error);
                    }
                    log::warn!("OHOS mount resource skipped: {error}");
                }
                self.request_frame()?;
            }
            Message::Error(error) => {
                self.report(error);
            }
            Message::RequestFrame => self.request_frame()?,
            Message::EngineFrameRequested => {
                if self.laid_out
                    && self.is_active()
                    && self.engine_in_flight == 0
                    && !self
                        .phase
                        .as_ref()
                        .is_some_and(super::frame_source::Phase::is_running)
                {
                    self.dispatch_engine_frame();
                } else {
                    self.engine_wanted = true;
                }
                self.schedule_frame()?;
            }
            Message::EngineFrameCompleted => {
                debug_assert!(self.engine_in_flight > 0);
                self.engine_in_flight = self.engine_in_flight.saturating_sub(1);
                self.schedule_frame()?;
            }
            Message::CompositeDemand => self.schedule_frame()?,
            Message::Reclaim(_)
            | Message::VideoRetired(_)
            | Message::VideoUi(..)
            | Message::Close
            | Message::HostClosed => unreachable!(),
        }
        self.update_back();
        Ok(())
    }
    fn is_active(&self) -> bool {
        self.engine.is_some()
            && self.attached
            && self.visible
            && self.environment.lifecycle != nsh_engine::Lifecycle::Background
    }
    fn update_back(&mut self) {
        let active = self.is_active();
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            let was_active = dispatcher
                .active
                .swap(active, std::sync::atomic::Ordering::AcqRel);
            if was_active != active {
                if let Some(mounter) = &self.mounter {
                    mounter.shaders.wake();
                }
                // Preserve outstanding engine requests across background and
                // detach; native state gets one sample when it becomes visible.
                if active {
                    self.frame_wanted = true;
                }
                if let Err(error) = self.schedule_frame() {
                    self.report(error.to_string());
                }
            }
            dispatcher.wants_back.store(
                active && self.mounter.as_ref().is_some_and(Mounter::wants_back),
                std::sync::atomic::Ordering::Release,
            );
        }
    }
    fn update_environment(&mut self, changes: NshEnvironment) -> Result<()> {
        let environment = changes.apply(self.environment)?;
        if environment == self.environment {
            return Ok(());
        }
        if let Some(mounter) = &self.mounter {
            // Publish density before the engine records text for this environment.
            mounter.text_scale.store(
                environment.pixel_ratio.to_bits(),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        if let Some(engine) = &self.engine {
            engine.control().set_environment(environment);
        }
        let lifecycle_changed = self.environment.lifecycle != environment.lifecycle;
        self.environment = environment;
        let mut updated = Ok(());
        if let Some(mounter) = &self.mounter {
            if lifecycle_changed && environment.lifecycle != nsh_engine::Lifecycle::Active {
                updated = updated.and(mounter.cancel_input());
            }
            // Lifecycle metadata must reach the compositor even if stopping
            // an independent input/scroll owner failed.
            updated = updated.and(mounter.environment(environment));
        }
        // A failed native cleanup must not leave the old active frame vote or
        // worker visibility in place after the lifecycle has already changed.
        if lifecycle_changed {
            self.update_back();
        }
        updated
    }
    fn report(&self, error: String) {
        self.errors.report(error);
    }
    fn dispatch_engine_frame(&mut self) {
        self.engine_in_flight += 1;
        let dispatcher = self.dispatcher.clone();
        let control = self.engine.as_ref().unwrap().control();
        control.frame();
        control.flush(move |_| {
            if let Some(dispatcher) = dispatcher.upgrade() {
                dispatcher.send(Message::EngineFrameCompleted);
            }
        });
    }
    fn step_frame(&mut self, timestamp: u64) -> Result<bool> {
        let rate_driver = self.rate_driver.as_ref().unwrap().clone();
        if !self.is_active() || !rate_driver.active() {
            return Ok(false);
        }
        if std::mem::take(&mut self.engine_wanted) {
            self.dispatch_engine_frame();
        }
        self.frame_wanted = false;
        let mounter = self.mounter.as_mut().unwrap();
        let sampled = mounter.frame(Some(timestamp));
        if self.frame_rate.has_shader_demand()
            && let Some(worker) = mounter.compositor.borrow().as_ref()
        {
            worker.frame(timestamp);
        }
        self.native_demand = sampled.as_ref().copied().unwrap_or_default();
        let retired = self.retire();
        let scheduled = self.schedule_frame();
        self.update_back();
        sampled?;
        retired?;
        scheduled?;
        Ok(rate_driver.active())
    }
    fn request_frame(&mut self) -> Result<()> {
        self.frame_wanted = true;
        self.schedule_frame()
    }
    fn schedule_frame(&mut self) -> Result<()> {
        use super::frame_rate::UiDemand;
        let demand = if self.engine_wanted || self.engine_in_flight > 0 || self.frame_wanted {
            UiDemand::Uncapped
        } else {
            self.native_demand
        };
        self.frame_rate.set_ui_demand(demand);
        let active =
            self.is_active() && (demand != UiDemand::Idle || self.frame_rate.has_shader_demand());
        let rate_driver = self.rate_driver.as_ref().unwrap();
        rate_driver.set_active(active)?;
        if let Err(error) = self
            .phase
            .as_ref()
            .unwrap()
            .set_running(active, self.frame_rate.desired_range())
        {
            let _ = rate_driver.set_active(false);
            return Err(error.into());
        }
        Ok(())
    }
    fn retire(&mut self) -> Result<()> {
        if let Some(mounter) = &mut self.mounter
            && let Some(epoch) = mounter.freeze_retired()
        {
            self.rate_driver.as_ref().unwrap().retire(epoch)?;
        }
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        // Stop the UI subscription first. An uncancellable Native callback may
        // still release retired resources, but cannot sample this generation.
        self.phase.take();
        let stopped = self
            .rate_driver
            .as_ref()
            .map_or(Ok(()), |rate_driver| rate_driver.set_active(false));
        self.engine_wanted = false;
        self.engine_in_flight = 0;
        self.frame_wanted = false;
        self.native_demand = super::frame_rate::UiDemand::Idle;
        self.frame_rate
            .set_ui_demand(crate::app::frame_rate::UiDemand::Idle);
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            if self
                .mounter
                .as_ref()
                .is_some_and(|mounter| mounter.compositor.borrow().is_some())
            {
                self.retiring_dispatcher = Some(dispatcher.clone());
            }
            dispatcher
                .active
                .store(false, std::sync::atomic::Ordering::Release);
            dispatcher
                .wants_back
                .store(false, std::sync::atomic::Ordering::Release);
            dispatcher
                .live
                .store(false, std::sync::atomic::Ordering::Release);
            prune_dead_tasks();
        }
        if let Some(engine) = self.engine.take() {
            #[cfg(feature = "dev")]
            let testing = self.testing.take();
            #[cfg(feature = "dev")]
            let closed = self.test_closed.clone();
            std::thread::spawn(move || {
                drop(engine);
                #[cfg(feature = "dev")]
                {
                    drop(testing);
                    closed.store(true, std::sync::atomic::Ordering::Release);
                }
            });
        }
        let closed = if let Some(mounter) = &mut self.mounter {
            mounter.host.system.retire(&mounter.host, &self.dispatcher);
            mounter.host.apply_selected_status();
            mounter.close()
        } else {
            Ok(())
        };
        let retired = self.retire();
        stopped.and(closed).and(retired)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        prune_dead_tasks();
        if let Some(engine) = self.engine.take() {
            #[cfg(feature = "dev")]
            let testing = self.testing.take();
            std::thread::spawn(move || {
                drop(engine);
                #[cfg(feature = "dev")]
                drop(testing);
            });
        }
        self.mounter.take();
    }
}

pub(crate) struct Runtime {
    state: Rc<RefCell<State>>,
    _dispatcher: Arc<Dispatcher>,
}
pub(crate) struct App {
    #[cfg(feature = "dev")]
    pub(crate) testing: Option<Arc<crate::app::testing::Session>>,
    pub(crate) owner: std::sync::Weak<crate::app::surface::Queue>,
    pub(crate) app: nsh_engine::App,
    pub(crate) assets: Arc<dyn crate::services::assets::StreamAssets>,
    pub(crate) chunks: Option<Arc<crate::app::chunks::ResourceChunks>>,
    #[cfg(feature = "dev")]
    pub(crate) reload: Option<(u64, std::sync::Weak<crate::app::surface::Queue>)>,
}
impl App {
    pub(crate) fn rust(
        root: nsh_core::element::Root,
        assets: Arc<dyn crate::services::assets::StreamAssets>,
    ) -> Self {
        Self::new(root, assets)
    }
    pub(crate) fn new(
        app: impl Into<nsh_engine::App>,
        assets: Arc<dyn crate::services::assets::StreamAssets>,
    ) -> Self {
        Self {
            #[cfg(feature = "dev")]
            testing: None,
            owner: std::sync::Weak::new(),
            app: app.into(),
            assets,
            chunks: None,
            #[cfg(feature = "dev")]
            reload: None,
        }
    }
    pub(crate) fn grant(&mut self, granted: Option<nsh_engine::Capabilities>) {
        if let nsh_engine::App::Js(bundle) = &mut self.app {
            bundle.policy.granted = granted;
        }
    }
}
impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        env: Env,
        parent: NativeNode,
        app: App,
        size: Size,
        config: &Config,
        components: crate::interop::component::Components,
        scroll_watchers: crate::scroll::watch::ScrollWatchers,
        errors: Errors,
    ) -> napi_ohos::Result<Self> {
        config.host.check()?;
        let scale = config.host.scale()?;
        let vsync = Arc::new(
            config
                .host
                .window_id()
                .and_then(|id| Vsync::try_new_for_associated_window(id, "nsh"))
                .or_else(|| Vsync::try_new("nsh"))
                .ok_or_else(|| {
                    napi_ohos::Error::from_reason("failed to create OHOS native VSync")
                })?,
        );
        let frame_rate = crate::app::frame_rate::FrameRate::new(vsync.clone(), config.host.clone())
            .map_err(|error| {
                napi_ohos::Error::from_reason(format!("failed to start VSync rate worker: {error}"))
            })?;
        let status = frame_rate.apply_initial();
        if status != 0 {
            return Err(napi_ohos::Error::from_reason(format!(
                "failed to configure OHOS native VSync rate: {status}"
            )));
        }
        // UI and shader demand share this one NativeVSync rate owner.
        let shader_rate = frame_rate.clone();
        let state = Rc::new(RefCell::new(State {
            #[cfg(feature = "dev")]
            testing: app.testing.clone(),
            #[cfg(feature = "dev")]
            test_closed: Arc::default(),
            parent: Rc::downgrade(&parent),
            engine: None,
            mounter: None,
            dispatcher: Weak::new(),
            rate_driver: None,
            phase: None,
            frame_rate,
            errors,
            engine_wanted: false,
            engine_in_flight: 0,
            native_demand: super::frame_rate::UiDemand::Idle,
            laid_out: false,
            attached: true,
            visible: true,
            frame_wanted: false,
            retiring_dispatcher: None,
            environment: Environment::default(),
            #[cfg(feature = "dev")]
            reload: app.reload,
            #[cfg(feature = "dev")]
            hot_version: None,
        }));
        let ui_state = state.clone();
        let wake = env.create_function_from_closure::<(), (), _>("nshCommit", |_| Ok(()))?;
        let dispatcher = Arc::new(Dispatcher {
            owner: app.owner,
            queue: wake
                .build_threadsafe_function::<Message>()
                .callee_handled::<false>()
                .build_callback(move |ctx| {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut state = ui_state.borrow_mut();
                        if let Err(error) = state.receive(ctx.value) {
                            state.report(error.to_string());
                            log::error!("OHOS surface: {error}");
                        }
                    }))
                    .is_err()
                    {
                        log::error!("panic in OHOS runtime TSFN callback");
                    }
                    Ok(())
                })?,
            wants_back: std::sync::atomic::AtomicBool::new(false),
            video_fullscreen: std::sync::atomic::AtomicBool::new(false),
            active: std::sync::atomic::AtomicBool::new(config.host.is_foreground()),
            manages_status_bar: config.manages_status_bar,
            status_bar_configured: std::sync::atomic::AtomicBool::new(false),
            status_bar: std::sync::Mutex::new(None),
            keyboard: std::sync::Mutex::default(),
            live: std::sync::atomic::AtomicBool::new(true),
        });
        state.borrow_mut().dispatcher = Arc::downgrade(&dispatcher);
        state.borrow_mut().rate_driver = Some(super::frame_source::RateDriver::new(
            vsync,
            shader_rate.clone(),
            Arc::downgrade(&dispatcher),
        ));
        let weak = Rc::downgrade(&state);
        let rate_driver = state.borrow().rate_driver.as_ref().unwrap().clone();
        state.borrow_mut().phase = Some(super::frame_source::Phase::new(
            env,
            &config.host,
            rate_driver,
            move |timestamp| {
                let Some(state) = weak.upgrade() else {
                    return Ok(false);
                };
                let mut state = state.try_borrow_mut().map_err(reentry)?;
                match state.step_frame(timestamp) {
                    Ok(active) => Ok(active),
                    Err(error) => {
                        state.report(error.to_string());
                        if let Some(rate_driver) = &state.rate_driver {
                            let _ = rate_driver.set_active(false);
                        }
                        Ok(false)
                    }
                }
            },
        )?);
        let environment = Environment {
            os: Os::Android,
            platform: Some(crate::PLATFORM),
            clipboard_images: Some(true),
            canvas_render: true,
            web_input: true,
            web_evaluate: true,
            notifications: true,
            speech: config.host.speech_available,
            share: config.host.share_available,
            file_requests: true,
            audio: true,
            pixel_ratio: scale,
            window: size.into(),
            ..Environment::default()
        };
        let environment = config.host.environment().apply(environment)?;
        let mut environment = config.environment.clone().apply(environment)?;
        let os = crate::services::desktop::os_for_idiom(environment.idiom);
        environment.os = os;
        let initial_displays = if os.is_desktop() {
            let (window_state, title_bar, displays) = config.host.desktop_snapshot();
            environment.window_state = window_state;
            environment.title_bar = title_bar;
            displays
        } else {
            Vec::new()
        };
        dispatcher.keyboard.lock().unwrap().desktop = os.is_desktop();
        state.borrow_mut().environment = environment;
        let access = match &app.app {
            nsh_engine::App::Js(bundle) => bundle.policy.access(),
            nsh_engine::App::Rust(_) => nsh_engine::Policy::default().access(),
        };
        let engine_assets: Arc<dyn nsh_engine::Assets> = app.assets.clone();
        let images = crate::media::image::Loader::new(
            engine_assets.clone(),
            access.clone(),
            Arc::downgrade(&dispatcher),
        );
        let access = Arc::new(access);
        let assets = app.assets.clone();
        let main_window_options = app.app.main_window().cloned();
        let storage: Option<Arc<dyn nsh_engine::StorageBackend>> =
            config.storage.as_ref().map(|directory| {
                nsh_engine::FileStorage::open(directory) as Arc<dyn nsh_engine::StorageBackend>
            });
        let main_window_launch = if os.is_desktop() {
            main_window_options.as_ref().and_then(|options| {
                let kept = storage
                    .as_deref()
                    .and_then(|backend| options.kept_placement(backend, &initial_displays));
                crate::services::desktop::wire_main_window_launch(options, kept)
            })
        } else {
            None
        };
        #[allow(unused_mut)]
        let mut engine_config = EngineConfig {
            app: app.app,
            viewport: size.into(),
            trace: false,
            content_culling: true,
            network: Some(crate::services::net::network()),
            websocket: None,
            refuse_connections: None,
            filesystem: None,
            assets: Some(engine_assets),
            environment,
            header_heights: crate::services::desktop::header_heights_for_os(os),
            limits: config.limits,
            ui_sets: crate::ui_sets(),
            runtime: None,
            support_dir: None,
            data_dir: Some(config.host.data_directory()),
            cache: config.cache.clone(),
            storage,
            clock: Default::default(),
            context: Default::default(),
            pinned: Default::default(),
            clipboard: None,
            host: config.channel.clone(),
            picker: None,
            speech: None,
            audio: None,
            windows: None,
            login_item: None,
        };
        #[cfg(feature = "dev")]
        if let Some(testing) = &app.testing {
            engine_config.data_dir = Some(testing.directory.clone());
            engine_config.cache = nsh_engine::CacheOptions::at(nsh_engine::CacheLocation::Dir(
                testing.directory.join("cache"),
            ));
            testing.model.set_up_engine(&mut engine_config);
        }
        let host_tokens = config.host_tokens.clone();
        let host_fonts = config.fonts.clone();
        let fonts = crate::text::fonts::Fonts::shared();
        let engine_fonts = fonts.clone();
        let scale = environment.pixel_ratio;
        let text_scale = Arc::new(std::sync::atomic::AtomicU32::new(scale.to_bits()));
        let engine_scale = text_scale.clone();
        // Engine::start_with installs Core's image context from these assets,
        // access policy and disk cache. Its transport also serves SVG/font loads.
        nsh_core::image::loader::set_transport(crate::media::image::transport());
        let engine = Engine::start_with(
            engine_config,
            move || OhosTextEngine::new(engine_scale, engine_fonts),
            Sink(dispatcher.clone(), app.chunks),
            move |control| {
                if !initial_displays.is_empty() {
                    control.set_displays(initial_displays);
                }
                if let Some(tokens) = host_tokens {
                    control.set_tokens(Some(tokens));
                }
                for font in host_fonts {
                    control.register_font(font);
                }
            },
        )
        .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        images.set_disk_cache(engine.image_cache());
        let control = engine.control();
        #[cfg(feature = "dev")]
        if let Some(testing) = &app.testing {
            testing.model.engine_started(control.clone());
        }
        let mut initial_main_window = crate::services::desktop::MainWindowState::default();
        if let Some(options) = &main_window_options {
            initial_main_window.apply_attributes(&options.attributes);
        }
        let context = Rc::new(crate::render::mount::UiContext {
            theme: RefCell::default(),
            cursor: Default::default(),
            shaders: crate::render::shader::Shaders::new(shader_rate),
            fullscreen_video: RefCell::default(),
            scroll_reports: RefCell::default(),
            scroll_watchers,
            flushing_batch: std::cell::Cell::new(false),
            scene: RefCell::default(),
            compositor: std::cell::RefCell::default(),
            composite_nodes: std::cell::RefCell::default(),
            key_focus: RefCell::default(),
            selected_text: RefCell::default(),
            accessibility_dirty: RefCell::default(),
            accessibility_focus: RefCell::default(),
            accessibility_live_regions: std::cell::Cell::new(0),
            accessibility_wrappers: std::cell::Cell::new(0),
            leaving_nodes: std::cell::Cell::new(0),
            pending_delete_count: std::cell::Cell::new(0),
            unreclaimed_delete_count: std::cell::Cell::new(0),
            arkui: crate::render::arkui::empty_batch(),
            arkui_views: Default::default(),
            deferred_webs: RefCell::default(),
            host: config.host.clone(),
            dispatcher: Arc::downgrade(&dispatcher),
            env,
            scale: std::cell::Cell::new(scale),
            text_scale,
            control: control.clone(),
            animator: crate::render::animation::Animator::new(control, Arc::downgrade(&dispatcher)),
            images,
            access,
            assets,
            components,
            touches: RefCell::default(),
            canvas_bytes: Rc::new(Cell::new(0)),
            fonts,
            platform: Cell::new(environment),
            main_window: RefCell::new(initial_main_window),
            appearance: Cell::new(nsh_core::platform::Appearance::System),
        });
        let mut mounter = Mounter::new(parent, size, context).map_err(napi_error)?;
        if let Some(launch) = main_window_launch {
            mounter.note_main_window_launch_applied();
            config.host.desktop_command(&launch)?;
        }
        state.borrow_mut().mounter = Some(mounter);
        state.borrow_mut().engine = Some(engine);
        let runtime = Self {
            state,
            _dispatcher: dispatcher,
        };
        // Keep rollback ownership before registering with the shared session.
        // A rejected route must still close the engine and detach its root.
        config.host.attach(Arc::downgrade(&runtime._dispatcher))?;
        Ok(runtime)
    }
    #[cfg(feature = "dev")]
    pub(crate) fn test_close(&self) -> napi_ohos::Result<Arc<std::sync::atomic::AtomicBool>> {
        self.close()?;
        Ok(self.state.borrow().test_closed.clone())
    }
    #[cfg(feature = "dev")]
    pub(crate) fn test_query(
        &self,
        query: nsh_dev::protocol::SceneQuery,
    ) -> std::result::Result<serde_json::Value, String> {
        let state = self.state.try_borrow().map_err(|e| e.to_string())?;
        state
            .mounter
            .as_ref()
            .ok_or("the test app has no mounted views")?
            .test_query(query)
    }
    #[cfg(feature = "dev")]
    pub(crate) fn test_type(&self, text: &str) -> std::result::Result<(), String> {
        let state = self.state.try_borrow().map_err(|e| e.to_string())?;
        state
            .mounter
            .as_ref()
            .ok_or("the test app has no mounted views")?
            .test_type(text)
    }
    #[cfg(feature = "dev")]
    pub(crate) fn test_key(&self, kind: &str) -> std::result::Result<(), String> {
        let state = self.state.try_borrow().map_err(|e| e.to_string())?;
        state
            .mounter
            .as_ref()
            .ok_or("the test app has no mounted views")?
            .test_key(kind)
    }
    #[cfg(feature = "dev")]
    pub(crate) fn test_paste(
        &self,
        content: nsh_core::PasteEvent,
    ) -> std::result::Result<(), String> {
        let state = self.state.try_borrow().map_err(|e| e.to_string())?;
        state
            .mounter
            .as_ref()
            .ok_or("the test app has no mounted views")?
            .test_paste(content)
    }
    pub(crate) fn resize(&self, size: Size) -> napi_ohos::Result<()> {
        let mut state = self.state.try_borrow_mut().map_err(reentry)?;
        if state.engine.is_none() {
            return Ok(());
        }
        let window = size.into();
        let changed = state.environment.window != window;
        state.environment.window = window;
        if let Some(mounter) = &state.mounter {
            mounter.resize(size).map_err(napi_error)?;
        }
        if changed && let Some(engine) = &state.engine {
            // `Message::Resize` only relayouts the viewport; reactive
            // environment signals (`windowSize()`, `window()`, `md:`/`lg:`
            // classes) read `Environment::window`.
            engine.control().set_environment(state.environment);
        }
        Ok(())
    }
    pub(crate) fn set_attached(&self, attached: bool) -> napi_ohos::Result<()> {
        let mut state = self.state.try_borrow_mut().map_err(reentry)?;
        if state.engine.is_none() {
            return Ok(());
        }
        let changed = state.attached != attached;
        state.attached = attached;
        let mut updated = Ok(());
        if let Some(mounter) = &state.mounter {
            if !attached {
                updated = updated.and(mounter.cancel_input().map_err(napi_error));
            }
            if changed {
                updated = updated.and(mounter.native_context(attached).map_err(napi_error));
            }
        }
        state.update_back();
        updated
    }
    pub(crate) fn set_visible(&self, visible: bool) -> napi_ohos::Result<()> {
        let mut state = self.state.try_borrow_mut().map_err(reentry)?;
        if state.engine.is_none() {
            return Ok(());
        }
        state.visible = visible;
        let updated = if !visible && let Some(mounter) = &state.mounter {
            mounter.cancel_input().map_err(napi_error)
        } else {
            Ok(())
        };
        state.update_back();
        updated
    }
    #[cfg(feature = "dev")]
    pub(crate) fn hot_update(&self, update: nsh_engine::HotUpdate, version: (u64, u64)) -> bool {
        if let Ok(mut state) = self.state.try_borrow_mut()
            && state.engine.is_some()
        {
            state.hot_version = Some(version);
            state.engine.as_ref().unwrap().control().hot_update(update);
            return true;
        }
        false
    }
    pub(crate) fn layout_changed(&self) -> napi_ohos::Result<()> {
        let mut state = self.state.try_borrow_mut().map_err(reentry)?;
        let first_layout = !state.laid_out;
        state.laid_out = true;
        if first_layout
            && state.environment.os.is_desktop()
            && let Some(mounter) = &state.mounter
        {
            mounter.request_desktop_focus();
        }
        let changes = if let Some(mounter) = &state.mounter {
            mounter
                .host
                .window_areas()
                .map(|(visual, keyboard)| mounter.window_environment(visual, keyboard))
                .transpose()
                .map_err(napi_error)?
        } else {
            None
        };
        if let Some(changes) = changes {
            state.update_environment(changes).map_err(napi_error)?;
        }
        Ok(())
    }
    pub(crate) fn set_environment(&self, changes: NshEnvironment) -> napi_ohos::Result<()> {
        let mut state = self.state.try_borrow_mut().map_err(reentry)?;
        if state.engine.is_none() {
            return Ok(());
        }
        state.update_environment(changes).map_err(napi_error)
    }
    pub(crate) fn set_tokens(&self, tokens: Option<nsh_engine::Tokens>) -> napi_ohos::Result<()> {
        let state = self.state.try_borrow().map_err(reentry)?;
        if let Some(engine) = &state.engine {
            engine.control().set_tokens(tokens);
        }
        Ok(())
    }
    pub(crate) fn register_font(&self, data: Vec<u8>) -> napi_ohos::Result<()> {
        let state = self.state.try_borrow().map_err(reentry)?;
        if let Some(engine) = &state.engine {
            engine.control().register_font(data);
        }
        Ok(())
    }
    pub(crate) fn open_url_scheme(&self, url: String) -> napi_ohos::Result<()> {
        let state = self.state.try_borrow().map_err(reentry)?;
        if let Some(engine) = &state.engine {
            engine.control().dispatch_event(
                nsh_core::ROOT_ID,
                nsh_core::EventKind::UrlScheme,
                nsh_core::EventPayload::Map(vec![(
                    nsh_core::url_scheme::URL,
                    nsh_core::PayloadValue::Str(url),
                )]),
            );
        }
        Ok(())
    }
    pub(crate) fn is_running(&self) -> bool {
        self._dispatcher
            .live
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn is_busy(&self) -> bool {
        self.state.try_borrow_mut().is_err()
    }
    pub(crate) fn close(&self) -> napi_ohos::Result<()> {
        self.state
            .try_borrow_mut()
            .map_err(reentry)?
            .close()
            .map_err(napi_error)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::error!("OHOS engine close: {error}");
            self._dispatcher.send(Message::Close);
        }
    }
}
fn napi_error(error: crate::render::node::Error) -> napi_ohos::Error {
    napi_ohos::Error::from_reason(error.to_string())
}
fn reentry(_: impl std::fmt::Display) -> napi_ohos::Error {
    napi_ohos::Error::from_reason("defer engine mutation until the next host turn")
}
