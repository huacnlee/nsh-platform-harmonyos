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
    app::{
        environment::NshEnvironment,
        options::Config,
        preboot::{FlushGuard, MountGate},
    },
    render::{
        geometry::{Size, WindowRect, full_window_matches},
        mount::{Mounter, window_avoid_environment},
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
    DesktopMenuBar(crate::services::desktop::MenuBarKeyAction),
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
    ShadersUnavailable,
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
    WebEvalResult {
        target_id: u32,
        key: String,
        envelope: String,
    },
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

#[derive(Default)]
struct Inbox {
    queue: std::collections::VecDeque<Message>,
    scheduled: bool,
}

pub(crate) struct Dispatcher {
    pub(crate) owner: std::sync::Weak<crate::app::surface::Queue>,
    inbox: Arc<std::sync::Mutex<Inbox>>,
    wake: ThreadsafeFunction<(), (), (), Status, false>,
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
        let should_wake = {
            let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.queue.push_back(message);
            if inbox.scheduled {
                false
            } else {
                inbox.scheduled = true;
                true
            }
        };
        if should_wake {
            self.call_wake();
        }
    }
    fn call_wake(&self) {
        let status = self.wake.call((), ThreadsafeFunctionCallMode::NonBlocking);
        if status != Status::Ok {
            self.inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .scheduled = false;
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
struct Sink {
    gate: Arc<MountGate<Arc<Dispatcher>>>,
    chunks: Option<Arc<crate::app::chunks::ResourceChunks>>,
    control: Arc<std::sync::Mutex<Option<nsh_engine::EngineControl>>>,
}
impl MountSink for Sink {
    fn load_chunk(&self, id: &str) -> bool {
        let Some(chunks) = &self.chunks else {
            return false;
        };
        self.gate.on_load_chunk(
            id,
            |id| {
                let control = self
                    .control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                control.is_some_and(|control| chunks.load_to_control(id, control))
            },
            |dispatcher, id| chunks.load(id, Arc::downgrade(dispatcher)),
        )
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
        self.gate.on_submit(batch, |dispatcher, batch| {
            dispatcher.send(Message::Batch(batch))
        });
    }
    fn report_error(&self, message: &str) {
        self.gate.on_error(message.into(), |dispatcher, error| {
            dispatcher.send(Message::Error(error))
        });
    }
    fn request_frame(&self) {
        self.gate
            .on_request_frame(|dispatcher| dispatcher.send(Message::EngineFrameRequested));
    }
    #[cfg(feature = "dev")]
    fn hot_update_failed(&self) {
        self.gate
            .on_hot_failed(|dispatcher| dispatcher.send(Message::HotFailed));
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
                            let prev_state = self.environment.window_state;
                            let became_focused =
                                state.focused && !prev_state.is_some_and(|prev| prev.focused);
                            let lost_focus =
                                !state.focused && prev_state.is_some_and(|prev| prev.focused);
                            let mode_changed = prev_state.map(|prev| prev.mode) != Some(state.mode);
                            if prev_state != Some(state) {
                                self.environment.window_state = Some(state);
                                let effective = self.effective_environment(self.environment);
                                self.engine
                                    .as_ref()
                                    .unwrap()
                                    .control()
                                    .set_environment(effective);
                                let mounter = self.mounter.as_mut().unwrap();
                                mounter.environment(effective)?;
                                mounter.on_main_window_state(state);
                                if mode_changed || lost_focus {
                                    mounter.on_main_window_chrome_change(state)?;
                                }
                            }
                            if became_focused
                                && self.attached
                                && self.laid_out
                                && let Some(mounter) = &self.mounter
                                && mounter.batch_count() > 0
                            {
                                mounter.request_desktop_focus();
                            }
                        }
                    }
                    DesktopEvent::TitleBar(edges) => {
                        if self.environment.os.is_desktop() && self.environment.title_bar != edges {
                            self.environment.title_bar = edges;
                            let effective = self.effective_environment(self.environment);
                            self.engine
                                .as_ref()
                                .unwrap()
                                .control()
                                .set_environment(effective);
                            let mounter = self.mounter.as_ref().unwrap();
                            mounter.environment(effective)?;
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
            Message::DesktopMenuBar(action) => {
                self.mounter
                    .as_mut()
                    .unwrap()
                    .desktop_menu_bar_key(action)?;
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
                self.apply_batch(batch)?;
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
            Message::ShadersUnavailable => {
                if let Some(engine) = &self.engine {
                    engine.control().shaders_unavailable();
                }
            }
            Message::WebEvalResult {
                target_id,
                key,
                envelope,
            } => {
                crate::media::webview::on_eval_result(target_id, &key, &envelope);
            }
            Message::Reclaim(_)
            | Message::VideoRetired(_)
            | Message::VideoUi(..)
            | Message::Close
            | Message::HostClosed => unreachable!(),
        }
        self.update_back();
        Ok(())
    }
    fn apply_batch(&mut self, batch: MountBatch) -> Result<()> {
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
        if first_batch
            && self.attached
            && self.laid_out
            && self.environment.os.is_desktop()
            && result.is_ok()
        {
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
    fn effective_environment(
        &self,
        mut env: nsh_core::platform::Environment,
    ) -> nsh_core::platform::Environment {
        if let Some(mounter) = &self.mounter {
            let inset = mounter.menu_bar_inset();
            if inset > 0.0 {
                env.window.height = nsh_core::Points((env.window.height.0 - inset).max(0.0));
            }
        }
        env
    }
    fn update_environment(&mut self, changes: NshEnvironment) -> Result<()> {
        let mut environment = changes.apply(self.environment)?;
        let idiom_changed = environment.idiom != self.environment.idiom;
        if idiom_changed {
            let os = crate::services::desktop::os_for_idiom(environment.idiom);
            environment.os = os;
            if let Some(dispatcher) = self.dispatcher.upgrade() {
                dispatcher.keyboard.lock().unwrap().desktop = os.is_desktop();
            }
        }
        if environment == self.environment {
            return Ok(());
        }
        let effective = self.effective_environment(environment);
        if let Some(mounter) = &self.mounter {
            // Publish density before the engine records text for this environment.
            mounter.text_scale.store(
                effective.pixel_ratio.to_bits(),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        if let Some(engine) = &self.engine {
            engine.control().set_environment(effective);
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
            updated = updated.and(mounter.environment(effective));
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
            let storage = self.mounter.as_ref().map(|m| m.host.data_directory());
            defer_engine_drop(storage, move || {
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
            let storage = self.mounter.as_ref().map(|m| m.host.data_directory());
            defer_engine_drop(storage, move || {
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
/// A running NSH engine created without requiring `UIContext` or `Mounter`.
/// Shared by both preboot (`NshPreload`) and direct `Runtime::start`.
pub(crate) struct BootedEngine {
    engine: Option<Engine>,
    gate: Arc<MountGate<Arc<Dispatcher>>>,
    environment: Environment,
    size: Size,
    size_px: (i32, i32),
    text_scale: Arc<std::sync::atomic::AtomicU32>,
    fonts: Arc<std::sync::Mutex<crate::text::fonts::Fonts>>,
    access: Arc<nsh_engine::Access>,
    assets: Arc<dyn crate::services::assets::StreamAssets>,
    chunks: Option<Arc<crate::app::chunks::ResourceChunks>>,
    main_window_options: Option<nsh_core::desktop::MainWindowOptions>,
    main_window_launch: Option<crate::services::desktop::WireCommand>,
    storage_dir: std::path::PathBuf,
    owner: std::sync::Weak<crate::app::surface::Queue>,
    #[cfg(feature = "dev")]
    testing: Option<Arc<crate::app::testing::Session>>,
    #[cfg(feature = "dev")]
    reload: Option<(u64, std::sync::Weak<crate::app::surface::Queue>)>,
}

impl BootedEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        app: App,
        size: Size,
        size_px: (i32, i32),
        config: &Config,
        pending_urls: Vec<String>,
        gate: Arc<MountGate<Arc<Dispatcher>>>,
        flush_guard: Option<FlushGuard<Arc<Dispatcher>>>,
        assume_full_window: bool,
    ) -> napi_ohos::Result<Self> {
        config.host.check()?;
        let scale = config.host.scale()?;
        let environment = Environment {
            os: Os::Android,
            platform: Some(crate::PLATFORM),
            clipboard_images: Some(true),
            canvas_render: true,
            webview_requests: true,
            auth_session: true,
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
        let mut environment = config.host.environment().apply(environment)?;
        // Populate initial safeArea before Frame 1 for full-window surfaces so
        // the very first layout matches the real window without a Frame-2 jump.
        let viewport_px = (
            size.width as f64 * scale as f64,
            size.height as f64 * scale as f64,
        );
        let apply_window_avoid =
            assume_full_window || full_window_matches(viewport_px, config.host.window_size());
        if apply_window_avoid && let Some((visual, keyboard)) = config.host.window_areas() {
            let avoid = window_avoid_environment(
                WindowRect {
                    x: 0.0,
                    y: 0.0,
                    width: viewport_px.0,
                    height: viewport_px.1,
                },
                scale as f64,
                visual,
                keyboard,
            );
            environment = avoid.apply(environment)?;
        }
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
        let access = match &app.app {
            nsh_engine::App::Js(bundle) => bundle.policy.access(),
            nsh_engine::App::Rust(_) => nsh_engine::Policy::default().access(),
        };
        let engine_assets: Arc<dyn nsh_engine::Assets> = app.assets.clone();
        let access = Arc::new(access);
        let assets = app.assets.clone();
        // Wait for any retiring engine using the same host storage/data directory
        // to finish flushing before opening the new FileStorage/Engine.
        let host_data_dir = config.host.data_directory();
        await_engine_drop(Some(&host_data_dir));
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
            support_dir: Some(host_data_dir.clone()),
            data_dir: None,
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
        let sink_control = Arc::new(std::sync::Mutex::new(None));
        let setup_control = sink_control.clone();
        let chunks = app.chunks.clone();
        // Engine::start_with installs Core's image context from these assets,
        // access policy and disk cache. Its transport also serves SVG/font loads.
        nsh_core::image::loader::set_transport(crate::media::image::transport());
        let engine = Engine::start_with(
            engine_config,
            move || OhosTextEngine::new(engine_scale, engine_fonts),
            Sink {
                gate: gate.clone(),
                chunks: app.chunks,
                control: sink_control,
            },
            move |control| {
                *setup_control.lock().unwrap_or_else(|e| e.into_inner()) = Some(control.clone());
                if !initial_displays.is_empty() {
                    control.set_displays(initial_displays);
                }
                if let Some(tokens) = host_tokens {
                    control.set_tokens(Some(tokens));
                }
                for font in host_fonts {
                    control.register_font(font);
                }
                for url in pending_urls {
                    control.dispatch_event(
                        nsh_core::ROOT_ID,
                        nsh_core::EventKind::UrlScheme,
                        nsh_core::EventPayload::Map(vec![(
                            nsh_core::url_scheme::URL,
                            nsh_core::PayloadValue::Str(url),
                        )]),
                    );
                }
                if let Some(guard) = flush_guard {
                    control.flush(move |_| {
                        guard.finish();
                    });
                }
            },
        )
        .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        #[cfg(feature = "dev")]
        if let Some(testing) = &app.testing {
            testing.model.engine_started(engine.control());
        }
        Ok(Self {
            engine: Some(engine),
            gate,
            environment,
            size,
            size_px,
            text_scale,
            fonts,
            access,
            assets,
            chunks,
            main_window_options,
            main_window_launch,
            storage_dir: host_data_dir,
            owner: app.owner,
            #[cfg(feature = "dev")]
            testing: app.testing,
            #[cfg(feature = "dev")]
            reload: app.reload,
        })
    }

    pub(crate) fn size_px(&self) -> (i32, i32) {
        self.size_px
    }

    pub(crate) fn set_owner(&mut self, owner: std::sync::Weak<crate::app::surface::Queue>) {
        self.owner = owner;
    }

    fn close_unattached(&mut self) {
        self.gate.close();
        if let Some(engine) = self.engine.take() {
            let storage = Some(self.storage_dir.clone());
            defer_engine_drop(storage, move || drop(engine));
        }
    }
}

impl Drop for BootedEngine {
    fn drop(&mut self) {
        if self.engine.is_some() {
            self.close_unattached();
        }
    }
}

struct UiShell {
    state: Rc<RefCell<State>>,
    dispatcher: Arc<Dispatcher>,
    shader_rate: Arc<crate::app::frame_rate::FrameRate>,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        env: Env,
        parent: NativeNode,
        app: App,
        size: Size,
        size_px: (i32, i32),
        attached: bool,
        visible: bool,
        pending_urls: Vec<String>,
        config: &Config,
        components: crate::interop::component::Components,
        scroll_watchers: crate::scroll::watch::ScrollWatchers,
        errors: Errors,
    ) -> napi_ohos::Result<Self> {
        config.host.check()?;
        let shell = Self::prepare_ui_shell(
            env,
            &parent,
            app.owner.clone(),
            attached,
            visible,
            config,
            errors,
            #[cfg(feature = "dev")]
            app.testing.clone(),
            #[cfg(feature = "dev")]
            app.reload.clone(),
        )?;
        let gate = MountGate::new_live(shell.dispatcher.clone());
        let booted =
            BootedEngine::start(app, size, size_px, config, pending_urls, gate, None, false)?;
        Self::finish_attach(
            env,
            parent,
            shell,
            booted,
            config,
            components,
            scroll_watchers,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_preboot(
        env: Env,
        parent: NativeNode,
        mut booted: BootedEngine,
        owner: std::sync::Weak<crate::app::surface::Queue>,
        attach_size: Option<Size>,
        attached: bool,
        visible: bool,
        config: &Config,
        components: crate::interop::component::Components,
        scroll_watchers: crate::scroll::watch::ScrollWatchers,
        errors: Errors,
    ) -> napi_ohos::Result<Self> {
        config.host.check()?;
        booted.set_owner(owner.clone());
        let boot_size = booted.size;
        let shell = Self::prepare_ui_shell(
            env,
            &parent,
            owner,
            attached,
            visible,
            config,
            errors,
            #[cfg(feature = "dev")]
            booted.testing.clone(),
            #[cfg(feature = "dev")]
            booted.reload.clone(),
        )?;
        let runtime = Self::finish_attach(
            env,
            parent,
            shell,
            booted,
            config,
            components,
            scroll_watchers,
            true,
        )?;
        if let Some(size) = attach_size
            && size != boot_size
        {
            runtime.resize(size)?;
        }
        Ok(runtime)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_ui_shell(
        env: Env,
        parent: &NativeNode,
        owner: std::sync::Weak<crate::app::surface::Queue>,
        attached: bool,
        visible: bool,
        config: &Config,
        errors: Errors,
        #[cfg(feature = "dev")] testing: Option<Arc<crate::app::testing::Session>>,
        #[cfg(feature = "dev")] reload: Option<(u64, std::sync::Weak<crate::app::surface::Queue>)>,
    ) -> napi_ohos::Result<UiShell> {
        config.host.scale()?;
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
            testing,
            #[cfg(feature = "dev")]
            test_closed: Arc::default(),
            parent: Rc::downgrade(parent),
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
            attached,
            visible,
            frame_wanted: false,
            retiring_dispatcher: None,
            environment: Environment::default(),
            #[cfg(feature = "dev")]
            reload,
            #[cfg(feature = "dev")]
            hot_version: None,
        }));
        let ui_state = state.clone();
        let inbox = Arc::new(std::sync::Mutex::new(Inbox::default()));
        let route_ref: Rc<std::cell::OnceCell<Weak<Dispatcher>>> =
            Rc::new(std::cell::OnceCell::new());
        let drain_inbox = inbox.clone();
        let drain_route = route_ref.clone();
        let wake = env.create_function_from_closure::<(), (), _>("nshCommit", |_| Ok(()))?;
        let dispatcher = Arc::new(Dispatcher {
            owner,
            inbox,
            wake: wake
                .build_threadsafe_function::<()>()
                .callee_handled::<false>()
                .build_callback(move |_| {
                    for pass in 0..4 {
                        let messages = {
                            let mut inbox = drain_inbox.lock().unwrap_or_else(|e| e.into_inner());
                            if inbox.queue.is_empty() {
                                inbox.scheduled = false;
                                break;
                            }
                            inbox.scheduled = true;
                            std::mem::take(&mut inbox.queue)
                        };
                        let deferred =
                            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let Ok(mut state) = ui_state.try_borrow_mut() else {
                                    // Re-enqueue on synchronous re-entry at front to preserve FIFO order.
                                    let mut inbox =
                                        drain_inbox.lock().unwrap_or_else(|e| e.into_inner());
                                    for msg in messages.into_iter().rev() {
                                        inbox.queue.push_front(msg);
                                    }
                                    inbox.scheduled = true;
                                    drop(inbox);
                                    if let Some(route) = drain_route.get().and_then(Weak::upgrade) {
                                        route.call_wake();
                                    }
                                    return true;
                                };
                                for msg in messages {
                                    if let Err(error) = state.receive(msg) {
                                        state.report(error.to_string());
                                        log::error!("OHOS surface: {error}");
                                    }
                                }
                                false
                            })) {
                                Ok(deferred) => deferred,
                                Err(_) => {
                                    log::error!("panic in OHOS runtime TSFN callback");
                                    false
                                }
                            };
                        if deferred {
                            break;
                        }
                        if pass == 3 {
                            let has_more = {
                                let mut inbox =
                                    drain_inbox.lock().unwrap_or_else(|e| e.into_inner());
                                let has_more = !inbox.queue.is_empty();
                                inbox.scheduled = has_more;
                                has_more
                            };
                            if has_more
                                && let Some(route) = drain_route.get().and_then(Weak::upgrade)
                            {
                                route.call_wake();
                            }
                        }
                    }
                    Ok(())
                })?,
            wants_back: std::sync::atomic::AtomicBool::new(false),
            video_fullscreen: std::sync::atomic::AtomicBool::new(false),
            active: std::sync::atomic::AtomicBool::new(
                attached && visible && config.host.is_foreground(),
            ),
            manages_status_bar: config.manages_status_bar,
            status_bar_configured: std::sync::atomic::AtomicBool::new(false),
            status_bar: std::sync::Mutex::new(None),
            keyboard: std::sync::Mutex::default(),
            live: std::sync::atomic::AtomicBool::new(true),
        });
        let _ = route_ref.set(Arc::downgrade(&dispatcher));
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
        Ok(UiShell {
            state,
            dispatcher,
            shader_rate,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_attach(
        env: Env,
        parent: NativeNode,
        shell: UiShell,
        mut booted: BootedEngine,
        config: &Config,
        components: crate::interop::component::Components,
        scroll_watchers: crate::scroll::watch::ScrollWatchers,
        drain_buffered: bool,
    ) -> napi_ohos::Result<Self> {
        let UiShell {
            state,
            dispatcher,
            shader_rate,
        } = shell;
        let engine = booted
            .engine
            .take()
            .ok_or_else(|| napi_ohos::Error::from_reason("preboot engine already closed"))?;
        let environment = booted.environment;
        let size = booted.size;
        let scale = environment.pixel_ratio;
        let engine_assets: Arc<dyn nsh_engine::Assets> = booted.assets.clone();
        let images = crate::media::image::Loader::new(
            engine_assets,
            (*booted.access).clone(),
            Arc::downgrade(&dispatcher),
        );
        images.set_disk_cache(engine.image_cache());
        let control = engine.control();
        dispatcher.keyboard.lock().unwrap().desktop = environment.os.is_desktop();
        state.borrow_mut().environment = environment;
        let mut initial_main_window = crate::services::desktop::MainWindowState::default();
        if let Some(options) = &booted.main_window_options {
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
            text_scale: booted.text_scale.clone(),
            control: control.clone(),
            animator: crate::render::animation::Animator::new(control, Arc::downgrade(&dispatcher)),
            images,
            access: booted.access.clone(),
            assets: booted.assets.clone(),
            components,
            touches: RefCell::default(),
            canvas_bytes: Rc::new(Cell::new(0)),
            fonts: booted.fonts.clone(),
            platform: Cell::new(environment),
            main_window: RefCell::new(initial_main_window),
            appearance: Cell::new(nsh_core::platform::Appearance::System),
        });
        let mut mounter = Mounter::new(parent, size, context).map_err(napi_error)?;
        if let Some(launch) = booted.main_window_launch.take() {
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
        if drain_buffered && let Some(buffered) = booted.gate.attach(runtime._dispatcher.clone()) {
            let chunks = booted.chunks.clone();
            let dispatcher_weak = Arc::downgrade(&runtime._dispatcher);
            let mut st = runtime.state.try_borrow_mut().map_err(reentry)?;
            for error in buffered.errors {
                st.report(error);
            }
            if let Some(chunks) = chunks {
                for id in buffered.pending_chunks {
                    chunks.load(&id, dispatcher_weak.clone());
                }
            }
            // Apply every buffered MountBatch synchronously before ArkUI builds
            // Frame 1, so ContentSlot already carries the complete native tree.
            for batch in buffered.batches {
                if let Err(error) = st.apply_batch(batch) {
                    st.report(error.to_string());
                    return Err(napi_error(error));
                }
            }
            if buffered.frame_requested {
                st.engine_wanted = true;
            }
            st.update_back();
            st.schedule_frame().map_err(napi_error)?;
        }
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
        if !changed
            && state
                .mounter
                .as_ref()
                .is_none_or(|m| m.surface_size() == size)
        {
            return Ok(());
        }
        state.environment.window = window;
        if let Some(mounter) = &state.mounter {
            mounter.resize(size).map_err(napi_error)?;
        }
        if changed && let Some(engine) = &state.engine {
            // `Message::Resize` only relayouts the viewport; reactive
            // environment signals (`windowSize()`, `window()`, `md:`/`lg:`
            // classes) read `Environment::window`.
            let effective = state.effective_environment(state.environment);
            engine.control().set_environment(effective);
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
                if updated.is_ok()
                    && attached
                    && state.laid_out
                    && state.environment.os.is_desktop()
                    && mounter.batch_count() > 0
                {
                    mounter.request_desktop_focus();
                }
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
            && state.attached
            && state.environment.os.is_desktop()
            && let Some(mounter) = &state.mounter
            && mounter.batch_count() > 0
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
    pub(crate) fn hit_app(&self, point_px: (f32, f32)) -> crate::input::intercept::SurfaceHit {
        let Ok(state) = self.state.try_borrow() else {
            return crate::input::intercept::SurfaceHit::Indeterminate;
        };
        state
            .mounter
            .as_ref()
            .map_or(crate::input::intercept::SurfaceHit::Miss, |mounter| {
                mounter.hit_app(point_px)
            })
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
type PendingEngineDrop = (Option<std::path::PathBuf>, std::thread::JoinHandle<()>);

fn pending_engine_drops() -> &'static std::sync::Mutex<Vec<PendingEngineDrop>> {
    static DROPS: std::sync::OnceLock<std::sync::Mutex<Vec<PendingEngineDrop>>> =
        std::sync::OnceLock::new();
    DROPS.get_or_init(Default::default)
}

fn await_engine_drop(storage: Option<&std::path::Path>) {
    let mut handles = Vec::new();
    {
        let mut drops = pending_engine_drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < drops.len() {
            let matches = match (storage, drops[i].0.as_deref()) {
                (Some(a), Some(b)) => a == b,
                _ => drops[i].1.is_finished(),
            };
            if matches {
                handles.push(drops.remove(i).1);
            } else {
                i += 1;
            }
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
}

fn defer_engine_drop(storage: Option<std::path::PathBuf>, drop_fn: impl FnOnce() + Send + 'static) {
    let handle = std::thread::Builder::new()
        .name("nsh-engine-drop".into())
        .spawn(drop_fn);
    if let Ok(handle) = handle {
        let mut drops = pending_engine_drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        drops.retain(|(_, h)| !h.is_finished());
        drops.push((storage, handle));
    }
}

fn napi_error(error: crate::render::node::Error) -> napi_ohos::Error {
    napi_ohos::Error::from_reason(error.to_string())
}
fn reentry(_: impl std::fmt::Display) -> napi_ohos::Error {
    napi_ohos::Error::from_reason("defer engine mutation until the next host turn")
}
