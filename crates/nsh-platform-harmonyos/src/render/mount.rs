//! Applies each engine commit on the ArkUI thread. Deleted boxes stay alive
//! until a native frame and a subsequent event-loop turn have completed.

#[cfg(feature = "dev")]
mod testing;

use std::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

use nsh_core::{Command, MountBatch, MountItem, NodeId, ViewKind};
use nsh_engine::EngineControl;
use ohos_arkui_binding::component::attribute::ArkUICommonAttribute;

use crate::{
    app::frame_rate::UiDemand,
    render::{
        geometry::{Rect, Size},
        node::{Error, Node, Result},
    },
    text::typography::NativeText,
};

struct MountTarget {
    parent: std::rc::Weak<std::cell::RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    root: std::rc::Weak<std::cell::RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
}
impl MountTarget {
    fn mount(&mut self, root: crate::render::node::NativeNode) -> Result<()> {
        if let Some(parent) = self.parent.upgrade() {
            parent.borrow_mut().insert_child(root.clone(), 0)?;
            self.root = Rc::downgrade(&root);
        }
        Ok(())
    }
    fn unmount(&mut self) -> Result<()> {
        if let Some(root) = self.root.upgrade()
            && let Some(parent) = self.parent.upgrade()
        {
            // Deletion may already have detached this root before a later
            // cleanup step failed. Never remove a newer runtime's root.
            let index = parent
                .borrow()
                .children()
                .iter()
                .position(|child| Rc::ptr_eq(child, &root));
            if let Some(index) = index {
                parent.borrow_mut().remove_child(index)?;
            }
        }
        self.root = std::rc::Weak::new();
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Theme {
    pub(crate) tokens: Option<Arc<nsh_core::element::Tokens>>,
}
impl Theme {
    pub(crate) fn color(&self, name: &str, dark: bool) -> u32 {
        let names: &[&str] = match name {
            "popover" => &["popover", "card", "background"],
            "popover-foreground" => &["popover-foreground", "card-foreground", "foreground"],
            "card" => &["card", "background"],
            _ => &[name],
        };
        if let Some(value) = self
            .tokens
            .as_ref()
            .and_then(|tokens| names.iter().find_map(|name| tokens.color(name)))
        {
            return crate::text::typography::argb(
                if dark { value.dark } else { value.light }.into(),
            );
        }
        match (name, dark) {
            ("foreground" | "popover-foreground" | "primary", true) => 0xfffafafa,
            ("foreground" | "popover-foreground" | "primary", false) => 0xff171717,
            ("border", true) => 0xff404040,
            ("border", false) => 0xffe5e5e5,
            ("destructive", true) => 0xfff87171,
            ("destructive", false) => 0xffdc2626,
            ("card" | "popover", true) => 0xff171717,
            (_, true) => 0xff0a0a0a,
            (_, false) => 0xffffffff,
        }
    }
}

pub(crate) struct UiContext {
    pub(crate) theme: std::cell::RefCell<Theme>,
    pub(crate) cursor: crate::input::cursor::State,
    pub(crate) shaders: super::shader::Shaders,
    /// The video whose native Stack is temporarily the root's last child.
    pub(crate) fullscreen_video: std::cell::RefCell<std::rc::Weak<Node>>,
    pub(crate) scroll_reports: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) scroll_watchers: crate::scroll::watch::ScrollWatchers,
    pub(crate) flushing_batch: std::cell::Cell<bool>,
    pub(crate) scene: std::cell::RefCell<Option<Rc<crate::render::composition::scene::Scene>>>,
    pub(crate) compositor:
        std::cell::RefCell<Option<Rc<crate::render::composition::worker::Worker>>>,
    pub(crate) composite_nodes: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) key_focus: std::cell::RefCell<std::rc::Weak<Node>>,
    pub(crate) selected_text: std::cell::RefCell<Option<std::rc::Weak<Node>>>,
    pub(crate) accessibility_dirty: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) accessibility_focus: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) accessibility_live_regions: std::cell::Cell<usize>,
    pub(crate) accessibility_wrappers: std::cell::Cell<usize>,
    /// How many nodes are leaving (exit transitions, popped screens): while
    /// none is, no mount item needs the walk up its ancestors that looks
    /// for one ([`Node::set_leaving`]).
    pub(crate) leaving_nodes: std::cell::Cell<usize>,
    pub(crate) pending_delete_count: std::cell::Cell<usize>,
    pub(crate) unreclaimed_delete_count: std::cell::Cell<usize>,
    pub(crate) arkui: std::cell::RefCell<crate::interop::sdk::Batch>,
    pub(crate) arkui_views: std::cell::RefCell<Vec<crate::interop::attachment::NativeAttachment>>,
    pub(crate) deferred_webs: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) host: Arc<crate::app::session::Services>,
    pub(crate) dispatcher: std::sync::Weak<crate::app::runtime::Dispatcher>,
    pub(crate) env: napi_ohos::Env,
    pub(crate) scale: std::cell::Cell<f32>,
    pub(crate) text_scale: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) control: EngineControl,
    pub(crate) animator: Rc<crate::render::animation::Animator>,
    pub(crate) images: Arc<crate::media::image::Loader>,
    pub(crate) access: Arc<nsh_engine::Access>,
    pub(crate) assets: Arc<dyn crate::services::assets::StreamAssets>,
    pub(crate) components: crate::interop::component::Components,
    pub(crate) touches: crate::input::touch::State,
    pub(crate) fonts: Arc<std::sync::Mutex<crate::text::fonts::Fonts>>,
    pub(crate) canvas_bytes: Rc<std::cell::Cell<usize>>,
    pub(crate) platform: std::cell::Cell<nsh_engine::Environment>,
    pub(crate) main_window: std::cell::RefCell<crate::services::desktop::MainWindowState>,
    pub(crate) appearance: std::cell::Cell<nsh_core::platform::Appearance>,
}

struct BatchGuard(Rc<UiContext>, bool);

impl BatchGuard {
    fn new(context: Rc<UiContext>) -> Self {
        let previous = context.flushing_batch.replace(true);
        Self(context, previous)
    }
}

impl Drop for BatchGuard {
    fn drop(&mut self) {
        self.0.flushing_batch.set(self.1);
    }
}

pub(crate) struct Mounter {
    context: Rc<UiContext>,
    mount: MountTarget,

    // IDs grow across navigation. Keep only mounted nodes and preserve ID order
    // for focus, environment updates and reverse-order teardown.
    nodes: BTreeMap<NodeId, Rc<Node>>,
    // Back priority must not depend on insertion order.
    stacks: Vec<NodeId>,
    removed: Vec<Rc<Node>>,
    retired: VecDeque<(u64, Vec<Rc<Node>>)>,
    epoch: u64,
    /// Batches applied, from 1: what the mount log counts by.
    batches: u64,
    modals: Vec<std::rc::Weak<crate::navigation::modal::Modal>>,
    pending_delete: Vec<NodeId>,
    deferred: Vec<MountItem>,
    uncreated_scroll_content: rustc_hash::FxHashMap<NodeId, Size>,
    dialogs: crate::services::dialogs::Dialogs,
    desktop: crate::services::desktop::DesktopState,
    offscreen_renders: crate::render::canvas_render::OffscreenRenders,
    surface_size: std::cell::Cell<Size>,
}

impl std::ops::Deref for Mounter {
    type Target = UiContext;
    fn deref(&self) -> &UiContext {
        &self.context
    }
}

