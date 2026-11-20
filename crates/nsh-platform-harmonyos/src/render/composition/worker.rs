//! One compositor worker per surface; inputs and replies are generation scoped.
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use nsh_core::{Filter, NodeId, animation::AnimationValue, shader::ShaderLayer};
use ohos_native_window_binding::NativeWindow;

use super::{
    mailbox::{Coalesce, CommandSender, PlanOp, SceneMailbox, command_channel},
    raster::{Scene as ScenePlan, Shared as SceneShared},
    readback::{RgbaFrame, TextureContext, TextureReader},
};
use crate::{
    app::{
        frame_rate::FrameRate,
        runtime::{Dispatcher, Message},
    },
    render::{
        composition::filter::Image,
        node::{Node, Result},
    },
};

pub(crate) const MAX_LAYERS: usize = 128;
pub(super) const MAX_SURFACE_PIXELS: u64 = 67_108_864;

#[derive(Clone, PartialEq)]
struct Settings {
    gpu: bool,
    box_size: nsh_core::Size<nsh_core::Points>,
    width: u32,
    height: u32,
    filters: Vec<Filter>,
    scale: f32,
    density: f32,
    insets: [u32; 4],
    scene: bool,
    capture: bool,
    keep_gpu_input: bool,
    navigation: Option<crate::render::shader::navigation::Key>,
    backdrop: Option<NodeId>,
    first_screen_layer: Option<NodeId>,
    shaders: Vec<(NodeId, Arc<ShaderLayer>)>,
}
impl Settings {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
    fn native_output(&self) -> bool {
        self.gpu
            && !self.keep_gpu_input
            && !self.capture
            && self.filters.is_empty()
            && self.shaders.is_empty()
    }
}
struct Output {
    frame: RgbaFrame,
    x: f32,
    y: f32,
}
struct State {
    settings: Settings,
    screen_origin: Option<(i32, i32)>,
    output: Option<Output>,
    settings_revision: u64,
    presented_navigation: Option<crate::render::shader::navigation::Key>,
    published_geom: Option<(u32, u32, u32, u32)>,
    initialized: Option<std::result::Result<Option<u64>, String>>,
    output_surface: Option<u64>,
    /// Producer destination acknowledged by the UI attachment.
    surface: Option<u64>,
    closed: bool,
}
struct Shared {
    released: AtomicBool,
    active: AtomicBool,

    awaiting_origin: AtomicBool,
    state: Mutex<State>,
    queued: AtomicBool,
    posted: AtomicBool,
    animating: AtomicBool,
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Key {
    pub(crate) id: NodeId,
    pub(crate) generation: u32,
    overlay: bool,
}
impl Key {
    pub(super) fn is_overlay(self) -> bool {
        self.overlay
    }
    fn slot(self) -> (NodeId, bool) {
        (self.id, self.overlay)
    }
}

impl Shared {
    fn accepts_revision(&self, revision: u64) -> bool {
        let state = self.state.lock().unwrap();
        !state.closed && state.settings_revision == revision
    }
}

enum Task {
    Create {
        key: Key,
        shared: Arc<Shared>,
    },
    Precompile(Vec<Arc<ShaderLayer>>),
    ShaderWarmed,
    Fill(super::fill::Command),
    Frame(u64),
    Resume,
    Wake(Key, Weak<Shared>),
    Slot(Key, NodeId, u16, AnimationValue),
    Transition(Key, NodeId, std::time::Duration),
    Capture {
        key: crate::render::shader::navigation::Key,
        source: Key,
        before_screen: bool,
    },
    CancelCapture(crate::render::shader::navigation::Key),
    Navigation(Navigation<()>),
    Window(Key, NativeWindow),
    WindowGone(Key, usize),
    RetireWindow(Key, Arc<AtomicBool>),
    Plan,
    Drain,
    Close,
}
impl Coalesce for Task {
    fn merge_at_tail(&self) -> bool {
        matches!(self, Self::Frame(_))
    }

    fn is_barrier(&self) -> bool {
        use super::fill::Command as FillCmd;
        match self {
            Self::Frame(_)
            | Self::Resume
            | Self::Plan
            | Self::Precompile(_)
            | Self::ShaderWarmed
            | Self::Slot(..) => false,
            Self::Fill(FillCmd::Slot(..) | FillCmd::Size(..) | FillCmd::Shown(..)) => false,
            Self::Create { .. }
            | Self::Fill(
                FillCmd::Layer(..)
                | FillCmd::Window(..)
                | FillCmd::WindowGone(..)
                | FillCmd::Remove(..),
            )
            | Self::Wake(..)
            | Self::Transition(..)
            | Self::Capture { .. }
            | Self::CancelCapture(..)
            | Self::Navigation(..)
            | Self::Window(..)
            | Self::WindowGone(..)
            | Self::RetireWindow(..)
            | Self::Drain
            | Self::Close => true,
        }
    }

