//! Native dialog presentation and desktop SubWindow hosts. Mobile close animations
//! retain the native subtree until completion; desktop secondary windows mount into
//! real HarmonyOS SubWindows backed by `NodeContent`.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use nsh_core::{
    EdgeInsets, Edges, EventKind, EventPayload, ModalAnimation, ModalConfig, ModalPresentation,
    Points,
    desktop::{
        CloseReason, OpenWindow, WindowAttributes, WindowCloseCause, WindowControl, WindowEvent,
        WindowFrameStyle, WindowMode, WindowPosition, WindowRequest, WindowState,
    },
};
use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent,
    common::node::ArkUINode,
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Custom,
    },
    dialog::Dialog,
    types::{advanced::LengthMetricUnit, alignment::Alignment},
};

use crate::render::{
    geometry::{Size, WindowRect, visual_insets},
    node::{Node, Result},
};

thread_local! {
    static SUB_WINDOWS: RefCell<BTreeMap<(u32, u32), Weak<Modal>>> = RefCell::default();
}

pub(crate) fn attach_sub_window(
    env: napi_ohos::Env,
    session_id: u32,
    window_id: u32,
    content: napi_ohos::bindgen_prelude::Object<'_>,
    platform_window_id: i32,
) -> napi_ohos::Result<bool> {
    let modal = SUB_WINDOWS.with(|map| {
        map.borrow()
            .get(&(session_id, window_id))
            .and_then(Weak::upgrade)
    });
    let Some(modal) = modal else {
        return Ok(false);
    };
    modal.attach_sub_window_root(env, content, platform_window_id)
}

struct Transition {
    start: f64,
    from: f32,
    to: f32,
    duration: f64,
}
type SettledCallback = Box<dyn FnOnce(bool)>;

struct DesktopWindow {
    session_id: u32,
    window_id: u32,
    modal: bool,
    parent_window_id: Option<u32>,
    attrs: WindowAttributes,
    mode: WindowMode,
    minimized: bool,
    focused: bool,
    hidden: bool,
    /// Outer window size in vp.
    size: Size,
    /// Drawable content viewport size in vp.
    content_size: Size,
    normal_size: Size,
    /// Outer window origin in screen vp, refreshed from native window events.
    origin: Option<(f32, f32)>,
    normal_origin: Option<(f32, f32)>,
    scale_factor: f32,
    platform_window_id: Option<i32>,
    sub_window_opened: bool,
    closed_emitted: bool,
}

struct SubWindowMount {
    container: Rc<RefCell<ArkUINode>>,
    _root: crate::interop::attachment::ContentRoot,
}

impl Drop for SubWindowMount {
    fn drop(&mut self) {
        let count = self.container.borrow().children().len();
        for index in (0..count).rev() {
            if let Err(error) = self.container.borrow_mut().remove_child(index) {
                log::warn!("OHOS subwindow detach child: {error}");
            }
        }
        // `_root` drops immediately after this body, removing and disposing the empty `container`.
    }
}