impl Mounter {
    pub(crate) fn batch_count(&self) -> u64 {
        self.batches
    }
    pub(crate) fn scene_ready(&self) -> Result<()> {
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.ready()?;
        }
        self.flush_arkui()
    }
    pub(crate) fn invalidate_canvases(&self) -> Result<()> {
        for node in self.nodes.values() {
            if let Some(canvas) = node.canvas.borrow().as_ref() {
                canvas.invalidate()?;
                if node.scene_active() {
                    node.invalidate_scene();
                }
            }
        }
        Ok(())
    }
    pub(crate) fn composite_ready(&self, id: NodeId, generation: u32) -> Result<()> {
        if let Some(worker) = self.compositor.borrow().as_ref() {
            worker.collect();
        }
        if let Some(node) = self.nodes.get(&id) {
            node.composite_ready(generation)?;
            if id == nsh_core::ROOT_ID {
                self.shaders.reveal()?;
            } else if let Some(root) = self.nodes.get(&nsh_core::ROOT_ID)
                && root.composite_dirty.get()
            {
                Self::attribute(root.update_wrapper());
            }
            if let Some(modal) = node.modal.borrow().as_ref() {
                modal.refresh_presentation()?;
            }
        }
        self.flush_arkui()
    }
    pub(crate) fn navigation_event(
        &self,
        event: crate::render::shader::navigation::Event,
    ) -> Result<()> {
        self.shaders.navigation_event(event)?;
        self.reconcile_navigation_capture()?;
        self.flush_arkui()?;
        self.animator.request_frame();
        Ok(())
    }
    pub(crate) fn text_selection(
        &self,
        id: NodeId,
        command: crate::text::selection::controller::Command,
    ) -> Result<()> {
        let selected = self
            .selected_text
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade);
        if let Some(node) = selected.filter(|node| node.id == id) {
            node.selection_command(command)?;
        }
        Ok(())
    }
    #[cfg(feature = "api-23")]
    pub(crate) fn accessibility_event(
        &self,
        id: NodeId,
        event: ohos_accessibility_binding::EventType,
    ) -> Result<()> {
        // A callback queued before removal cannot reach a retired node.
        if let Some(node) = self.nodes.get(&id) {
            node.accessibility_provider.send_event(event)?;
        }
        Ok(())
    }

    pub(crate) fn environment(&self, environment: nsh_engine::Environment) -> Result<()> {
        let density_changed =
            self.scale.replace(environment.pixel_ratio) != environment.pixel_ratio;
        let reduce_motion_changed = self.platform.get().reduce_motion != environment.reduce_motion;
        let was_background = self.platform.get().lifecycle == nsh_engine::Lifecycle::Background;
        self.platform.set(environment);
        let mut result = Ok(());
        for node in self.nodes.values() {
            if density_changed {
                result = result.and(node.density_changed());
            }
            if let Some(screen) = node.screen.borrow().as_ref() {
                result = result.and(screen.update_environment());
            }
            if let Some(modal) = node.modal.borrow().as_ref() {
                modal.refresh_metrics();
                result = result.and(
                    node.mounted()
                        .borrow()
                        .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)
                        .map_err(Into::into),
                );
            }
        }
        let background = environment.lifecycle == nsh_engine::Lifecycle::Background;
        if background {
            result = result.and(self.shaders.cancel_navigation("background"));
        }
        if reduce_motion_changed || background || was_background {
            result = result.and(self.reconcile_navigation_capture());
            result = result.and(self.flush_arkui());
        }
        if density_changed {
            if let Some(scene) = self.scene.borrow().as_ref() {
                scene.invalidate();
            }
            result = result.and(self.flush_arkui());
            self.animator.request_frame();
        }
        result.and(self.system_appearance())
    }
    fn apply_main_backdrop(&self) {
        if let Ok(root) = self.node(nsh_core::ROOT_ID) {
            use nsh_core::{desktop::Backdrop, platform::Appearance};
            use ohos_arkui_binding::{
                common::attribute::ArkUINodeAttributeNumber,
                types::{advanced::BlurStyle, attribute::ArkUINodeAttributeType as A},
            };
            let backdrop = self.main_window.borrow().backdrop;
            let style = match backdrop {
                Backdrop::None => None,
                Backdrop::Window => Some(BlurStyle::BackgroundThick),
                Backdrop::Sidebar => Some(BlurStyle::BackgroundRegular),
                Backdrop::Popover => Some(BlurStyle::BackgroundThin),
                Backdrop::Hud => Some(BlurStyle::BackgroundUltraThick),
            };
            if let Some(style) = style {
                // `ArkUI_ColorMode`: 0 = SYSTEM, 1 = LIGHT, 2 = DARK.
                let color_mode = match self.appearance.get() {
                    Appearance::System => 0,
                    Appearance::Light => 1,
                    Appearance::Dark => 2,
                };
                let _ = root.native.borrow().set_background_blur_style([
                    ArkUINodeAttributeNumber::Int(u32::from(style) as i32),
                    ArkUINodeAttributeNumber::Int(color_mode),
                ]);
            } else {
                let _ = root.native.borrow().reset_attribute(A::BackgroundBlurStyle);
            }
        }
    }
    fn system_appearance(&self) -> Result<()> {
        let appearance = self.appearance.get();
        self.host.set_appearance(appearance)?;
        self.apply_main_backdrop();
        let dark =
            appearance.resolve(self.platform.get().color_scheme) == nsh_engine::ColorScheme::Dark;
        let config = self.dispatcher.upgrade().and_then(|route| {
            let status = *route.status_bar.lock().unwrap();
            status
                .filter(|(_, previous)| *previous != dark)
                .map(|(config, _)| config)
        });
        if let Some(config) = config {
            self.host
                .status_bar(self.dispatcher.clone(), config, dark)?;
        }
        Ok(())
    }
    pub(crate) fn native_context(&self, ready: bool) -> Result<()> {
        // Context lifetime is shared by every owner. A single SDK failure must
        // not leave the remaining owners attached to an invalid context.
        let mut result = Ok(());
        if !ready {
            result = result.and(self.shaders.cancel_navigation("context-lost"));
            self.deferred_webs.borrow_mut().clear();
            let mut pending = self.arkui.borrow_mut();
            pending.editors.clear();
            pending.commands.clear();
            pending.views.clear();
            pending.webs.clear();
            pending.web_commands.clear();
            // Retirements keep their targets until SDK cleanup or the next ready batch.
        }
        for node in self.nodes.values() {
            if !ready {
                if let Some(wrapper) = node.wrapper.borrow().as_ref() {
                    wrapper.invalidate_context();
                }
                if let Some(scroll) = node.scroll.borrow().as_ref()
                    && let Some(plane) = scroll.overlay.borrow().as_ref()
                {
                    plane.invalidate_context();
                }
            }
            if ready {
                result = result.and(node.update_wrapper());
            }
            if let Some(input) = node.input.borrow().as_ref() {
                input.context(ready);
            }
            if let Some(web) = node.web.borrow().as_ref() {
                result = result.and(web.context(ready));
            }
            if let Some(video) = node.video.borrow().as_ref() {
                result = result.and(video.context(ready));
            }
        }
        result = result.and(self.shaders.reconcile_hosts_now());
        if ready {
            result = result.and(self.flush_arkui());
        }
        result
    }
    pub(crate) fn cancel_input(&self) -> Result<()> {
        let mut result = Ok(());
        let desktop = self.platform.get().os.is_desktop();
        if let Ok(root) = self.node(nsh_core::ROOT_ID) {
            result = result.and(self.touches.borrow_mut().cancel(root));
        }
        for node in self.nodes.values() {
            if !desktop && let Some(input) = node.input.borrow().as_ref() {
                result = result.and(input.suspend());
            }
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                result = result.and(scroll.suspend());
                result = result.and(scroll.block(false));
            }
        }
        result
    }
    pub(crate) fn flush_scroll_layout(&self, id: NodeId) -> Result<()> {
        if let Ok(node) = self.node(id)
            && let Some(scroll) = node.scroll.borrow().as_ref()
        {
            scroll.flush_layout()?;
        }
        Ok(())
    }
    pub(crate) fn wants_back(&self) -> bool {
        self.fullscreen_video.borrow().strong_count() > 0
            || self.dialogs.visible()
            || self
                .modals
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .any(|modal| modal.is_visible())
            || self.back_stack(None).is_some()
            || self.nodes.get(&nsh_core::ROOT_ID).is_some_and(|root| {
                root.interaction
                    .mask()
                    .contains(nsh_core::EventKind::BackPress)
            })
    }
    pub(crate) fn back(&mut self) -> Result<()> {
        let fullscreen = self.fullscreen_video.borrow().upgrade();
        if let Some(owner) = fullscreen
            && let Some(video) = owner.video.borrow().as_ref()
        {
            return video.set_fullscreen(false);
        }
        if self.dialogs.visible() {
            // ArkUI handles back and cancelability for its system dialogs.
            // Do not reply early while the window still displays the dialog.
            return Ok(());
        }
        self.route_back(true)
    }
    pub(crate) fn dismiss_keyboard(&self) -> Result<()> {
        self.node(nsh_core::ROOT_ID)?.dismiss_keyboard()
    }
    fn back_fallback(&self) -> Result<()> {
        self.route_back(false)
    }
    fn route_back(&self, app_listener: bool) -> Result<()> {
        use nsh_core::navigation::{Back, back_target};
        let desktop = self.platform.get().os.is_desktop();
        let modal = self
            .modals
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|modal| modal.is_desktop_focused())
            .or_else(|| {
                self.modals
                    .iter()
                    .rev()
                    .filter_map(std::rc::Weak::upgrade)
                    .find(|modal| {
                        modal.is_visible()
                            && (!desktop
                                || (!modal.is_desktop_window() && !modal.is_main_window_host())
                                || modal.is_desktop_modal())
                    })
            });
        let target = back_target(
            modal.as_ref().and_then(|modal| modal.node_id()),
            app_listener
                && self
                    .node(nsh_core::ROOT_ID)?
                    .interaction
                    .mask()
                    .contains(nsh_core::EventKind::BackPress),
            |scope| self.back_stack(scope).map(|(id, _)| id),
        );
        match target {
            Some(Back::Stack(id)) => {
                if let Some((_, screen)) = self.stack_candidate(id) {
                    screen.request_back()?;
                }
            }
            Some(Back::Modal(_)) => {
                if let Some(modal) = modal {
                    modal.request_close()?;
                }
            }
            Some(Back::App) => self.control.dispatch_event(
                nsh_core::ROOT_ID,
                nsh_core::EventKind::BackPress,
                nsh_core::EventPayload::None,
            ),
            None => {
                if !desktop {
                    // BackHandler runs in the engine after the native callback has
                    // returned. If it declines and no NSH root can go back, resume
                    // the host router's default action instead of swallowing it.
                    self.host
                        .send_host_command(crate::app::session::HostCommand {
                            kind: "back".into(),
                            ..Default::default()
                        })?;
                }
            }
        }
        Ok(())
    }
    fn contains(&self, ancestor: NodeId, mut child: NodeId) -> bool {
        while let Ok(node) = self.node(child) {
            if child == ancestor {
                return true;
            }
            let Some(parent) = node.parent.get() else {
                break;
            };
            child = parent;
        }
        false
    }
    fn stack_candidate(
        &self,
        id: NodeId,
    ) -> Option<(NodeId, Rc<crate::navigation::stack::Screen>)> {
        let stack = self.node(id).ok().filter(|node| node.visible_in_tree())?;
        let screens: Vec<_> = stack
            .children
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter_map(|node| node.screen.borrow().clone())
            .filter(|screen| !screen.popped.get())
            .collect();
        let top = screens.last()?;
        (screens.len() > 1 || top.config.borrow().prevent_back).then(|| (id, top.clone()))
    }
    fn back_stack(
        &self,
        scope: Option<NodeId>,
    ) -> Option<(NodeId, Rc<crate::navigation::stack::Screen>)> {
        let candidates: Vec<_> = self
            .stacks
            .iter()
            .copied()
            .filter(|id| scope.is_none_or(|scope| self.contains(scope, *id)))
            .filter_map(|id| self.stack_candidate(id))
            .collect();
        nsh_core::navigation::innermost(&candidates, |(outer, _), (inner, _)| {
            self.contains(*outer, *inner)
        })
        .cloned()
    }
    pub(crate) fn reconcile_navigation(&mut self) -> Result<()> {
        let result = (|| -> Result<()> {
            self.shaders.reconcile_hosts_now()?;
            crate::navigation::stack::reconcile(
                self.stacks.iter().filter_map(|id| self.nodes.get(id)),
            )?;
            for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
                if self.is_unopened_window_container(&modal) {
                    modal.cancel_pending_present();
                    continue;
                }
                modal.present_if_pending()?;
            }
            Ok(())
        })();
        // Capture releases deferred by a batch still need synchronization if
        // a preceding host, stack or modal update failed.
        result.and(self.reconcile_navigation_capture())
    }
    fn reconcile_navigation_capture(&self) -> Result<()> {
        let res = self
            .shaders
            .reconcile_navigation_capture(self.platform.get().reduce_motion);
        if !self.flushing_batch.get() {
            for node in self.nodes.values() {
                if let Some(scroll) = node.scroll.borrow().as_ref() {
                    Self::attribute(scroll.update_child_culling());
                }
            }
        }
        res
    }
    fn is_unopened_window_container(&self, modal: &crate::navigation::modal::Modal) -> bool {
        if !self.platform.get().os.is_desktop()
            || modal.has_modal_config()
            || modal.is_desktop_window()
            || modal.is_main_window_host()
        {
            return false;
        }
        let Some(id) = modal.node_id() else {
            return false;
        };
        let Ok(node) = self.node(id) else {
            return false;
        };
        node.parent.get() == Some(nsh_core::ROOT_ID)
            && node
                .children
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .all(|child| child.modal.borrow().is_some())
    }
    fn root_has_non_modal_children(&self) -> bool {
        let Ok(root) = self.node(nsh_core::ROOT_ID) else {
            return false;
        };
        root.children
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|child| child.modal.borrow().is_none())
    }
    pub(crate) fn note_main_window_launch_applied(&mut self) {
        self.desktop.main_window_launch_applied = true;
    }
    pub(crate) fn on_main_window_state(&self, state: nsh_core::desktop::WindowState) {
        if let Some(id) = self.desktop.hosted_main_window {
            self.control.dispatch_event(
                id,
                nsh_core::EventKind::Window,
                nsh_core::EventPayload::Window(Box::new(nsh_core::desktop::WindowEvent::State(
                    Box::new(state),
                ))),
            );
        }
    }
    pub(crate) fn on_main_window_chrome_change(
        &mut self,
        state: nsh_core::desktop::WindowState,
    ) -> Result<()> {
        if !state.focused || state.mode == nsh_core::desktop::WindowMode::FullScreen {
            self.desktop.menu_bar_alt_shown = false;
        }
        self.sync_menu_bar()
    }
    pub(crate) fn on_main_window_title_bar(&self, edges: nsh_core::Edges<nsh_core::Points>) {
        if let Some(id) = self.desktop.hosted_main_window {
            self.control.dispatch_event(
                id,
                nsh_core::EventKind::Window,
                nsh_core::EventPayload::Window(Box::new(nsh_core::desktop::WindowEvent::TitleBar(
                    edges,
                ))),
            );
        }
    }
    fn apply_main_window_attributes(
        &mut self,
        attrs: &nsh_core::desktop::WindowAttributes,
    ) -> Result<()> {
        if let Some(close_on_press) = attrs.close_on_press {
            self.desktop.close_on_press = close_on_press;
        }
        let title_or_frame_changed = attrs.title_bar.is_some() || attrs.frame.is_some();
        self.main_window.borrow_mut().apply_attributes(attrs);
        if attrs.backdrop.is_some() {
            self.apply_main_backdrop();
        }
        self.host
            .desktop_command(&crate::services::desktop::wire_attributes(attrs))?;
        if title_or_frame_changed {
            self.sync_menu_bar()?;
        }
        Ok(())
    }
    fn open_hosted_main_window(
        &mut self,
        id: NodeId,
        modal: &crate::navigation::modal::Modal,
        open: nsh_core::desktop::OpenWindow,
    ) -> Result<()> {
        let root = self.node(nsh_core::ROOT_ID)?.clone();
        modal.open_in_main_window(&root)?;
        self.desktop.hosted_main_window = Some(id);
        self.apply_main_window_attributes(&open.options.attributes)?;
        if (!self.desktop.main_window_launch_applied || !open.implicit)
            && let Some(launch) = crate::services::desktop::wire_open_main_window_launch(&open)
        {
            self.desktop.main_window_launch_applied = true;
            self.host.desktop_command(&launch)?;
        }
        if let Some(state) = self.platform.get().window_state {
            self.on_main_window_state(state);
        }
        let title_bar = self.platform.get().title_bar;
        if title_bar != nsh_core::Edges::default() {
            self.on_main_window_title_bar(title_bar);
        }
        Ok(())
    }
    pub(crate) fn dialog_result(&mut self, id: u32, index: i32) -> Result<()> {
        self.dialogs.finish(id, index)
    }
    fn apply_desktop_command(
        &mut self,
        id: NodeId,
        command: nsh_core::desktop::DesktopCommand,
    ) -> Result<()> {
        use crate::services::desktop::{
            WireCommand, build_tray_icon, global_shortcut_registered_payload, ohos_hotkey,
            serialize_menu_items, tray_title_text, wire_request,
        };
        use nsh_core::desktop::DesktopCommand;
        if id != nsh_core::ROOT_ID {
            if self.desktop.hosted_main_window == Some(id) {
                match command {
                    DesktopCommand::SetAttributes(attrs) => {
                        self.apply_main_window_attributes(&attrs)?;
                    }
                    DesktopCommand::Request(request) => {
                        if matches!(request, nsh_core::desktop::WindowRequest::Close) {
                            self.close_desktop_children(id)?;
                            self.close_desktop_children(nsh_core::ROOT_ID)?;
                            self.desktop.hosted_main_window = None;
                            if let Ok(node) = self.node(id)
                                && let Some(modal) = node.modal.borrow().clone()
                            {
                                modal.close(false)?;
                            }
                        } else {
                            self.host.desktop_command(&wire_request(&request))?;
                        }
                    }
                    DesktopCommand::PopupMenu { items, at } => {
                        let generation = self.desktop.next_popup_generation();
                        let wire_items = serialize_menu_items(&items, &self.desktop.labels);
                        let inset = self.desktop.menu_bar_inset.get();
                        let pos = at
                            .map(|p| (p.x.0, p.y.0 + inset))
                            .or_else(|| self.touches.borrow().last_pointer());
                        self.host.desktop_command(&WireCommand::PopupMenu {
                            window_id: None,
                            generation,
                            x: pos.map(|p| p.0),
                            y: pos.map(|p| p.1),
                            items: wire_items,
                        })?;
                    }
                    _ => {}
                }
                return Ok(());
            }
            match command {
                DesktopCommand::PopupMenu { items, at } => {
                    let generation = self.desktop.next_popup_generation();
                    let wire_items = serialize_menu_items(&items, &self.desktop.labels);
                    let pos = at
                        .map(|p| (p.x.0, p.y.0))
                        .or_else(|| self.touches.borrow().last_pointer());
                    self.host.desktop_command(&WireCommand::PopupMenu {
                        window_id: Some(id),
                        generation,
                        x: pos.map(|p| p.0),
                        y: pos.map(|p| p.1),
                        items: wire_items,
                    })?;
                }
                other => {
                    let Ok(node) = self.node(id) else {
                        return Ok(());
                    };
                    let modal = node.modal.borrow().clone();
                    if let Some(modal) = modal {
                        match other {
                            DesktopCommand::OpenWindow(mut open) => {
                                let should_host_in_main = self.desktop.hosted_main_window.is_none()
                                    && (open.implicit
                                        || (open.parent.is_none()
                                            && !open.options.modal
                                            && open.options.kind
                                                == nsh_core::desktop::WindowKind::Normal
                                            && !self.root_has_non_modal_children()));
                                if should_host_in_main {
                                    self.open_hosted_main_window(id, &modal, *open)?;
                                } else {
                                    if open.parent == self.desktop.hosted_main_window {
                                        open.parent = Some(nsh_core::ROOT_ID);
                                    }
                                    modal.open_desktop_window(*open)?;
                                }
                            }
                            DesktopCommand::SetAttributes(attrs) => {
                                modal.set_desktop_attributes(*attrs)?;
                            }
                            DesktopCommand::Request(req) => modal.desktop_request(req)?,
                            _ => {}
                        }
                    }
                }
            }
            return Ok(());
        }
        match command {
            DesktopCommand::SetAttributes(attrs) => {
                self.apply_main_window_attributes(&attrs)?;
            }
            DesktopCommand::Request(request) => {
                if matches!(request, nsh_core::desktop::WindowRequest::Close) {
                    self.close_desktop_children(nsh_core::ROOT_ID)?;
                }
                self.host.desktop_command(&wire_request(&request))?;
            }
            DesktopCommand::Quit => {
                self.host.desktop_command(&WireCommand::Quit)?;
            }
            DesktopCommand::Restart => {
                self.host.desktop_command(&WireCommand::Restart)?;
            }
            DesktopCommand::SetQuitOnRequest(quit) => {
                self.desktop.quit_on_request = quit;
                if let Some(route) = self.dispatcher.upgrade() {
                    route.keyboard.lock().unwrap().quit_on_request = quit;
                }
            }
            DesktopCommand::SetMenu(menu) => {
                self.desktop.menu = menu.clone();
                if let Some(route) = self.dispatcher.upgrade() {
                    route.keyboard.lock().unwrap().menu = menu;
                }
                self.sync_menu_bar()?;
            }
            DesktopCommand::SetMenuBar(mode) => {
                self.desktop.menu_bar_mode = mode;
                self.desktop.menu_bar_alt_shown = false;
                self.sync_menu_bar()?;
            }
            DesktopCommand::SetMenuLabels(labels) => {
                self.desktop.labels = *labels;
                if self.desktop.menu.is_some() {
                    self.sync_menu_bar()?;
                }
            }
            DesktopCommand::PopupMenu { items, at } => {
                let generation = self.desktop.next_popup_generation();
                let wire_items = serialize_menu_items(&items, &self.desktop.labels);
                let pos = at
                    .map(|p| (p.x.0, p.y.0 + self.desktop.menu_bar_inset.get()))
                    .or_else(|| self.touches.borrow().last_pointer());
                self.host.desktop_command(&WireCommand::PopupMenu {
                    window_id: None,
                    generation,
                    x: pos.map(|p| p.0),
                    y: pos.map(|p| p.1),
                    items: wire_items,
                })?;
            }
            DesktopCommand::SetTray(Some(tray)) => {
                let items = serialize_menu_items(&tray.menu, &self.desktop.labels);
                let icon = build_tray_icon(tray.icon.as_ref(), self.assets.as_ref());
                self.host.desktop_command(&WireCommand::SetTray {
                    tooltip: tray.tooltip,
                    title: tray_title_text(&tray.title),
                    click: tray.click,
                    icon,
                    items,
                })?;
            }
            DesktopCommand::SetTray(None) => {
                self.host.desktop_command(&WireCommand::RemoveTray)?;
            }
            DesktopCommand::RegisterGlobalShortcut { id, shortcut } => {
                if let Some((pre_keys, final_key)) = ohos_hotkey(shortcut) {
                    self.host
                        .desktop_command(&WireCommand::RegisterGlobalShortcut {
                            id,
                            pre_keys,
                            final_key,
                        })?;
                } else {
                    let (kind, payload) = global_shortcut_registered_payload(id, false);
                    self.control
                        .dispatch_event(nsh_core::ROOT_ID, kind, payload);
                }
            }
            DesktopCommand::UnregisterGlobalShortcut(id) => {
                self.host
                    .desktop_command(&WireCommand::UnregisterGlobalShortcut { id })?;
            }
            DesktopCommand::OpenWindow(_)
            | DesktopCommand::SetQuitMode(_)
            | DesktopCommand::SetQuitRequestOptions(_)
            | DesktopCommand::CancelQuitRequest => {}
        }
        Ok(())
    }
    fn ensure_menu_bar_host(&mut self) -> Result<u32> {
        if let Some((_, attachment)) = &self.desktop.menu_bar_host {
            return Ok(attachment.id());
        }
        use ohos_arkui_binding::{
            component::built_in_component::Stack,
            types::{advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A},
        };
        let host = Stack::new()?.into_node();
        {
            let borrowed = host.borrow();
            borrowed.set_length_metric_unit(LengthMetricUnit::Vp)?;
            borrowed.width(self.surface_size.get().width.max(0.0))?;
            borrowed.height(crate::services::desktop::MENU_BAR_HEIGHT)?;
            borrowed.set_i32_attribute(A::StackAlignContent, 0)?;
        }
        let attachment = crate::interop::attachment::NativeAttachment::new(&host)?;
        if let Some(parent) = self.mount.parent.upgrade() {
            let idx = parent.borrow().children().len();
            parent.borrow_mut().insert_child(host.clone(), idx)?;
        }
        let target_id = attachment.id();
        self.desktop.menu_bar_host = Some((host, attachment));
        Ok(target_id)
    }

    fn sync_menu_bar(&mut self) -> Result<()> {
        self.sync_menu_bar_with_open(None)
    }

    fn menu_bar_available(&self) -> bool {
        let os = self.platform.get().os;
        let has_menu = self
            .desktop
            .menu
            .as_ref()
            .is_some_and(|items| !items.is_empty());
        let main = self.main_window.borrow();
        let full_screen = self
            .platform
            .get()
            .window_state
            .is_some_and(|state| state.mode == nsh_core::desktop::WindowMode::FullScreen);
        crate::services::desktop::menu_bar_visible(
            os,
            has_menu,
            main.title_bar_hidden,
            main.frame_none,
            full_screen,
            nsh_core::desktop::MenuBarMode::Shown,
            false,
        )
    }

    pub(crate) fn desktop_menu_bar_key(
        &mut self,
        action: crate::services::desktop::MenuBarKeyAction,
    ) -> Result<()> {
        use crate::services::desktop::MenuBarKeyAction;
        use nsh_core::desktop::MenuBarMode;
        if !self.menu_bar_available() {
            return Ok(());
        }
        match action {
            MenuBarKeyAction::ToggleAlt => {
                if self.desktop.menu_bar_mode == MenuBarMode::AutoHide {
                    self.desktop.menu_bar_alt_shown = !self.desktop.menu_bar_alt_shown;
                    self.sync_menu_bar()?;
                }
            }
            MenuBarKeyAction::Hide => {
                if self.desktop.menu_bar_mode == MenuBarMode::AutoHide
                    && self.desktop.menu_bar_alt_shown
                {
                    self.desktop.menu_bar_alt_shown = false;
                    self.sync_menu_bar()?;
                }
            }
            MenuBarKeyAction::ToggleF10 => {
                if self.desktop.menu_bar_mode == MenuBarMode::AutoHide
                    && self.desktop.menu_bar_alt_shown
                {
                    self.desktop.menu_bar_alt_shown = false;
                    self.sync_menu_bar()?;
                } else {
                    if self.desktop.menu_bar_mode == MenuBarMode::AutoHide {
                        self.desktop.menu_bar_alt_shown = true;
                    }
                    self.sync_menu_bar_with_open(Some(None))?;
                }
            }
            MenuBarKeyAction::OpenInitial(initial) => {
                if self.desktop.menu_bar_mode == MenuBarMode::AutoHide {
                    self.desktop.menu_bar_alt_shown = true;
                }
                self.sync_menu_bar_with_open(Some(Some(initial)))?;
            }
        }
        Ok(())
    }

    fn sync_menu_bar_with_open(&mut self, open: Option<Option<char>>) -> Result<()> {
        use crate::services::desktop::{
            MENU_BAR_HEIGHT, WireCommand, menu_bar_first_index, menu_bar_initial_index,
            menu_bar_visible, serialize_menu_items,
        };
        use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
        let os = self.platform.get().os;
        if !os.is_desktop() {
            return Ok(());
        }
        let has_menu = self
            .desktop
            .menu
            .as_ref()
            .is_some_and(|items| !items.is_empty());
        let main = self.main_window.borrow();
        let full_screen = self
            .platform
            .get()
            .window_state
            .is_some_and(|state| state.mode == nsh_core::desktop::WindowMode::FullScreen);
        let visible = menu_bar_visible(
            os,
            has_menu,
            main.title_bar_hidden,
            main.frame_none,
            full_screen,
            self.desktop.menu_bar_mode,
            self.desktop.menu_bar_alt_shown,
        );
        drop(main);
        if let Some(route) = self.dispatcher.upgrade() {
            let mut keyboard = route.keyboard.lock().unwrap();
            keyboard.menu_bar_mode = self.desktop.menu_bar_mode;
            keyboard.menu_bar_revealed =
                visible && self.desktop.menu_bar_mode == nsh_core::desktop::MenuBarMode::AutoHide;
        }
        let target_id = if visible {
            let id = self.ensure_menu_bar_host()?;
            if let Some((host, _)) = &self.desktop.menu_bar_host {
                let borrowed = host.borrow();
                borrowed.width(self.surface_size.get().width.max(0.0))?;
                borrowed.height(MENU_BAR_HEIGHT)?;
                borrowed.set_i32_attribute(A::Visibility, 0)?;
            }
            Some(id)
        } else {
            if let Some((host, _)) = &self.desktop.menu_bar_host {
                let borrowed = host.borrow();
                borrowed.height(0.0)?;
                borrowed.set_i32_attribute(A::Visibility, 2)?;
            }
            self.desktop
                .menu_bar_host
                .as_ref()
                .map(|(_, attachment)| attachment.id())
        };
        let items = if visible {
            self.desktop
                .menu
                .as_deref()
                .map(|items| serialize_menu_items(items, &self.desktop.labels))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let open_index = visible
            .then_some(open)
            .flatten()
            .and_then(|which| match which {
                Some(ch) => menu_bar_initial_index(&items, ch),
                None => menu_bar_first_index(&items),
            })
            .and_then(|idx| u32::try_from(idx).ok());
        self.host.desktop_command(&WireCommand::SetMenuBar {
            visible,
            target_id,
            open_index,
            items,
        })?;
        let inset = if visible { MENU_BAR_HEIGHT } else { 0.0 };
        if (inset - self.desktop.menu_bar_inset.get()).abs() > f32::EPSILON {
            self.desktop.menu_bar_inset.set(inset);
            self.apply_surface_size(self.surface_size.get())?;
        }
        Ok(())
    }
    pub(crate) fn desktop_event(
        &mut self,
        event: crate::services::desktop::DesktopEvent,
    ) -> Result<()> {
        use crate::services::desktop::{
            DesktopEvent, WireCommand, global_shortcut_registered_payload, menu_closed_payload,
            menu_select_payload, tray_click_payload,
        };
        use nsh_core::{
            EventKind, EventPayload,
            desktop::{WindowCloseCause, WindowEvent},
        };
        match event {
            DesktopEvent::ClosePress(reason) => {
                if let Some(window_id) = self.desktop.hosted_main_window {
                    self.control.dispatch_event(
                        window_id,
                        EventKind::Window,
                        EventPayload::Window(Box::new(WindowEvent::ClosePress { reason })),
                    );
                    if self.desktop.close_on_press {
                        self.close_desktop_children(window_id)?;
                        self.close_desktop_children(nsh_core::ROOT_ID)?;
                        self.control.dispatch_event(
                            window_id,
                            EventKind::Window,
                            EventPayload::Window(Box::new(WindowEvent::Closed {
                                cause: WindowCloseCause::ClosePress,
                            })),
                        );
                    }
                } else {
                    self.control.dispatch_event(
                        nsh_core::ROOT_ID,
                        EventKind::Window,
                        EventPayload::Window(Box::new(WindowEvent::ClosePress { reason })),
                    );
                    if self.desktop.close_on_press {
                        self.close_desktop_children(nsh_core::ROOT_ID)?;
                        self.control.dispatch_event(
                            nsh_core::ROOT_ID,
                            EventKind::Window,
                            EventPayload::Window(Box::new(WindowEvent::Closed {
                                cause: WindowCloseCause::ClosePress,
                            })),
                        );
                        self.host.desktop_command(&WireCommand::Quit)?;
                    }
                }
            }
            DesktopEvent::MenuSelect {
                generation,
                id,
                role,
            } => {
                if generation != 0 && generation == self.desktop.popup_generation {
                    self.desktop.popup_open = false;
                } else if generation == 0
                    && self.desktop.menu_bar_mode == nsh_core::desktop::MenuBarMode::AutoHide
                    && self.desktop.menu_bar_alt_shown
                {
                    self.desktop.menu_bar_alt_shown = false;
                    self.sync_menu_bar()?;
                }
                self.desktop_menu_action(id, role)?;
            }
            DesktopEvent::MenuClose { generation } => {
                if generation == self.desktop.popup_generation {
                    self.desktop.selection_menu_target = None;
                    if self.desktop.popup_open {
                        self.desktop.popup_open = false;
                        let (kind, payload) = menu_closed_payload();
                        self.control
                            .dispatch_event(nsh_core::ROOT_ID, kind, payload);
                    }
                }
            }
            DesktopEvent::GlobalShortcutRegistered { id, registered } => {
                let (kind, payload) = global_shortcut_registered_payload(id, registered);
                self.control
                    .dispatch_event(nsh_core::ROOT_ID, kind, payload);
            }
            DesktopEvent::GlobalShortcutTrigger { id } => {
                if id != 0 {
                    let (kind, payload) = menu_select_payload(id);
                    self.control
                        .dispatch_event(nsh_core::ROOT_ID, kind, payload);
                }
            }
            DesktopEvent::TrayClick { id, bounds } => {
                if id != 0 {
                    let (kind, payload) = tray_click_payload(id, bounds);
                    self.control
                        .dispatch_event(nsh_core::ROOT_ID, kind, payload);
                }
            }
            DesktopEvent::SecondInstance(launch) => {
                self.control.dispatch_event(
                    nsh_core::ROOT_ID,
                    EventKind::SecondInstance,
                    launch.to_payload(),
                );
            }
            DesktopEvent::SubWindowState { window_id, state } => {
                if let Ok(node) = self.node(window_id)
                    && let Some(modal) = node.modal.borrow().clone()
                {
                    modal.on_sub_window_state(state)?;
                }
            }
            DesktopEvent::SubWindowTitleBar { window_id, edges } => {
                if let Ok(node) = self.node(window_id)
                    && let Some(modal) = node.modal.borrow().clone()
                {
                    modal.on_sub_window_title_bar(edges);
                }
            }
            DesktopEvent::SubWindowClosePress { window_id, reason } => {
                if let Ok(node) = self.node(window_id)
                    && let Some(modal) = node.modal.borrow().clone()
                {
                    let parent_id = modal
                        .desktop_parent_window_id()
                        .unwrap_or(nsh_core::ROOT_ID);
                    modal.on_sub_window_close_press(reason)?;
                    if !modal.is_visible() {
                        self.cursor.clear_window(window_id);
                        self.close_desktop_children(window_id)?;
                        self.request_desktop_focus_for(parent_id);
                    }
                }
            }
            DesktopEvent::SubWindowBlur { window_id } => {
                if let Ok(node) = self.node(window_id)
                    && let Some(modal) = node.modal.borrow().clone()
                {
                    let parent_id = modal
                        .desktop_parent_window_id()
                        .unwrap_or(nsh_core::ROOT_ID);
                    modal.on_sub_window_blur()?;
                    if !modal.is_visible() {
                        self.cursor.clear_window(window_id);
                        self.close_desktop_children(window_id)?;
                        self.request_desktop_focus_for(parent_id);
                    }
                }
            }
            DesktopEvent::SubWindowDestroyed { window_id } => {
                self.cursor.clear_window(window_id);
                if let Ok(node) = self.node(window_id)
                    && let Some(modal) = node.modal.borrow().clone()
                {
                    let parent_id = modal
                        .desktop_parent_window_id()
                        .unwrap_or(nsh_core::ROOT_ID);
                    modal.on_sub_window_destroyed()?;
                    self.close_desktop_children(window_id)?;
                    self.request_desktop_focus_for(parent_id);
                }
            }
            DesktopEvent::WindowState(_)
            | DesktopEvent::TitleBar(_)
            | DesktopEvent::Displays(_) => {}
        }
        Ok(())
    }
    fn close_desktop_children(&self, parent_id: NodeId) -> Result<()> {
        let children: Vec<(NodeId, Rc<crate::navigation::modal::Modal>)> = self
            .modals
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter(|m| m.is_desktop_window() && m.is_visible())
            .filter(|m| m.desktop_parent_window_id() == Some(parent_id))
            .filter_map(|m| m.node_id().map(|id| (id, m)))
            .collect();
        for (child_id, child) in children {
            self.close_desktop_children(child_id)?;
            child.close_for_parent()?;
        }
        Ok(())
    }
    fn focused_desktop_modal(&self) -> Option<Rc<crate::navigation::modal::Modal>> {
        self.modals
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|m| m.is_desktop_focused())
    }
    pub(crate) fn desktop_menu_action(
        &mut self,
        id: u32,
        role: Option<nsh_core::desktop::MenuRole>,
    ) -> Result<()> {
        use crate::services::desktop::{WireCommand, menu_select_payload, wire_request};
        use nsh_core::{
            EventKind, EventPayload,
            desktop::{MenuRole, WindowEvent, WindowMode, WindowRequest},
        };
        let selection_target = self.desktop.selection_menu_target.take();
        if let Some(role) = role {
            match role {
                MenuRole::Quit => {
                    if self.desktop.quit_on_request {
                        self.host.desktop_command(&WireCommand::Quit)?;
                    } else {
                        self.control.dispatch_event(
                            nsh_core::ROOT_ID,
                            EventKind::Window,
                            EventPayload::Window(Box::new(WindowEvent::QuitRequest)),
                        );
                    }
                    return Ok(());
                }
                MenuRole::Close => {
                    if let Some(modal) = self.focused_desktop_modal() {
                        modal.on_sub_window_close_press(nsh_core::desktop::CloseReason::Menu)?;
                        if !modal.is_visible()
                            && let Some(window_id) = modal.node_id()
                        {
                            self.close_desktop_children(window_id)?;
                        }
                        return Ok(());
                    }
                    return self.desktop_event(crate::services::desktop::DesktopEvent::ClosePress(
                        nsh_core::desktop::CloseReason::Menu,
                    ));
                }
                MenuRole::Minimize => {
                    if let Some(modal) = self.focused_desktop_modal() {
                        modal.desktop_request(WindowRequest::Minimize)?;
                    } else {
                        self.host
                            .desktop_command(&wire_request(&WindowRequest::Minimize))?;
                    }
                    return Ok(());
                }
                MenuRole::Zoom => {
                    if let Some(modal) = self.focused_desktop_modal() {
                        modal.desktop_toggle_maximize()?;
                    } else {
                        self.host
                            .desktop_command(&WireCommand::ToggleMaximize { window_id: None })?;
                    }
                    return Ok(());
                }
                MenuRole::ToggleFullScreen => {
                    if let Some(modal) = self.focused_desktop_modal() {
                        modal.desktop_toggle_fullscreen()?;
                        return Ok(());
                    }
                    let next = if self
                        .platform
                        .get()
                        .window_state
                        .is_some_and(|s| s.mode == WindowMode::FullScreen)
                    {
                        WindowMode::Normal
                    } else {
                        WindowMode::FullScreen
                    };
                    self.host
                        .desktop_command(&wire_request(&WindowRequest::SetMode(next)))?;
                    return Ok(());
                }
                MenuRole::Copy
                | MenuRole::Cut
                | MenuRole::SelectAll
                | MenuRole::Paste
                | MenuRole::Undo
                | MenuRole::Redo => {
                    if role == MenuRole::SelectAll
                        && let Some(target_id) = selection_target
                        && let Ok(target) = self.node(target_id)
                    {
                        if let Ok(window) = self.node(target.key_window_id())
                            && let Some(sink) = window.focus_sink.borrow().as_ref()
                        {
                            let _ = sink.borrow().request_focus();
                        }
                        target.selection_select_all_target(true)?;
                        return Ok(());
                    }
                    if matches!(role, MenuRole::Copy | MenuRole::SelectAll) {
                        let selected = self
                            .selected_text
                            .borrow()
                            .as_ref()
                            .and_then(std::rc::Weak::upgrade);
                        if let Some(node) = selected
                            && node.has_selection()
                        {
                            let cmd = if role == MenuRole::Copy {
                                crate::text::selection::controller::Command::Copy
                            } else {
                                crate::text::selection::controller::Command::SelectAll
                            };
                            node.selection_command(cmd)?;
                            return Ok(());
                        }
                    }
                    let focused_input = self
                        .dispatcher
                        .upgrade()
                        .and_then(|route| route.keyboard.lock().unwrap().input);
                    if let Some(input_id) = focused_input
                        && let Ok(node) = self.node(input_id)
                        && let Some(input) = node.input.borrow().as_ref()
                    {
                        match role {
                            MenuRole::SelectAll => {
                                input.select_all()?;
                                return Ok(());
                            }
                            MenuRole::Paste => {
                                self.host.system.paste_to_input(
                                    &self.host,
                                    self.dispatcher.clone(),
                                    input_id,
                                    input.report_paste(),
                                )?;
                                return Ok(());
                            }
                            MenuRole::Copy | MenuRole::Cut | MenuRole::Undo | MenuRole::Redo
                                if node.interaction.mask().contains(EventKind::EditAction) =>
                            {
                                let action = match role {
                                    MenuRole::Copy => "copy",
                                    MenuRole::Cut => "cut",
                                    MenuRole::Undo => "undo",
                                    MenuRole::Redo => "redo",
                                    _ => unreachable!(),
                                };
                                node.emit(
                                    EventKind::EditAction,
                                    EventPayload::Map(vec![(
                                        "action",
                                        nsh_core::PayloadValue::Str(action.to_owned()),
                                    )]),
                                );
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if id != 0 {
            let (kind, payload) = menu_select_payload(id);
            self.control
                .dispatch_event(nsh_core::ROOT_ID, kind, payload);
        }
        Ok(())
    }
    pub(crate) fn request_desktop_focus(&self) {
        self.request_desktop_focus_for(nsh_core::ROOT_ID);
    }
    pub(crate) fn request_desktop_focus_for(&self, window_id: NodeId) {
        let mut stale_other_window_focus = false;
        let has_input_focus = self
            .dispatcher
            .upgrade()
            .and_then(|route| route.keyboard.lock().unwrap().focused)
            .is_some_and(|focused_id| {
                if let Ok(node) = self.node(focused_id)
                    && node.visible_in_tree()
                    && node.key_window_id() == window_id
                {
                    true
                } else {
                    stale_other_window_focus = true;
                    false
                }
            });
        let has_child_focus = self
            .key_focus
            .borrow()
            .upgrade()
            .is_some_and(|node| node.visible_in_tree() && node.key_window_id() == window_id);
        if has_input_focus || has_child_focus {
            return;
        }
        if stale_other_window_focus && let Some(route) = self.dispatcher.upgrade() {
            let mut keyboard = route.keyboard.lock().unwrap();
            keyboard.focused = None;
            keyboard.input = None;
            keyboard.composing = false;
        }
        if let Ok(window) = self.node(window_id) {
            if let Some(sink) = window.focus_sink.borrow().as_ref() {
                let _ = sink.borrow().request_focus();
            } else {
                let _ = window.native.borrow().request_focus();
            }
        }
    }
    pub(crate) fn desktop_selection_menu(
        &mut self,
        target_id: NodeId,
        x: f32,
        y: f32,
    ) -> Result<()> {
        use crate::services::desktop::{WireCommand, WireMenuItem, serialize_menu_items};
        use nsh_core::desktop::MenuRole;
        let Ok(target) = self.node(target_id).cloned() else {
            return Ok(());
        };
        if !(target.selectable() || target.interaction.selectable_view()) {
            return Ok(());
        }
        self.desktop.selection_menu_target = Some(target_id);
        let has_selection = self
            .selected_text
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
            .is_some_and(|node| node.has_selection());
        let mut items = Vec::new();
        if has_selection {
            items.push(WireMenuItem {
                kind: "action".into(),
                id: 0,
                label: self.desktop.labels.label(MenuRole::Copy).to_owned(),
                enabled: true,
                checked: None,
                shortcut: Some("Ctrl+C".into()),
                role: Some("copy".into()),
                destructive: false,
                items: Vec::new(),
            });
        }
        items.push(WireMenuItem {
            kind: "action".into(),
            id: 0,
            label: self.desktop.labels.label(MenuRole::SelectAll).to_owned(),
            enabled: true,
            checked: None,
            shortcut: Some("Ctrl+A".into()),
            role: Some("selectAll".into()),
            destructive: false,
            items: Vec::new(),
        });
        let mut custom = target.interaction.selection_menu();
        if custom.is_empty() {
            let mut ancestor = target.parent_node.borrow().upgrade();
            while let Some(node) = ancestor {
                if node.interaction.selectable_view() {
                    let menu = node.interaction.selection_menu();
                    if !menu.is_empty() {
                        custom = menu;
                        break;
                    }
                }
                ancestor = node.parent_node.borrow().upgrade();
            }
        }
        if !custom.is_empty() {
            let extra = serialize_menu_items(&custom, &self.desktop.labels);
            if !extra.is_empty() {
                items.push(WireMenuItem {
                    kind: "separator".into(),
                    id: 0,
                    label: String::new(),
                    enabled: false,
                    checked: None,
                    shortcut: None,
                    role: None,
                    destructive: false,
                    items: Vec::new(),
                });
                items.extend(extra);
            }
        }
        let window_id = target.key_window_id();
        let generation = self.desktop.next_selection_popup_generation();
        self.host.desktop_command(&WireCommand::PopupMenu {
            window_id: (window_id != nsh_core::ROOT_ID).then_some(window_id),
            generation,
            x: Some(x),
            y: Some(y),
            items,
        })?;
        Ok(())
    }
    pub(crate) fn key_scroll(
        &self,
        window: NodeId,
        key_move: nsh_core::scroll::KeyMove,
        horizontal: bool,
    ) -> Result<()> {
        let mut target = self
            .key_focus
            .borrow()
            .upgrade()
            .filter(|n| n.visible_in_tree() && n.key_window_id() == window);
        while let Some(node) = target {
            if let Some(scroll) = node.scroll.borrow().as_ref()
                && scroll.can_key_scroll(horizontal)
            {
                return scroll.key_scroll(key_move, horizontal);
            }
            target = node.parent_node.borrow().upgrade();
        }
        // Fallback: largest visible scroll view on the active window along that axis.
        let best = self
            .nodes
            .values()
            .filter(|node| node.visible_in_tree() && node.key_window_id() == window)
            .filter_map(|node| {
                let scroll = node.scroll.borrow().clone()?;
                scroll.can_key_scroll(horizontal).then(|| {
                    let frame = node.frame.get();
                    (frame.width * frame.height, scroll)
                })
            })
            .max_by(|(a, _), (b, _)| a.total_cmp(b));
        if let Some((_, scroll)) = best {
            scroll.key_scroll(key_move, horizontal)?;
        }
        Ok(())
    }
    pub(crate) fn window_environment(
        &self,
        visual: [crate::app::session::AvoidArea; 3],
        keyboard: crate::app::session::AvoidArea,
    ) -> Result<crate::app::environment::NshEnvironment> {
        let root = self.node(1)?;
        let Some(viewport) = self.mount.parent.upgrade() else {
            return Ok(crate::app::environment::NshEnvironment::default());
        };
        // The external viewport has already been laid out by the host. A newly
        // attached texture wrapper may not have propagated its window origin yet.
        let origin = viewport.borrow().layout_position_in_window()?;
        let frame = root.frame.get();
        let scale = self.scale.get() as f64;
        let viewport = super::geometry::WindowRect {
            x: origin.x as f64,
            y: origin.y as f64,
            width: frame.width as f64 * scale,
            height: frame.height as f64 * scale,
        };
        let rect = |rect: crate::app::session::AvoidRect| super::geometry::WindowRect {
            x: rect.left as f64,
            y: rect.top as f64,
            width: rect.width as f64,
            height: rect.height as f64,
        };
        let [top, right, bottom, left] = super::geometry::visual_insets(
            viewport,
            visual.into_iter().filter(|area| area.visible).map(|area| {
                [
                    rect(area.top_rect),
                    rect(area.right_rect),
                    rect(area.bottom_rect),
                    rect(area.left_rect),
                ]
            }),
        );
        Ok(crate::app::environment::NshEnvironment {
            safe_top: Some(top / scale),
            safe_right: Some(right / scale),
            safe_bottom: Some(bottom / scale),
            safe_left: Some(left / scale),
            keyboard_height: Some(if keyboard.visible {
                viewport
                    .intersection(rect(keyboard.bottom_rect))
                    .map_or(0.0, |area| area.height / scale)
            } else {
                0.0
            }),
            ..crate::app::environment::NshEnvironment::default()
        })
    }
    pub(crate) fn new(
        parent: crate::render::node::NativeNode,
        size: Size,
        context: Rc<UiContext>,
    ) -> Result<Self> {
        let mut mount = MountTarget {
            parent: Rc::downgrade(&parent),
            root: std::rc::Weak::new(),
        };
        let root = Node::new(1, ViewKind::View, context.clone())?;
        root.set_frame(Rect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        })?;
        root.ensure_wrapper()?;
        mount.mount(root.mounted())?;
        *root.mount_parent.borrow_mut() = mount.parent.clone();
        root.set_events(nsh_core::EventMask::default())?;
        context.shaders.root(&root);
        *context.scene.borrow_mut() = Some(Rc::new(crate::render::composition::scene::Scene::new(
            &root,
        )));
        Ok(Self {
            dialogs: crate::services::dialogs::Dialogs::new(context.clone()),
            desktop: Default::default(),
            offscreen_renders: Default::default(),
            surface_size: std::cell::Cell::new(size),
            context,
            mount,
            nodes: BTreeMap::from([(nsh_core::ROOT_ID, root)]),
            stacks: Vec::new(),
            removed: Vec::new(),
            retired: VecDeque::new(),
            epoch: 0,
            batches: 0,
            modals: Vec::new(),
            pending_delete: Vec::new(),
            deferred: Vec::new(),
            uncreated_scroll_content: rustc_hash::FxHashMap::default(),
        })
    }
    fn node(&self, id: NodeId) -> Result<&Rc<Node>> {
        self.nodes.get(&id).ok_or(Error::InvalidNode(id))
    }

    fn detach(&mut self, id: NodeId) -> Result<()> {
        self.unlink(id, false)
    }

    fn unlink(&mut self, id: NodeId, retain: bool) -> Result<()> {
        if self.desktop.hosted_main_window == Some(id) {
            self.desktop.hosted_main_window = None;
        }
        let node = self.node(id)?;
        let parent = node.parent.get().map(|id| self.node(id)).transpose()?;
        let retain = retain && node.modal.borrow().is_none() && parent.is_some();
        if let Some(modal) = node.modal.borrow().as_ref() {
            Self::attribute(modal.close(true));
        } else if !retain && let Some(container) = node.mount_parent.borrow().upgrade() {
            let mounted = node.mounted();
            let index = container
                .borrow()
                .children()
                .iter()
                .rposition(|entry| Rc::ptr_eq(entry, &mounted));
            if let Some(index) = index {
                container
                    .borrow_mut()
                    .remove_child(index)
                    .map_err(|error| Error::Tree(Box::new(error.into())))?;
            }
        }
        if let Some(parent) = parent {
            let index = parent
                .children
                .borrow()
                .iter()
                .rposition(|entry| entry.upgrade().is_some_and(|child| child.id == id))
                .ok_or(Error::InvalidNode(id))?;
            parent.children.borrow_mut().remove(index);
            let parent_dying = !retain && (parent.pending_delete.get() || !parent.active.get());
            if !parent_dying {
                parent.invalidate_composites();
            }
            if retain {
                // Keep a removed-but-live FrameNode reachable for the next export
                // reconciliation. Visibility None removes it from layout and RS.
                parent.retained_children.borrow_mut().retain(|entry| {
                    entry.upgrade().is_some_and(|child| {
                        child.retained.get()
                            && child.retained_owner.borrow().ptr_eq(&Rc::downgrade(parent))
                    })
                });
                parent
                    .retained_children
                    .borrow_mut()
                    .push(Rc::downgrade(node));
                *node.retained_owner.borrow_mut() = Rc::downgrade(parent);
                node.retained.set(true);
                Self::attribute(node.restore_style());
            }
            if !parent_dying {
                if parent.children.borrow().is_empty() && !parent.clips_children() {
                    Self::attribute(parent.update_layout_bounds());
                }
                if let Some(scroll) = parent.scroll.borrow().as_ref() {
                    Self::attribute(scroll.sync_inferred_size());
                }
                parent.accessibility_state.changed(parent);
            }
        }
        if !retain {
            *node.mount_parent.borrow_mut() = std::rc::Weak::new();
            *node.retained_owner.borrow_mut() = std::rc::Weak::new();
            node.culled_by_scroll.set(false);
        }
        let scene_affected = node.scene_active()
            || parent.as_ref().is_some_and(|parent| parent.scene_active())
            || self
                .scene
                .borrow()
                .as_ref()
                .is_some_and(|scene| scene.is_ancestor_of_active_root(node));
        node.parent.set(None);
        *node.parent_node.borrow_mut() = std::rc::Weak::new();
        self.shaders.invalidate_visibility();
        if scene_affected {
            node.invalidate_scene();
        }
        Ok(())
    }

    /// Whether `id` or an ancestor is leaving or a closing modal: its
    /// content keeps what it showed until it is gone. One walk up for both,
    /// none while nothing leaves and no modal is open.
    fn frozen(&self, mut id: NodeId) -> bool {
        let leaving = self.leaving_nodes.get() > 0;
        let modals = self.modals.iter().any(|modal| modal.strong_count() > 0);
        if !leaving && !modals {
            return false;
        }
        loop {
            let Ok(node) = self.node(id) else {
                return false;
            };
            if (leaving && node.is_leaving())
                || (modals
                    && node
                        .modal
                        .borrow()
                        .as_ref()
                        .is_some_and(|modal| modal.closing()))
            {
                return true;
            }
            let Some(parent) = node.parent.get() else {
                return false;
            };
            id = parent;
        }
    }
    fn sync_pending_deletions(&self) {
        self.pending_delete_count.set(self.pending_delete.len());
        self.unreclaimed_delete_count
            .set(self.removed.len() + self.retired.len());
    }
    fn delete(&mut self, id: NodeId) -> Result<()> {
        if let Some(modal) = self.node(id)?.modal.borrow().as_ref() {
            Self::attribute(modal.close(true));
        }
        let frozen = self.frozen(id);
        if frozen {
            let node = self.node(id)?.clone();
            if !node.pending_delete.replace(true) {
                // Both exits still render their retained subtree. Deactivation
                // closes texture wrappers, so it belongs to delete_now after
                // the page/dialog transition has finished.
                self.pending_delete.push(id);
                self.sync_pending_deletions();
            }
            return Ok(());
        }
        self.delete_now(id)
    }
    fn mark_subtree_dying(&self, id: NodeId) {
        let Ok(node) = self.node(id) else { return };
        node.pending_delete.set(true);
        let children: Vec<NodeId> = node
            .children
            .borrow()
            .iter()
            .filter_map(|w| w.upgrade().map(|c| c.id))
            .collect();
        for child in children {
            self.mark_subtree_dying(child);
        }
    }
    fn orphan_subtree(&mut self, id: NodeId) -> Result<()> {
        self.mark_subtree_dying(id);
        self.orphan_subtree_inner(id)
    }
    fn orphan_subtree_inner(&mut self, id: NodeId) -> Result<()> {
        self.detach(id)?;
        // Children can be deleted later in the batch (or when QuickJS collects a
        // reference cycle). Sever every native edge and release native attachments
        // across the orphaned subtree before any endpoint is eligible for disposal.
        loop {
            let child = self
                .node(id)?
                .children
                .borrow()
                .last()
                .and_then(|n| n.upgrade());
            let Some(child) = child else { break };
            self.orphan_subtree_inner(child.id)?;
        }
        if let Ok(node) = self.node(id) {
            self.animator.remove(id);
            self.shaders.remove(id);
            *node.navigation_shader.borrow_mut() = None;
            node.deactivate();
            *node.text_drawing.borrow_mut() = None;
            *node.raster.borrow_mut() = None;
        }
        Ok(())
    }
    fn delete_now(&mut self, id: NodeId) -> Result<()> {
        self.delete_now_inner(id)?;
        self.sync_pending_deletions();
        Ok(())
    }
    fn delete_now_inner(&mut self, id: NodeId) -> Result<()> {
        self.uncreated_scroll_content.remove(&id);
        if self.desktop.hosted_main_window == Some(id) {
            self.desktop.hosted_main_window = None;
        }
        if !self.nodes.contains_key(&id) {
            return Ok(());
        }
        self.orphan_subtree(id)?;
        if let Some(node) = self.nodes.remove(&id) {
            if node.kind == ViewKind::Stack
                && let Ok(index) = self.stacks.binary_search(&id)
            {
                self.stacks.remove(index);
            }
            self.removed.push(node);
        }
        Ok(())
    }

    fn deferred_node(item: &MountItem) -> Option<NodeId> {
        match item {
            MountItem::ViewStyle { id, .. }
            | MountItem::Frame { id, .. }
            | MountItem::Text { id, .. }
            | MountItem::ScreenConfig { id, .. }
            | MountItem::ModalConfig { id, .. }
            | MountItem::Shader { id, .. }
            | MountItem::ShaderTransition { id, .. }
            | MountItem::NavShader { id, .. }
            | MountItem::Remove { id } => Some(*id),
            _ => None,
        }
    }
    fn finish_deferred(&mut self) -> Result<()> {
        let mut ready = Vec::new();
        for item in std::mem::take(&mut self.deferred) {
            let id = Self::deferred_node(&item).expect("deferred view update");
            if let Ok(node) = self.node(id) {
                if node.pending_delete.get() {
                    continue;
                }
                if self.frozen(id) {
                    self.deferred.push(item);
                } else {
                    ready.push(item);
                }
            }
        }
        if !ready.is_empty() {
            self.apply(MountBatch { items: ready })?;
        }
        Ok(())
    }

    fn attribute(result: Result<()>) {
        if let Err(error) = result {
            log::warn!(target: "nsh::mount", "native attribute skipped: {error}");
        }
    }
    pub(crate) fn apply(&mut self, batch: MountBatch) -> Result<()> {
        let batch_guard = BatchGuard::new(self.context.clone());
        let nested = batch_guard.1;
        let mut result = self.apply_batch(batch);
        if result.is_err() {
            // An early failure can follow removals whose capture/host work was
            // deferred by the batch. Reconcile and flush before returning it.
            result = result.and(self.reconcile_navigation_capture());
        }
        self.sync_pending_deletions();
        if nested {
            result
        } else {
            result.and(self.flush_arkui())
        }
    }

    fn apply_batch(&mut self, batch: MountBatch) -> Result<()> {
        self.finish_deferred()?;
        let crate::render::transition::BatchPrepass {
            deletions,
            deleted,
            removals,
            removed,
            mut motion,
        } = crate::render::transition::BatchMotion::scan(&batch.items);
        // Start a screen's exit before its descendants' delete operations. An
        // enclosing tree deletion retires immediately instead of animating orphans.
        for id in &deletions {
            let Ok(node) = self.node(*id) else {
                continue;
            };
            let Some(screen) = node.screen.borrow().as_ref().cloned() else {
                continue;
            };
            let mut ancestor = node.parent_node.borrow().upgrade();
            let mut enclosing_delete = false;
            while let Some(parent) = ancestor {
                if deleted.contains(&parent.id) {
                    enclosing_delete = true;
                    break;
                }
                ancestor = parent.parent_node.borrow().upgrade();
            }
            let animated = !enclosing_delete
                && screen
                    .previous()
                    .is_some_and(|previous| !deleted.contains(&previous.id));
            Self::attribute(screen.prepare_removal(animated));
            if animated {
                Self::attribute(screen.remove().map(|_| ()));
            }
        }
        for id in &deletions {
            if let Ok(node) = self.node(*id)
                && let Some(modal) = node.modal.borrow().as_ref()
            {
                Self::attribute(modal.close(true));
            }
        }
        for (id, animation) in removals {
            if !animation.is_some_and(|animation| animation.delete.is_some()) {
                continue;
            }
            let Ok(node) = self.node(id) else { continue };
            let mut parent = node.parent_node.borrow().upgrade();
            let mut enclosed = false;
            while let Some(ancestor) = parent {
                if removed.contains(&ancestor.id) {
                    enclosed = true;
                    break;
                }
                parent = ancestor.parent_node.borrow().upgrade();
            }
            if !enclosed {
                Self::attribute(node.removing(animation.as_ref()).map(|_| ()));
            }
        }
        // Each frame uses the most recent marker. Batch-local state also keeps
        // an unclosed scope from leaking into a later commit or deferred replay.
        let mut nav_shader_configured = false;
        let mut batch_created_ids: Vec<NodeId> = Vec::new();
        self.batches += 1;
        let items = batch.items.len();
        for (at, item) in batch.items.into_iter().enumerate() {
            if Self::deferred_node(&item)
                .is_some_and(|id| motion.allows_defer(id) && self.frozen(id))
            {
                self.deferred.push(item);
                continue;
            }

            let creating_root = matches!(
                item,
                MountItem::Create {
                    id: nsh_core::ROOT_ID,
                    ..
                }
            );
            let result = (|| -> Result<()> {
                match item {
                    MountItem::Component { id, name } => {
                        if let Some(component) = self.components.make(
                            self.env,
                            &name,
                            id,
                            self.control.clone(),
                            &self.host,
                        )? {
                            let node = self.node(id)?;
                            node.component_animated.borrow_mut().clear();
                            node.attach_component(component)?;
                            if node.scene_active() {
                                node.invalidate_scene();
                            }
                            self.animator.mark_dirty();
                        }
                    }
                    MountItem::ComponentPropSlots { id, names } => {
                        let node = self.node(id)?;
                        *node.component_slots.borrow_mut() = names;
                        node.component_animated.borrow_mut().clear();
                        self.animator.mark_dirty();
                    }
                    MountItem::Canvas { id, drawing } => {
                        let node = self.node(id)?;
                        if let Some(canvas) = node.canvas.borrow().as_ref() {
                            canvas.set(drawing)?;
                            if node.scene_active() {
                                node.invalidate_scene();
                            }
                        }
                    }
                    MountItem::ScreenConfig { id, config } => {
                        if !self.frozen(id)
                            && let Some(screen) = self.node(id)?.screen.borrow().as_ref()
                        {
                            screen.configure(*config)?;
                        }
                    }
                    MountItem::NativeProps { id, props } => match props {
                        nsh_core::NativeProps::WebView(props) => {
                            if let Some(web) = self.node(id)?.web.borrow().as_ref() {
                                web.configure(*props)?;
                            }
                        }
                        nsh_core::NativeProps::Video(props) => {
                            if let Some(video) = self.node(id)?.video.borrow().as_ref() {
                                video.configure(props)?;
                            }
                        }
                    },
                    MountItem::ComponentProps { id, props } => {
                        if let Some(component) = self.node(id)?.component.borrow().as_ref() {
                            component.props(props)?;
                        }
                    }
                    MountItem::Create { id, kind } => {
                        if self.nodes.contains_key(&id) {
                            return Err(Error::Tree(Box::new(Error::InvalidNode(id))));
                        }
                        let created = Node::new(id, kind, self.context.clone())
                            .map_err(|error| Error::Tree(Box::new(error)))?;
                        if id != nsh_core::ROOT_ID
                            && !matches!(kind, ViewKind::Screen | ViewKind::Modal)
                        {
                            created.batch_created.set(true);
                            batch_created_ids.push(id);
                        }
                        if let Some(size) = self.uncreated_scroll_content.remove(&id)
                            && let Some(scroll) = created.scroll.borrow().as_ref()
                        {
                            scroll.size(size)?;
                        }
                        self.nodes.insert(id, created);
                        motion.note_created(id);
                        if kind == ViewKind::Stack {
                            let index = self.stacks.partition_point(|stack| *stack < id);
                            self.stacks.insert(index, id);
                        }
                        let modal = self.node(id)?.modal.borrow().as_ref().map(Rc::downgrade);
                        if let Some(modal) = modal {
                            self.modals.push(modal);
                        }
                    }
                    MountItem::Insert {
                        parent,
                        child,
                        before,
                    } => {
                        if self.node(child)?.parent.get().is_some() {
                            self.detach(child).map_err(|error| match error {
                                Error::InvalidNode(_) => Error::Tree(Box::new(error)),
                                error => error,
                            })?;
                        }
                        let parent = self
                            .node(parent)
                            .map_err(|error| Error::Tree(Box::new(error)))?;
                        parent.enable_layout();
                        let node = self.node(child)?;
                        // A sibling not in the parent yet (a host retagged in this
                        // batch, inserted by its own later item) places the child
                        // last, as GTK's mounter does, rather than fail the batch.
                        let index = {
                            let children = parent.children.borrow();
                            before
                                .and_then(|before| {
                                    children
                                        .iter()
                                        .position(|n| n.upgrade().is_some_and(|n| n.id == before))
                                })
                                .unwrap_or(children.len())
                        };
                        if let Some(modal) = node.modal.borrow().as_ref() {
                            if !modal.is_desktop_window() && !modal.is_main_window_host() {
                                modal.schedule_present();
                            }
                        } else {
                            if !node.batch_created.get() {
                                node.flush_deferred_native()
                                    .map_err(|error| Error::Tree(Box::new(error)))?;
                                node.ensure_initial_layout_bounds()
                                    .map_err(|error| Error::Tree(Box::new(error)))?;
                            }
                            if parent.scroll.borrow().is_none() {
                                parent
                                    .promote_direct_paint()
                                    .map_err(|error| Error::Tree(Box::new(error)))?;
                            }
                            let container = parent.child_container();
                            let native_index = parent.child_index(index);
                            container
                                .borrow_mut()
                                .insert_child(node.mounted(), native_index)
                                .map_err(|error| Error::Tree(Box::new(error.into())))?;
                            // ArkUI insert/remove request measurement themselves.
                            // NeedMeasure adds SELF_AND_PARENT and would escape
                            // the engine-owned parent's measure boundary.
                            *node.mount_parent.borrow_mut() = Rc::downgrade(&container);
                        }
                        let was_empty = parent.children.borrow().is_empty();
                        parent
                            .children
                            .borrow_mut()
                            .insert(index, Rc::downgrade(node));
                        if was_empty && !parent.clips_children() {
                            Self::attribute(parent.update_layout_bounds());
                        }
                        if let Some(scroll) = parent.scroll.borrow().as_ref() {
                            Self::attribute(scroll.sync_inferred_size());
                        }
                        parent.accessibility_state.changed(parent);
                        node.parent.set(Some(parent.id));
                        *node.parent_node.borrow_mut() = Rc::downgrade(parent);
                        Self::attribute(node.sync_culled_subtree(parent.in_culled_subtree.get()));
                        node.invalidate_composites();
                        if parent.scene_active()
                            || node.scene_active()
                            || self
                                .scene
                                .borrow()
                                .as_ref()
                                .is_some_and(|scene| scene.has_inactive_requirements())
                        {
                            parent.invalidate_scene();
                        }
                        node.accessibility_state.changed(node);
                        Self::attribute(
                            node.interaction
                                .pointer(node, parent.interaction.blocks_children()),
                        );
                        if !node.children.borrow().is_empty() && parent.has_selectable_ancestor() {
                            Self::attribute(node.selection_configure());
                        }
                        if node.kind == ViewKind::HeaderItem
                            && let Some(screen) = parent.screen.borrow().as_ref()
                        {
                            Self::attribute(screen.update(screen.has_back()));
                        }
                        Self::attribute(node.inserted(motion.insert_animation()));
                        // This Insert supersedes a removal deferred while the node
                        // was leaving. Replay its other updates before newer ones.
                        self.deferred.retain(
                            |item| !matches!(item, MountItem::Remove { id } if *id == child),
                        );
                        self.finish_deferred()?;
                    }
                    MountItem::Remove { id } => {
                        if motion.is_created_in_batch(id) {
                            let node = self.node(id)?;
                            if node.live.borrow_mut().take().is_some() {
                                self.animator.remove(id);
                                Self::attribute(node.finish_removal());
                            }
                            self.unlink(id, true)?;
                        } else if !self.frozen(id)
                            && !self
                                .node(id)?
                                .removing(motion.remove_animation(id))
                                .unwrap_or_else(|error| {
                                    Self::attribute(Err(error));
                                    false
                                })
                        {
                            self.unlink(id, true)?;
                        }
                    }
                    MountItem::Delete { id } => {
                        self.uncreated_scroll_content.remove(&id);
                        if motion.note_deleted(id) {
                            if let Some(modal) = self.node(id)?.modal.borrow().as_ref() {
                                Self::attribute(modal.close(true));
                            }
                            self.delete_now(id)?;
                        } else {
                            if !self.frozen(id) {
                                Self::attribute(
                                    self.node(id)?
                                        .removing(motion.remove_animation(id))
                                        .map(|_| ()),
                                );
                            }
                            self.delete(id)?;
                        }
                    }
                    MountItem::Frame { id, frame } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            let mut rect: Rect = frame.into();
                            if id == nsh_core::ROOT_ID {
                                rect.y += self.desktop.menu_bar_inset.get();
                            }
                            self.node(id)?
                                .animated_frame(rect, motion.frame_animation(id))?;
                        }
                    }
                    MountItem::LayoutAnimation(animation) => motion.set_layout(animation),
                    MountItem::ViewStyle { id, style } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.node(id)?.style(&style)?;
                        }
                    }
                    MountItem::Text {
                        id,
                        layout,
                        content,
                        ..
                    } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            let node = self.node(id)?;
                            let raster = layout
                                .0
                                .downcast_ref::<NativeText>()
                                .ok_or(Error::Unsupported("foreign text layout"))?;
                            let shown: Arc<str> = Arc::from(raster.source.visible_text());
                            node.accessibility_state.text(node, &shown);
                            node.text(raster, content.into(), layout.clone())?;
                            node.interaction.text(node, &shown)?;
                            if node.scene_active() {
                                node.invalidate_scene();
                            }
                        }
                    }
                    MountItem::Gestures { id, values } => self
                        .node(id)?
                        .interaction
                        .gestures(self.node(id)?, values)?,
                    MountItem::Animation(op) => self.animator.apply(op),
                    MountItem::AnimationBindings { id, bindings } => {
                        self.animator.bind(self.node(id)?, bindings)?
                    }
                    MountItem::Events { id, events } => self.node(id)?.set_events(events)?,
                    MountItem::Keymap(map) => {
                        if let Some(dispatcher) = self.dispatcher.upgrade() {
                            dispatcher.keyboard.lock().unwrap().map = map;
                        }
                    }
                    MountItem::ViewProps { id, props } => {
                        self.node(id)?.interaction.props(self.node(id)?, *props)?
                    }
                    MountItem::AccessibilityLabel { id, label } => {
                        self.node(id)?.interaction.label(self.node(id)?, label)?
                    }
                    // A host retagged in this batch (a component whose root is an
                    // input) can have its tracking ahead of its new `Create`, which
                    // sends it again: skip a view not made yet, as UIKit's and GTK's
                    // mounters do, rather than fail the batch.
                    MountItem::TrackStates { id, states } => {
                        if let Ok(node) = self.node(id) {
                            node.interaction.track(node, states)?;
                        }
                    }
                    MountItem::ModalConfig { id, config } => {
                        if !self.frozen(id) {
                            let node = self.node(id)?;
                            if let Some(modal) = node.modal.borrow().as_ref() {
                                modal.configure(config)?;
                                if node.scene_active() {
                                    node.invalidate_scene();
                                }
                            }
                        }
                    }
                    MountItem::Image {
                        id,
                        source,
                        resize_mode,
                        options,
                    } => {
                        let node = self.node(id)?;
                        if let Some(image) = node.image.borrow().as_ref() {
                            image.configure(source, resize_mode, options)?;
                            if node.scene_active() {
                                node.invalidate_scene();
                            }
                        }
                    }
                    MountItem::Input { id, config } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.configure(*config)?;
                        }
                    }
                    MountItem::InputValue { id, text, count } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.value(text, count)?;
                        }
                    }
                    MountItem::InputContent { id, content } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.content(content.into())?;
                        }
                    }
                    MountItem::Shader { id, layer } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.shaders.set(self.node(id)?, layer)?;
                        }
                    }
                    MountItem::ShaderTransition { id, duration } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.shaders.transition(id, duration);
                        }
                    }
                    // Core content changes invalidate enclosing shader pictures.
                    MountItem::ShaderInput { id } => {
                        let node = self.node(id)?;
                        node.invalidate_composites();
                        if node.scene_active() {
                            node.invalidate_scene();
                        }
                    }
                    MountItem::NavShader { id, shader } => {
                        if !self.frozen(id) {
                            let configured = shader.is_some();
                            let precompile_layer = shader.as_ref().map(|spec| spec.layer.clone());
                            let node = self.node(id)?;
                            *node.navigation_shader.borrow_mut() = shader;
                            if !configured {
                                self.shaders.end_navigation(id)?;
                            } else {
                                nav_shader_configured = true;
                            }
                            node.ensure_wrapper()?;
                            node.restore_style()?;
                            if let Some(layer) = precompile_layer
                                && let Ok(worker) =
                                    crate::render::composition::worker::Worker::for_owner(node)
                            {
                                worker.precompile(layer);
                            }
                        }
                    }
                    // These projections are opt-in for the terminal backend.
                    MountItem::TreeOrder(_) | MountItem::ProjectStates { .. } => {}
                    MountItem::ScrollConfig {
                        id,
                        horizontal,
                        scrollbar_visible,
                        replaced,
                        scroll_value,
                        pull_value,
                        keyboard_dismiss,
                        mirrored,
                    } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.config(
                                horizontal,
                                scrollbar_visible && !replaced,
                                scroll_value,
                                pull_value,
                                keyboard_dismiss,
                                mirrored,
                            )?;
                        }
                    }
                    MountItem::ScrollContent { id, size } => {
                        if let Ok(node) = self.node(id) {
                            if let Some(scroll) = node.scroll.borrow().as_ref() {
                                scroll.size(size.into())?;
                            }
                        } else {
                            // Culled nested Scroll views emit ScrollContent during
                            // layout_roots before sync_culled emits their Create.
                            self.uncreated_scroll_content.insert(id, size.into());
                        }
                    }
                    MountItem::StickyCells { id, cells } => {
                        // A cell without a view yet is not pinned, as on UIKit and GTK.
                        let cells = cells
                            .into_iter()
                            .filter_map(|cell| {
                                let node = self.node(cell.cell).ok()?;
                                Some((cell, Rc::downgrade(node)))
                            })
                            .collect::<Vec<_>>();
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.sticky(cells)?;
                        }
                    }
                    MountItem::Refreshing { id, refreshing } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.refreshing(refreshing)?;
                        }
                    }
                    MountItem::ScrollBehavior { id, behavior } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.behavior(&behavior)?;
                        }
                    }
                    MountItem::ScrollBy { id, dx, dy } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.by(dx.0, dy.0)?;
                        }
                    }
                    MountItem::Command { id, command } => match command {
                        Command::StatusBar(config) => self.host.status_bar(
                            self.dispatcher.clone(),
                            config,
                            self.appearance
                                .get()
                                .resolve(self.platform.get().color_scheme)
                                == nsh_engine::ColorScheme::Dark,
                        )?,
                        Command::Desktop(desktop_cmd) => {
                            self.apply_desktop_command(id, *desktop_cmd)?;
                        }
                        Command::Tokens(tokens) => {
                            self.theme.borrow_mut().tokens = Some(tokens);
                            for node in self.nodes.values() {
                                if let Some(screen) = node.screen.borrow().as_ref() {
                                    screen.update_environment()?;
                                } else if node.modal.borrow().is_some() {
                                    node.restore_style()?;
                                }
                            }
                        }
                        Command::Announce(text) => self.host.accessibility_event(
                            self.dispatcher.clone(),
                            Some(text),
                            None,
                        )?,
                        Command::AccessibilityFocus => {
                            self.accessibility_focus
                                .borrow_mut()
                                .push(Rc::downgrade(self.node(id)?));
                        }
                        Command::Dialog(request) => {
                            let id = request.id;
                            if let Err(error) = self.dialogs.show(*request) {
                                self.control.dispatch_event(
                                    nsh_core::ROOT_ID,
                                    nsh_core::EventKind::Dialog,
                                    nsh_core::EventPayload::Map(vec![
                                        ("id", nsh_core::PayloadValue::Number(id as f64)),
                                        ("index", nsh_core::PayloadValue::Number(-1.0)),
                                    ]),
                                );
                                log::error!("OHOS dialog: {error}");
                            }
                        }
                        Command::CloseDialog(id) => self.dialogs.close(id)?,
                        Command::RenderCanvas {
                            id,
                            drawing,
                            width,
                            height,
                            scale,
                            format,
                        } => {
                            if let Err(failure) = self.offscreen_renders.start(
                                id,
                                drawing,
                                width,
                                height,
                                scale,
                                format,
                                self.context.platform.get().rtl,
                                &self.context.images,
                                &self.context.fonts,
                                &self.context.dispatcher,
                            ) {
                                self.control.dispatch_event(
                                    nsh_core::ROOT_ID,
                                    nsh_core::EventKind::CanvasImage,
                                    nsh_core::EventPayload::CanvasImage {
                                        id,
                                        result: Err(failure),
                                    },
                                );
                            }
                        }
                        Command::WebViewRequest(request) => {
                            if let Ok(node) = self.node(id)
                                && let Some(web) = node.web.borrow().as_ref()
                            {
                                web.command(Command::WebViewRequest(request))?;
                            } else {
                                self.control.dispatch_event(
                                    id,
                                    nsh_core::EventKind::WebViewReply,
                                    nsh_core::webview::reply(
                                        request.id,
                                        Err("WebView is not mounted".into()),
                                    ),
                                );
                            }
                        }
                        // Mode controls the desktop FPS overlay. Collection is
                        // independent on mobile. Orientations are iOS/Android-only.
                        Command::PerfMonitorMode(_) | Command::Orientations(_) => {}
                        Command::Back => self.back_fallback()?,
                        Command::ExitApp => {
                            log::warn!("OHOS ExitApp: the embedding host owns Ability lifetime")
                        }
                        Command::ImageInfo { id, uri } => {
                            self.images
                                .load(crate::media::image::Target::Info(id), uri, None);
                        }
                        Command::OpenUrl { id, url } => {
                            if self
                                .host
                                .open_url(self.dispatcher.clone(), id, url)
                                .is_err()
                            {
                                self.control.dispatch_event(
                                    nsh_core::ROOT_ID,
                                    nsh_core::EventKind::OpenUrl,
                                    nsh_core::EventPayload::Map(vec![
                                        ("id", nsh_core::PayloadValue::Number(id as f64)),
                                        ("opened", nsh_core::PayloadValue::Bool(false)),
                                    ]),
                                );
                            }
                        }
                        command @ (Command::AuthSession { .. }
                        | Command::CloseAuthSession(_)
                        | Command::Share { .. }
                        | Command::SetClipboardItem(_)
                        | Command::SetClipboardImage(_)
                        | Command::ReadClipboardImage { .. }
                        | Command::ReadClipboardFiles { .. }
                        | Command::SetClipboard(_)
                        | Command::ReadClipboard { .. }
                        | Command::Speech(_)
                        | Command::Notification(_)) => {
                            self.host.system.command(
                                &self.host,
                                self.dispatcher.clone(),
                                command,
                            )?;
                        }
                        Command::PickImages { id, request } => {
                            if let Err(error) = self.host.pick(
                                self.dispatcher.clone(),
                                id,
                                nsh_core::picker::Pick::Images(request),
                            ) {
                                log::warn!("OHOS pickImages failed: {error}");
                            }
                        }
                        Command::PickFiles { id, request } => {
                            if let Err(error) = self.host.pick(
                                self.dispatcher.clone(),
                                id,
                                nsh_core::picker::Pick::Files(request),
                            ) {
                                log::warn!("OHOS pickFiles failed: {error}");
                            }
                        }
                        Command::Native { name, args } => {
                            if let Some(component) = self.node(id)?.component.borrow().as_ref() {
                                component.command(&name, &args)?;
                            }
                        }
                        Command::Blur if id == nsh_core::ROOT_ID => {
                            for node in self.nodes.values() {
                                if let Some(input) = node.input.borrow().as_ref() {
                                    input.focus(false)?;
                                }
                            }
                        }
                        Command::Focus | Command::Blur => {
                            if let Some(web) = self.node(id)?.web.borrow().as_ref() {
                                web.command(command)?;
                            } else if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.focus(matches!(command, Command::Focus))?;
                            } else {
                                let node = self.node(id)?.clone();
                                node.interaction
                                    .command_focus(&node, matches!(command, Command::Focus))?;
                            }
                        }
                        Command::SetSelection { start, end } => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.selection(start, end)?;
                            }
                        }
                        Command::InsertText(text) => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.insert_text(text)?;
                            }
                        }
                        Command::OpenEditMenu { bounds, actions } => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.open_edit_menu(bounds, actions)?;
                            }
                        }
                        Command::CloseEditMenu => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.close_edit_menu()?;
                            }
                        }
                        Command::SetJsResponder { block } => {
                            let mut node = Some(self.node(id)?.clone());
                            while let Some(current) = node {
                                if let Some(scroll) = current.scroll.borrow().as_ref() {
                                    scroll.block(block)?;
                                }
                                node = current.parent_node.borrow().upgrade();
                            }
                        }
                        Command::ClearJsResponder => {
                            for node in self.nodes.values() {
                                if let Some(scroll) = node.scroll.borrow().as_ref() {
                                    scroll.block(false)?;
                                }
                            }
                        }
                        Command::ZoomTo {
                            scale,
                            center,
                            animated,
                        } => {
                            if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                                scroll.zoom_to(scale, center.map(|p| (p.x.0, p.y.0)), animated)?;
                            }
                        }
                        Command::ScrollTo { x, y, animated } => {
                            if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                                scroll.command_to(x.0, y.0, animated)?;
                            } else if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.scroll_to(x.0, y.0, animated);
                            }
                        }
                        command @ Command::Seek(_) => {
                            if let Some(video) = self.node(id)?.video.borrow().as_ref() {
                                video.command(command)?;
                            }
                        }
                        command @ (Command::Reload
                        | Command::GoBack
                        | Command::GoForward
                        | Command::StopLoading
                        | Command::InjectJavaScript(_)
                        | Command::PostMessage(_)
                        | Command::Load(_)
                        | Command::SendInput(_)
                        | Command::Evaluate { .. }
                        | Command::CancelEvaluate { .. }) => {
                            let node = self.node(id)?;
                            if let Some(web) = node.web.borrow().as_ref() {
                                web.command(command)?;
                            } else if let Command::Evaluate { request, .. } = command {
                                crate::media::webview::abort_evaluation(&self.control, id, request);
                            }
                        }
                        Command::LayoutAnimation(_)
                        | Command::SetAppUrl(_)
                        | Command::PushAppUrl(_)
                        | Command::CancelPress
                        | Command::ClearSelection => {}
                        Command::Haptic(haptic) => {
                            self.host.haptic(haptic)?;
                        }
                        Command::PerfMonitor(enabled) => {
                            self.control
                                .set_perf_monitor(enabled.then(Default::default));
                        }
                        Command::ScrollToIndex { .. }
                        | Command::ScrollToEnd { .. }
                        | Command::Reveal(_)
                        | Command::MeasureScrollPosition { .. }
                        | Command::SetStatusBar(_)
                        | Command::Measure { .. }
                        | Command::HostCall { .. }
                        | Command::HostPost { .. } => {
                            return Err(Error::Unsupported("unresolved core command"));
                        }
                        Command::PickScreenColor { id } => self.control.dispatch_event(
                            nsh_core::ROOT_ID,
                            nsh_core::EventKind::ScreenColor,
                            nsh_core::EventPayload::Map(vec![(
                                "id",
                                nsh_core::PayloadValue::Number(f64::from(id)),
                            )]),
                        ),
                        Command::Audio(nsh_core::audio::AudioCommand::Load {
                            id,
                            bytes,
                            hint,
                            max_duration,
                        }) => {
                            // Sound.load waits for Loaded before Play or Unload, so
                            // staging on a worker preserves the command order.
                            let host = self.host.clone();
                            let route = self.dispatcher.clone();
                            std::thread::Builder::new()
                                .name("nsh-audio-load".into())
                                .spawn(move || {
                                    if let Err(error) = host.system.load_sound(
                                        &host,
                                        route,
                                        id,
                                        bytes,
                                        hint,
                                        max_duration,
                                    ) {
                                        log::warn!("OHOS sound load failed: {error}");
                                    }
                                })
                                .map_err(|error| {
                                    napi_ohos::Error::from_reason(error.to_string())
                                })?;
                        }
                        Command::Audio(audio) => {
                            self.host.system.audio(
                                &self.host,
                                self.dispatcher.clone(),
                                &self.assets,
                                audio,
                            )?;
                        }
                        Command::FileRequest { id, request } => {
                            self.host.system.file_request(
                                &self.host,
                                self.dispatcher.clone(),
                                id,
                                *request,
                            )?;
                        }
                    },
                    // Appearance and direction also reach native controls and SDK subtrees.
                    MountItem::Appearance { appearance } => {
                        self.appearance.set(appearance);
                        self.system_appearance()?;
                        if let Some(scene) = self.scene.borrow().as_ref()
                            && scene.is_active()
                        {
                            scene.invalidate();
                        }
                    }
                    MountItem::LayoutDirection { rtl } => {
                        use ohos_arkui_binding::types::{
                            attribute::ArkUINodeAttributeType as A, direction::Direction,
                        };
                        self.node(nsh_core::ROOT_ID)?
                            .native
                            .borrow()
                            .set_i32_attribute(
                                A::Direction,
                                if rtl { Direction::Rtl } else { Direction::Ltr }.into(),
                            )?;
                        if let Some(scene) = self.scene.borrow().as_ref()
                            && scene.is_active()
                        {
                            scene.invalidate();
                        }
                    }
                    MountItem::Overflow { id, extent } => {
                        let node = self.node(id)?;
                        node.overflow.set(extent.map(Rect::from));
                        node.invalidate_composites();
                        node.apply_clip()?;
                        if node.scene_active() {
                            node.invalidate_scene();
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                if creating_root {
                    return Err(error);
                }
                // Attribute and resource failures affect this item only. Later
                // items (including updates after a stale node) still apply.
                log::warn!(
                    target: "nsh::mount",
                    "skipped mount item: batch={} item={at}/{items}: {error}",
                    self.batches
                );
            }
        }
        for &id in &batch_created_ids {
            if let Ok(node) = self.node(id) {
                node.batch_created.set(false);
            }
        }
        Self::attribute(self.reconcile_navigation());
        for node in std::mem::take(&mut *self.deferred_webs.borrow_mut())
            .into_iter()
            .filter_map(|weak| weak.upgrade())
        {
            if let Some(web) = node.web.borrow().as_ref() {
                Self::attribute(web.flush_mount_gate(&node));
            }
        }
        if nav_shader_configured {
            self.shaders.log_idle_configured();
        }
        Self::attribute(self.animator.flush());
        for node in self.nodes.values() {
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                Self::attribute(scroll.update_child_culling());
            }
        }
        for &id in &batch_created_ids {
            if let Ok(node) = self.node(id)
                && !node.pending_delete.get()
                && node.mount_parent.borrow().strong_count() > 0
            {
                Self::attribute(node.flush_deferred_native());
            }
        }
        Self::attribute(self.shaders.frame().map(|_| ()));
        let selected = self
            .selected_text
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade);
        if let Some(owner) = selected {
            Self::attribute(owner.selection_refresh());
        }
        Ok(())
    }

    pub(crate) fn image_ready(&mut self, ready: crate::media::image::Ready) -> Result<()> {
        match ready.target {
            crate::media::image::Target::Canvas {
                id,
                generation,
                index,
            } => {
                if let Ok(node) = self.node(id)
                    && let Some(canvas) = node.canvas.borrow().as_ref()
                {
                    canvas.ready(generation, index, ready.data)?;
                }
            }
            crate::media::image::Target::OffscreenCanvas { id, index } => {
                self.offscreen_renders.image_ready(
                    id,
                    index,
                    ready.data,
                    &self.context.fonts,
                    &self.context.dispatcher,
                );
            }
            crate::media::image::Target::Info(id) => {
                crate::media::image::info(&self.control, id, ready.data)
            }
            crate::media::image::Target::View {
                id,
                generation,
                placeholder,
            } => {
                if let Ok(node) = self.node(id)
                    && let Some(image) = node.image.borrow().as_ref()
                {
                    image.ready(generation, placeholder, ready.data)?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn image_progress(&self, progress: crate::media::image::Progress) {
        if let crate::media::image::Target::View {
            id,
            generation,
            placeholder: false,
        } = progress.target
            && let Ok(node) = self.node(id)
            && let Some(image) = node.image.borrow().as_ref()
        {
            image.progress(generation, progress.loaded, progress.total);
        }
    }
    pub(crate) fn flush_scroll_reports(&self) -> Result<()> {
        let pending = std::mem::take(&mut *self.scroll_reports.borrow_mut());
        for node in pending.into_iter().filter_map(|node| node.upgrade()) {
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                scroll.flush_report()?;
            }
        }
        // A shader view scrolled into or out of sight starts or stops.
        self.shaders.reveal()
    }
    pub(crate) fn frame(&mut self, timestamp: Option<u64>) -> Result<UiDemand> {
        let mut demand = self.animator.tick(timestamp)?;
        let mut settled_leaving = false;
        for id in self.animator.take_removals() {
            if let Ok(node) = self.node(id) {
                settled_leaving = true;
                if node.pending_delete.get() {
                    node.set_leaving(false);
                } else {
                    node.finish_removal()?;
                    self.unlink(id, true)?;
                }
            }
        }
        demand = demand.merge(UiDemand::from_animation(self.shaders.frame()?, None));
        self.modals.retain(|modal| modal.strong_count() > 0);
        let closing_modals_before = self
            .modals
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter(|modal| modal.closing())
            .count();
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            demand = demand.merge(UiDemand::from_animation(
                modal.frame(self.animator.frame_time())?,
                None,
            ));
        }
        let closing_modals_after = self
            .modals
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter(|modal| modal.closing())
            .count();
        let modal_settled = closing_modals_after < closing_modals_before;
        let can_unfreeze = settled_leaving
            || modal_settled
            || (self.leaving_nodes.get() == 0 && closing_modals_after == 0);
        let mut pending = std::mem::take(&mut self.pending_delete);
        let mut reconcile_capture = settled_leaving;
        let mut result = Ok(());
        if !pending.is_empty() && can_unfreeze {
            let _batch_guard = BatchGuard::new(self.context.clone());
            // Detaching in reverse mount order removes trailing native siblings
            // in O(1) instead of shifting the prefix on each child removal.
            let mut still_frozen = Vec::new();
            while let Some(id) = pending.pop() {
                if self.frozen(id) {
                    still_frozen.push(id);
                } else {
                    // Even a failed detach may already have removed effects.
                    reconcile_capture = true;
                    if let Err(error) = self.delete_now_inner(id) {
                        pending.push(id);
                        result = Err(error);
                        break;
                    }
                }
            }
            still_frozen.reverse();
            pending.extend(still_frozen);
        }
        self.pending_delete = pending;
        if reconcile_capture {
            result = result.and(self.reconcile_navigation_capture());
        }
        self.sync_pending_deletions();
        result = result.and(self.finish_deferred());
        result.and(self.flush_arkui()).map(|()| demand)
    }

    pub(crate) fn menu_bar_inset(&self) -> f32 {
        self.desktop.menu_bar_inset.get()
    }

    pub(crate) fn resize(&self, size: Size) -> Result<()> {
        self.surface_size.set(size);
        self.apply_surface_size(size)
    }

    fn apply_surface_size(&self, size: Size) -> Result<()> {
        let inset = self.desktop.menu_bar_inset.get();
        let content_size = Size {
            width: size.width,
            height: (size.height - inset).max(0.0),
        };
        let mut platform = self.platform.get();
        platform.window = content_size.into();
        self.platform.set(platform);
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            modal.refresh_metrics();
            if let Some(id) = modal.node_id() {
                self.node(id)?
                    .mounted()
                    .borrow()
                    .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            }
        }
        if let Some((host, _)) = &self.desktop.menu_bar_host {
            let borrowed = host.borrow();
            borrowed.width(size.width.max(0.0))?;
            borrowed.height(inset)?;
        }
        self.node(1)?.set_frame(Rect {
            x: 0.0,
            y: inset,
            width: content_size.width,
            height: content_size.height,
        })?;
        if let Some(owner) = self.fullscreen_video.borrow().upgrade()
            && let Some(video) = owner.video.borrow().as_ref()
        {
            video.fullscreen_frame(size)?;
        }
        self.control.set_environment(platform);
        self.control.resize(content_size.into());
        Ok(())
    }

    pub(crate) fn input_paste(
        &self,
        id: NodeId,
        text: String,
        html: Option<String>,
        files: Option<String>,
        image: Option<String>,
        report_only: bool,
    ) -> Result<()> {
        if let Some(node) = self.nodes.get(&id)
            && let Some(input) = node.input.borrow().as_ref()
        {
            input.complete_paste(text, html, files, image, report_only)?;
        }
        Ok(())
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.scroll_watchers.clear_nodes();
        self.dialogs.clear();
        self.offscreen_renders.close();
        if self.desktop.popup_open {
            self.desktop.popup_open = false;
            let _ = self
                .host
                .desktop_command(&crate::services::desktop::WireCommand::ClosePopupMenu);
        }
        if self.desktop.menu.take().is_some()
            || self.desktop.menu_bar_inset.get() > 0.0
            || self.desktop.menu_bar_host.is_some()
        {
            self.desktop.menu_bar_inset.set(0.0);
            let _ = self
                .host
                .desktop_command(&crate::services::desktop::WireCommand::SetMenuBar {
                    visible: false,
                    target_id: None,
                    open_index: None,
                    items: Vec::new(),
                });
            if let Some((host, attachment)) = self.desktop.menu_bar_host.take() {
                drop(attachment);
                if let Some(parent) = self.mount.parent.upgrade() {
                    let index = parent
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &host));
                    if let Some(index) = index {
                        let _ = parent.borrow_mut().remove_child(index);
                    }
                }
            }
        }
        let mut result = self.mount.unmount();
        if result.is_ok()
            && let Ok(root) = self.node(nsh_core::ROOT_ID)
        {
            // MountTarget already removed the engine root from the viewport.
            // Its normal deletion must not remove that native edge a second time.
            *root.mount_parent.borrow_mut() = std::rc::Weak::new();
        }
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            result = result.and(modal.close(false));
        }
        self.pending_delete.clear();
        self.deferred.clear();
        self.uncreated_scroll_content.clear();
        let ids: Vec<_> = self.nodes.keys().rev().copied().collect();
        for id in ids {
            result = result.and(self.delete_now(id));
        }
        result = result.and(self.shaders.close());
        self.sync_pending_deletions();
        result = result.and(self.flush_arkui());
        // Drain independent GPU owners even if one native node failed to
        // detach. Flush first: it can enqueue final scene/window commands.
        // The completion releases the runtime's retirement keepalive.
        if let Some(worker) = self.compositor.borrow().as_ref() {
            worker.collect();
            worker.drain();
        }
        result
    }

    pub(crate) fn freeze_retired(&mut self) -> Option<u64> {
        if self.removed.is_empty() {
            self.sync_pending_deletions();
            return None;
        }
        self.epoch += 1;
        self.retired
            .push_back((self.epoch, std::mem::take(&mut self.removed)));
        self.sync_pending_deletions();
        Some(self.epoch)
    }

    pub(crate) fn reclaim(&mut self, epoch: u64) {
        while self.retired.front().is_some_and(|(id, _)| *id <= epoch) {
            self.retired.pop_front();
        }
        self.sync_pending_deletions();
    }
}

impl Drop for Mounter {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::error!("OHOS detach failed: {error}");
        }
    }
}