    fn try_merge(&mut self, incoming: &mut Option<Self>) -> bool {
        use super::fill::Command as FillCmd;
        match (self, incoming.as_ref()) {
            (Self::Frame(timestamp), Some(Self::Frame(_))) => {
                if let Some(Self::Frame(next)) = incoming.take() {
                    *timestamp = (*timestamp).max(next);
                }
                true
            }
            (Self::Resume, Some(Self::Resume))
            | (Self::Plan, Some(Self::Plan))
            | (Self::ShaderWarmed, Some(Self::ShaderWarmed)) => {
                incoming.take();
                true
            }
            (Self::Precompile(layers), Some(Self::Precompile(_))) => {
                if let Some(Self::Precompile(more)) = incoming.take() {
                    layers.extend(more);
                }
                true
            }
            (Self::Slot(k1, id1, s1, v1), Some(Self::Slot(k2, id2, s2, _)))
                if k1 == k2 && id1 == id2 && s1 == s2 =>
            {
                if let Some(Self::Slot(_, _, _, next)) = incoming.take() {
                    *v1 = next;
                }
                true
            }
            (Self::Fill(FillCmd::Slot(k1, s1, v1)), Some(Self::Fill(FillCmd::Slot(k2, s2, _))))
                if k1 == k2 && s1 == s2 =>
            {
                if let Some(Self::Fill(FillCmd::Slot(_, _, next))) = incoming.take() {
                    *v1 = next;
                }
                true
            }
            (
                Self::Fill(FillCmd::Size(k1, sz1, sc1)),
                Some(Self::Fill(FillCmd::Size(k2, _, _))),
            ) if k1 == k2 => {
                if let Some(Self::Fill(FillCmd::Size(_, next_sz, next_sc))) = incoming.take() {
                    *sz1 = next_sz;
                    *sc1 = next_sc;
                }
                true
            }
            (Self::Fill(FillCmd::Shown(k1, sh1)), Some(Self::Fill(FillCmd::Shown(k2, _))))
                if k1 == k2 =>
            {
                if let Some(Self::Fill(FillCmd::Shown(_, next_sh))) = incoming.take() {
                    *sh1 = next_sh;
                }
                true
            }
            _ => false,
        }
    }
}
struct Navigation<S> {
    target: Key,
    key: crate::render::shader::navigation::Key,
    snapshot: S,
    duration: std::time::Duration,
    backwards: bool,
    reverse_inputs: bool,
    complete: bool,
}
struct Snapshot {
    target: super::gles::Target,
    coverage: super::geometry::Coverage,
    origin: (f32, f32),
}
struct PendingCapture {
    key: crate::render::shader::navigation::Key,
    source: Key,
    before_screen: bool,
}
pub(crate) struct Worker {
    send: CommandSender<Task>,
    plan: Arc<SceneMailbox<ScenePlan>>,
    thread: Option<std::thread::JoinHandle<()>>,
    retired: RefCell<Vec<(Arc<Shared>, super::presentation::Presentation)>>,
    retired_windows: RefCell<Vec<(Arc<AtomicBool>, super::presentation::Presentation)>>,
    retired_fills: Arc<Mutex<Vec<super::fill::Key>>>,
    drained: Arc<AtomicBool>,
}
#[derive(Clone, Copy)]
enum WindowTarget {
    Composite(Key),
    Fill(super::fill::Key),
}
#[derive(Clone)]
pub(crate) struct WindowEvents {
    target: WindowTarget,
    send: CommandSender<Task>,
}
impl WindowEvents {
    pub(super) fn created(&self, window: NativeWindow) {
        let task = match self.target {
            WindowTarget::Composite(key) => Task::Window(key, window),
            WindowTarget::Fill(key) => Task::Fill(super::fill::Command::Window(key, window)),
        };
        let _ = self.send.send(task);
    }
    pub(super) fn destroyed(&self, raw: usize) {
        let task = match self.target {
            WindowTarget::Composite(key) => Task::WindowGone(key, raw),
            WindowTarget::Fill(key) => Task::Fill(super::fill::Command::WindowGone(key, raw)),
        };
        let _ = self.send.send(task);
    }
}
impl Worker {
    pub(crate) fn for_owner(owner: &Node) -> Result<Rc<Self>> {
        let mut slot = owner.compositor.borrow_mut();
        if slot.is_none() {
            *slot = Some(Rc::new(
                Self::new(owner.dispatcher.clone(), owner.shaders.frame_rate()).map_err(
                    |error| {
                        napi_ohos::Error::from_reason(format!(
                            "Could not start OHOS compositor: {error}"
                        ))
                    },
                )?,
            ));
        }
        Ok(slot.as_ref().unwrap().clone())
    }
    pub(crate) fn precompile(&self, layer: Arc<ShaderLayer>) {
        let _ = self.send.send(Task::Precompile(vec![layer]));
    }
    pub(crate) fn fill(&self, command: super::fill::Command) {
        let _ = self.send.send(Task::Fill(command));
    }
    pub(crate) fn fill_window(&self, key: super::fill::Key) -> WindowEvents {
        WindowEvents {
            target: WindowTarget::Fill(key),
            send: self.send.clone(),
        }
    }
    pub(crate) fn take_retired_fills(&self) -> Vec<super::fill::Key> {
        std::mem::take(&mut *self.retired_fills.lock().unwrap())
    }
    pub(crate) fn new(route: Weak<Dispatcher>, rate: Arc<FrameRate>) -> std::io::Result<Self> {
        let (send, receive) = command_channel();
        let callback = send.clone();
        let plan = Arc::new(SceneMailbox::new());
        let pending_plan = plan.clone();
        let retired_fills = Arc::new(Mutex::new(Vec::new()));
        let completed_fills = retired_fills.clone();
        let drained = Arc::new(AtomicBool::new(false));
        let worker_drained = drained.clone();
        let thread = std::thread::Builder::new()
            .name("nsh-compositor".into())
            .spawn(move || {
                let mut vote = rate.vote();
                let mut entries = Entries::new();
                let mut gpu: Option<TextureContext> = None;
                let mut gpu_error_reported = false;
                let mut scene: Option<ScenePlan> = None;
                let mut scene_stats = super::scene_policy::SceneStats::default();
                let mut fills = super::fill::Fills::default();
                let mut clock = crate::render::frame_clock::FrameClock::default();
                let mut scene_dirty = false;
                let mut readback_warmed = false;
                let mut filters_warmed = false;
                let mut precompiler: Option<super::precompile::Precompiler> = None;
                let mut precompiler_failed = false;
                let mut pending_precompiles: std::collections::VecDeque<Arc<ShaderLayer>> =
                    std::collections::VecDeque::new();
                let mut pending_captures: Vec<PendingCapture> = Vec::new();
                let mut captured: Vec<(
                    crate::render::shader::navigation::Key,
                    Snapshot,
                )> = Vec::new();
                const WINDOW_GRACE: std::time::Duration = std::time::Duration::from_millis(50);
                let ensure_precompiler = |precompiler: &mut Option<super::precompile::Precompiler>,
                                          failed: &mut bool,
                                          gpu: &Option<TextureContext>,
                                          callback: &CommandSender<Task>| {
                    if precompiler.is_none() && !*failed {
                        let notify = callback.clone();
                        match super::precompile::Precompiler::spawn(move || {
                            let _ = notify.send(Task::ShaderWarmed);
                        }) {
                            Some(p) => {
                                if let Some(gpu) = gpu.as_ref() {
                                    gpu.attach_binary_store(p.binary_store());
                                }
                                *precompiler = Some(p);
                            }
                            None => *failed = true,
                        }
                    }
                };
                loop {
                    // Publish demand after every command, including failure and
                    // retirement paths that continue the loop. The UI owns the
                    // only NativeVSync request and supplies our frame timestamp.
                    let animating = route
                        .upgrade()
                        .is_some_and(|r| r.active.load(Ordering::Acquire))
                        && (scene_dirty
                            || fills.animating()
                            || entries.values().any(|entry| {
                                let pending_nav =
                                    entry.navigation.as_ref().is_some_and(|n| !n.complete);
                                (entry.window.is_some() || entry.settings.scene || pending_nav)
                                    && entry.shared.active.load(Ordering::Acquire)
                                    && entry.shared.animating.load(Ordering::Acquire)
                            }));
                    if vote.update(animating)
                        && let Some(route) = route.upgrade()
                    {
                        route.send(Message::CompositeDemand);
                    }
                    let nav_active = entries
                        .values()
                        .any(|entry| entry.navigation.as_ref().is_some_and(|n| !n.complete));
                    let has_idle_work = !pending_precompiles.is_empty()
                        || (!nav_active
                            && gpu.is_some()
                            && (!readback_warmed
                                || !filters_warmed
                                || fills.has_pending_warmup(precompiler.as_ref())));
                    let window_grace = if has_idle_work {
                        entries
                            .values()
                            .filter(|entry| {
                                entry.settings.gpu
                                    && entry.window.is_none()
                                    && entry.shared.active.load(Ordering::Acquire)
                                    && !entry.shared.state.lock().unwrap().closed
                            })
                            .filter_map(|entry| {
                                entry
                                    .window_wait_since
                                    .and_then(|since| WINDOW_GRACE.checked_sub(since.elapsed()))
                                    .filter(|remaining| !remaining.is_zero())
                            })
                            .min()
                    } else {
                        None
                    };
                    let task = if !has_idle_work {
                        match receive.recv() {
                            Ok(task) => task,
                            Err(()) => break,
                        }
                    } else if let Some(wait) = window_grace {
                        match receive.recv_timeout(wait) {
                            Ok(Some(task)) => task,
                            Ok(None) => continue,
                            Err(()) => break,
                        }
                    } else if let Some(task) = receive.try_recv() {
                        task
                    } else {
                        if gpu.is_none() {
                            match TextureContext::new() {
                                Ok(ctx) => {
                                    if let Some(p) = precompiler.as_ref() {
                                        ctx.attach_binary_store(p.binary_store());
                                    }
                                    gpu = Some(ctx);
                                    gpu_error_reported = false;
                                }
                                Err(error) => {
                                    log::warn!("shader precompile context: {error}");
                                    pending_precompiles.clear();
                                    continue;
                                }
                            }
                        }
                        if let Some(gpu) = gpu.as_ref() {
                            let mut did_work = false;
                            while let Some(layer) = pending_precompiles.pop_front() {
                                if super::gles::is_warmed(gpu, &layer) {
                                    continue;
                                }
                                if let Err(error) = super::gles::precompile(gpu, &layer) {
                                    log::warn!("shader precompile: {error}");
                                }
                                did_work = true;
                                break;
                            }
                            if !did_work && !nav_active {
                                if !readback_warmed {
                                    readback_warmed = true;
                                    if let Err(error) = gpu.precompile_readback() {
                                        log::warn!("readback precompile: {error}");
                                    }
                                } else if !filters_warmed {
                                    filters_warmed = true;
                                    if let Err(error) = super::gles::precompile_filters(gpu) {
                                        log::warn!("filter precompile: {error}");
                                    }
                                } else if fills.has_pending_warmup(precompiler.as_ref()) {
                                    fills.warmup_one(
                                        gpu,
                                        precompiler.as_ref(),
                                        route.upgrade().as_deref(),
                                    );
                                }
                            }
                        }
                        continue;
                    };
                    let frame = matches!(&task, Task::Frame(_));
                    let mut repaint = false;
                    let mut fill_frame = false;
                    match task {
                        Task::Close => break,
                        Task::Drain => {
                            scene = None;
                            scene_stats.reset_idle();
                            pending_precompiles.clear();
                            captured.clear();
                            if let Some(route) = route.upgrade() {
                                for pending in pending_captures.drain(..) {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Failed(
                                            pending.key,
                                        ),
                                    ));
                                }
                            } else {
                                pending_captures.clear();
                            }
                            for entry in std::mem::take(&mut entries).into_values() {
                                retire_entry(entry, &route);
                                }
                            completed_fills.lock().unwrap().extend(fills.clear());
                            worker_drained.store(true, Ordering::Release);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: nsh_core::ROOT_ID,
                                    generation: 0,
                                });
                                route.send(Message::Shader);
                            }
                        }
                        Task::Precompile(layers) => {
                            ensure_precompiler(&mut precompiler, &mut precompiler_failed, &gpu, &callback);
                            if let Some(p) = precompiler.as_ref() {
                                for layer in layers {
                                    if gpu
                                        .as_ref()
                                        .is_none_or(|g| !super::gles::has_program(g, &layer))
                                    {
                                        p.submit_or_compiling(&layer);
                                    }
                                }
                            } else {
                                pending_precompiles.extend(layers);
                            }
                        }
                        Task::ShaderWarmed => {
                            fill_frame = true;
                            for entry in entries.values() {
                                if entry.waiting_for_shader {
                                    wake(&callback, entry.key, &entry.shared);
                                }
                            }
                        }
                        Task::Fill(super::fill::Command::Remove(key)) => {
                            fills.remove(key);
                            completed_fills.lock().unwrap().push(key);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Shader);
                            }
                        }
                        Task::Fill(command) => {
                            let upgraded = route.upgrade();
                            if let super::fill::Command::Layer(_, ref layer) = command {
                                ensure_precompiler(
                                    &mut precompiler,
                                    &mut precompiler_failed,
                                    &gpu,
                                    &callback,
                                );
                                if let Some(p) = precompiler.as_ref()
                                    && gpu
                                        .as_ref()
                                        .is_none_or(|g| !super::gles::has_program(g, layer))
                                {
                                    p.submit_or_compiling(layer);
                                }
                            }
                            if gpu.is_none() {
                                match TextureContext::new() {
                                    Ok(ctx) => {
                                        if let Some(p) = precompiler.as_ref() {
                                            ctx.attach_binary_store(p.binary_store());
                                        }
                                        gpu = Some(ctx);
                                        gpu_error_reported = false;
                                    }
                                    Err(error) => {
                                        log::warn!("shader fill context: {error}");
                                        if !gpu_error_reported
                                            && let Some(route) = upgraded.as_deref()
                                        {
                                            gpu_error_reported = true;
                                            route.send(Message::Error(format!(
                                                "shader fill context: {error}"
                                            )));
                                        }
                                    }
                                }
                            }
                            if let Some(gpu) = gpu.as_ref() {
                                fills.apply(command, gpu, upgraded.as_deref());
                            }
                            // Apply every update, but present only on VSync. A
                            // batch can change several animated uniforms; drawing
                            // all fills after each command starves later exports.
                        }
                        Task::Resume => {
                            repaint = scene.is_some();
                            fill_frame = true;
                            fills.mark_windows_dirty();
                            if route
                                .upgrade()
                                .is_some_and(|r| r.active.load(Ordering::Acquire))
                            {
                                for entry in entries.values_mut() {
                                    if entry.window.is_some() {
                                        entry.window_dirty = true;
                                    }
                                    wake(&callback, entry.key, &entry.shared);
                                }
                            }
                        }
                        Task::Frame(timestamp) => {
                            let time = std::time::Duration::from_secs_f64(
                                clock.sample(Some(timestamp), 0.0) / 1000.0,
                            );
                            fills.sample(time);
                            for entry in entries.values_mut() {
                                entry.shaders.sample(time);
                            }
                            repaint = scene.is_some()
                                && entries.values().any(|entry| {
                                    entry.settings.scene
                                        && (entry
                                            .navigation
                                            .as_ref()
                                            .is_some_and(|nav| !nav.complete)
                                            || (entry.shared.active.load(Ordering::Acquire)
                                                && entry.shared.animating.load(Ordering::Acquire)))
                                });
                            fill_frame = true;
                            for entry in entries.values() {
                                if route
                                    .upgrade()
                                    .is_some_and(|r| r.active.load(Ordering::Acquire))
                                    && (!entry.settings.scene
                                        || entry
                                            .navigation
                                            .as_ref()
                                            .is_some_and(|nav| !nav.complete))
                                    && entry.shared.active.load(Ordering::Acquire)
                                    && entry.shared.animating.load(Ordering::Acquire)
                                {
                                    wake(&callback, entry.key, &entry.shared);
                                }
                            }
                        }
                        Task::Plan => match pending_plan.take() {
                            PlanOp::Set(next) => {
                                scene = Some(next);
                                repaint = true;
                            }
                            PlanOp::Clear => {
                                scene = None;
                                scene_stats.reset_idle();
                            }
                            PlanOp::None => {}
                        },
                        Task::Create { key, shared } => {
                            let result = (|| {
                                if shared.state.lock().unwrap().closed {
                                    return Err("composite retired".into());
                                }
                                let occupied_others = entries.len() - usize::from(entries.contains_key(&key.slot()));
                                if occupied_others >= MAX_LAYERS {
                                    return Err("compositor layer budget exceeded".into());
                                }
                                let settings = shared.state.lock().unwrap().settings.clone();
                                check_pixels(&entries, key, settings.pixels())?;
                                if gpu.is_none() {
                                    let ctx = TextureContext::new().map_err(|error| error.to_string())?;
                                    if let Some(p) = precompiler.as_ref() {
                                        ctx.attach_binary_store(p.binary_store());
                                    }
                                    gpu = Some(ctx);
                                }
                                let mut reader = create_input(gpu.as_ref().unwrap(), &settings, key, &shared, &callback)?;
                                let id = reader.as_mut().map(TextureReader::id).transpose().map_err(|e| e.to_string())?;
                                if !settings.shaders.is_empty() {
                                    ensure_precompiler(
                                        &mut precompiler,
                                        &mut precompiler_failed,
                                        &gpu,
                                        &callback,
                                    );
                                    if let Some(p) = precompiler.as_ref() {
                                        for (_, layer) in &settings.shaders {
                                            if !super::gles::has_program(gpu.as_ref().unwrap(), layer) {
                                                p.submit_or_compiling(layer);
                                            }
                                        }
                                    } else {
                                        pending_precompiles
                                            .extend(settings.shaders.iter().map(|(_, layer)| layer.clone()));
                                    }
                                }

                                if let Some(old) = entries.remove(&key.slot()) {
                                    retire_entry(old, &route);
                                }
                                let window_wait_since = settings.gpu.then(std::time::Instant::now);
                                entries.insert(key.slot(), Entry {
                                    key,
                                    shared: shared.clone(),
                                    reader,
                                    gpu: gpu.as_ref().unwrap().clone(),
                                    settings,
                                    scene_source: None,
                                    scene_output: None,
                                    shaders: super::gles::Effects::new(gpu.as_ref().unwrap()),
                                    filters: super::gles::Filters::new(gpu.as_ref().unwrap()),
                                    modal_composition: None,
                                    backdrop_dirty: false,
                                    filtered: None,
                                    window: None,
                                    window_wait_since,
                                    window_dirty: false,
                                    waiting_for_shader: false,
                                    pending_slots: Vec::new(),
                                    pending_transitions: Vec::new(),
                                    pending_readback_bytes: 0,
                                    navigation: None,
                                    completed: None,
                                });
                                Ok(id)
                            })();
                            if result.is_err() {
                                shared.released.store(true, Ordering::Release);
                                shared.state.lock().unwrap().initialized = Some(result);
                            } else if let Some(entry) = entries.get(&key.slot()) {
                                entry.publish_input(entry.settings.native_output(), result);
                            }
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: key.id,
                                    generation: key.generation,
                                });
                            }
                        }
                        Task::Wake(key, weak_shared) => {
                            let Some(entry) = entries
                                .get(&key.slot())
                                .filter(|entry| entry.key == key)
                            else {
                                if let Some(shared) = weak_shared.upgrade() {
                                    shared.queued.store(false, Ordering::Release);
                                }
                                continue;
                            };
                            entry.shared.queued.store(false, Ordering::Release);
                            if entry.shared.state.lock().unwrap().closed {
                                if let Some(old) = entries.remove(&key.slot()) {
                                    retire_entry(old, &route);
                                }
                                if let Some(route) = route.upgrade() {
                                    route.send(Message::Composite {
                                        id: key.id,
                                        generation: key.generation,
                                    });
                                }
                                continue;
                            }
                            if !route
                                .upgrade()
                                .is_some_and(|r| r.active.load(Ordering::Acquire))
                            {
                                continue;
                            }
                            let was_animating = entry.shared.animating.load(Ordering::Acquire);
                            let (settings, revision) = {
                                let state = entry.shared.state.lock().unwrap();
                                (state.settings.clone(), state.settings_revision)
                            };
                            let backdrop = settings.backdrop.and_then(|id| entries.get(&(id, false)).filter(|source| !source.shared.state.lock().unwrap().closed)
                                .and_then(|source| source.shaders.before_screen.as_ref()).map(super::gles::Target::picture));
                            let result =
                                check_pixels(&entries, key, settings.pixels()).and_then(|()| {
                                    if !settings.shaders.is_empty() && !settings.scene {
                                        ensure_precompiler(
                                            &mut precompiler,
                                            &mut precompiler_failed,
                                            &gpu,
                                            &callback,
                                        );
                                    }
                                    let entry = entries.get_mut(&key.slot()).unwrap();
                                    entry.update_input(&settings, &callback)?;
                                    entry.render(
                                        settings,
                                        revision,
                                        backdrop,
                                        precompiler.as_ref(),
                                    )
                                });
                            let upgraded = route.upgrade();
                            match result {
                                Ok(changed) => {
                                    let is_scene = entries
                                        .get(&key.slot())
                                        .is_some_and(|entry| entry.settings.scene);
                                    repaint = changed && is_scene;
                                    if changed {
                                        for above in entries.values_mut().filter(|e| e.settings.backdrop == Some(key.id)) {
                                            above.backdrop_dirty = true;
                                            wake(&callback, above.key, &above.shared);
                                        }
                                    }
                                },
                                Err(error) => {
                                    let entry = entries.get_mut(&key.slot()).unwrap();
                                    entry.shared.animating.store(false, Ordering::Release);
                                    entry.shared.awaiting_origin.store(false, Ordering::Release);
                                    if let Some(navigation) = entry.navigation.take()
                                        && !navigation.complete
                                        && let Some(route) = &upgraded
                                    {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Completed(
                                                navigation.key,
                                            ),
                                        ));
                                    }
                                    fail_captures_for_source(
                                        &mut pending_captures,
                                        key,
                                        upgraded.as_deref(),
                                    );
                                    if let Some(route) = &upgraded {
                                        route.send(Message::Error(format!(
                                            "compositor node {} generation {}: {error}",
                                            key.id, key.generation
                                        )));
                                    }
                                    continue;
                                }
                            }
                            fulfill_captures(
                                &mut entries,
                                &mut pending_captures,
                                &mut captured,
                                Some(key),
                                upgraded.as_deref(),
                            );
                            let completed_nav =
                                entries.get_mut(&key.slot()).unwrap().completed.take();
                            if let Some(route) = &upgraded {
                                let shared = &entries.get(&key.slot()).unwrap().shared;
                                let started_animating =
                                    !was_animating && shared.animating.load(Ordering::Acquire);
                                let publish = {
                                    let state = shared.state.lock().unwrap();
                                    state.initialized.is_some()
                                        || (!state.settings.scene
                                            && (state.output.is_some() || started_animating))
                                };
                                if publish && !shared.posted.swap(true, Ordering::AcqRel) {
                                    route.send(Message::Composite {
                                        id: key.id,
                                        generation: key.generation,
                                    });
                                }
                                if let Some(key) = completed_nav {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Completed(key),
                                    ));
                                }
                            }
                        }
                        Task::Slot(key, id, slot, value) => {
                            if let Some(entry) = entries
                                .get_mut(&key.slot())
                                .filter(|entry| entry.key == key)
                            {
                                repaint = scene.is_some() && entry.settings.scene;
                                let shaders = entry.shared.state.lock().unwrap().settings.shaders.clone();
                                if !entry.settings.scene && !shaders.is_empty() {
                                    ensure_precompiler(
                                        &mut precompiler,
                                        &mut precompiler_failed,
                                        &gpu,
                                        &callback,
                                    );
                                }
                                if entry.waiting_for_shader
                                    || (!entry.settings.scene
                                        && entry.needs_shader_warmup(&shaders, precompiler.as_ref()))
                                {
                                    entry.waiting_for_shader = true;
                                    if let Some((_, _, existing)) = entry
                                        .pending_slots
                                        .iter_mut()
                                        .find(|(eid, eslot, _)| *eid == id && *eslot == slot)
                                    {
                                        *existing = value;
                                    } else {
                                        entry.pending_slots.push((id, slot, value));
                                    }
                                } else {
                                    if let Err(error) = entry.shaders.layers(&shaders) {
                                        log::warn!("shader layers: {error}");
                                    }
                                    entry.shaders.slot(id, slot, value);
                                    wake(&callback, key, &entry.shared);
                                }
                            }
                        }
                        Task::Transition(key, id, duration) => {
                            if let Some(entry) = entries
                                .get_mut(&key.slot())
                                .filter(|entry| entry.key == key)
                            {
                                repaint = scene.is_some() && entry.settings.scene;
                                let shaders = entry.shared.state.lock().unwrap().settings.shaders.clone();
                                if !entry.settings.scene && !shaders.is_empty() {
                                    ensure_precompiler(
                                        &mut precompiler,
                                        &mut precompiler_failed,
                                        &gpu,
                                        &callback,
                                    );
                                }
                                if entry.waiting_for_shader
                                    || (!entry.settings.scene
                                        && entry.needs_shader_warmup(&shaders, precompiler.as_ref()))
                                {
                                    entry.waiting_for_shader = true;
                                    entry.pending_transitions.push((id, duration));
                                } else {
                                    if let Err(error) = entry
                                        .shaders
                                        .layers(&shaders)
                                        .and_then(|()| entry.shaders.transition(id, duration))
                                    {
                                        log::warn!("shader transition: {error}");
                                    }
                                    wake(&callback, key, &entry.shared);
                                }
                            }
                        }
                        Task::Capture {
                            key,
                            source,
                            before_screen,
                        } => {
                            captured.retain(|(k, _)| k.owner != key.owner);
                            pending_captures.retain(|p| p.key.owner != key.owner);
                            let upgraded = route.upgrade();
                            let entry = entries
                                .get_mut(&source.slot())
                                .filter(|entry| {
                                    entry.key == source
                                        && !entry.shared.state.lock().unwrap().closed
                                });
                            if let Some(entry) = entry {
                                match entry.snapshot(before_screen) {
                                    Ok(Some(snapshot)) => {
                                        captured.push((key, snapshot));
                                        if let Some(route) = &upgraded {
                                            route.send(Message::Navigation(
                                                crate::render::shader::navigation::Event::Captured(
                                                    key,
                                                ),
                                            ));
                                        }
                                    }
                                    Ok(None) => {
                                        if entry.settings.scene {
                                            repaint = true;
                                        }
                                        wake(&callback, source, &entry.shared);
                                        pending_captures.push(PendingCapture {
                                            key,
                                            source,
                                            before_screen,
                                        });
                                    }
                                    Err(error) => {
                                        log::warn!("shader navigation capture: {error}");
                                        if let Some(route) = &upgraded {
                                            route.send(Message::Navigation(
                                                crate::render::shader::navigation::Event::Failed(
                                                    key,
                                                ),
                                            ));
                                        }
                                    }
                                }
                            } else if let Some(route) = &upgraded {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Failed(key),
                                ));
                            }
                        }
                        Task::CancelCapture(key) => {
                            pending_captures.retain(|p| p.key != key);
                            captured.retain(|(k, _)| *k != key);
                        }
                        Task::Navigation(request) => {
                            repaint = scene.is_some();
                            let snapshot = captured
                                .iter()
                                .position(|(key, _)| *key == request.key)
                                .map(|idx| captured.swap_remove(idx).1);
                            let request = Navigation {
                                snapshot,
                                target: request.target,
                                key: request.key,
                                duration: request.duration,
                                backwards: request.backwards,
                                reverse_inputs: request.reverse_inputs,
                                complete: false,
                            };
                            if let Some(entry) = entries
                                .get_mut(&request.target.slot())
                                .filter(|e| e.key == request.target)
                            {
                                if request.snapshot.is_none() {
                                    if let Some(route) = route.upgrade() {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Failed(
                                                request.key,
                                            ),
                                        ));
                                    }
                                } else {
                                    let need_origin = entry
                                        .shared
                                        .state
                                        .lock()
                                        .unwrap()
                                        .screen_origin
                                        .is_none();
                                    entry
                                        .shared
                                        .awaiting_origin
                                        .store(need_origin, Ordering::Release);
                                    if let Some(old) = entry.navigation.replace(request)
                                        && !old.complete
                                        && let Some(route) = route.upgrade()
                                    {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Completed(
                                                old.key,
                                            ),
                                        ));
                                    }
                                    entry.shared.animating.store(true, Ordering::Release);
                                }
                                wake(&callback, entry.key, &entry.shared);
                            } else if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Completed(
                                        request.key,
                                    ),
                                ));
                            }
                        }
                        Task::Window(key, window) => {
                            if let Some(entry) = entries
                                .get_mut(&key.slot())
                                .filter(|e| e.key == key)
                            {
                                let output_surface = window.surface_id().map_err(|e| e.to_string());
                                let input_surface = entry.reader.as_mut().map(TextureReader::id)
                                    .transpose().map_err(|e| e.to_string());
                                let first_window = {
                                    let mut state = entry.shared.state.lock().unwrap();
                                    let available =
                                        state.output_surface.is_none() && output_surface.is_ok();
                                    state.output_surface = output_surface.as_ref().ok().copied();
                                    available
                                };
                                let raw = window.raw() as usize;
                                entry.window_wait_since = None;
                                if let Some(existing) = entry.window.as_ref()
                                    && existing.raw() == raw
                                {
                                    existing.invalidate_geometry();
                                    entry.window_dirty = true;
                                } else {
                                    entry.window = Some(gpu.as_ref().unwrap().window(window));
                                    let mut state = entry.shared.state.lock().unwrap();
                                    state.published_geom = None;
                                    state.presented_navigation = None;
                                    entry.window_dirty = true;
                                }
                                let native_output = entry.settings.native_output();
                                let destination = if native_output {
                                    if let Some(window) = &entry.window {
                                        window.release_egl();
                                    }
                                    output_surface.map(Some)
                                } else {
                                    output_surface.and(input_surface)
                                };
                                entry.publish_input(native_output, destination);
                                if first_window
                                    && key.id == nsh_core::ROOT_ID
                                    && !key.is_overlay()
                                    && let Some(route) = route.upgrade()
                                {
                                    // The input may have been published before its output window existed.
                                    route.send(Message::Composite {
                                        id: key.id,
                                        generation: key.generation,
                                    });
                                }
                                wake(&callback, key, &entry.shared);
                            }
                        }
                        Task::RetireWindow(key, released) => {
                            if let Some(entry) = entries
                                .get_mut(&key.slot())
                                .filter(|entry| entry.key == key)
                            {
                                entry.window = None;
                                let mut state = entry.shared.state.lock().unwrap();
                                state.output_surface = None;
                                state.presented_navigation = None;
                            }
                            released.store(true, Ordering::Release);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: key.id,
                                    generation: key.generation,
                                });
                            }
                        }
                        Task::WindowGone(key, raw) => {
                            if let Some(entry) = entries
                                .get_mut(&key.slot())
                                .filter(|e| e.key == key)
                                && entry.window.as_ref().is_some_and(|w| w.raw() == raw)
                            {
                                entry.window = None;
                                let mut state = entry.shared.state.lock().unwrap();
                                state.output_surface = None;
                                state.presented_navigation = None;
                            }
                        }
                    }
                    let upgraded_for_frame = route.upgrade();
                    if fill_frame
                        && let Some(gpu) = gpu.as_ref()
                        && upgraded_for_frame
                            .as_ref()
                            .is_some_and(|r| r.active.load(Ordering::Acquire))
                    {
                        fills.render(gpu, precompiler.as_ref(), upgraded_for_frame.as_deref());
                    }
                    // Many exports wake together during navigation. Compose their latest
                    // pictures once on VSync, so command delivery never waits for a full
                    // scene raster after each individual texture notification.
                    scene_dirty = scene.is_some() && (scene_dirty || repaint);
                    if frame
                        && let Some(scene) = &scene
                        && std::mem::take(&mut scene_dirty)
                    {
                        let upgraded = upgraded_for_frame;
                        {
                            let mut active_planes = 0usize;
                            let mut pre_readback_bytes = 0u64;
                            for entry in entries.values_mut() {
                                if entry.settings.scene
                                    && entry.shared.active.load(Ordering::Acquire)
                                {
                                    active_planes += 1;
                                }
                                pre_readback_bytes = pre_readback_bytes
                                    .saturating_add(std::mem::take(&mut entry.pending_readback_bytes));
                            }
                            let mut scene_sources = SceneEntries {
                                entries: &mut entries,
                                gpu: &mut gpu,
                                readback_bytes: pre_readback_bytes,
                                upload_bytes: 0,
                            };
                            let failed = scene.render(&mut scene_sources, upgraded.as_deref());
                            let frame_readback = scene_sources.readback_bytes;
                            let frame_upload = scene_sources.upload_bytes;
                            if let Some(snap) = scene_stats.record(
                                std::time::Instant::now(),
                                active_planes,
                                frame_readback,
                                frame_upload,
                            ) {
                                log::info!(
                                    "[NSH] scene-stats active_planes={} readback_bytes={} upload_bytes={} frames={} window_readback_bytes={} window_upload_bytes={}",
                                    snap.active_planes,
                                    snap.readback_bytes,
                                    snap.upload_bytes,
                                    snap.window_frames,
                                    snap.window_readback_bytes,
                                    snap.window_upload_bytes,
                                );
                            }
                            for entry in entries.values_mut() {
                                if failed
                                    && entry.settings.scene
                                    && let Some(nav) = entry.navigation.as_mut()
                                    && !nav.complete
                                {
                                    nav.complete = true;
                                    entry.completed = Some(nav.key);
                                }
                                if let Some(key) = entry.completed.take()
                                    && let Some(route) = &upgraded
                                {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Completed(key),
                                    ));
                                }
                            }
                            if !failed {
                                fulfill_captures(
                                    &mut entries,
                                    &mut pending_captures,
                                    &mut captured,
                                    None,
                                    upgraded.as_deref(),
                                );
                            }
                        }
                    }
                    if !pending_captures.is_empty() {
                        let upgraded = route.upgrade();
                        if frame {
                            fulfill_captures(
                                &mut entries,
                                &mut pending_captures,
                                &mut captured,
                                None,
                                upgraded.as_deref(),
                            );
                        }
                        pending_captures.retain(|pending| {
                            let live = entries
                                .get(&pending.source.slot())
                                .is_some_and(|entry| {
                                    entry.key == pending.source
                                        && !entry.shared.state.lock().unwrap().closed
                                });
                            if live {
                                true
                            } else {
                                if let Some(route) = &upgraded {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Failed(
                                            pending.key,
                                        ),
                                    ));
                                }
                                false
                            }
                        });
                    }
                }
                // Deterministic exit handshake: release every remaining entry,
                // retired window, and pending navigation even on early shutdown.
                if vote.update(false)
                    && let Some(route) = route.upgrade()
                {
                    route.send(Message::CompositeDemand);
                }
                drop(scene);
                drop(captured);
                if let Some(route) = route.upgrade() {
                    for pending in pending_captures.drain(..) {
                        route.send(Message::Navigation(
                            crate::render::shader::navigation::Event::Failed(pending.key),
                        ));
                    }
                }
                while let Some(task) = receive.try_recv() {
                    match task {
                        Task::Create { shared, .. } => {
                            shared.released.store(true, Ordering::Release);
                        }
                        Task::RetireWindow(_, released) => {
                            released.store(true, Ordering::Release);
                        }
                        Task::Capture { key, .. } => {
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Failed(key),
                                ));
                            }
                        }
                        Task::Navigation(request) => {
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Completed(
                                        request.key,
                                    ),
                                ));
                            }
                        }
                        Task::Wake(_, weak_shared) => {
                            if let Some(shared) = weak_shared.upgrade() {
                                shared.queued.store(false, Ordering::Release);
                            }
                        }
                        Task::Fill(super::fill::Command::Remove(key)) => {
                            fills.remove(key);
                            completed_fills.lock().unwrap().push(key);
                        }
                        _ => {}
                    }
                }
                for entry in std::mem::take(&mut entries).into_values() {
                    retire_entry(entry, &route);
                    }
                completed_fills.lock().unwrap().extend(fills.clear());
                drop(precompiler);
                drop(gpu);
                worker_drained.store(true, Ordering::Release);
                if let Some(route) = route.upgrade() {
                    route.send(Message::Composite {
                        id: nsh_core::ROOT_ID,
                        generation: 0,
                    });
                    route.send(Message::Shader);
                }
            })?;
        Ok(Self {
            send,
            plan,
            thread: Some(thread),
            retired: RefCell::default(),
            retired_windows: RefCell::default(),
            retired_fills,
            drained,
        })
    }
    pub(crate) fn drain(&self) {
        let _ = self.send.send(Task::Drain);
    }
    pub(crate) fn drained(&self) -> bool {
        self.drained.load(Ordering::Acquire)
    }
    pub(crate) fn wake(&self) {
        let _ = self.send.send(Task::Resume);
    }
    /// Samples the same display frame as the UI animator. The command mailbox
    /// coalesces queued frames to their latest timestamp under worker backpressure.
    pub(crate) fn frame(&self, timestamp: u64) {
        let _ = self.send.send(Task::Frame(timestamp));
    }
    pub(crate) fn cancel_capture(&self, key: crate::render::shader::navigation::Key) {
        let _ = self.send.send(Task::CancelCapture(key));
    }
    pub(crate) fn collect(&self) {
        self.retired_windows
            .borrow_mut()
            .retain(|(released, _)| !released.load(Ordering::Acquire));
        self.retired
            .borrow_mut()
            .retain(|(shared, _)| !shared.released.load(Ordering::Acquire));
    }
    pub(crate) fn clear_plan(&self) {
        if self.plan.clear() {
            let _ = self.send.send(Task::Plan);
        }
    }
    pub(crate) fn plan(
        &self,
        plan: Arc<crate::render::composition::draw::Plan>,
        shared: Arc<SceneShared>,
    ) {
        if self.plan.set(ScenePlan { plan, shared }) {
            let _ = self.send.send(Task::Plan);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.send.send(Task::Close);
        // The worker owns its EGL resources and releases them on its own lane.
        self.thread.take();
    }
}
fn wake(send: &CommandSender<Task>, key: Key, shared: &Arc<Shared>) {
    if !shared.queued.swap(true, Ordering::AcqRel)
        && send.send(Task::Wake(key, Arc::downgrade(shared))).is_err()
    {
        shared.queued.store(false, Ordering::Release);
    }
}
fn retire_entry(mut entry: Entry, route: &Weak<Dispatcher>) {
    if let Some(nav) = entry.navigation.take()
        && !nav.complete
        && let Some(route) = route.upgrade()
    {
        route.send(Message::Navigation(
            crate::render::shader::navigation::Event::Completed(nav.key),
        ));
    }
    let shared = entry.shared.clone();
    drop(entry);
    shared.released.store(true, Ordering::Release);
}
fn fail_captures_for_source(
    pending_captures: &mut Vec<PendingCapture>,
    source: Key,
    route: Option<&Dispatcher>,
) {
    pending_captures.retain(|pending| {
        if pending.source == source {
            if let Some(route) = route {
                route.send(Message::Navigation(
                    crate::render::shader::navigation::Event::Failed(pending.key),
                ));
            }
            false
        } else {
            true
        }
    });
}
fn fulfill_captures(
    entries: &mut Entries,
    pending_captures: &mut Vec<PendingCapture>,
    captured: &mut Vec<(crate::render::shader::navigation::Key, Snapshot)>,
    only_source: Option<Key>,
    route: Option<&Dispatcher>,
) {
    if pending_captures.is_empty() {
        return;
    }
    pending_captures.retain(|pending| {
        if only_source.is_some_and(|source| pending.source != source) {
            return true;
        }
        let Some(entry) = entries.get_mut(&pending.source.slot()).filter(|entry| {
            entry.key == pending.source && !entry.shared.state.lock().unwrap().closed
        }) else {
            if let Some(route) = route {
                route.send(Message::Navigation(
                    crate::render::shader::navigation::Event::Failed(pending.key),
                ));
            }
            return false;
        };
        match entry.snapshot(pending.before_screen) {
            Ok(Some(snapshot)) => {
                captured.retain(|(k, _)| k.owner != pending.key.owner);
                captured.push((pending.key, snapshot));
                if let Some(route) = route {
                    route.send(Message::Navigation(
                        crate::render::shader::navigation::Event::Captured(pending.key),
                    ));
                }
                false
            }
            Ok(None) => true,
            Err(error) => {
                log::warn!("shader navigation capture: {error}");
                if let Some(route) = route {
                    route.send(Message::Navigation(
                        crate::render::shader::navigation::Event::Failed(pending.key),
                    ));
                }
                false
            }
        }
    });
}
fn check_pixels(entries: &Entries, key: Key, pixels: u64) -> std::result::Result<(), String> {
    if pixels > 16_777_216
        || entries
            .values()
            .filter(|entry| entry.key.slot() != key.slot())
            .map(|entry| entry.settings.pixels())
            .sum::<u64>()
            + pixels
            > MAX_SURFACE_PIXELS
    {
        Err("compositor pixel budget exceeded".into())
    } else {
        Ok(())
    }
}
// Node IDs grow throughout a session. Only live producers take space or participate in a frame.
type Entries = BTreeMap<(NodeId, bool), Entry>;