pub(crate) struct Modal {
    owner: Weak<Node>,
    self_weak: RefCell<Weak<Modal>>,
    dialog: RefCell<Option<Dialog>>,
    sub_window: RefCell<Option<SubWindowMount>>,
    config: RefCell<ModalConfig>,
    desktop: RefCell<Option<DesktopWindow>>,
    hosted_in_main: Cell<bool>,
    has_modal_config: Cell<bool>,
    pending_present: Cell<bool>,
    shown: Cell<bool>,
    show_emitted: Cell<bool>,
    progress: Cell<f32>,
    transition: RefCell<Option<Transition>>,
    shader: RefCell<Option<Rc<crate::render::shader::navigation::Navigation>>>,
    closing: Cell<bool>,
    insets: Cell<EdgeInsets>,
    /// Native window geometry, refreshed by presentation, measure and window events.
    available: Cell<Option<Size>>,
    reported: Cell<nsh_core::navigation::Reported>,
    overlay: RefCell<Option<crate::app::session::Overlay>>,
    on_settled: RefCell<Vec<SettledCallback>>,
}
impl Modal {
    #[cfg(feature = "dev")]
    pub(crate) fn test_animating(&self) -> bool {
        self.transition.borrow().is_some()
    }
    pub(crate) fn new(owner: Weak<Node>) -> Result<Rc<Self>> {
        let modal = Rc::new(Self {
            owner,
            self_weak: RefCell::new(Weak::new()),
            dialog: RefCell::new(None),
            sub_window: RefCell::default(),
            config: RefCell::new(ModalConfig::default()),
            desktop: RefCell::new(None),
            hosted_in_main: Cell::new(false),
            has_modal_config: Cell::new(false),
            pending_present: Cell::new(false),
            shown: Cell::new(false),
            show_emitted: Cell::new(false),
            progress: Cell::new(0.0),
            transition: RefCell::default(),
            shader: RefCell::default(),
            closing: Cell::new(false),
            insets: Cell::new(EdgeInsets::default()),
            available: Cell::new(None),
            reported: Cell::new(nsh_core::navigation::Reported::default()),
            overlay: RefCell::default(),
            on_settled: RefCell::default(),
        });
        *modal.self_weak.borrow_mut() = Rc::downgrade(&modal);
        Ok(modal)
    }
    fn ensure_dialog(&self) -> Result<()> {
        if self.dialog.borrow().is_some() {
            return Ok(());
        }
        let dialog = Dialog::new()?;
        dialog.custom_style(true)?;
        dialog.custom_animation(true)?;
        dialog.modal_mode(true)?;
        dialog.auto_cancel(true)?;
        dialog.content_alignment(Alignment::Center)?;
        let weak = self.self_weak.borrow().clone();
        dialog.dismiss_handler(move |reason| {
            if let Some(modal) = weak.upgrade() {
                let reason: i32 = reason.into();
                if reason == 0 {
                    // ArkUI delivers hardware Back to the native dialog before
                    // the ability. Route it through the same nested-stack policy.
                    if let Some(owner) = modal.owner.upgrade()
                        && let Some(route) = owner.dispatcher.upgrade()
                    {
                        route.send(crate::app::runtime::Message::Back);
                    }
                } else {
                    crate::render::node::callback(|| modal.on_dismiss_attempt(reason));
                }
            }
            // Dismissal is always answered from mounted props and animated here.
            true
        })?;
        *self.dialog.borrow_mut() = Some(dialog);
        Ok(())
    }
    pub(crate) fn node_id(&self) -> Option<nsh_core::NodeId> {
        self.owner.upgrade().map(|node| node.id)
    }
    pub(crate) fn presentation(&self) -> ModalPresentation {
        self.config.borrow().presentation
    }
    pub(crate) fn is_desktop_window(&self) -> bool {
        self.desktop.borrow().is_some()
    }
    pub(crate) fn is_main_window_host(&self) -> bool {
        self.hosted_in_main.get()
    }
    pub(crate) fn has_modal_config(&self) -> bool {
        self.has_modal_config.get()
    }
    pub(crate) fn cancel_pending_present(&self) {
        self.pending_present.set(false);
    }
    pub(crate) fn open_in_main_window(&self, host_root: &Rc<Node>) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        self.pending_present.set(false);
        self.hosted_in_main.set(true);
        self.config.borrow_mut().animation = ModalAnimation::None;
        self.shown.set(true);
        self.closing.set(false);
        self.show_emitted.set(true);
        host_root.enable_layout();
        if host_root.scroll.borrow().is_none() {
            host_root.promote_direct_paint()?;
        }
        if !owner.batch_created.get() {
            owner.flush_deferred_native()?;
        }
        if owner.mount_parent.borrow().upgrade().is_none() {
            let container = host_root.child_container();
            let index = container.borrow().children().len();
            container
                .borrow_mut()
                .insert_child(owner.mounted(), index)?;
            *owner.mount_parent.borrow_mut() = Rc::downgrade(&container);
        }
        let size: Size = owner.platform.get().window.into();
        self.available.set(Some(size));
        owner.invalidate_mounted_geometry();
        if size.width > 0.0 && size.height > 0.0 {
            self.report_metrics(&owner, size);
            owner.set_frame(crate::render::geometry::Rect {
                x: 0.0,
                y: 0.0,
                width: size.width,
                height: size.height,
            })?;
            owner.update_layout_bounds()?;
        }
        owner.restore_style()?;
        owner.update_wrapper()?;
        Ok(())
    }
    fn close_main_window_host(&self) -> Result<()> {
        self.pending_present.set(false);
        self.hosted_in_main.set(false);
        self.shown.set(false);
        self.closing.set(false);
        self.available.set(None);
        if let Some(owner) = self.owner.upgrade()
            && let Some(parent) = std::mem::take(&mut *owner.mount_parent.borrow_mut()).upgrade()
        {
            let mounted = owner.mounted();
            let index = parent
                .borrow()
                .children()
                .iter()
                .position(|child| Rc::ptr_eq(child, &mounted));
            if let Some(index) = index {
                let _ = parent.borrow_mut().remove_child(index);
            }
        }
        Ok(())
    }
    pub(crate) fn is_desktop_focused(&self) -> bool {
        self.shown.get()
            && !self.closing.get()
            && self
                .desktop
                .borrow()
                .as_ref()
                .is_some_and(|w| w.focused && !w.hidden && !w.minimized)
    }
    pub(crate) fn is_desktop_modal(&self) -> bool {
        self.is_visible()
            && self
                .desktop
                .borrow()
                .as_ref()
                .is_some_and(|w| w.modal && !w.hidden && !w.minimized)
    }
    pub(crate) fn desktop_movable(&self) -> bool {
        self.desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.movable != Some(false) && w.mode == WindowMode::Normal)
    }
    pub(crate) fn desktop_parent_window_id(&self) -> Option<u32> {
        self.desktop
            .borrow()
            .as_ref()
            .and_then(|w| w.parent_window_id.or(Some(nsh_core::ROOT_ID)))
    }
    pub(crate) fn close_for_parent(&self) -> Result<()> {
        if !self.is_desktop_window() || !self.shown.get() {
            return Ok(());
        }
        let owner = self.owner.upgrade();
        self.close_sub_window(false)?;
        if let Some(owner) = owner {
            self.emit_desktop_closed(&owner, WindowCloseCause::ParentClose);
        }
        Ok(())
    }
    pub(crate) fn desktop_transparent(&self) -> bool {
        self.desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.transparent == Some(true))
    }
    pub(crate) fn desktop_frame_none(&self) -> bool {
        self.desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.frame == Some(WindowFrameStyle::None))
    }
    pub(crate) fn desktop_backdrop(&self) -> bool {
        self.desktop.borrow().as_ref().is_some_and(|w| {
            matches!(
                w.attrs.backdrop,
                Some(
                    nsh_core::desktop::Backdrop::Window
                        | nsh_core::desktop::Backdrop::Sidebar
                        | nsh_core::desktop::Backdrop::Popover
                        | nsh_core::desktop::Backdrop::Hud
                )
            )
        })
    }
    fn clamp_desktop_size(attrs: &WindowAttributes, size: Size) -> Size {
        let (w, h) = crate::services::desktop::clamp_window_size((size.width, size.height), attrs);
        Size::new(w, h)
    }
    pub(crate) fn open_desktop_window(&self, open: OpenWindow) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let requested = open
            .placement
            .map(|p| Size::new(p.bounds.size.width.0, p.bounds.size.height.0))
            .or_else(|| open.options.default_size.map(Into::into))
            .unwrap_or_else(|| {
                let (w, h) = crate::services::desktop::default_sub_window_size(open.options.kind);
                Size::new(w, h)
            });
        let size = Self::clamp_desktop_size(&open.options.attributes, requested);
        let origin = open
            .placement
            .map(|p| (p.bounds.origin.x.0, p.bounds.origin.y.0))
            .or(match open.options.default_position {
                Some(WindowPosition::At(point)) => Some((point.x.0, point.y.0)),
                _ => None,
            });
        let mode = open
            .placement
            .map(|p| p.mode)
            .or(open.options.default_mode)
            .unwrap_or(WindowMode::Normal);
        let focused = open.options.attributes.activate != Some(false)
            && open.options.attributes.focusable != Some(false);
        let wire = crate::services::desktop::wire_open_sub_window(
            owner.id,
            &open,
            (size.width, size.height),
        );
        let session_id = owner.host.session_id();
        let parent_window_id = open.parent.filter(|id| *id != nsh_core::ROOT_ID);
        self.config.borrow_mut().animation = ModalAnimation::None;
        *self.desktop.borrow_mut() = Some(DesktopWindow {
            session_id,
            window_id: owner.id,
            modal: open.options.modal,
            parent_window_id,
            attrs: open.options.attributes,
            mode,
            minimized: false,
            focused,
            hidden: false,
            size,
            content_size: size,
            normal_size: size,
            origin,
            normal_origin: origin,
            scale_factor: owner.scale.get().max(1.0),
            platform_window_id: None,
            sub_window_opened: true,
            closed_emitted: false,
        });
        if let Some(modal_rc) = owner.modal.borrow().as_ref() {
            let weak = Rc::downgrade(modal_rc);
            SUB_WINDOWS.with(|map| {
                map.borrow_mut().insert((session_id, owner.id), weak);
            });
        }
        self.pending_present.set(false);
        self.shown.set(true);
        self.closing.set(false);
        self.show_emitted.set(true);
        self.available.set(Some(size));
        self.report_metrics(&owner, size);
        owner.set_frame(owner.raw_frame.get())?;
        owner.restore_style()?;
        owner.update_wrapper()?;
        owner.host.desktop_command(&wire)?;
        Ok(())
    }
    fn attach_sub_window_root(
        &self,
        env: napi_ohos::Env,
        content: napi_ohos::bindgen_prelude::Object<'_>,
        platform_window_id: i32,
    ) -> napi_ohos::Result<bool> {
        if !self.shown.get() || self.closing.get() {
            return Ok(false);
        }
        let Some(owner) = self.owner.upgrade() else {
            return Ok(false);
        };
        if owner.pending_delete.get() {
            return Ok(false);
        }
        let mut root = crate::interop::attachment::ContentRoot::from_object(env, content)?;
        let container = Custom::new()
            .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?
            .into_node();
        {
            let node = container.borrow();
            node.set_length_metric_unit(LengthMetricUnit::Px)
                .and_then(|()| node.background_color(0))
                .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        }
        let measure_children = Rc::downgrade(&container);
        container.borrow_mut().on_custom_measure(move |event| {
            crate::render::node::callback(|| {
                let constraint = event.layout_constraint_in_measure().ok_or(
                    crate::render::node::Error::Unsupported("missing subwindow constraints"),
                )?;
                event.set_measured_size(
                    constraint.max_width().max(0),
                    constraint.max_height().max(0),
                )?;
                if let Some(parent) = measure_children.upgrade() {
                    let mut measure = crate::render::layout::ChildMeasure::new(event);
                    for child in parent.borrow().children() {
                        measure.child(&child.borrow())?;
                    }
                }
                Ok(())
            })
        });
        let layout_children = Rc::downgrade(&container);
        container.borrow_mut().on_custom_layout(move |event| {
            crate::render::node::callback(|| {
                event.set_layout_position(0, 0)?;
                if let Some(parent) = layout_children.upgrade() {
                    for child in parent.borrow().children() {
                        event.layout_child(&child.borrow(), 0, 0)?;
                    }
                }
                Ok(())
            })
        });
        container
            .borrow_mut()
            .insert_child(owner.mounted(), 0)
            .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
        if let Err(error) = root.mount(container.clone()) {
            let _ = container.borrow_mut().remove_child(0);
            return Err(napi_ohos::Error::from_reason(error.to_string()));
        }
        *self.sub_window.borrow_mut() = Some(SubWindowMount {
            container,
            _root: root,
        });
        if let Some(win) = self.desktop.borrow_mut().as_mut() {
            win.platform_window_id = Some(platform_window_id);
        }
        owner
            .host
            .set_sub_window_platform_id(owner.id, platform_window_id);
        let _ = self.refresh_desktop_layout();
        Ok(true)
    }
    pub(crate) fn set_desktop_attributes(&self, diff: WindowAttributes) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        {
            let mut slot = self.desktop.borrow_mut();
            let Some(win) = slot.as_mut() else {
                return Ok(());
            };
            win.attrs.merge(&diff);
        }
        owner.restore_style()?;
        owner
            .host
            .desktop_command(&crate::services::desktop::wire_attributes_for(
                Some(owner.id),
                &diff,
            ))?;
        Ok(())
    }
    pub(crate) fn desktop_request(&self, request: WindowRequest) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if matches!(request, WindowRequest::Close) {
            return self.close_sub_window(false);
        }
        let mut content_resize = None;
        {
            let mut slot = self.desktop.borrow_mut();
            let Some(win) = slot.as_mut() else {
                return Ok(());
            };
            match &request {
                WindowRequest::Show | WindowRequest::Restore => {
                    win.hidden = false;
                    win.minimized = false;
                }
                WindowRequest::Focus => {
                    win.hidden = false;
                    win.minimized = false;
                    win.focused = true;
                }
                WindowRequest::Hide => {
                    win.hidden = true;
                    win.focused = false;
                }
                WindowRequest::Minimize => {
                    win.minimized = true;
                    win.focused = false;
                }
                WindowRequest::SetMode(mode) => {
                    win.mode = *mode;
                }
                WindowRequest::SetBounds(frame) => {
                    let mut size = win.size;
                    if let Some(w) = frame.width {
                        size.width = w.0;
                    }
                    if let Some(h) = frame.height {
                        size.height = h.0;
                    }
                    win.size = Self::clamp_desktop_size(&win.attrs, size);
                    if frame.x.is_some() || frame.y.is_some() {
                        let base = win.origin.unwrap_or((0.0, 0.0));
                        win.origin = Some((
                            frame.x.map_or(base.0, |x| x.0),
                            frame.y.map_or(base.1, |y| y.0),
                        ));
                    }
                    if win.mode == WindowMode::Normal {
                        win.normal_size = win.size;
                        win.normal_origin = win.origin;
                    }
                }
                WindowRequest::SetContentSize { width, height } => {
                    let mut next = win.content_size;
                    if let Some(w) = width {
                        next.width = w.0;
                    }
                    if let Some(h) = height {
                        next.height = h.0;
                    }
                    let next = Self::clamp_desktop_size(&win.attrs, next);
                    win.content_size = next;
                    content_resize = Some(next);
                }
                WindowRequest::SetPlacement(placement) => {
                    win.mode = placement.mode;
                    win.normal_size = Self::clamp_desktop_size(
                        &win.attrs,
                        Size::new(
                            placement.bounds.size.width.0,
                            placement.bounds.size.height.0,
                        ),
                    );
                    win.normal_origin =
                        Some((placement.bounds.origin.x.0, placement.bounds.origin.y.0));
                    if placement.mode == WindowMode::Normal {
                        win.size = win.normal_size;
                        win.origin = win.normal_origin;
                    }
                }
                WindowRequest::Center { .. }
                | WindowRequest::RequestAttention { .. }
                | WindowRequest::Close => {}
            }
        }
        owner
            .host
            .desktop_command(&crate::services::desktop::wire_request_for(
                Some(owner.id),
                &request,
            ))?;
        if let Some(size) = content_resize {
            self.available.set(Some(size));
            self.report_metrics(&owner, size);
        }
        Ok(())
    }
    pub(crate) fn desktop_window_control(&self, control: WindowControl) -> Result<()> {
        match control {
            WindowControl::Minimize => self.desktop_request(WindowRequest::Minimize),
            WindowControl::Maximize => self.desktop_toggle_maximize(),
            WindowControl::Close => self.on_sub_window_close_press(CloseReason::CloseButton),
        }
    }
    pub(crate) fn desktop_start_moving(&self) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let movable = self
            .desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.movable != Some(false) && w.mode == WindowMode::Normal);
        if !movable {
            return Ok(());
        }
        owner
            .host
            .desktop_command(&crate::services::desktop::WireCommand::StartMoving {
                window_id: Some(owner.id),
            })?;
        Ok(())
    }
    pub(crate) fn desktop_toggle_maximize(&self) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let maximizable = self
            .desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.maximizable != Some(false));
        if !maximizable {
            return Ok(());
        }
        owner
            .host
            .desktop_command(&crate::services::desktop::WireCommand::ToggleMaximize {
                window_id: Some(owner.id),
            })?;
        Ok(())
    }
    pub(crate) fn desktop_toggle_fullscreen(&self) -> Result<()> {
        let next = {
            let slot = self.desktop.borrow();
            let Some(win) = slot.as_ref() else {
                return Ok(());
            };
            if win.mode == WindowMode::FullScreen {
                WindowMode::Normal
            } else {
                WindowMode::FullScreen
            }
        };
        self.desktop_request(WindowRequest::SetMode(next))
    }
    pub(crate) fn desktop_resizable(&self) -> bool {
        self.desktop.borrow().as_ref().is_some_and(|w| {
            w.mode == WindowMode::Normal
                && w.attrs.resizable != Some(false)
                && matches!(
                    w.attrs.size_to_content,
                    None | Some(nsh_core::desktop::SizeToContent::Off)
                )
        })
    }
    pub(crate) fn desktop_resize_edge(
        &self,
        local: (f32, f32),
    ) -> Option<nsh_core::desktop::ResizeEdge> {
        if !self.desktop_resizable() || !self.desktop_frame_none() {
            return None;
        }
        let owner = self.owner.upgrade()?;
        let frame = owner.frame.get();
        crate::services::desktop::resize_edge(
            local.0,
            local.1,
            frame.width,
            frame.height,
            crate::services::desktop::FRAMELESS_RESIZE_MARGIN_VP,
        )
    }
    pub(crate) fn desktop_resize_anchor(&self) -> Option<((f32, f32), (f32, f32))> {
        let slot = self.desktop.borrow();
        let win = slot.as_ref()?;
        Some((
            win.origin.unwrap_or((0.0, 0.0)),
            (win.size.width, win.size.height),
        ))
    }
    pub(crate) fn desktop_resize_from(
        &self,
        edge: nsh_core::desktop::ResizeEdge,
        start_origin: (f32, f32),
        start_size: (f32, f32),
        total_dx: f32,
        total_dy: f32,
    ) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let next = {
            let mut slot = self.desktop.borrow_mut();
            let Some(win) = slot.as_mut() else {
                return Ok(());
            };
            if win.mode != WindowMode::Normal
                || win.attrs.resizable == Some(false)
                || !matches!(
                    win.attrs.size_to_content,
                    None | Some(nsh_core::desktop::SizeToContent::Off)
                )
            {
                return Ok(());
            }
            let min_w = win
                .attrs
                .min_size
                .flatten()
                .map_or(1.0, |s| s.width.0.max(1.0));
            let min_h = win
                .attrs
                .min_size
                .flatten()
                .map_or(1.0, |s| s.height.0.max(1.0));
            let max_w = win.attrs.max_size.flatten().map(|s| s.width.0);
            let max_h = win.attrs.max_size.flatten().map(|s| s.height.0);
            let (next_origin, next_size) = crate::services::desktop::apply_resize_delta(
                edge,
                start_origin,
                start_size,
                total_dx,
                total_dy,
                (min_w, min_h),
                (max_w, max_h),
            );
            win.origin = Some(next_origin);
            win.normal_origin = win.origin;
            win.size = Size::new(next_size.0, next_size.1);
            win.normal_size = win.size;
            (next_origin, next_size)
        };
        let frame = nsh_core::desktop::WindowFrame {
            x: Some(Points(next.0.0)),
            y: Some(Points(next.0.1)),
            width: Some(Points(next.1.0)),
            height: Some(Points(next.1.1)),
            ..Default::default()
        };
        owner
            .host
            .desktop_command(&crate::services::desktop::wire_request_for(
                Some(owner.id),
                &WindowRequest::SetBounds(frame),
            ))?;
        Ok(())
    }
    pub(crate) fn on_sub_window_state(&self, state: WindowState) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let content_size = Size::new(
            state.content_bounds.size.width.0,
            state.content_bounds.size.height.0,
        );
        {
            let mut slot = self.desktop.borrow_mut();
            let Some(win) = slot.as_mut() else {
                return Ok(());
            };
            win.mode = state.mode;
            win.minimized = state.minimized;
            win.focused = state.focused;
            win.hidden = !state.visible && !state.minimized;
            if state.scale_factor.is_finite() && state.scale_factor > 0.0 {
                win.scale_factor = state.scale_factor;
            }
            if state.bounds.size.width.0 > 0.0 && state.bounds.size.height.0 > 0.0 {
                win.size = Size::new(state.bounds.size.width.0, state.bounds.size.height.0);
                win.origin = Some((state.bounds.origin.x.0, state.bounds.origin.y.0));
            }
            if content_size.width > 0.0 && content_size.height > 0.0 {
                win.content_size = content_size;
            }
            let normal = state.placement.bounds;
            if normal.size.width.0 > 0.0 && normal.size.height.0 > 0.0 {
                win.normal_size = Size::new(normal.size.width.0, normal.size.height.0);
                win.normal_origin = Some((normal.origin.x.0, normal.origin.y.0));
            }
        }
        if content_size.width > 0.0 && content_size.height > 0.0 {
            self.available.set(Some(content_size));
            self.report_metrics(&owner, content_size);
        }
        owner.emit(
            EventKind::Window,
            EventPayload::Window(Box::new(WindowEvent::State(Box::new(state)))),
        );
        Ok(())
    }
    pub(crate) fn on_sub_window_title_bar(&self, edges: Edges<Points>) {
        if let Some(owner) = self.owner.upgrade() {
            owner.emit(
                EventKind::Window,
                EventPayload::Window(Box::new(WindowEvent::TitleBar(edges))),
            );
        }
    }
    pub(crate) fn on_sub_window_close_press(&self, reason: CloseReason) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let close_on_press = self
            .desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.close_on_press != Some(false));
        owner.emit(
            EventKind::Window,
            EventPayload::Window(Box::new(WindowEvent::ClosePress { reason })),
        );
        if close_on_press {
            self.close_sub_window(false)?;
            self.emit_desktop_closed(&owner, WindowCloseCause::ClosePress);
        }
        Ok(())
    }
    pub(crate) fn on_sub_window_blur(&self) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let should_close = self
            .desktop
            .borrow()
            .as_ref()
            .is_some_and(|w| w.attrs.close_on_blur == Some(true));
        if should_close && self.shown.get() {
            self.close_sub_window(false)?;
            self.emit_desktop_closed(&owner, WindowCloseCause::Blur);
        }
        Ok(())
    }
    pub(crate) fn on_sub_window_destroyed(&self) -> Result<()> {
        let owner = self.owner.upgrade();
        self.close_sub_window(true)?;
        if let Some(owner) = owner {
            self.emit_desktop_closed(&owner, WindowCloseCause::ClosePress);
        }
        Ok(())
    }
    fn emit_desktop_closed(&self, owner: &Node, cause: WindowCloseCause) {
        let should_emit = self
            .desktop
            .borrow_mut()
            .as_mut()
            .is_some_and(|w| !std::mem::replace(&mut w.closed_emitted, true));
        if should_emit {
            owner.emit(
                EventKind::Window,
                EventPayload::Window(Box::new(WindowEvent::Closed { cause })),
            );
        }
    }
    fn close_sub_window(&self, already_destroyed: bool) -> Result<()> {
        self.pending_present.set(false);
        self.closing.set(false);
        self.shown.set(false);
        self.show_emitted.set(false);
        self.available.set(None);
        self.notify_settled(false);
        if let Some(owner) = self.owner.upgrade() {
            let key = self
                .desktop
                .borrow()
                .as_ref()
                .map(|w| (w.session_id, w.window_id))
                .unwrap_or((owner.host.session_id(), owner.id));
            SUB_WINDOWS.with(|map| {
                map.borrow_mut().remove(&key);
            });
            owner.host.remove_sub_window_platform_id(owner.id);
            // Unmount the native subtree from the SubWindow's NodeContent before
            // ArkTS calls destroyWindow(), preventing dangling native handles.
            self.sub_window.borrow_mut().take();
            let send_close = self.desktop.borrow_mut().as_mut().is_some_and(|win| {
                win.hidden = true;
                win.focused = false;
                win.platform_window_id = None;
                std::mem::replace(&mut win.sub_window_opened, false)
            });
            if send_close && !already_destroyed {
                owner.host.desktop_command(
                    &crate::services::desktop::WireCommand::CloseSubWindow {
                        window_id: owner.id,
                    },
                )?;
            }
        } else {
            self.sub_window.borrow_mut().take();
        }
        Ok(())
    }
    fn refresh_desktop_layout(&self) -> Result<()> {
        self.refresh_metrics();
        if let Some(owner) = self.owner.upgrade() {
            owner.set_frame(owner.raw_frame.get())?;
            owner.restore_style()?;
            owner.update_wrapper()?;
            owner
                .mounted()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            owner
                .mounted()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
            owner
                .native
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            owner
                .native
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
            owner
                .native
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    pub(crate) fn configure(&self, config: ModalConfig) -> Result<()> {
        self.has_modal_config.set(true);
        if self.closing.get() {
            return Ok(());
        }
        if self.is_desktop_window() || self.hosted_in_main.get() {
            self.config.replace(config);
            return Ok(());
        }
        let presentation = config.presentation;
        let previous = self.config.replace(config);
        self.ensure_dialog()?;
        {
            let dialog = self.dialog.borrow();
            let dialog = dialog.as_ref().expect("ensured modal dialog");
            dialog.content_alignment(match presentation {
                ModalPresentation::FullScreen | ModalPresentation::FormSheet => Alignment::Center,
                ModalPresentation::PageSheet => Alignment::Bottom,
            })?;
            match presentation {
                ModalPresentation::FullScreen => {
                    dialog.corner_radius(0.0, 0.0, 0.0, 0.0)?;
                }
                ModalPresentation::PageSheet => {
                    dialog.corner_radius(16.0, 16.0, 0.0, 0.0)?;
                }
                ModalPresentation::FormSheet => {
                    dialog.corner_radius(16.0, 16.0, 16.0, 16.0)?;
                }
            }
        }
        self.theme()?;
        self.refresh_metrics();
        if let Some(owner) = self.owner.upgrade() {
            if self.shown.get() && previous.presentation != presentation {
                owner.shaders.modal_presentation_changed(&owner)?;
            }
            owner.set_frame(owner.raw_frame.get())?;
            owner.restore_style()?;
            owner.update_wrapper()?;
            owner
                .mounted()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            owner
                .mounted()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
            owner
                .native
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            owner
                .native
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
        }
        Ok(())
    }
    fn theme(&self) -> Result<()> {
        if self.is_desktop_window() || self.hosted_in_main.get() {
            return Ok(());
        }
        self.ensure_dialog()?;
        let config = self.config.borrow();
        let full = config.presentation == ModalPresentation::FullScreen;
        let dialog = self.dialog.borrow();
        let dialog = dialog.as_ref().expect("ensured modal dialog");
        // A full-screen modal owns its backdrop and hit-test surface.
        // ArkUI's default modal mask would dim that backdrop a second time.
        // Sheets still need the native mask outside their smaller content bounds.
        dialog.modal_mode(!full)?;
        // A full-screen surface paints to the keyboard edge. Restore ArkUI's
        // default 16vp spacing when this controller changes back to a sheet.
        dialog.keyboard_avoid_distance(
            if full { 0.0 } else { 16.0 },
            ohos_arkui_binding::types::advanced::LengthMetricUnit::Vp,
        )?;
        // `owner.native` paints the sheet's background, rounded corners and slide/fade
        // transition; keep the outer dialog container transparent so no stationary box
        // shows behind an animating sheet.
        dialog.background_color(0)?;
        Ok(())
    }
    pub(crate) fn schedule_present(&self) {
        self.pending_present.set(true);
    }
    pub(crate) fn present_if_pending(&self) -> Result<()> {
        if self.pending_present.replace(false) {
            self.present()?;
        }
        Ok(())
    }
    pub(crate) fn present(&self) -> Result<()> {
        self.pending_present.set(false);
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if self.hosted_in_main.get() {
            self.shown.set(true);
            self.closing.set(false);
            self.refresh_metrics();
            return Ok(());
        }
        if self.is_desktop_window() {
            self.shown.set(true);
            self.closing.set(false);
            self.refresh_desktop_layout()?;
            return Ok(());
        }
        self.refresh_metrics();
        owner.set_frame(owner.raw_frame.get())?;
        let was_shown = self.shown.replace(true);
        let was_closing = self.closing.replace(false);
        if !was_shown {
            self.show_emitted.set(false);
            self.theme()?;
            {
                let dialog = self.dialog.borrow();
                let dialog = dialog.as_ref().expect("ensured modal dialog");
                dialog.content_node(&owner.mounted())?;
                dialog.show()?;
            }
            owner.shaders.modal_shown(&owner, true)?;
            *self.overlay.borrow_mut() = Some(owner.host.overlay(owner.dispatcher.clone())?);
        }
        owner
            .mounted()
            .borrow()
            .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
        owner
            .mounted()
            .borrow()
            .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
        owner
            .native
            .borrow()
            .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
        owner
            .native
            .borrow()
            .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedLayout)?;
        if !was_shown || was_closing {
            self.begin(1.0)?;
        }
        Ok(())
    }
    pub(crate) fn request_close(&self) -> Result<()> {
        self.on_dismiss_attempt(0)
    }
    fn on_dismiss_attempt(&self, reason: i32) -> Result<()> {
        if self.closing.get() || self.hosted_in_main.get() {
            return Ok(());
        }
        if self.is_desktop_window() {
            let (cause, should_close) = {
                let slot = self.desktop.borrow();
                let Some(win) = slot.as_ref() else {
                    return Ok(());
                };
                match reason {
                    1 => (
                        WindowCloseCause::Blur,
                        win.attrs.close_on_blur == Some(true),
                    ),
                    _ => (
                        WindowCloseCause::Escape,
                        win.attrs.close_on_escape == Some(true),
                    ),
                }
            };
            if should_close {
                self.close_sub_window(false)?;
                if let Some(owner) = self.owner.upgrade() {
                    self.emit_desktop_closed(&owner, cause);
                }
            }
            return Ok(());
        }
        let config = self.config.borrow();
        let (reason_str, should_close) = match reason {
            1 => ("backdropPress", config.close_on_backdrop_press),
            3 => ("swipe", config.close_on_swipe),
            _ => ("cancel", config.close_on_escape),
        };
        if let Some(owner) = self.owner.upgrade() {
            owner.emit(
                EventKind::RequestClose,
                EventPayload::Map(vec![(
                    "reason",
                    nsh_core::PayloadValue::Str(reason_str.into()),
                )]),
            );
        }
        if should_close {
            self.close(true)?;
        }
        Ok(())
    }
    pub(crate) fn is_visible(&self) -> bool {
        self.shown.get()
    }
    pub(crate) fn is_entering(&self) -> bool {
        !self.closing.get()
            && (self
                .transition
                .borrow()
                .as_ref()
                .is_some_and(|transition| transition.to == 1.0)
                || self
                    .shader
                    .borrow()
                    .as_ref()
                    .is_some_and(|nav| !nav.backwards && nav.running()))
    }
    pub(crate) fn when_settled(&self, callback: impl FnOnce(bool) + 'static) {
        if self.is_entering() {
            self.on_settled.borrow_mut().push(Box::new(callback));
        } else {
            callback(self.shown.get() && !self.closing.get());
        }
    }
    fn notify_settled(&self, entered: bool) {
        let callbacks = std::mem::take(&mut *self.on_settled.borrow_mut());
        for callback in callbacks {
            callback(entered);
        }
    }
    pub(crate) fn close(&self, animated: bool) -> Result<()> {
        if self.hosted_in_main.get() {
            return self.close_main_window_host();
        }
        if self.is_desktop_window() {
            return self.close_sub_window(false);
        }
        self.pending_present.set(false);
        self.notify_settled(false);
        if !self.shown.get() {
            return Ok(());
        }
        if self.closing.replace(true) && animated {
            return Ok(());
        }
        if animated {
            self.begin(0.0)?;
        } else {
            self.finish(0.0)?;
        }
        Ok(())
    }
    pub(crate) fn on_shader_captured(
        &self,
        navigation: &crate::render::shader::navigation::Navigation,
    ) -> Result<()> {
        let matches = self
            .shader
            .borrow()
            .as_ref()
            .is_some_and(|current| current.key == navigation.key);
        if matches && !navigation.backwards {
            self.progress.set(1.0);
        }
        Ok(())
    }
    pub(crate) fn refresh_presentation(&self) -> Result<()> {
        if self.shader.borrow().is_some() {
            self.paint()?;
        }
        Ok(())
    }
    fn start_transition(&self, to: f32) -> Result<()> {
        let reduced = self
            .owner
            .upgrade()
            .is_some_and(|owner| owner.platform.get().reduce_motion);
        let duration = nsh_core::navigation::modal_motion(self.config.borrow().animation, reduced)
            .map_or(0.0, |motion| {
                motion.0.as_secs_f64() * 1000.0 * (self.progress.get() - to).abs() as f64
            });
        if duration <= 0.0 {
            return self.finish(to);
        }
        *self.transition.borrow_mut() = Some(Transition {
            start: self
                .owner
                .upgrade()
                .map_or(0.0, |owner| owner.animator.now()),
            from: self.progress.get(),
            to,
            duration,
        });
        self.paint()?;
        if let Some(owner) = self.owner.upgrade() {
            owner.animator.request_frame();
        }
        Ok(())
    }
    fn begin(&self, to: f32) -> Result<()> {
        if let Some(owner) = self.owner.upgrade()
            && let Some(navigation) = owner.shaders.begin_navigation(&owner, None, to == 0.0)?
        {
            self.transition.borrow_mut().take();
            *self.shader.borrow_mut() = Some(navigation);
            self.paint()?;
            owner.animator.request_frame();
            return Ok(());
        }
        let had_shader = self.shader.borrow_mut().take().is_some();
        if let Some(owner) = self.owner.upgrade() {
            if to == 0.0 {
                owner.shaders.modal_shown(&owner, false)?;
                owner.shaders.finish_modal_close(&owner, had_shader)?;
            } else if had_shader {
                owner.shaders.end_navigation(owner.id)?;
            }
        }
        self.start_transition(to)
    }
    pub(crate) fn frame(&self, now: f64) -> Result<bool> {
        let shader = self
            .shader
            .borrow()
            .as_ref()
            .map(|n| (n.running(), n.take_fallback(), n.backwards));
        if let Some((running, fallback, backwards)) = shader {
            if running {
                self.paint()?;
                return Ok(true);
            }
            let to = if backwards { 0.0 } else { 1.0 };
            if fallback {
                self.shader.borrow_mut().take();
                if let Some(owner) = self.owner.upgrade() {
                    if backwards {
                        owner.shaders.finish_modal_close(&owner, true)?;
                    } else {
                        owner.shaders.end_navigation(owner.id)?;
                    }
                }
                if !backwards && self.progress.get() == 1.0 {
                    self.progress.set(0.0);
                }
                self.start_transition(to)?;
            } else {
                self.finish(to)?;
                return Ok(false);
            }
        }
        let state = self.transition.borrow().as_ref().map(|transition| {
            let t = ((now - transition.start) / transition.duration).clamp(0.0, 1.0);
            let eased = nsh_core::navigation::ease(t);
            (
                transition.from + (transition.to - transition.from) * eased,
                t >= 1.0,
                transition.to,
            )
        });
        if let Some((value, done, to)) = state {
            self.progress.set(value);
            self.paint()?;
            if done {
                self.finish(to)?;
            }
        }
        Ok(self.transition.borrow().is_some())
    }
    fn paint(&self) -> Result<()> {
        if let Some(owner) = self.owner.upgrade() {
            if let Some(nav) = self.shader.borrow().as_ref() {
                let has_presented = owner.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                    wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .is_some_and(|composite| composite.has_presented(&owner, nav.key))
                });
                let waiting_open =
                    crate::render::shader::navigation::modal_open_waiting(nav, has_presented);
                return owner.presentation(if waiting_open { 0.0 } else { 1.0 }, 0.0);
            }
            let progress = self.progress.get();
            let crossfade = nsh_core::navigation::modal_motion(
                self.config.borrow().animation,
                owner.platform.get().reduce_motion,
            )
            .is_some_and(|motion| motion.1);
            let fade = if crossfade { progress } else { 1.0 };
            let dy = if self.config.borrow().animation == ModalAnimation::Slide && !crossfade {
                nsh_core::navigation::modal_in_offset(Points(owner.frame.get().height), progress).0
            } else {
                0.0
            };
            owner.presentation(fade, dy)?;
        }
        Ok(())
    }
    fn finish(&self, to: f32) -> Result<()> {
        let had_shader = self.shader.borrow_mut().take().is_some();
        self.transition.borrow_mut().take();
        self.progress.set(to);
        if to == 0.0 {
            self.closing.set(false);
            self.show_emitted.set(false);
            let was_shown = self.shown.replace(false);
            if was_shown {
                if let Some(dialog) = self.dialog.borrow().as_ref() {
                    dialog.close()?;
                    self.overlay.borrow_mut().take();
                    dialog.remove_content()?;
                } else {
                    self.overlay.borrow_mut().take();
                }
                self.available.set(None);
            }
            if let Some(owner) = self.owner.upgrade() {
                owner.shaders.modal_shown(&owner, false)?;
                owner.shaders.finish_modal_close(&owner, had_shader)?;
                owner.presentation(1.0, 0.0)?;
                if was_shown {
                    owner.emit(EventKind::Dismiss, EventPayload::None);
                }
            }
            self.notify_settled(false);
        } else {
            if had_shader && let Some(owner) = self.owner.upgrade() {
                owner.shaders.end_navigation(owner.id)?;
            }
            self.paint()?;
            if !self.show_emitted.replace(true)
                && let Some(owner) = self.owner.upgrade()
            {
                owner.emit(EventKind::Show, EventPayload::None);
            }
            self.notify_settled(self.shown.get() && !self.closing.get());
        }
        Ok(())
    }
    pub(crate) fn safe_top(&self) -> f32 {
        self.insets.get().top.0
    }
    pub(crate) fn closing(&self) -> bool {
        self.closing.get() && self.shown.get()
    }
    fn size_for_bounds(&self, owner: &Node, width: f32, height: f32) -> Size {
        if self.is_desktop_window() || self.hosted_in_main.get() {
            return Size::new(width, height);
        }
        match self.config.borrow().presentation {
            ModalPresentation::FullScreen => Size::new(width, height),
            ModalPresentation::PageSheet => {
                let scale = owner.scale.get() as f64;
                let safe_top = Self::insets_for_rect(
                    owner,
                    WindowRect {
                        width: width as f64 * scale,
                        height: height as f64 * scale,
                        ..WindowRect::default()
                    },
                )
                .map_or(owner.platform.get().safe_area.top.0, |insets| insets.top.0);
                Size::new(width.min(720.0), (height - safe_top - 24.0).max(0.0))
            }
            ModalPresentation::FormSheet => {
                Size::new(width.min(540.0), (height - 32.0).clamp(0.0, 620.0))
            }
        }
    }
    fn sub_window_scale(&self, owner: &Node) -> f32 {
        self.desktop
            .borrow()
            .as_ref()
            .map(|w| w.scale_factor)
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or_else(|| owner.scale.get().max(1.0))
    }
    fn sub_window_drawable_size(&self, owner: &Node) -> Option<Size> {
        let id = self.desktop.borrow().as_ref()?.platform_window_id?;
        let window = ohos_window_manager_binding::Window::from_id(id);
        let bounds = window.properties().ok()?.drawable_rect;
        let scale = self.sub_window_scale(owner);
        (bounds.width > 0 && bounds.height > 0)
            .then(|| Size::new(bounds.width as f32 / scale, bounds.height as f32 / scale))
    }
    fn window_size(owner: &Node) -> Option<Size> {
        let window = ohos_window_manager_binding::Window::from_id(owner.host.window_id()? as i32);
        let bounds = window.properties().ok()?.drawable_rect;
        (bounds.width > 0 && bounds.height > 0).then(|| {
            Size::new(
                bounds.width as f32 / owner.scale.get(),
                bounds.height as f32 / owner.scale.get(),
            )
        })
    }
    fn insets_for_rect(owner: &Node, viewport: WindowRect) -> Option<EdgeInsets> {
        let (visual, _) = owner.host.window_areas()?;
        let rect = |rect: crate::app::session::AvoidRect| WindowRect {
            x: rect.left as f64,
            y: rect.top as f64,
            width: rect.width as f64,
            height: rect.height as f64,
        };
        let [top, right, bottom, left] = visual_insets(
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
        let scale = owner.scale.get() as f64;
        Some(EdgeInsets {
            top: Points((top / scale) as f32),
            right: Points((right / scale) as f32),
            bottom: Points((bottom / scale) as f32),
            left: Points((left / scale) as f32),
        })
    }
    fn report_metrics(&self, owner: &Node, size: Size) {
        let insets = if self.is_desktop_window() {
            EdgeInsets::default()
        } else if self.hosted_in_main.get() {
            owner.platform.get().safe_area
        } else {
            // The dialog is rooted in the window, independently of the embedding
            // surface. Only the avoid areas intersecting its own bounds are insets.
            let safe = owner
                .mounted()
                .borrow()
                .layout_position_in_window()
                .ok()
                .and_then(|origin| {
                    let scale = owner.scale.get() as f64;
                    Self::insets_for_rect(
                        owner,
                        WindowRect {
                            x: origin.x as f64,
                            y: origin.y as f64,
                            width: size.width as f64 * scale,
                            height: size.height as f64 * scale,
                        },
                    )
                })
                .unwrap_or(owner.platform.get().safe_area);
            let presentation = self.config.borrow().presentation;
            let full = presentation == ModalPresentation::FullScreen;
            EdgeInsets {
                top: if full { safe.top } else { Points::ZERO },
                bottom: if presentation == ModalPresentation::FormSheet {
                    Points::ZERO
                } else {
                    safe.bottom
                },
                left: if size.width >= owner.platform.get().window.width.0 {
                    safe.left
                } else {
                    Points::ZERO
                },
                right: if size.width >= owner.platform.get().window.width.0 {
                    safe.right
                } else {
                    Points::ZERO
                },
            }
        };
        self.insets.set(insets);
        if size.height > 0.0 {
            // Reported only on change, as every backend: the core's root size, and
            // `onInsets` (`safeAreaInsets()` inside the modal) when the insets changed.
            let mut reported = self.reported.get();
            let changed = reported.update(size.into(), insets);
            self.reported.set(reported);
            if let Some(insets_changed) = changed {
                owner.control.set_root_size(owner.id, size.into(), insets);
                if insets_changed {
                    owner.emit(
                        EventKind::Insets,
                        nsh_core::navigation::insets_payload(insets),
                    );
                }
            }
        }
    }
    pub(crate) fn refresh_metrics(&self) -> Option<Size> {
        let owner = self.owner.upgrade()?;
        if self.hosted_in_main.get() {
            let size: Size = owner.platform.get().window.into();
            self.available.set(Some(size));
            if size.width > 0.0 && size.height > 0.0 {
                self.report_metrics(&owner, size);
                let _ = owner.set_frame(crate::render::geometry::Rect {
                    x: 0.0,
                    y: 0.0,
                    width: size.width,
                    height: size.height,
                });
                return Some(size);
            }
            return None;
        }
        if self.is_desktop_window() {
            let size = self
                .sub_window_drawable_size(&owner)
                .or_else(|| self.desktop.borrow().as_ref().map(|w| w.content_size));
            self.available.set(size);
            return self.sync_metrics();
        }
        self.available.set(Self::window_size(&owner));
        self.sync_metrics()
    }
    pub(crate) fn sync_metrics(&self) -> Option<Size> {
        let owner = self.owner.upgrade()?;
        if self.hosted_in_main.get() {
            let size: Size = owner.platform.get().window.into();
            if size.width <= 0.0 || size.height <= 0.0 {
                return None;
            }
            self.available.set(Some(size));
            self.report_metrics(&owner, size);
            return Some(size);
        }
        if self.is_desktop_window() {
            let size = self
                .available
                .get()
                .or_else(|| self.desktop.borrow().as_ref().map(|w| w.content_size))?;
            if size.width <= 0.0 || size.height <= 0.0 {
                return None;
            }
            self.report_metrics(&owner, size);
            return Some(size);
        }
        // A native Dialog fills its window, which may be larger than the NSH
        // surface after the host has already avoided system bars.
        let available = self
            .available
            .get()
            .unwrap_or_else(|| owner.platform.get().window.into());
        if available.width <= 0.0 || available.height <= 0.0 {
            return None;
        }
        let size = self.size_for_bounds(&owner, available.width, available.height);
        self.report_metrics(&owner, size);
        Some(size)
    }
    pub(crate) fn measure(&self, event: &NodeCustomEvent) -> Result<Size> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(Size::default());
        };
        if self.hosted_in_main.get() {
            let size: Size = owner.platform.get().window.into();
            self.available.set(Some(size));
            if size.width > 0.0 && size.height > 0.0 {
                self.report_metrics(&owner, size);
            }
            return Ok(size);
        }
        if self.is_desktop_window() {
            let fallback = self
                .desktop
                .borrow()
                .as_ref()
                .map_or_else(Size::default, |w| w.content_size);
            let scale = self.sub_window_scale(&owner);
            let available = if let Some(constraint) = event.layout_constraint_in_measure() {
                let bound = |value: i32, fallback: f32| {
                    let value = value as f32 / scale;
                    if value > 0.0 && value <= 16384.0 {
                        value
                    } else {
                        fallback
                    }
                };
                Size::new(
                    bound(constraint.max_width(), fallback.width),
                    bound(constraint.max_height(), fallback.height),
                )
            } else {
                self.sub_window_drawable_size(&owner).unwrap_or(fallback)
            };
            self.available.set(Some(available));
            self.report_metrics(&owner, available);
            return Ok(available);
        }
        let available = if let Some(size) = Self::window_size(&owner) {
            size
        } else if let Some(constraint) = event.layout_constraint_in_measure() {
            let bound = |value: i32, fallback: f32| {
                let value = value as f32 / owner.scale.get();
                if value > 0.0 && value <= 16384.0 {
                    value
                } else {
                    fallback
                }
            };
            let width = bound(constraint.max_width(), owner.platform.get().window.width.0);
            let height = bound(
                constraint.max_height(),
                owner.platform.get().window.height.0,
            );
            Size::new(width, height)
        } else {
            owner.platform.get().window.into()
        };
        self.available.set(Some(available));
        let size = self.size_for_bounds(&owner, available.width, available.height);
        self.report_metrics(&owner, size);
        Ok(size)
    }
    pub(crate) fn laid_out(&self) {
        if let Some(owner) = self.owner.upgrade() {
            let frame = owner.frame.get();
            self.report_metrics(&owner, Size::new(frame.width, frame.height));
        }
    }
}
impl Drop for Modal {
    fn drop(&mut self) {
        let is_desktop = self.desktop.get_mut().is_some();
        if let Some(win) = self.desktop.get_mut().as_mut() {
            SUB_WINDOWS.with(|map| {
                map.borrow_mut().remove(&(win.session_id, win.window_id));
            });
            self.sub_window.get_mut().take();
            if std::mem::replace(&mut win.sub_window_opened, false)
                && let Some(owner) = self.owner.upgrade()
            {
                let _ = owner.host.desktop_command(
                    &crate::services::desktop::WireCommand::CloseSubWindow {
                        window_id: win.window_id,
                    },
                );
            }
        } else {
            self.sub_window.get_mut().take();
        }
        if let Some(mut dialog) = self.dialog.get_mut().take() {
            if self.shown.get() && !is_desktop {
                let _ = dialog.close();
            }
            if let Err(error) = dialog.dispose() {
                log::warn!("OHOS dialog retirement: {error}");
            }
        }
    }
}
