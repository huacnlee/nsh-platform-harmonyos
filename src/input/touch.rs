//! Surface-local touch paths and native pan activation. No callback waits for JS.
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use nsh_core::{
    EventKind, EventPayload, PointerEvents, PointerType, TouchEvent, TouchPhase, TouchPoint,
};
use ohos_arkui_binding::arkui_input_binding::{ArkUIInputEvent, UIInputAction, UIInputToolType};

use crate::render::node::{Node, Result};

struct Finger {
    point: TouchPoint,
    node: Weak<Node>,
    start: (f32, f32),
    pans: Vec<(Weak<Node>, bool)>,
}
struct KeyboardTap {
    finger: u32,
    start: (f32, f32),
    input: Weak<Node>,
}
struct LastTap {
    time_ms: f64,
    window: (f32, f32),
    clicks: u32,
    event_time_ns: i64,
}
struct BackSwipe {
    screen: Weak<crate::navigation::stack::Screen>,
    target: Weak<Node>,
    finger: u32,
    start: (f32, f32),
    local_x: f32,
    sign: f32,
    width: f32,
    active: bool,
    last: (f32, f64),
    velocity: f32,
}
struct SelectionDrag {
    owner: Weak<Node>,
    finger: u32,
    start: (f32, f32),
    moved: bool,
    desktop: bool,
    pointer: Option<(f32, f32)>,
    frame_time: f64,
    blocked: Vec<Weak<crate::scroll::controller::Scroller>>,
}
enum WindowResizeTarget {
    SubWindow(Weak<crate::navigation::modal::Modal>),
    MainWindow,
}
struct WindowResize {
    target: WindowResizeTarget,
    edge: nsh_core::desktop::ResizeEdge,
    start_display: (f32, f32),
    start_origin: (f32, f32),
    start_size: (f32, f32),
    last: (f32, f32),
}
struct PendingWindowDrag {
    start_window: (f32, f32),
    modal: Option<Weak<crate::navigation::modal::Modal>>,
    is_main: bool,
}
impl SelectionDrag {
    fn unblock(&self) -> Result<()> {
        let mut failure = None;
        for scroll in self.blocked.iter().filter_map(Weak::upgrade) {
            if let Err(error) = scroll.block(false) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
const MOUSE_MULTI_CLICK_MS: f64 = 400.0;
const MOUSE_MULTI_CLICK_DISTANCE_VP: f32 = 12.0;
const TOUCH_MULTI_TAP_MS: f64 = 350.0;
const TOUCH_MULTI_TAP_DISTANCE_VP: f32 = 24.0;
const WINDOW_DRAG_THRESHOLD_VP: f32 = 3.0;

fn advance_clicks(
    slot: &mut Option<LastTap>,
    event_time_ns: i64,
    window: (f32, f32),
    max_interval_ms: f64,
    max_distance: f32,
) -> u32 {
    if let Some(last) = slot.as_ref()
        && last.event_time_ns == event_time_ns
        && event_time_ns != 0
    {
        return last.clicks;
    }
    let time_ms = event_time_ns as f64 / 1_000_000.0;
    let clicks = match slot.as_ref() {
        Some(last)
            if time_ms >= last.time_ms
                && time_ms - last.time_ms <= max_interval_ms
                && (window.0 - last.window.0).hypot(window.1 - last.window.1) <= max_distance =>
        {
            (last.clicks % 3) + 1
        }
        _ => 1,
    };
    *slot = Some(LastTap {
        time_ms,
        window,
        clicks,
        event_time_ns,
    });
    clicks
}

#[derive(Default)]
pub(crate) struct Touches {
    fingers: Vec<Finger>,
    activated: Option<Weak<Node>>,
    timestamp: f64,
    modifiers: nsh_core::keymap::Modifiers,
    keyboard_tap: Option<KeyboardTap>,
    suppressed_presses: Vec<nsh_core::NodeId>,
    back: Option<BackSwipe>,
    primary: Option<u32>,
    selection: Option<SelectionDrag>,
    window_resize: Option<WindowResize>,
    pending_window_drag: Option<PendingWindowDrag>,
    mouse_up_targets: Vec<Weak<Node>>,
    last_pointer: rustc_hash::FxHashMap<nsh_core::NodeId, (f32, f32)>,
    last_tap: Option<LastTap>,
    last_mouse_press: Option<LastTap>,
    handled_context_menu: Option<(i64, i32, i32)>,
    down_recorded: bool,
    last_down_time_ns: Option<i64>,
}
impl Touches {
    pub(crate) fn record_pointer_for(&mut self, window_id: nsh_core::NodeId, point: (f32, f32)) {
        if point.0.is_finite() && point.1.is_finite() {
            self.last_pointer.insert(window_id, point);
        }
    }
    pub(crate) fn last_pointer_for(&self, window_id: nsh_core::NodeId) -> Option<(f32, f32)> {
        self.last_pointer.get(&window_id).copied()
    }
    pub(crate) fn claim_context_menu(&mut self, event_time_ns: i64, wx: f32, wy: f32) -> bool {
        let key = (event_time_ns, wx.round() as i32, wy.round() as i32);
        if self.handled_context_menu == Some(key) {
            return false;
        }
        self.handled_context_menu = Some(key);
        true
    }
    pub(crate) fn record_mouse_press(&mut self, event_time_ns: i64, window: (f32, f32)) -> u32 {
        advance_clicks(
            &mut self.last_mouse_press,
            event_time_ns,
            window,
            MOUSE_MULTI_CLICK_MS,
            MOUSE_MULTI_CLICK_DISTANCE_VP,
        )
    }
    pub(crate) fn record_down(&mut self, window_id: nsh_core::NodeId, input: &ArkUIInputEvent) {
        if matches!(input.action, UIInputAction::Up | UIInputAction::Cancel) {
            self.down_recorded = false;
            return;
        }
        if input.action != UIInputAction::Down || input.pointer_count() != 1 {
            return;
        }
        let event_time_ns = input.event_time();
        let window = (
            input.pointer_window_x_by_index(0),
            input.pointer_window_y_by_index(0),
        );
        self.record_pointer_for(window_id, window);
        // Deduplicate a single Down event bubbling across child -> root, while
        // still advancing if an earlier Up/Cancel was consumed by a native child.
        if self.down_recorded
            && (event_time_ns == 0 || self.last_down_time_ns == Some(event_time_ns))
        {
            return;
        }
        self.down_recorded = true;
        self.last_down_time_ns = Some(event_time_ns);
        let _ = advance_clicks(
            &mut self.last_tap,
            event_time_ns,
            window,
            TOUCH_MULTI_TAP_MS,
            TOUCH_MULTI_TAP_DISTANCE_VP,
        );
    }
    pub(crate) fn clicks(&self) -> u32 {
        self.last_tap.as_ref().map_or(1, |tap| tap.clicks)
    }
    pub(crate) fn cancel(&mut self, root: &Node) -> Result<()> {
        self.primary = None;
        self.last_tap = None;
        self.last_mouse_press = None;
        self.handled_context_menu = None;
        self.down_recorded = false;
        self.last_down_time_ns = None;
        self.window_resize = None;
        self.pending_window_drag = None;
        self.mouse_up_targets.clear();
        if let Some(drag) = self.selection.take() {
            drag.unblock()?;
            if let Some(owner) = drag.owner.upgrade() {
                owner.selection_menu_show(false)?;
            }
        }
        if !root.platform.get().os.is_desktop() {
            root.clear_text_selection()?;
        }
        if let Some(back) = self.back.take()
            && back.active
            && let Some(screen) = back.screen.upgrade()
        {
            screen.finish_swipe(false)?;
            if let Some(owner) = screen.owner() {
                block(&owner, false)?;
            }
        }
        self.keyboard_tap = None;
        self.suppressed_presses.clear();
        if !self.fingers.is_empty() {
            let points = self
                .fingers
                .iter_mut()
                .map(|f| {
                    f.point.changed = true;
                    f.point.pressure = 0.0;
                    f.point.clone()
                })
                .collect();
            root.control.dispatch_event(
                nsh_core::ROOT_ID,
                EventKind::Touch,
                EventPayload::Touch(Box::new(TouchEvent {
                    phase: TouchPhase::Cancel,
                    timestamp: self.timestamp,
                    modifiers: self.modifiers,
                    points,
                    activated: None,
                })),
            );
            self.fingers.clear();
        }
        if let Some(node) = self.activated.take().and_then(|n| n.upgrade()) {
            block(&node, false)?;
        }
        Ok(())
    }

    pub(crate) fn input(&mut self, root: &Rc<Node>, input: &ArkUIInputEvent) -> Result<()> {
        self.record_down(root.key_window_id(), input);
        self.timestamp = input.event_time() as f64 / 1_000_000.0;
        let phase = match input.action {
            UIInputAction::Down => TouchPhase::Start,
            UIInputAction::Move => TouchPhase::Move,
            UIInputAction::Up => TouchPhase::End,
            UIInputAction::Cancel => TouchPhase::Cancel,
        };
        let count = input.pointer_count().min(32);
        if phase == TouchPhase::Start {
            self.modifiers = root.host.modifiers();
            self.keyboard_tap = None;
            if count == 1 {
                self.suppressed_presses.clear();
            }
        }
        let changed = if matches!(phase, TouchPhase::Start | TouchPhase::End) {
            input.get_changed_pointer_id().ok()
        } else {
            None
        };
        let phase = if matches!(phase, TouchPhase::Start | TouchPhase::End) && changed.is_none() {
            TouchPhase::Cancel
        } else {
            phase
        };
        let mut surface = root.clone();
        loop {
            if surface.kind == nsh_core::ViewKind::Modal {
                break;
            }
            let parent = surface.parent_node.borrow().upgrade();
            let Some(parent) = parent else { break };
            surface = parent;
        }
        let origin = surface.native.borrow().layout_position_in_window()?;
        if phase == TouchPhase::Start && count == 1 {
            self.primary = changed;
        }
        if self.window_resize.is_some() {
            if matches!(phase, TouchPhase::End | TouchPhase::Cancel) {
                self.primary = None;
            }
            return Ok(());
        }
        if self.selection_input(root, input, phase, changed, count)? {
            return Ok(());
        }
        if matches!(phase, TouchPhase::End | TouchPhase::Cancel) {
            self.primary = None;
        }
        if self.back_input(root, input, phase, changed, count)? {
            return Ok(());
        }
        // ArkUI returns coordinates in the target node's configured metric unit.
        // NSH nodes use vp; native layout-position helpers return physical pixels.
        for i in 0..count {
            let id = input.pointer_id(i) as u32;
            let local = (input.pointer_x_by_index(i), input.pointer_y_by_index(i));
            let window = (
                input.pointer_window_x_by_index(i),
                input.pointer_window_y_by_index(i),
            );
            let page = (
                window.0 - origin.x as f32 / root.scale.get(),
                window.1 - origin.y as f32 / root.scale.get(),
            );
            if phase == TouchPhase::Start && changed == Some(id) {
                let Some(node) = hit(root, local, &nsh_core::Matrix::IDENTITY)? else {
                    continue;
                };
                let consumes = count == 1 && self.start_keyboard_tap(&surface, &node, id, page);
                let mut parent = Some(node.clone());
                let mut path = Vec::new();
                let mut pans = Vec::new();
                while let Some(node) = parent {
                    if consumes {
                        self.suppressed_presses.push(node.id);
                    }
                    if node.interaction.mask().contains(EventKind::Touch) {
                        path.push(node.id);
                    }
                    if node.interaction.pan_activation().is_some() {
                        pans.push((Rc::downgrade(&node), false));
                    }
                    parent = node.parent_node.borrow().upgrade();
                }
                if !path.is_empty() && !consumes {
                    self.fingers.push(Finger {
                        point: TouchPoint {
                            id,
                            target: node.id,
                            path,
                            page: nsh_core::Point::new(
                                nsh_core::Points(page.0),
                                nsh_core::Points(page.1),
                            ),
                            location: nsh_core::Point::default(),
                            changed: true,
                            pressure: 0.5,
                            tilt_x: 0.0,
                            tilt_y: 0.0,
                            pointer_type: match input.tool_type {
                                UIInputToolType::Mouse | UIInputToolType::Touchpad => {
                                    PointerType::Mouse
                                }
                                UIInputToolType::Pen => PointerType::Pen,
                                _ => PointerType::Touch,
                            },
                        },
                        node: Rc::downgrade(&node),
                        start: page,
                        pans,
                    });
                }
            }
            if self.keyboard_tap.as_ref().is_some_and(|tap| {
                tap.finger == id
                    && ((page.0 - tap.start.0).abs() > 10.0 || (page.1 - tap.start.1).abs() > 10.0)
            }) {
                self.keyboard_tap = None;
            }
            if let Some(finger) = self.fingers.iter_mut().find(|f| f.point.id == id) {
                let down_pressure = TouchPoint::default_pressure(
                    phase,
                    changed == Some(id) || phase == TouchPhase::Cancel,
                );
                let measured = input.pointer_pressure(i);
                finger.point.pressure =
                    if down_pressure > 0.0 && measured.is_finite() && measured > 0.0 {
                        measured.min(1.0)
                    } else {
                        down_pressure
                    };
                if finger.point.pointer_type == PointerType::Pen {
                    finger.point.tilt_x = input.pointer_tilt_x(i).clamp(-90.0, 90.0);
                    finger.point.tilt_y = input.pointer_tilt_y(i).clamp(-90.0, 90.0);
                }
                finger.point.page =
                    nsh_core::Point::new(nsh_core::Points(page.0), nsh_core::Points(page.1));
                if let Some(node) = finger.node.upgrade()
                    && let Some(matrix) = node.input_in_root(root)?
                    && let Some((x, y)) = crate::render::geometry::local_point(&matrix.0, local)
                {
                    finger.point.location =
                        nsh_core::Point::new(nsh_core::Points(x), nsh_core::Points(y));
                }
            }
        }
        if phase == TouchPhase::Cancel {
            self.keyboard_tap = None;
        }
        if phase == TouchPhase::End
            && self
                .keyboard_tap
                .as_ref()
                .is_some_and(|tap| Some(tap.finger) == changed)
            && let Some(tap) = self.keyboard_tap.take()
            && let Some(node) = tap.input.upgrade()
            && let Some(input) = node.input.borrow().as_ref()
        {
            input.focus(false)?;
        }
        let mut activated = None;
        for finger in &mut self.fingers {
            finger.point.changed = matches!(phase, TouchPhase::Move | TouchPhase::Cancel)
                || changed == Some(finger.point.id);
            if phase == TouchPhase::Move && self.activated.is_none() {
                for (node, failed) in &mut finger.pans {
                    if *failed {
                        continue;
                    }
                    let Some(node) = node.upgrade() else {
                        *failed = true;
                        continue;
                    };
                    match node.interaction.pan_activation().and_then(|p| {
                        p.decide(
                            nsh_core::Points(finger.point.page.x.0 - finger.start.0),
                            nsh_core::Points(finger.point.page.y.0 - finger.start.1),
                        )
                    }) {
                        Some(false) => *failed = true,
                        Some(true) => {
                            block(&node, true)?;
                            let mut ancestor = Some(node.clone());
                            while let Some(current) = ancestor {
                                self.suppressed_presses.push(current.id);
                                ancestor = current.parent_node.borrow().upgrade();
                            }
                            self.activated = Some(Rc::downgrade(&node));
                            activated = Some(node.id);
                            break;
                        }
                        None => {}
                    }
                }
            }
        }
        if !self.fingers.is_empty() {
            root.control.dispatch_event(
                nsh_core::ROOT_ID,
                EventKind::Touch,
                EventPayload::Touch(Box::new(TouchEvent {
                    phase,
                    timestamp: self.timestamp,
                    modifiers: self.modifiers,
                    points: self.fingers.iter().map(|f| f.point.clone()).collect(),
                    activated,
                })),
            );
        }
        if phase == TouchPhase::End {
            self.fingers.retain(|f| !f.point.changed);
        }
        if phase == TouchPhase::Cancel {
            self.fingers.clear()
        }
        if self.fingers.is_empty()
            && let Some(node) = self.activated.take().and_then(|n| n.upgrade())
        {
            block(&node, false)?;
        }
        Ok(())
    }
    pub(crate) fn press_allowed(&self, id: nsh_core::NodeId) -> bool {
        !self.suppressed_presses.contains(&id)
    }
    pub(crate) fn start_window_resize(
        &mut self,
        modal: &Rc<crate::navigation::modal::Modal>,
        edge: nsh_core::desktop::ResizeEdge,
        display_point: (f32, f32),
    ) {
        let Some((start_origin, start_size)) = modal.desktop_resize_anchor() else {
            return;
        };
        self.pending_window_drag = None;
        self.window_resize = Some(WindowResize {
            target: WindowResizeTarget::SubWindow(Rc::downgrade(modal)),
            edge,
            start_display: display_point,
            start_origin,
            start_size,
            last: display_point,
        });
    }
    pub(crate) fn start_main_window_resize(
        &mut self,
        edge: nsh_core::desktop::ResizeEdge,
        display_point: (f32, f32),
        start_origin: (f32, f32),
        start_size: (f32, f32),
    ) {
        self.pending_window_drag = None;
        self.window_resize = Some(WindowResize {
            target: WindowResizeTarget::MainWindow,
            edge,
            start_display: display_point,
            start_origin,
            start_size,
            last: display_point,
        });
    }
    pub(crate) fn window_resizing(&self) -> bool {
        self.window_resize.is_some()
    }
    pub(crate) fn move_window_resize(
        &mut self,
        owner: &Rc<Node>,
        display_point: (f32, f32),
    ) -> Result<bool> {
        let Some(mut resize) = self.window_resize.take() else {
            return Ok(false);
        };
        let step_dx = display_point.0 - resize.last.0;
        let step_dy = display_point.1 - resize.last.1;
        resize.last = display_point;
        match &resize.target {
            WindowResizeTarget::SubWindow(weak) => {
                let Some(modal) = weak.upgrade() else {
                    return Ok(false);
                };
                if step_dx.abs() > 0.01 || step_dy.abs() > 0.01 {
                    let total_dx = display_point.0 - resize.start_display.0;
                    let total_dy = display_point.1 - resize.start_display.1;
                    modal.desktop_resize_from(
                        resize.edge,
                        resize.start_origin,
                        resize.start_size,
                        total_dx,
                        total_dy,
                    )?;
                }
            }
            WindowResizeTarget::MainWindow => {
                if step_dx.abs() > 0.01 || step_dy.abs() > 0.01 {
                    let total_dx = display_point.0 - resize.start_display.0;
                    let total_dy = display_point.1 - resize.start_display.1;
                    let main = owner.main_window.borrow();
                    let min_w = main.min_size.map_or(1.0, |s| s.width.0.max(1.0));
                    let min_h = main.min_size.map_or(1.0, |s| s.height.0.max(1.0));
                    let max_w = main.max_size.map(|s| s.width.0);
                    let max_h = main.max_size.map(|s| s.height.0);
                    drop(main);
                    let (next_origin, next_size) = crate::services::desktop::apply_resize_delta(
                        resize.edge,
                        resize.start_origin,
                        resize.start_size,
                        total_dx,
                        total_dy,
                        (min_w, min_h),
                        (max_w, max_h),
                    );
                    let frame = nsh_core::desktop::WindowFrame {
                        x: Some(nsh_core::Points(next_origin.0)),
                        y: Some(nsh_core::Points(next_origin.1)),
                        width: Some(nsh_core::Points(next_size.0)),
                        height: Some(nsh_core::Points(next_size.1)),
                        ..Default::default()
                    };
                    owner
                        .host
                        .desktop_command(&crate::services::desktop::wire_request_for(
                            None,
                            &nsh_core::desktop::WindowRequest::SetBounds(frame),
                        ))?;
                }
            }
        }
        self.window_resize = Some(resize);
        Ok(true)
    }
    pub(crate) fn end_window_resize(&mut self) {
        self.window_resize = None;
    }
    pub(crate) fn start_pending_window_drag(
        &mut self,
        start_window: (f32, f32),
        modal: Option<&Rc<crate::navigation::modal::Modal>>,
        is_main: bool,
    ) {
        self.pending_window_drag = Some(PendingWindowDrag {
            start_window,
            modal: modal.map(Rc::downgrade),
            is_main,
        });
    }
    pub(crate) fn move_pending_window_drag(
        &mut self,
        owner: &Rc<Node>,
        window_point: (f32, f32),
    ) -> Result<bool> {
        let Some(pending) = self.pending_window_drag.as_ref() else {
            return Ok(false);
        };
        if (window_point.0 - pending.start_window.0).hypot(window_point.1 - pending.start_window.1)
            < WINDOW_DRAG_THRESHOLD_VP
        {
            return Ok(false);
        }
        let pending = self.pending_window_drag.take().unwrap();
        if let Some(modal) = pending.modal.and_then(|m| m.upgrade()) {
            modal.desktop_start_moving()?;
            return Ok(true);
        }
        if pending.is_main {
            owner
                .host
                .desktop_command(&crate::services::desktop::WireCommand::StartMoving {
                    window_id: None,
                })?;
            return Ok(true);
        }
        Ok(false)
    }
    pub(crate) fn clear_pending_window_drag(&mut self) {
        self.pending_window_drag = None;
    }
    pub(crate) fn capture_mouse_up(&mut self, targets: Vec<Weak<Node>>) {
        self.mouse_up_targets = targets;
    }
    pub(crate) fn take_mouse_up_targets(&mut self) -> Vec<Weak<Node>> {
        std::mem::take(&mut self.mouse_up_targets)
    }
    pub(crate) fn selection_started(&mut self, owner: &Rc<Node>, point: (f32, f32)) -> Result<()> {
        let Some(finger) = self.primary else {
            return Ok(());
        };
        let blocked = self.claim_selection(owner)?;
        self.selection = Some(SelectionDrag {
            owner: Rc::downgrade(owner),
            finger,
            start: point,
            moved: false,
            desktop: false,
            pointer: None,
            frame_time: owner.animator.now(),
            blocked,
        });
        Ok(())
    }
    pub(crate) fn selection_dragging(&self, id: nsh_core::NodeId) -> bool {
        self.selection.as_ref().is_some_and(|drag| {
            drag.moved && drag.owner.upgrade().is_some_and(|owner| owner.id == id)
        })
    }
    pub(crate) fn selection_held(&self) -> bool {
        self.selection.is_some()
    }
    fn claim_selection(
        &mut self,
        owner: &Rc<Node>,
    ) -> Result<Vec<Weak<crate::scroll::controller::Scroller>>> {
        self.keyboard_tap = None;
        self.back = None;
        if !self.fingers.is_empty() {
            owner.control.dispatch_event(
                nsh_core::ROOT_ID,
                EventKind::Touch,
                EventPayload::Touch(Box::new(TouchEvent {
                    phase: TouchPhase::Cancel,
                    timestamp: self.timestamp,
                    modifiers: self.modifiers,
                    activated: None,
                    points: self
                        .fingers
                        .drain(..)
                        .map(|mut f| {
                            f.point.changed = true;
                            f.point.pressure = 0.0;
                            f.point
                        })
                        .collect(),
                })),
            );
        }
        if let Some(old) = self.activated.take().and_then(|n| n.upgrade()) {
            block(&old, false)?;
        }
        let mut node = Some(owner.clone());
        let mut blocked = Vec::new();
        while let Some(current) = node {
            self.suppressed_presses.push(current.id);
            if let Some(scroll) = current.scroll.borrow().as_ref() {
                blocked.push(Rc::downgrade(scroll));
            }
            node = current.parent_node.borrow().upgrade();
        }
        if let Err(error) = block(owner, true) {
            for scroll in blocked.iter().filter_map(Weak::upgrade) {
                let _ = scroll.block(false);
            }
            return Err(error);
        }
        Ok(blocked)
    }
    fn selection_input(
        &mut self,
        root: &Rc<Node>,
        input: &ArkUIInputEvent,
        phase: TouchPhase,
        changed: Option<u32>,
        count: u32,
    ) -> Result<bool> {
        let is_mouse = matches!(
            input.tool_type,
            UIInputToolType::Mouse | UIInputToolType::Touchpad
        );
        if phase == TouchPhase::Start && count == 1 {
            // A popup can consume the preceding contact's release.
            if let Some(old) = self.selection.take() {
                old.unblock()?;
            }
            let local = (input.pointer_x_by_index(0), input.pointer_y_by_index(0));
            if is_mouse {
                if let Some(target) = hit(root, local, &nsh_core::Matrix::IDENTITY)?
                    && target.selectable()
                    && let Some(point) = target
                        .input_in_root(root)?
                        .and_then(|m| crate::render::geometry::local_point(&m.0, local))
                {
                    let clicks = self.clicks();
                    let extend = self.modifiers.shift;
                    if target.selection_press(point.0, point.1, clicks, extend)? {
                        let owner = target
                            .selected_text
                            .borrow()
                            .as_ref()
                            .and_then(Weak::upgrade);
                        let owner = owner.unwrap_or(target);
                        let immediate = clicks >= 2 || extend;
                        let blocked = if immediate {
                            self.claim_selection(&owner)?
                        } else {
                            Vec::new()
                        };
                        self.selection = Some(SelectionDrag {
                            owner: Rc::downgrade(&owner),
                            finger: changed.unwrap_or(0),
                            start: point,
                            moved: immediate,
                            desktop: true,
                            pointer: Some(local),
                            frame_time: owner.animator.now(),
                            blocked,
                        });
                        if immediate {
                            owner.animator.track_selection(root);
                            return Ok(true);
                        }
                        // Plain single click passes through so links/buttons still fire onPress;
                        // moving past the drag threshold claims the gesture below.
                        return Ok(false);
                    }
                }
                root.clear_text_selection()?;
                return Ok(false);
            }
            let selected = root.selected_text.borrow().as_ref().and_then(Weak::upgrade);
            if let Some(owner) = selected {
                let point = owner
                    .input_in_root(root)?
                    .and_then(|m| crate::render::geometry::local_point(&m.0, local));
                if owner.visible_in_tree()
                    && let Some(point) = point
                {
                    if owner.selection_handle(point)? {
                        let blocked = self.claim_selection(&owner)?;
                        owner.selection_menu_show(false)?;
                        self.selection = Some(SelectionDrag {
                            owner: Rc::downgrade(&owner),
                            finger: changed.unwrap_or(0),
                            start: point,
                            moved: true,
                            desktop: false,
                            pointer: Some(local),
                            frame_time: owner.animator.now(),
                            blocked,
                        });
                        owner.animator.track_selection(root);
                        return Ok(true);
                    }
                    if let Some(hit) = hit(root, local, &nsh_core::Matrix::IDENTITY)?
                        && hit.selection.range.get().is_some()
                    {
                        self.suppressed_presses.push(hit.id);
                        owner.selection_menu_show(!owner.selection.menu.get())?;
                        return Ok(true);
                    }
                }
                root.clear_text_selection()?;
            }
        }
        let Some(mut drag) = self.selection.take() else {
            return Ok(false);
        };
        let Some(owner) = drag.owner.upgrade() else {
            drag.unblock()?;
            return Ok(false);
        };
        let index = (0..count).find(|&i| input.pointer_id(i) as u32 == drag.finger);
        let cancelled = phase == TouchPhase::Cancel || count > 1 || !owner.visible_in_tree();
        if !cancelled && let Some(index) = index {
            let local = (
                input.pointer_x_by_index(index),
                input.pointer_y_by_index(index),
            );
            drag.pointer = Some(local);
            if let Some(point) = owner
                .input_in_root(root)?
                .and_then(|m| crate::render::geometry::local_point(&m.0, local))
            {
                let threshold = if drag.desktop { 4.0 } else { 8.0 };
                if !drag.moved
                    && phase == TouchPhase::Move
                    && (point.0 - drag.start.0).hypot(point.1 - drag.start.1) > threshold
                {
                    drag.moved = true;
                    if drag.desktop && drag.blocked.is_empty() {
                        drag.blocked = self.claim_selection(&owner)?;
                    }
                    owner.selection_menu_show(false)?;
                }
                if drag.moved && phase == TouchPhase::Move {
                    owner.selection_extend(point)?;
                    owner.animator.track_selection(root);
                }
            }
        }
        let consumed = !drag.desktop || drag.moved;
        if cancelled || phase == TouchPhase::End || index.is_none() {
            self.primary = None;
            drag.unblock()?;
            if cancelled {
                owner.selection_clear()?;
            } else if drag.desktop {
                if !owner.has_selection() {
                    owner.selection_clear()?;
                }
            } else {
                owner.selection_menu_show(true)?;
            }
        } else {
            self.selection = Some(drag);
        }
        Ok(consumed)
    }
    pub(crate) fn selection_tick(&mut self, root: &Rc<Node>, now: f64) -> Result<bool> {
        let Some(drag) = self.selection.as_mut().filter(|drag| drag.moved) else {
            return Ok(false);
        };
        let Some(owner) = drag.owner.upgrade() else {
            drag.unblock()?;
            self.selection = None;
            return Ok(false);
        };
        if !owner.visible_in_tree() || !owner.has_selection() {
            drag.unblock()?;
            self.selection = None;
            return Ok(false);
        }
        let elapsed = now - drag.frame_time;
        drag.frame_time = now;
        let Some(pointer) = drag.pointer else {
            return Ok(false);
        };
        let mut parent = owner.parent_node.borrow().upgrade();
        let mut scrolling = false;
        while let Some(node) = parent {
            if let Some(scroll) = node.scroll.borrow().as_ref()
                && let Some(point) = node
                    .input_in_root(root)?
                    .and_then(|matrix| crate::render::geometry::local_point(&matrix.0, pointer))
                && scroll.selection_scroll(point, elapsed)?
            {
                scrolling = true;
                break;
            }
            if node.id == root.id {
                break;
            }
            parent = node.parent_node.borrow().upgrade();
        }
        if let Some(point) = owner
            .input_in_root(root)?
            .and_then(|matrix| crate::render::geometry::local_point(&matrix.0, pointer))
        {
            owner.selection_extend(point)?;
        }
        Ok(scrolling)
    }
    fn back_input(
        &mut self,
        root: &Rc<Node>,
        input: &ArkUIInputEvent,
        phase: TouchPhase,
        changed: Option<u32>,
        count: u32,
    ) -> Result<bool> {
        if root.platform.get().os.is_desktop()
            || matches!(
                input.tool_type,
                UIInputToolType::Mouse | UIInputToolType::Touchpad
            )
        {
            return Ok(false);
        }
        if phase == TouchPhase::Start
            && count == 1
            && !root.interaction.mask().contains(EventKind::BackPress)
        {
            self.suppressed_presses.clear();
            let local = (input.pointer_x_by_index(0), input.pointer_y_by_index(0));
            let target = hit(root, local, &nsh_core::Matrix::IDENTITY)?;
            let mut parent = target.clone();
            while let Some(node) = parent {
                if let Some(screen) = node.screen.borrow().as_ref()
                    && let Some(matrix) = node.input_in_root(root)?
                    && let Some((x, _)) = crate::render::geometry::local_point(&matrix.0, local)
                    && screen.can_swipe(x)
                {
                    self.back = Some(BackSwipe {
                        screen: Rc::downgrade(screen),
                        target: target.as_ref().map_or_else(Weak::new, Rc::downgrade),
                        finger: input.pointer_id(0) as u32,
                        start: (
                            input.pointer_window_x_by_index(0),
                            input.pointer_window_y_by_index(0),
                        ),
                        local_x: x,
                        sign: if node.platform.get().rtl { -1.0 } else { 1.0 },
                        width: node.frame.get().width.max(1.0),
                        active: false,
                        last: (0.0, self.timestamp),
                        velocity: 0.0,
                    });
                    break;
                }
                parent = node.parent_node.borrow().upgrade();
            }
        }
        let Some(mut back) = self.back.take() else {
            return Ok(false);
        };
        let Some(screen) = back.screen.upgrade() else {
            return Ok(false);
        };
        let index = (0..count).find(|&index| input.pointer_id(index) as u32 == back.finger);
        let cancelled = phase == TouchPhase::Cancel
            || count != 1
            || index.is_none()
            || !screen.can_swipe(back.local_x);
        if cancelled {
            if back.active {
                screen.finish_swipe(false)?;
                if let Some(owner) = screen.owner() {
                    block(&owner, false)?;
                }
            }
            return Ok(back.active);
        }
        let index = index.unwrap();
        let dx = (input.pointer_window_x_by_index(index) - back.start.0) * back.sign;
        let dy = input.pointer_window_y_by_index(index) - back.start.1;
        if !back.active && phase == TouchPhase::Move {
            if dy.abs() > 8.0 && dy.abs() >= dx.abs() || dx < -8.0 {
                return Ok(false);
            }
            if dx > 8.0 && dx > dy.abs() {
                back.active = true;
                self.keyboard_tap = None;
                // Cancel the JS responder before claiming the native edge gesture.
                if !self.fingers.is_empty() {
                    root.control.dispatch_event(
                        nsh_core::ROOT_ID,
                        EventKind::Touch,
                        EventPayload::Touch(Box::new(TouchEvent {
                            phase: TouchPhase::Cancel,
                            timestamp: self.timestamp,
                            modifiers: self.modifiers,
                            activated: None,
                            points: self
                                .fingers
                                .iter()
                                .map(|finger| {
                                    let mut point = finger.point.clone();
                                    point.changed = true;
                                    point.pressure = 0.0;
                                    point
                                })
                                .collect(),
                        })),
                    );
                    self.fingers.clear();
                }
                let mut target = back.target.upgrade();
                while let Some(node) = target {
                    self.suppressed_presses.push(node.id);
                    target = node.parent_node.borrow().upgrade();
                }
                if let Some(owner) = screen.owner() {
                    block(&owner, true)?;
                }
            }
        }
        if back.active {
            let elapsed = self.timestamp - back.last.1;
            if elapsed > 0.0 && dx != back.last.0 {
                back.velocity = (dx - back.last.0) * 1000.0 / elapsed as f32;
                back.last = (dx, self.timestamp);
            }
            screen.swipe(dx)?;
            if phase == TouchPhase::End && changed == Some(back.finger) {
                let recent_velocity = if self.timestamp - back.last.1 <= 100.0 {
                    back.velocity
                } else {
                    0.0
                };
                screen.finish_swipe(crate::navigation::motion::complete_swipe(
                    dx,
                    back.width,
                    recent_velocity,
                ))?;
                if let Some(owner) = screen.owner() {
                    block(&owner, false)?;
                }
                return Ok(true);
            }
        }
        let consumed = back.active;
        if phase != TouchPhase::End {
            self.back = Some(back);
        }
        Ok(consumed)
    }
    fn start_keyboard_tap(
        &mut self,
        root: &Rc<Node>,
        target: &Rc<Node>,
        finger: u32,
        start: (f32, f32),
    ) -> bool {
        use nsh_core::KeyboardTaps;
        let mut parent = Some(target.clone());
        let mut handled = false;
        let mut policy = None;
        while let Some(node) = parent {
            if node.kind == nsh_core::ViewKind::Input {
                return false;
            }
            handled |= [EventKind::Press, EventKind::LongPress, EventKind::Touch]
                .into_iter()
                .any(|event| node.interaction.mask().contains(event) || node.text_listens(event));
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                policy = Some(scroll.keyboard_taps());
                break;
            }
            parent = node.parent_node.borrow().upgrade();
        }
        let dismiss = match policy {
            Some(KeyboardTaps::Never) => true,
            Some(KeyboardTaps::Handled) => !handled,
            _ => false,
        };
        if !dismiss {
            return false;
        }
        let focused = root
            .dispatcher
            .upgrade()
            .and_then(|route| route.keyboard.lock().unwrap().input);
        let Some(input) = focused
            .and_then(|id| find(root, id))
            .filter(|node| node.input.borrow().is_some())
        else {
            return false;
        };
        self.keyboard_tap = Some(KeyboardTap {
            finger,
            start,
            input: Rc::downgrade(&input),
        });
        policy == Some(KeyboardTaps::Never) && root.platform.get().keyboard_height.0 > 0.0
    }
}
fn find(root: &Rc<Node>, id: nsh_core::NodeId) -> Option<Rc<Node>> {
    if root.id == id {
        return Some(root.clone());
    }
    root.children
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .find_map(|child| find(&child, id))
}
fn block(node: &Rc<Node>, blocked: bool) -> Result<()> {
    let mut node = Some(node.clone());
    while let Some(current) = node {
        if let Some(scroll) = current.scroll.borrow().as_ref() {
            scroll.block(blocked)?;
        }
        node = current.parent_node.borrow().upgrade();
    }
    Ok(())
}
pub(crate) fn hit_app(root: &Rc<Node>, point: (f32, f32)) -> Result<bool> {
    Ok(hit(root, point, &nsh_core::Matrix::IDENTITY)?
        .is_some_and(|node| node.id != nsh_core::ROOT_ID))
}
pub(crate) fn hit(
    root: &Rc<Node>,
    point: (f32, f32),
    world: &nsh_core::Matrix,
) -> Result<Option<Rc<Node>>> {
    if !root.input_visible() {
        return Ok(None);
    }
    let pointer = root.interaction.pointer_events();
    if pointer == PointerEvents::None {
        return Ok(None);
    }
    let Some((x, y)) = crate::render::geometry::local_point(&world.0, point) else {
        return Ok(None);
    };
    let frame = root.frame.get();
    let area = nsh_core::input::hit_area(
        nsh_core::geometry::point(nsh_core::Points(x), nsh_core::Points(y)),
        nsh_core::geometry::size(
            nsh_core::Points(frame.width),
            nsh_core::Points(frame.height),
        ),
        root.interaction.hit_slop(),
        root.overflow.get().map(Into::into),
        root.clips_children(),
        nsh_core::input::laid_out(root.kind, root.id),
    );
    if pointer != PointerEvents::BoxOnly && area.children {
        let plane = root.input_children_transform()?;
        // Sticky cells are raised above their siblings in the native tree.
        for priority in [true, false] {
            for child in root
                .children
                .borrow()
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .filter(|child| {
                    child.kind != nsh_core::ViewKind::Modal
                        || child
                            .modal
                            .borrow()
                            .as_ref()
                            .is_some_and(|modal| modal.is_main_window_host())
                })
            {
                if child.input_priority() == priority {
                    let matrix = child.input_in_plane(&plane)?.mul(world);
                    if let Some(node) = hit(&child, point, &matrix)? {
                        return Ok(Some(node));
                    }
                }
            }
        }
    }
    Ok((area.in_slop && pointer != PointerEvents::BoxNone).then(|| root.clone()))
}
// Each native callback takes one short-lived borrow; the engine owns responder negotiation.
pub(crate) type State = RefCell<Touches>;