struct Entry {
    key: Key,
    shared: Arc<Shared>,
    reader: Option<TextureReader>,
    gpu: TextureContext,
    settings: Settings,
    scene_source: Option<Image>,
    scene_output: Option<(super::geometry::Coverage, (f32, f32))>,
    shaders: super::gles::Effects,
    filters: super::gles::Filters,
    modal_composition: Option<super::gles::Filters>,
    backdrop_dirty: bool,
    filtered: Option<super::gles::Target>,
    window: Option<super::readback::Window>,
    window_wait_since: Option<std::time::Instant>,
    window_dirty: bool,
    waiting_for_shader: bool,
    pending_slots: Vec<(NodeId, u16, AnimationValue)>,
    pending_transitions: Vec<(NodeId, std::time::Duration)>,
    pending_readback_bytes: u64,
    navigation: Option<Navigation<Option<Snapshot>>>,
    completed: Option<crate::render::shader::navigation::Key>,
}
fn create_input(
    gpu: &TextureContext,
    settings: &Settings,
    key: Key,
    shared: &Arc<Shared>,
    callback: &CommandSender<Task>,
) -> std::result::Result<Option<TextureReader>, String> {
    if settings.native_output() {
        return Ok(None);
    }
    let callback = callback.clone();
    let weak = Arc::downgrade(shared);
    let mut reader = TextureReader::with_context(gpu, move || {
        if let Some(shared) = weak.upgrade() {
            wake(&callback, key, &shared);
        }
    })
    .map_err(|e| e.to_string())?;
    reader
        .resize(settings.width, settings.height)
        .map_err(|e| e.to_string())?;
    Ok(Some(reader))
}
impl Entry {
    fn publish_input(
        &self,
        native_output: bool,
        destination: std::result::Result<Option<u64>, String>,
    ) {
        let mut state = self.shared.state.lock().unwrap();
        // An engine commit may have changed producers while this worker was
        // creating its input. Such a reply must not migrate the native tree.
        if state.settings.native_output() != native_output {
            return;
        }
        if destination.as_ref().is_ok_and(|id| *id == state.surface) {
            return;
        }
        state.initialized = Some(destination);
    }
    fn update_input(
        &mut self,
        settings: &Settings,
        callback: &CommandSender<Task>,
    ) -> std::result::Result<(), String> {
        if self.reader.is_some() == settings.native_output() {
            self.reader = create_input(&self.gpu, settings, self.key, &self.shared, callback)?;
        }
        let id = if settings.native_output() {
            if let Some(window) = &self.window {
                window.release_egl();
            }
            self.shared.state.lock().unwrap().output_surface
        } else {
            self.reader
                .as_mut()
                .map(TextureReader::id)
                .transpose()
                .map_err(|e| e.to_string())?
        };
        // Comparing with the UI's acknowledged destination also covers a fast
        // native -> GPU -> native reversal coalesced into one worker wake.
        self.publish_input(settings.native_output(), Ok(id));
        Ok(())
    }
    fn needs_shader_warmup(
        &self,
        shaders: &[(NodeId, Arc<ShaderLayer>)],
        precompiler: Option<&super::precompile::Precompiler>,
    ) -> bool {
        !shaders.is_empty()
            && shaders.iter().any(|(_, layer)| {
                !super::gles::has_program(&self.gpu, layer)
                    && precompiler.is_some_and(|p| p.submit_or_compiling(layer))
            })
    }
    fn render(
        &mut self,
        settings: Settings,
        revision: u64,
        backdrop: Option<super::gles::Picture>,
        precompiler: Option<&super::precompile::Precompiler>,
    ) -> std::result::Result<bool, String> {
        let installed = self.navigation.as_ref().map(|nav| nav.key);
        if settings.navigation.is_some() && settings.navigation != installed {
            // The settings and captured picture travel separately. Keep the
            // previous buffer on screen until the picture for this generation arrives.
            return Ok(false);
        }
        if settings.navigation.is_none() {
            if let Some(old) = self.navigation.take()
                && !old.complete
            {
                self.completed = Some(old.key);
            }
        } else if self.navigation.as_ref().is_some_and(|nav| nav.complete) {
            // Keep the terminal picture until its generation is retired by the UI.
            return Ok(false);
        }
        let was_waiting = std::mem::take(&mut self.waiting_for_shader);
        if !settings.scene
            && !settings.native_output()
            && self.needs_shader_warmup(&settings.shaders, precompiler)
        {
            // Keep draining producer buffers so RSRenderThread never stalls
            // while the shader compiles on the background precompile thread.
            if let Some(reader) = self.reader.as_mut() {
                reader
                    .resize(settings.width, settings.height)
                    .map_err(|e| e.to_string())?;
                reader.drain_oes().map_err(|e| e.to_string())?;
            }
            self.waiting_for_shader = true;
            self.shared.animating.store(true, Ordering::Release);
            return Ok(false);
        }
        let mut changed =
            self.settings != settings || std::mem::take(&mut self.window_dirty) || was_waiting;
        changed |= std::mem::take(&mut self.backdrop_dirty);
        self.shaders.capture_before = settings.first_screen_layer;
        if settings.first_screen_layer.is_none() {
            self.shaders.before_screen = None;
        }
        if settings.backdrop.is_none() {
            self.modal_composition = None;
        }
        self.shaders.layers(&settings.shaders)?;
        for (id, slot, value) in self.pending_slots.drain(..) {
            self.shaders.slot(id, slot, value);
            changed = true;
        }
        for (id, duration) in self.pending_transitions.drain(..) {
            if let Err(error) = self.shaders.transition(id, duration) {
                log::warn!("shader transition: {error}");
            }
            changed = true;
        }
        if !self.settings.gpu && settings.gpu && self.window.is_none() {
            self.window_wait_since = Some(std::time::Instant::now());
        } else if !settings.gpu {
            self.window_wait_since = None;
        }
        self.settings = settings;
        let direct = !self.settings.scene;
        let x = -(self.settings.insets[0] as f32);
        let y = -(self.settings.insets[1] as f32);
        if self.settings.native_output() {
            self.scene_source = None;
            self.scene_output = None;
            self.shaders.clear_output();
            self.shaders.release_recycled();
            self.filtered = None;
            self.shared.awaiting_origin.store(false, Ordering::Release);
            self.shared.animating.store(false, Ordering::Release);
            if let Some(window) = &self.window {
                window.release_egl();
                // ArkUI submits directly to the persistent output queue. No
                // NativeImage resolve or additional full-screen draw is needed.
                self.record_geometry(self.settings.width, self.settings.height, x, y);
            }
            return Ok(false);
        }
        let reader = self.reader.as_mut().unwrap();
        reader
            .resize(self.settings.width, self.settings.height)
            .map_err(|e| e.to_string())?;
        if direct
            && self.settings.keep_gpu_input
            && !self.settings.capture
            && self.settings.filters.is_empty()
            && self.settings.shaders.is_empty()
        {
            changed |= reader.drain_oes().map_err(|e| e.to_string())?;
            self.shared.animating.store(false, Ordering::Release);
            self.shaders.clear_output();
            self.filtered = None;
            if !changed || !reader.has_frame() {
                return Ok(false);
            }
            let Some(window) = self.window.as_ref() else {
                return Ok(false);
            };
            if !self.shared.accepts_revision(revision) {
                return Ok(false);
            }
            if !reader.present_input(window).map_err(|e| e.to_string())? {
                self.window_dirty = true;
                self.shared.animating.store(true, Ordering::Release);
                return Ok(false);
            }
            return Ok(self.record_gpu_output(
                revision,
                self.settings.width,
                self.settings.height,
                x,
                y,
            ));
        }
        if direct {
            changed |= reader.update_texture().map_err(|e| e.to_string())?;
        } else if let Some(frame) = reader.read().map_err(|e| e.to_string())? {
            self.pending_readback_bytes += frame.pixels.len() as u64;
            let mut image = Image::new(frame);
            image.x = x;
            image.y = y;
            self.scene_source = Some(image);
            changed = true;
        }
        changed |= self.start_navigation()?;
        let pending_nav = self.navigation.as_ref().is_some_and(|nav| !nav.complete);
        if !changed && !self.shaders.wants_frame() && !pending_nav {
            self.shared.animating.store(false, Ordering::Release);
            return Ok(false);
        }
        if direct {
            let Some(source) = self.reader.as_ref().and_then(TextureReader::texture) else {
                // Only the producer can supply the first input. In particular,
                // offscreen native subtrees need not export a frame at all.
                // Their frame-available callback resumes this entry when ready.
                self.shared.animating.store(false, Ordering::Release);
                return Ok(false);
            };
            self.scene_source = None;
            self.scene_output = None;
            let (source, dx, dy, scale) =
                self.filters
                    .render(source, &self.settings.filters, self.settings.scale)?;
            let coverage = self.coverage();
            let mut filtered = coverage;
            filtered.insets[0] -= dx / scale;
            filtered.insets[1] -= dy / scale;
            filtered.insets[2] =
                source.1 as f32 / scale - filtered.box_size.width.0 - filtered.insets[0];
            filtered.insets[3] =
                source.2 as f32 / scale - filtered.box_size.height.0 - filtered.insets[1];
            let picture = if filtered != coverage
                || (source.1, source.2) != (self.settings.width, self.settings.height)
            {
                if self.filtered.as_ref().is_none_or(|target| {
                    (target.picture().1, target.picture().2)
                        != (self.settings.width, self.settings.height)
                }) {
                    self.filtered = Some(super::gles::Target::new(
                        &self.gpu,
                        self.settings.width,
                        self.settings.height,
                    )?);
                }
                let target = self.filtered.as_ref().unwrap();
                target.reframe(source, filtered, coverage)?;
                target.picture()
            } else {
                source
            };
            let picture = if self.settings.backdrop.is_some() {
                let Some(back) = backdrop else {
                    return Ok(false);
                };
                self.modal_composition
                    .get_or_insert_with(|| super::gles::Filters::new(&self.gpu))
                    .over(picture, back)?
            } else {
                picture
            };
            let repaint =
                self.shaders
                    .render(picture, changed, self.settings.scale, coverage, None)?;
            let published = self.publish_gpu(revision, repaint, x, y)?;
            if published
                && self.window.is_some()
                && let Some(navigation) = &mut self.navigation
                && navigation.snapshot.is_none()
                && !self.shaders.navigation_running(navigation.key.owner)
            {
                navigation.complete = true;
                self.completed = Some(navigation.key);
                self.shared
                    .animating
                    .store(self.shaders.wants_frame(), Ordering::Release);
            }
            return Ok(repaint);
        }
        self.shared
            .animating
            .store(self.shaders.wants_frame() || pending_nav, Ordering::Release);
        Ok(changed || self.shaders.wants_frame())
    }
    fn coverage(&self) -> super::geometry::Coverage {
        super::geometry::Coverage {
            box_size: self.settings.box_size,
            insets: self.settings.insets.map(|v| v as f32 / self.settings.scale),
        }
    }
    fn snapshot(&mut self, before_screen: bool) -> std::result::Result<Option<Snapshot>, String> {
        if let Some((coverage, origin)) = self.scene_output
            && let Some(output) = self.shaders.output()
        {
            return Ok(Some(Snapshot {
                target: super::gles::Target::copy(&self.gpu, output.picture())?,
                coverage,
                origin,
            }));
        }
        if self.settings.scene {
            return Ok(None);
        }
        let output = if before_screen {
            self.shaders.before_screen.as_ref()
        } else {
            self.shaders.output()
        };
        let picture = output
            .map(super::gles::Target::picture)
            .or(match self.reader.as_mut() {
                Some(reader) => reader.ensure_texture().map_err(|e| e.to_string())?,
                None => None,
            });
        let origin = self
            .shared
            .state
            .lock()
            .unwrap()
            .screen_origin
            .unwrap_or_default();
        picture
            .map(|picture| {
                Ok(Snapshot {
                    target: super::gles::Target::copy(&self.gpu, picture)?,
                    coverage: self.coverage(),
                    origin: (origin.0 as f32, origin.1 as f32),
                })
            })
            .transpose()
    }
    fn start_navigation(&mut self) -> std::result::Result<bool, String> {
        let coverage = self.coverage();
        let origin = self.shared.state.lock().unwrap().screen_origin;
        let Some(navigation) = self.navigation.as_mut() else {
            self.shared.awaiting_origin.store(false, Ordering::Release);
            return Ok(false);
        };
        if navigation.snapshot.is_none() {
            self.shared.awaiting_origin.store(false, Ordering::Release);
            return Ok(false);
        }
        let Some(origin) = origin else {
            self.shared.awaiting_origin.store(true, Ordering::Release);
            return Ok(false);
        };
        self.shared.awaiting_origin.store(false, Ordering::Release);
        let snapshot = navigation.snapshot.take().unwrap();
        let overflow = self
            .settings
            .shaders
            .iter()
            .find(|(id, _)| *id == navigation.key.owner)
            .map_or(0.0, |(_, layer)| layer.overflow.0);
        let destination = super::geometry::Coverage::shader(coverage.box_size, overflow);
        let (w, h, _) = destination.pixels(self.settings.scale);
        let mut source = snapshot.coverage;
        // Screen origins are physical pixels; convert their delta with the display density.
        let density = self.shared.state.lock().unwrap().settings.density;
        let dx = (snapshot.origin.0 - origin.0 as f32) / density;
        let dy = (snapshot.origin.1 - origin.1 as f32) / density;
        source.insets[0] -= dx;
        source.insets[1] -= dy;
        source.insets[2] += dx;
        source.insets[3] += dy;
        let target = super::gles::Target::new(&self.gpu, w, h)?;
        target.reframe(snapshot.target.picture(), source, destination)?;
        self.shaders.navigation(
            navigation.key.owner,
            target,
            destination,
            navigation.duration,
            navigation.backwards,
            navigation.reverse_inputs,
        )?;
        Ok(true)
    }
    fn record_gpu_output(&self, revision: u64, width: u32, height: u32, x: f32, y: f32) -> bool {
        let navigation = self
            .navigation
            .as_ref()
            .filter(|nav| nav.snapshot.is_none())
            .map(|nav| nav.key);
        let mut state = self.shared.state.lock().unwrap();
        if state.closed || state.settings_revision != revision {
            return false;
        }
        if state.presented_navigation != navigation {
            state.presented_navigation = navigation;
            // Geometry is often unchanged for a full-screen transition. The
            // first submission for this key must still wake modal presentation.
            state.published_geom = None;
        }
        Self::set_geometry(&mut state, width, height, x, y);
        true
    }
    fn record_geometry(&self, width: u32, height: u32, x: f32, y: f32) {
        Self::set_geometry(&mut self.shared.state.lock().unwrap(), width, height, x, y);
    }
    fn set_geometry(state: &mut State, width: u32, height: u32, x: f32, y: f32) {
        let geometry = (width, height, x.to_bits(), y.to_bits());
        if state.published_geom == Some(geometry) {
            return;
        }
        state.published_geom = Some(geometry);
        state.output = Some(Output {
            frame: RgbaFrame {
                width,
                height,
                pixels: Vec::new(),
            },
            x,
            y,
        });
    }

    fn publish_gpu(
        &mut self,
        revision: u64,
        repaint: bool,
        x: f32,
        y: f32,
    ) -> std::result::Result<bool, String> {
        let pending_nav = self.navigation.as_ref().is_some_and(|nav| !nav.complete);
        self.shared
            .animating
            .store(self.shaders.wants_frame() || pending_nav, Ordering::Release);
        let Some(output) = self.shaders.output() else {
            return Ok(false);
        };
        let Some(window) = &mut self.window else {
            return Ok(false);
        };
        if !repaint {
            return Ok(false);
        }
        if !self.shared.accepts_revision(revision) {
            return Ok(false);
        }
        // Window replacement and destruction are serialized on this worker.
        // A UI settings change only invalidates the reply from the old frame.
        let presented = output.present(window)?;
        if !presented {
            self.window_dirty = true;
            self.shared.animating.store(true, Ordering::Release);
            return Ok(false);
        }
        let (_, width, height) = output.picture();
        Ok(self.record_gpu_output(revision, width, height, x, y))
    }
}

struct SceneEntries<'a> {
    entries: &'a mut Entries,
    gpu: &'a mut Option<TextureContext>,
    readback_bytes: u64,
    upload_bytes: u64,
}
impl super::draw::Sources for SceneEntries<'_> {
    fn navigation(&self, key: Key) -> Option<crate::render::shader::navigation::Key> {
        let entry = self
            .entries
            .get(&key.slot())
            .filter(|entry| entry.key == key)?;
        entry
            .navigation
            .as_ref()
            .filter(|nav| nav.snapshot.is_none())
            .map(|nav| nav.key)
    }
    fn image(&self, key: Key) -> Option<(&Image, f32)> {
        self.entries
            .get(&key.slot())
            .filter(|entry| entry.key == key)
            .and_then(|entry| {
                entry
                    .scene_source
                    .as_ref()
                    .map(|image| (image, entry.settings.scale))
            })
    }
    fn shade(
        &mut self,
        key: Key,
        image: Image,
        backdrop: Option<Image>,
        scale: f32,
    ) -> std::result::Result<Image, String> {
        let Some(entry) = self
            .entries
            .get_mut(&key.slot())
            .filter(|entry| entry.key == key)
        else {
            return Ok(image);
        };
        entry
            .shaders
            .layers(&entry.shared.state.lock().unwrap().settings.shaders)?;
        entry.start_navigation()?;
        let box_size = entry.settings.box_size;
        let coverage = super::geometry::Coverage {
            box_size,
            insets: [
                -image.x / scale,
                -image.y / scale,
                (image.x + image.bitmap.width() as f32) / scale - box_size.width.0,
                (image.y + image.bitmap.height() as f32) / scale - box_size.height.0,
            ],
        };
        let (width, height, effective_scale) = coverage.pixels(scale);
        if self.gpu.is_none() {
            *self.gpu = Some(TextureContext::new().map_err(|error| error.to_string())?);
        }
        let gpu = self.gpu.as_ref().unwrap();
        let upload_bytes = &mut self.upload_bytes;
        let mut upload = |image: &Image| -> std::result::Result<super::gles::Target, String> {
            let resized = image.resized(width, height);
            let pixels = resized.bitmap.pixels().to_vec();
            *upload_bytes = upload_bytes.saturating_add(pixels.len() as u64);
            Ok(super::gles::Target::upload(
                gpu,
                &RgbaFrame {
                    width,
                    height,
                    pixels,
                },
            )?)
        };
        let input = upload(&image)?;
        let backdrop = backdrop
            .as_ref()
            .map(|backdrop| {
                // CSS filters may have enlarged the content beyond the backdrop capture.
                let aligned = backdrop.aligned(
                    image.bitmap.width(),
                    image.bitmap.height(),
                    (
                        (backdrop.x - image.x).round() as i32,
                        (backdrop.y - image.y).round() as i32,
                    ),
                );
                upload(&aligned)
            })
            .transpose()?;
        entry.shaders.render(
            input.picture(),
            true,
            effective_scale,
            coverage,
            backdrop.as_ref().map(super::gles::Target::picture),
        )?;
        let Some(output) = entry.shaders.output() else {
            return Ok(image);
        };
        let frame = output.read()?;
        self.readback_bytes = self
            .readback_bytes
            .saturating_add(frame.pixels.len() as u64);
        let origin = entry
            .shared
            .state
            .lock()
            .unwrap()
            .screen_origin
            .unwrap_or_default();
        entry.scene_output = Some((coverage, (origin.0 as f32, origin.1 as f32)));
        let mut result = Image::new(frame).resized(image.bitmap.width(), image.bitmap.height());
        result.x = image.x;
        result.y = image.y;
        if let Some(navigation) = &mut entry.navigation
            && navigation.snapshot.is_none()
            && !entry.shaders.navigation_running(navigation.key.owner)
            && !navigation.complete
        {
            navigation.complete = true;
            entry.completed = Some(navigation.key);
        }
        let pending_nav = entry.navigation.as_ref().is_some_and(|nav| !nav.complete);
        entry.shared.animating.store(
            entry.shaders.wants_frame() || pending_nav,
            Ordering::Release,
        );
        Ok(result)
    }
}

fn next_composite_generation() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    loop {
        let generation = NEXT.fetch_add(1, Ordering::Relaxed);
        if generation != 0 {
            return generation;
        }
    }
}

pub(crate) struct Composite {
    owner: std::rc::Weak<Node>,
    presentation: RefCell<Option<super::presentation::Presentation>>,
    failed: Cell<bool>,
    pub(crate) generation: u32,
    key: Key,
    shared: Arc<Shared>,
    worker: Rc<Worker>,
}
impl Composite {
    pub(crate) fn key(&self) -> Key {
        self.key
    }
    pub(crate) fn is_failed(&self) -> bool {
        self.failed.get()
    }
    pub(crate) fn has_presented(
        &self,
        owner: &Node,
        key: crate::render::shader::navigation::Key,
    ) -> bool {
        if owner.scene_active() {
            owner
                .scene
                .borrow()
                .as_ref()
                .is_some_and(|scene| scene.has_navigation_frame(owner.id, key))
        } else {
            self.shared.state.lock().unwrap().presented_navigation == Some(key)
        }
    }
    pub(crate) fn new(owner: &Rc<Node>) -> Result<Self> {
        Self::create(owner, false)
    }
    pub(crate) fn overlay(owner: &Rc<Node>) -> Result<Self> {
        Self::create(owner, true)
    }
    fn create(owner: &Rc<Node>, overlay: bool) -> Result<Self> {
        let generation = next_composite_generation();
        let worker = Worker::for_owner(owner)?;
        let initial_settings = settings(owner, overlay)?;
        let initial_origin = owner
            .mounted()
            .borrow()
            .layout_position_in_screen()
            .ok()
            .map(|origin| (origin.x, origin.y));
        let shared = Arc::new(Shared {
            released: AtomicBool::new(false),
            active: AtomicBool::new(true),

            awaiting_origin: AtomicBool::new(false),
            state: Mutex::new(State {
                settings: initial_settings.clone(),
                screen_origin: initial_origin,
                output: None,
                settings_revision: 0,
                presented_navigation: None,
                published_geom: None,
                initialized: None,
                output_surface: None,
                surface: None,
                closed: false,
            }),
            queued: AtomicBool::new(false),
            posted: AtomicBool::new(false),
            animating: AtomicBool::new(false),
        });
        let key = Key {
            id: owner.id,
            generation,
            overlay,
        };
        worker
            .send
            .send(Task::Create {
                key,
                shared: shared.clone(),
            })
            .map_err(|_| napi_ohos::Error::from_reason("OHOS compositor stopped"))?;
        if !overlay {
            let mut nodes = owner.composite_nodes.borrow_mut();
            nodes.retain(|node| node.strong_count() != 0);
            let weak = Rc::downgrade(owner);
            if !nodes
                .iter()
                .any(|existing| std::rc::Weak::ptr_eq(existing, &weak))
            {
                nodes.push(weak);
            }
        }
        let presentation = if initial_settings.gpu {
            let mut layer = super::presentation::Presentation::new(
                owner,
                key,
                WindowEvents {
                    target: WindowTarget::Composite(key),
                    send: worker.send.clone(),
                },
            )?;
            let ratio = initial_settings.density / initial_settings.scale;
            layer.update(
                (initial_settings.width as f32 * ratio).round().max(1.0) as u32,
                (initial_settings.height as f32 * ratio).round().max(1.0) as u32,
                -(initial_settings.insets[0] as f32 * ratio),
                -(initial_settings.insets[1] as f32 * ratio),
                initial_settings.density,
            )?;
            Some(layer)
        } else {
            None
        };
        Ok(Self {
            owner: Rc::downgrade(owner),
            presentation: RefCell::new(presentation),
            failed: Cell::new(false),
            generation,
            key,
            shared,
            worker,
        })
    }
    pub(crate) fn update(&self, owner: &Node) -> Result<bool> {
        if self.failed.get() {
            return Ok(false);
        }
        if !owner.composite_dirty.get()
            && self.shared.state.lock().unwrap().settings.scene == owner.scene_active()
        {
            return Ok(false);
        }
        let settings = settings(owner, self.key.overlay)?;
        if settings.gpu && self.presentation.borrow().is_none() {
            let layer = super::presentation::Presentation::new(
                owner,
                self.key,
                WindowEvents {
                    target: WindowTarget::Composite(self.key),
                    send: self.worker.send.clone(),
                },
            )?;
            *self.presentation.borrow_mut() = Some(layer);
        } else if !settings.gpu
            && let Some(mut layer) = self.presentation.borrow_mut().take()
        {
            layer.hide();
            let released = Arc::new(AtomicBool::new(false));
            self.worker
                .retired_windows
                .borrow_mut()
                .push((released.clone(), layer));
            let _ = self
                .worker
                .send
                .send(Task::RetireWindow(self.key, released));
        }
        if let Some(layer) = self.presentation.borrow_mut().as_mut() {
            let ratio = settings.density / settings.scale;
            layer.update(
                (settings.width as f32 * ratio).round().max(1.0) as u32,
                (settings.height as f32 * ratio).round().max(1.0) as u32,
                -(settings.insets[0] as f32 * ratio),
                -(settings.insets[1] as f32 * ratio),
                settings.density,
            )?;
        }

        let mut state = self.shared.state.lock().unwrap();
        if state.screen_origin.is_none() {
            state.screen_origin = owner
                .mounted()
                .borrow()
                .layout_position_in_screen()
                .ok()
                .map(|origin| (origin.x, origin.y));
        }
        let changed = state.settings != settings;
        if changed {
            if state.settings.native_output() != settings.native_output() {
                // Invalidate an unconsumed worker reply from the previous mode.
                // The current UI destination stays live until the new reply.
                state.initialized = None;
            }
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.settings = settings;
            drop(state);
            wake(&self.worker.send, self.key, &self.shared);
            owner
                .visual_node()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
        }
        Ok(changed)
    }
    pub(crate) fn surface_id(&self) -> Option<String> {
        if self.failed.get()
            || self
                .presentation
                .borrow()
                .as_ref()
                .is_some_and(|p| p.awaiting_capture())
        {
            return None;
        }
        // Only the worker can publish a new producer destination: switching to
        // native output must first relinquish EGL ownership of that queue.
        self.shared
            .state
            .lock()
            .unwrap()
            .surface
            .map(|id| id.to_string())
    }
    pub(crate) fn shader_slot(&self, id: NodeId, slot: u16, value: AnimationValue) {
        let _ = self.worker.send.send(Task::Slot(self.key, id, slot, value));
    }
    pub(crate) fn shader_transition(&self, id: NodeId, duration: std::time::Duration) {
        let _ = self
            .worker
            .send
            .send(Task::Transition(self.key, id, duration));
    }
    pub(crate) fn capture(&self, key: crate::render::shader::navigation::Key, before_screen: bool) {
        let _ = self.worker.send.send(Task::Capture {
            key,
            source: self.key,
            before_screen,
        });
    }
    pub(crate) fn shader_navigation(
        &self,
        key: crate::render::shader::navigation::Key,
        duration: std::time::Duration,
        backwards: bool,
        reverse_inputs: bool,
    ) {
        let _ = self.worker.send.send(Task::Navigation(Navigation {
            target: self.key,
            key,
            snapshot: (),
            duration,
            backwards,
            reverse_inputs,
            complete: false,
        }));
    }
    pub(crate) fn shader_frame(&self, active: bool) -> bool {
        let was_active = self.shared.active.swap(active, Ordering::AcqRel);
        let due = active && self.needs_shader_frame();
        if due && !was_active {
            wake(&self.worker.send, self.key, &self.shared);
        }
        due
    }
    pub(crate) fn needs_shader_frame(&self) -> bool {
        self.shared.animating.load(Ordering::Acquire)
    }
    pub(crate) fn insets(&self) -> crate::interop::sdk::TextureInsets {
        let state = self.shared.state.lock().unwrap();
        let [left, top, right, bottom] = state
            .settings
            .insets
            .map(|v| v as f64 / state.settings.scale as f64);
        crate::interop::sdk::TextureInsets {
            left,
            top,
            right,
            bottom,
        }
    }
    pub(crate) fn is_revealed(&self) -> bool {
        self.presentation
            .borrow()
            .as_ref()
            .is_some_and(super::presentation::Presentation::is_revealed)
    }
    pub(crate) fn ready(&self, owner: &Node) -> (bool, bool) {
        let (initialized, can_export, screen_effect) = {
            let mut state = self.shared.state.lock().unwrap();
            let initialized = state.initialized.take();
            if let Some(Ok(id)) = initialized.as_ref() {
                state.surface = *id;
            }
            (
                initialized,
                state.surface.is_some() && state.output_surface.is_some(),
                state.settings.navigation.is_none()
                    && !state.settings.capture
                    && state
                        .settings
                        .shaders
                        .iter()
                        .any(|(_, layer)| layer.role == nsh_core::shader::ShaderRole::Screen),
            )
        };
        // Keep the live native subtree visible during initial EGL and window setup.
        if can_export && let Some(layer) = self.presentation.borrow().as_ref() {
            layer.capture_before_export(owner, screen_effect);
        }
        let input_changed = initialized.is_some();
        if let Some(Err(error)) = initialized {
            log::warn!("OHOS compositor initialization: {error}");
            self.failed.set(true);
            if let Some(mut layer) = self.presentation.borrow_mut().take() {
                layer.hide();
            }
        }
        self.shared.posted.store(false, Ordering::Release);
        let output = self.shared.state.lock().unwrap().output.take();
        if let Some(output) = output {
            let mut presentation = self.presentation.borrow_mut();
            if let Some(layer) = presentation.as_mut() {
                let result = {
                    let state = self.shared.state.lock().unwrap();
                    let ratio = state.settings.density / state.settings.scale;
                    layer
                        .update(
                            (output.frame.width as f32 * ratio).round() as u32,
                            (output.frame.height as f32 * ratio).round() as u32,
                            output.x * ratio,
                            output.y * ratio,
                            state.settings.density,
                        )
                        .and_then(|()| layer.reveal())
                };
                match result {
                    Ok(revealed) => {
                        drop(presentation);
                        return (input_changed, revealed);
                    }
                    Err(error) => log::warn!("shader frame presentation: {error}"),
                }
                return (input_changed, false);
            }
        }
        (input_changed, false)
    }
    pub(crate) fn measure(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        if let Some(layer) = self.presentation.borrow().as_ref() {
            layer.measure(event)?;
        }
        Ok(())
    }
    pub(crate) fn layout(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        if let Some(owner) = self.owner.upgrade() {
            let origin = owner.mounted().borrow().layout_position_in_screen()?;
            let mut state = self.shared.state.lock().unwrap();
            let origin = Some((origin.x, origin.y));
            if state.screen_origin != origin {
                state.screen_origin = origin;
                drop(state);
                if self.shared.awaiting_origin.load(Ordering::Acquire) {
                    wake(&self.worker.send, self.key, &self.shared);
                }
            }
        }
        if let Some(layer) = self.presentation.borrow().as_ref() {
            layer.layout(event)?;
        }
        Ok(())
    }
}
impl Drop for Composite {
    fn drop(&mut self) {
        if let Some(mut layer) = self.presentation.get_mut().take() {
            layer.retire_gone();
            self.worker
                .retired
                .borrow_mut()
                .push((self.shared.clone(), layer));
        }
        self.shared.state.lock().unwrap().closed = true;
        wake(&self.worker.send, self.key, &self.shared);
    }
}
fn settings(owner: &Node, overlay: bool) -> Result<Settings> {
    let frame = owner.frame.get();
    let scene = owner.scene_active();
    // The window is a viewport. Moving toast/Sheet overflow must not resize
    // its input buffer or change the capture padding underneath the whole page.
    let bounds = if overlay || owner.id == nsh_core::ROOT_ID {
        crate::render::composition::geometry::Bounds::new(frame.width, frame.height)
    } else if scene {
        crate::render::composition::geometry::Bounds::new(frame.width, frame.height)
            .shaders(
                owner.shaders.overflow(owner.id),
                &owner.shader_layers.borrow(),
            )
            .own(&owner.style.borrow())
    } else {
        let bounds = owner.capture_bounds(0)?;
        let style = owner.style.borrow();
        bounds.filters(
            style
                .effects
                .as_ref()
                .map_or(&[], |effects| effects.filters.as_slice()),
        )
    };
    let (logical_w, logical_h) = (bounds.0[2] - bounds.0[0], bounds.0[3] - bounds.0[1]);
    let (_, _, mut scale) =
        super::geometry::target_size(logical_w.max(1.0), logical_h.max(1.0), owner.scale.get());
    if scale <= 0.0 {
        return Err(crate::render::node::Error::Unsupported(
            "invalid compositor coverage",
        ));
    }
    let dimensions = |scale| {
        let insets = bounds.insets(frame.width, frame.height, scale);
        let width = (frame.width * scale).ceil().max(1.0) as u32 + insets[0] + insets[2];
        let height = (frame.height * scale).ceil().max(1.0) as u32 + insets[1] + insets[3];
        (insets, width, height)
    };
    let (_, width, height) = dimensions(scale);
    if width.max(height) > 4096 {
        scale *= 4093.0 / width.max(height) as f32;
    }
    let (insets, width, height) = dimensions(scale);
    Ok(Settings {
        gpu: !scene && !overlay,
        box_size: nsh_core::Size::new(
            nsh_core::Points(frame.width),
            nsh_core::Points(frame.height),
        ),
        density: owner.scale.get(),
        width,
        height,
        insets,
        scene,
        capture: owner.navigation_capture.get(),
        keep_gpu_input: !overlay
            && (owner.id == nsh_core::ROOT_ID || owner.keeps_modal_shader_input()),
        navigation: if overlay || owner.shader_layers.borrow().is_empty() {
            None
        } else {
            owner.shaders.navigation_key(owner.id)
        },
        backdrop: owner.modal_backdrop.get(),
        first_screen_layer: owner.backdrop_layer.get().filter(|_| !overlay),
        shaders: if overlay {
            Vec::new()
        } else {
            let mut layers = owner.shader_layers.borrow().clone();
            layers.extend(owner.modal_screen_layers.borrow().iter().cloned());
            layers
        },
        filters: if overlay {
            Vec::new()
        } else {
            owner
                .style
                .borrow()
                .effects
                .as_ref()
                .map_or_else(Vec::new, |effects| effects.filters.clone())
        },
        scale,
    })
}
