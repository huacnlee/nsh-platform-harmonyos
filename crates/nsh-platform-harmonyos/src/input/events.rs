//! Native interaction state and view metadata. Input feedback is answered on
//! the UI thread from mounted props; notifications to the engine are asynchronous.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use nsh_core::{
    EventKind, EventMask, EventPayload, GestureValues, PayloadValue, PointerEvents, ViewProps,
    tailwind::state,
};
use ohos_arkui_binding::{
    arkui_input_binding::{
        ArkUIInputEvent, ModifierKey, UIAxisEventAction, UIInputAction, UIInputSourceType,
        UIInputToolType, UIMouseEventAction, UIMouseEventButton,
    },
    component::attribute::{ArkUICommonAttribute, ArkUIEvent, ArkUIGesture},
    gesture::{gesture_data::GestureData, inner_gesture::Gesture},
    types::{
        attribute::ArkUINodeAttributeType as A,
        drag::{DragResult, DropOperation},
        event::NodeEventType as E,
        gesture_direction::GestureDirection,
        gesture_event::GestureEventAction,
        view::{AccessibilityMode, HitTestMode},
    },
};

use crate::render::node::{Node, Result, callback};

pub(crate) struct Interaction {
    props: RefCell<ViewProps>,
    events: Cell<EventMask>,
    tracked: Cell<u16>,
    states: Cell<u16>,
    inherited_block: Cell<bool>,
    applied_hit_test: Cell<i32>,
    applied_internal_hit_test: Cell<i32>,
    click_registered: Cell<bool>,
    touch_registered: Cell<bool>,
    expanded_hit_region: Cell<bool>,
    hover_registered: Cell<bool>,
    key_focusable: Cell<bool>,
    focus_wrapped: Cell<bool>,
    focus_command_gen: Cell<u32>,
    focus_command_focus: Cell<bool>,
    mouse_registered: Cell<bool>,
    axis_registered: Cell<bool>,
    drag_registered: Cell<bool>,
    pinch_scale: Cell<f32>,
    opacity: Cell<Option<f32>>,
    applied_opacity: Cell<Option<f32>>,
    label: RefCell<Option<Arc<str>>>,
    applied_label: RefCell<Option<Arc<str>>>,
    long_press: RefCell<Option<(u32, Gesture)>>,
    long_fired: Cell<bool>,
    gestures: RefCell<Vec<Gesture>>,
    gesture_values: Cell<GestureValues>,
    gesture_start: Cell<[f32; 4]>,
}

impl Default for Interaction {
    fn default() -> Self {
        let default_hit: i32 = HitTestMode::Default.into();
        Self {
            props: RefCell::default(),
            events: Cell::default(),
            tracked: Cell::default(),
            states: Cell::default(),
            inherited_block: Cell::new(false),
            applied_hit_test: Cell::new(default_hit),
            applied_internal_hit_test: Cell::new(default_hit),
            click_registered: Cell::new(false),
            touch_registered: Cell::new(false),
            expanded_hit_region: Cell::new(false),
            hover_registered: Cell::new(false),
            key_focusable: Cell::new(false),
            focus_wrapped: Cell::new(false),
            focus_command_gen: Cell::new(0),
            focus_command_focus: Cell::new(false),
            mouse_registered: Cell::new(false),
            axis_registered: Cell::new(false),
            drag_registered: Cell::new(false),
            pinch_scale: Cell::new(1.0),
            opacity: Cell::new(Some(1.0)),
            applied_opacity: Cell::new(Some(1.0)),
            label: RefCell::default(),
            applied_label: RefCell::default(),
            long_press: RefCell::default(),
            long_fired: Cell::new(false),
            gestures: RefCell::default(),
            gesture_values: Cell::default(),
            gesture_start: Cell::default(),
        }
    }
}

impl Interaction {
    pub(crate) fn selectable_view(&self) -> bool {
        self.props.borrow().selectable_view
    }
    pub(crate) fn selection_menu(&self) -> Arc<[nsh_core::desktop::MenuItem]> {
        self.props.borrow().selection_menu.clone()
    }
    pub(crate) fn window_drag(&self) -> Option<bool> {
        self.props.borrow().window_drag
    }
    pub(crate) fn window_control(&self) -> Option<nsh_core::desktop::WindowControl> {
        self.props.borrow().window_control
    }
    pub(crate) fn window_resize(&self) -> Option<nsh_core::desktop::ResizeEdge> {
        self.props.borrow().window_resize
    }
    pub(crate) fn continues_text(&self) -> bool {
        self.props.borrow().continues_text
    }
    pub(crate) fn test_id(&self) -> Option<String> {
        self.props.borrow().test_id.as_deref().map(str::to_owned)
    }
    pub(crate) fn hit_region(&self, owner: &Node) -> Result<()> {
        let frame = owner.frame.get();
        let has_area = frame.width > 0.0 && frame.height > 0.0;
        let slop = if has_area {
            self.props.borrow().hit_slop
        } else {
            nsh_core::EdgeInsets::default()
        };
        let eff_w = if frame.width > 0.0 {
            frame.width
        } else {
            1.0 / owner.scale.get()
        };
        let eff_h = if frame.height > 0.0 {
            frame.height
        } else {
            1.0 / owner.scale.get()
        };
        // ArkUI visits the children of an unclipped node even outside its own
        // response region. Extending the parent to their bounding box lets a
        // decorative sibling intercept clicks in empty space between children.
        if slop == nsh_core::EdgeInsets::default() {
            if self.expanded_hit_region.replace(false) {
                owner.native.borrow().reset_attribute(A::ResponseRegion)?;
            }
        } else {
            self.expanded_hit_region.set(true);
            owner.native.borrow().set_attribute(
                A::ResponseRegion,
                [
                    -slop.left.0,
                    -slop.top.0,
                    (eff_w + slop.left.0 + slop.right.0) / eff_w * 100.0,
                    (eff_h + slop.top.0 + slop.bottom.0) / eff_h * 100.0,
                ]
                .into(),
            )?;
        }
        Ok(())
    }
    pub(crate) fn hit_slop(&self) -> nsh_core::EdgeInsets {
        self.props.borrow().hit_slop
    }
    pub(crate) fn pointer_events(&self) -> PointerEvents {
        self.props.borrow().pointer_events
    }
    pub(crate) fn pan_activation(&self) -> Option<nsh_core::PanActivation> {
        self.props.borrow().pan_activation
    }
    pub(crate) fn key_interest(
        &self,
        kind: EventKind,
        stroke: Option<&nsh_core::keymap::Keystroke>,
    ) -> (bool, bool) {
        let props = self.props.borrow();
        if props.accessibility.state.disabled {
            return (false, false);
        }
        (
            self.mask().contains(kind),
            props.key_capture.captures_key(stroke),
        )
    }
    pub(crate) fn needs_focus_wrapper(&self, owner: &Node) -> bool {
        if matches!(
            owner.kind,
            nsh_core::ViewKind::Input
                | nsh_core::ViewKind::Native
                | nsh_core::ViewKind::WebView
                | nsh_core::ViewKind::Modal
        ) || owner.id == nsh_core::ROOT_ID
        {
            return false;
        }
        let props = self.props.borrow();
        !props.accessibility.state.disabled
            && (props.focusable == Some(true) || self.focus_wrapped.get())
    }
    pub(crate) fn wrapper_key_focusable(&self, owner: &Node) -> bool {
        self.needs_focus_wrapper(owner)
    }
    pub(crate) fn focus_command(&self) -> (u32, bool) {
        (self.focus_command_gen.get(), self.focus_command_focus.get())
    }
    pub(crate) fn command_focus(&self, owner: &Rc<Node>, focused: bool) -> Result<()> {
        if matches!(
            owner.kind,
            nsh_core::ViewKind::Input
                | nsh_core::ViewKind::Native
                | nsh_core::ViewKind::WebView
                | nsh_core::ViewKind::Modal
        ) || owner.id == nsh_core::ROOT_ID
        {
            return Ok(());
        }
        if focused {
            let props = self.props.borrow();
            if props.focusable == Some(false) || props.accessibility.state.disabled {
                return Ok(());
            }
            drop(props);
            self.focus_wrapped.set(true);
        }
        if !focused && !self.needs_focus_wrapper(owner) && owner.wrapper.borrow().is_none() {
            return Ok(());
        }
        let next = self.focus_command_gen.get().wrapping_add(1).max(1);
        self.focus_command_gen.set(next);
        self.focus_command_focus.set(focused);
        owner.ensure_wrapper()?;
        owner.update_wrapper()
    }
    fn sync_key_focus(&self, owner: &Rc<Node>) -> Result<()> {
        if matches!(
            owner.kind,
            nsh_core::ViewKind::Input
                | nsh_core::ViewKind::Native
                | nsh_core::ViewKind::WebView
                | nsh_core::ViewKind::Modal
        ) || owner.id == nsh_core::ROOT_ID
        {
            return Ok(());
        }
        let focusable = self
            .props
            .borrow()
            .focus_stop(self.mask().contains(EventKind::Press));
        let changed = self.key_focusable.replace(focusable) != focusable;
        if self.needs_focus_wrapper(owner) {
            owner.ensure_wrapper()?;
        } else if changed {
            owner.update_wrapper()?;
        }
        Ok(())
    }
    pub(crate) fn mask(&self) -> EventMask {
        self.events.get()
    }
    pub(crate) fn events(&self, owner: &Rc<Node>, events: EventMask) -> Result<()> {
        self.events.set(events);
        self.sync_key_focus(owner)?;
        if let Some(scroll) = owner.scroll.borrow().as_ref() {
            scroll.refreshable(events.contains(EventKind::Refresh))?;
        }
        let blocked = self.inherited_block.get()
            || matches!(
                self.props.borrow().pointer_events,
                PointerEvents::None | PointerEvents::BoxNone
            );
        let click = !blocked
            && (events.contains(EventKind::Press)
                || owner.text_listens(EventKind::Press)
                || self.props.borrow().window_control.is_some());
        if self.click_registered.replace(click) != click {
            if click {
                let weak = Rc::downgrade(owner);
                owner
                    .native
                    .borrow_mut()
                    .on_event(E::OnClick, move |event| {
                        if let Some(owner) = weak.upgrade()
                            && !owner.interaction.long_fired.replace(false)
                        {
                            owner
                                .control
                                .set_input_modality(nsh_core::platform::InputModality::Pointer);
                            if let Some(control) = owner.interaction.window_control() {
                                callback(|| dispatch_window_control(&owner, control));
                                return;
                            }
                            // OnClick's component payload is physical pixels on
                            // device; touch input below already uses the node's vp.
                            owner.press(
                                EventKind::Press,
                                event.component_f32(0).unwrap_or_default() / owner.scale.get(),
                                event.component_f32(1).unwrap_or_default() / owner.scale.get(),
                            );
                        }
                    });
            } else {
                owner.native.borrow_mut().off_event(E::OnClick)?;
            }
        }
        let touch = !blocked
            && (owner.id == nsh_core::ROOT_ID
                || owner.kind == nsh_core::ViewKind::Modal
                || owner.selectable()
                || click
                || events.contains(EventKind::PressDown)
                || events.contains(EventKind::PressUp)
                || self.tracked.get() & state::PRESSED != 0);
        if self.touch_registered.replace(touch) != touch {
            if touch {
                let weak = Rc::downgrade(owner);
                owner.native.borrow_mut().on_touch_event(move |event| {
                    callback(|| {
                        if let Some(owner) = weak.upgrade()
                            && let Some(input) = event.input_event()
                        {
                            if input.action == UIInputAction::Down {
                                owner
                                    .control
                                    .set_input_modality(nsh_core::platform::InputModality::Pointer);
                            }
                            if owner.id == nsh_core::ROOT_ID
                                || owner.kind == nsh_core::ViewKind::Modal
                            {
                                owner.touches.borrow_mut().input(&owner, &input)?;
                            }
                            let x = input.pointer_x_by_index(0);
                            let y = input.pointer_y_by_index(0);
                            match input.action {
                                UIInputAction::Down => {
                                    owner
                                        .touches
                                        .borrow_mut()
                                        .record_down(owner.scale.get(), &input);
                                    owner.interaction.long_fired.set(false);
                                    if owner.press_feedback_allowed() {
                                        owner.interaction.state(&owner, state::PRESSED, true)?;
                                        owner.emit(EventKind::PressDown, owner.press_payload(x, y));
                                    }
                                }
                                UIInputAction::Up | UIInputAction::Cancel => {
                                    owner.interaction.state(&owner, state::PRESSED, false)?;
                                    if owner.press_feedback_allowed() {
                                        owner.emit(EventKind::PressUp, owner.press_payload(x, y));
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    })
                });
            } else {
                owner.native.borrow_mut().off_event(E::TouchEvent)?;
            }
        }
        let is_surface_root =
            owner.id == nsh_core::ROOT_ID || owner.kind == nsh_core::ViewKind::Modal;
        let hover = !blocked
            && (events.contains(EventKind::MouseEnter)
                || events.contains(EventKind::MouseLeave)
                || self.tracked.get() & state::HOVERED != 0
                || owner.style.borrow().cursor != nsh_core::Cursor::Auto
                || (is_surface_root && owner.platform.get().os.is_desktop()));
        if self.hover_registered.replace(hover) != hover {
            if hover {
                let weak = Rc::downgrade(owner);
                owner
                    .native
                    .borrow_mut()
                    .on_event(E::OnHoverEvent, move |event| {
                        callback(|| {
                            if let Some(owner) = weak.upgrade()
                                && let Some(input) = event.input_event()
                            {
                                let hovered = input.is_hovered();
                                if !hovered
                                    && (owner.id == nsh_core::ROOT_ID
                                        || owner.kind == nsh_core::ViewKind::Modal)
                                {
                                    owner.cursor.clear_dynamic(&owner)?;
                                }
                                owner.cursor.update(&owner, hovered)?;
                                owner.interaction.state(&owner, state::HOVERED, hovered)?;
                                let kind = if hovered {
                                    EventKind::MouseEnter
                                } else {
                                    EventKind::MouseLeave
                                };
                                if owner.interaction.mask().contains(kind) {
                                    owner.emit(
                                        kind,
                                        owner.mouse_payload(
                                            input.pointer_x(),
                                            input.pointer_y(),
                                            None,
                                        ),
                                    );
                                }
                            }
                            Ok(())
                        })
                    });
            } else {
                owner.native.borrow_mut().off_event(E::OnHoverEvent)?;
            }
        }
        let mouse = !blocked
            && ((is_surface_root && owner.platform.get().os.is_desktop())
                || owner.selectable()
                || events.contains(EventKind::MouseMove)
                || events.contains(EventKind::ContextMenu));
        if self.mouse_registered.replace(mouse) != mouse {
            if mouse {
                let weak = Rc::downgrade(owner);
                owner.native.borrow_mut().on_mouse(move |event| {
                    callback(|| {
                        if let Some(owner) = weak.upgrade()
                            && let Some(input) = event.input_event()
                        {
                            let x = input.pointer_x();
                            let y = input.pointer_y();
                            let wx = input.pointer_window_x();
                            let wy = input.pointer_window_y();
                            let dx = input.pointer_display_x();
                            let dy = input.pointer_display_y();
                            owner.touches.borrow_mut().record_pointer((wx, wy));
                            let mask = owner.interaction.mask();
                            let is_root = owner.id == nsh_core::ROOT_ID
                                || owner.kind == nsh_core::ViewKind::Modal;
                            let desktop = owner.platform.get().os.is_desktop();
                            match input.mouse_action() {
                                UIMouseEventAction::Move => {
                                    if is_root {
                                        let left_pressed = pressed_mouse_button(&input)
                                            == Some(nsh_core::MouseButton::Left);
                                        if !left_pressed {
                                            owner.touches.borrow_mut().end_window_resize();
                                            owner.touches.borrow_mut().clear_pending_window_drag();
                                        } else {
                                            if owner
                                                .touches
                                                .borrow_mut()
                                                .move_window_resize(&owner, (dx, dy))?
                                            {
                                                return Ok(());
                                            }
                                            if owner
                                                .touches
                                                .borrow_mut()
                                                .move_pending_window_drag(&owner, (wx, wy))?
                                            {
                                                return Ok(());
                                            }
                                        }
                                        if desktop {
                                            update_desktop_pointer_cursor(&owner, x, y)?;
                                        }
                                    }
                                    if mask.contains(EventKind::MouseMove) {
                                        let pressed = pressed_mouse_button(&input);
                                        owner.emit(
                                            EventKind::MouseMove,
                                            owner.mouse_payload(x, y, pressed),
                                        );
                                    }
                                }
                                UIMouseEventAction::Release => {
                                    if is_root {
                                        let was_resizing =
                                            owner.touches.borrow().window_resizing();
                                        owner.touches.borrow_mut().end_window_resize();
                                        owner.touches.borrow_mut().clear_pending_window_drag();
                                        if was_resizing && desktop {
                                            update_desktop_pointer_cursor(&owner, x, y)?;
                                        }
                                        if let Some(button) = mouse_button(input.mouse_button()) {
                                            dispatch_mouse_up(&owner, x, y, button)?;
                                        }
                                    }
                                }
                                UIMouseEventAction::Press => {
                                    owner.control.set_input_modality(
                                        nsh_core::platform::InputModality::Pointer,
                                    );
                                    if is_root {
                                        capture_mouse_up_targets(&owner, x, y)?;
                                    }
                                    match input.mouse_button() {
                                        UIMouseEventButton::Left if is_root && desktop => {
                                            let clicks = owner
                                                .touches
                                                .borrow_mut()
                                                .record_mouse_press(input.event_time(), (wx, wy));
                                            if !handle_window_resize_press(
                                                &owner,
                                                x,
                                                y,
                                                (dx, dy),
                                            )? {
                                                handle_window_drag_press(
                                                    &owner,
                                                    x,
                                                    y,
                                                    (wx, wy),
                                                    clicks,
                                                )?;
                                                blur_desktop_focus_on_empty_press(&owner, x, y)?;
                                            }
                                        }
                                        UIMouseEventButton::Right => {
                                            if mask.contains(EventKind::ContextMenu) {
                                                owner.emit(
                                                    EventKind::ContextMenu,
                                                    owner.context_menu_payload(x, y),
                                                );
                                            } else if is_root && desktop {
                                                if let Some(target) =
                                                    desktop_right_click_selection_target(
                                                        &owner, x, y,
                                                    )?
                                                    && let Some(route) = owner.dispatcher.upgrade()
                                                {
                                                    route.send(
                                                        crate::app::runtime::Message::DesktopSelectionMenu {
                                                            target,
                                                            x: wx,
                                                            y: wy,
                                                        },
                                                    );
                                                }
                                            } else if !desktop
                                                && owner.selectable()
                                                && owner.has_selection()
                                            {
                                                owner.selection_menu_show(true)?;
                                            }
                                        }
                                        UIMouseEventButton::Back if is_root => {
                                            let _ = owner.host.handle_back();
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    })
                });
            } else {
                owner.native.borrow_mut().off_event(E::OnMouse)?;
            }
        }
        let axis =
            !blocked && (events.contains(EventKind::Wheel) || events.contains(EventKind::Pinch));
        if self.axis_registered.replace(axis) != axis {
            if axis {
                let weak = Rc::downgrade(owner);
                owner.native.borrow_mut().on_axis(move |event| {
                    callback(|| {
                        if let Some(owner) = weak.upgrade()
                            && let Some(input) = event.input_event()
                        {
                            let mask = owner.interaction.mask();
                            let x = input.pointer_x();
                            let y = input.pointer_y();
                            let action = input.axis_action();
                            let pinch_scale = input.axis_pinch_scale_value() as f32;
                            if mask.contains(EventKind::Pinch)
                                && pinch_scale.is_finite()
                                && pinch_scale > 0.0
                                && (pinch_scale != 1.0
                                    || matches!(
                                        action,
                                        UIAxisEventAction::Begin
                                            | UIAxisEventAction::End
                                            | UIAxisEventAction::Cancel
                                    ))
                            {
                                let (phase, delta) = match action {
                                    UIAxisEventAction::Begin => {
                                        owner.interaction.pinch_scale.set(pinch_scale);
                                        (nsh_core::PinchPhase::Start, 0.0)
                                    }
                                    UIAxisEventAction::End | UIAxisEventAction::Cancel => {
                                        let prev = owner.interaction.pinch_scale.replace(1.0);
                                        let delta = if prev > 0.0 {
                                            pinch_scale / prev - 1.0
                                        } else {
                                            0.0
                                        };
                                        (nsh_core::PinchPhase::End, delta)
                                    }
                                    _ => {
                                        let prev =
                                            owner.interaction.pinch_scale.replace(pinch_scale);
                                        let delta = if prev > 0.0 {
                                            pinch_scale / prev - 1.0
                                        } else {
                                            0.0
                                        };
                                        (nsh_core::PinchPhase::Move, delta)
                                    }
                                };
                                let modifiers = axis_modifiers(&owner, &input);
                                owner.emit(
                                    EventKind::Pinch,
                                    owner.pinch_payload(x, y, delta, phase, modifiers),
                                );
                                if takes_axis(&owner, x, y, EventKind::Pinch) {
                                    let _ = input.axis_set_propagation(false);
                                }
                                return Ok(());
                            }
                            let is_mouse = input.tool_type == UIInputToolType::Mouse
                                || (input.source_type == UIInputSourceType::Mouse
                                    && input.tool_type != UIInputToolType::Touchpad);
                            let modifiers = axis_modifiers(&owner, &input);
                            let vx = input.axis_horizontal_value();
                            let vy = input.axis_vertical_value();
                            if mask.contains(EventKind::Pinch)
                                && is_mouse
                                && modifiers.ctrl
                                && vy.is_finite()
                                && vy != 0.0
                            {
                                let delta = crate::services::desktop::ctrl_wheel_pinch_delta(
                                    (vy as f32) / 15.0,
                                );
                                if delta != 0.0 {
                                    owner.emit(
                                        EventKind::Pinch,
                                        owner.pinch_payload(
                                            x,
                                            y,
                                            delta,
                                            nsh_core::PinchPhase::Move,
                                            modifiers,
                                        ),
                                    );
                                    if takes_axis(&owner, x, y, EventKind::Pinch) {
                                        let _ = input.axis_set_propagation(false);
                                    }
                                }
                            } else if mask.contains(EventKind::Wheel)
                                && let Some(delta) =
                                    crate::services::desktop::wheel_delta_from_axis(
                                        vx,
                                        vy,
                                        is_mouse,
                                        owner.scale.get(),
                                        modifiers.shift && !modifiers.ctrl,
                                    )
                            {
                                owner.emit(
                                    EventKind::Wheel,
                                    owner.wheel_payload(x, y, delta, modifiers),
                                );
                                if takes_axis(&owner, x, y, EventKind::Wheel) {
                                    let _ = input.axis_set_propagation(false);
                                }
                            }
                        }
                        Ok(())
                    })
                });
            } else {
                owner.native.borrow_mut().off_event(E::OnAxis)?;
            }
        }
        let file_drop = !blocked
            && (events.contains(EventKind::FileDragEnter)
                || events.contains(EventKind::FileDragLeave)
                || events.contains(EventKind::FileDrop));
        if self.drag_registered.replace(file_drop) != file_drop {
            if file_drop {
                let weak_enter = Rc::downgrade(owner);
                owner.native.borrow_mut().on_drag_enter(move |event| {
                    callback(|| {
                        if let Some(owner) = weak_enter.upgrade()
                            && let Some(drag) = event.drag_event()
                        {
                            let _ = drag.set_suggested_drop_operation(DropOperation::Copy);
                            if owner.interaction.mask().contains(EventKind::FileDragEnter) {
                                let uris = extract_drag_file_uris(&drag);
                                owner.emit(EventKind::FileDragEnter, EventPayload::FileDrop(uris));
                            }
                        }
                        Ok(())
                    })
                });
                let weak_leave = Rc::downgrade(owner);
                owner.native.borrow_mut().on_drag_leave(move |_| {
                    callback(|| {
                        if let Some(owner) = weak_leave.upgrade()
                            && owner.interaction.mask().contains(EventKind::FileDragLeave)
                        {
                            owner.emit(EventKind::FileDragLeave, EventPayload::None);
                        }
                        Ok(())
                    })
                });
                let weak_drop = Rc::downgrade(owner);
                owner.native.borrow_mut().on_drop(move |event| {
                    callback(|| {
                        if let Some(owner) = weak_drop.upgrade()
                            && let Some(drag) = event.drag_event()
                        {
                            let uris = extract_drag_file_uris(&drag);
                            if !uris.is_empty() {
                                let _ = drag.set_drag_result(DragResult::Successful);
                                if owner.interaction.mask().contains(EventKind::FileDrop) {
                                    let scale = owner.scale.get().max(1.0);
                                    let page_position = nsh_core::Point::new(
                                        nsh_core::Points(drag.touch_point_x_to_window() / scale),
                                        nsh_core::Points(drag.touch_point_y_to_window() / scale),
                                    );
                                    owner.emit(
                                        EventKind::FileDrop,
                                        EventPayload::FileDropAt {
                                            uris,
                                            page_position,
                                        },
                                    );
                                }
                            }
                        }
                        Ok(())
                    })
                });
            } else {
                let mut native = owner.native.borrow_mut();
                native.off_event(E::OnDragEnter)?;
                native.off_event(E::OnDragLeave)?;
                native.off_event(E::OnDrop)?;
            }
        }
        let delay = self
            .props
            .borrow()
            .long_press_delay
            .unwrap_or(500.0)
            .clamp(1.0, i32::MAX as f32) as u32;
        let wants_long = !blocked
            && (owner.selectable()
                || events.contains(EventKind::LongPress)
                || owner.text_listens(EventKind::LongPress));
        if self.long_press.borrow().as_ref().map(|(delay, _)| *delay) != wants_long.then_some(delay)
        {
            self.clear_long(owner)?;
            if wants_long {
                let gesture =
                    Gesture::create_long_gesture(1, false, delay.min(i32::MAX as u32) as i32)?;
                let weak = Rc::downgrade(owner);
                gesture.on_gesture(GestureEventAction::Accept, move |event| {
                    if let Some(owner) = weak.upgrade() {
                        if owner.touches.borrow().selection_dragging(owner.id) {
                            return;
                        }
                        owner.interaction.long_fired.set(true);
                        // Gesture input snapshots are physical pixels even on VP nodes.
                        let (x, y) = event.input.map_or((0.0, 0.0), |input| {
                            (input.x / owner.scale.get(), input.y / owner.scale.get())
                        });
                        owner.press(EventKind::LongPress, x, y);
                        callback(|| {
                            owner.selection_begin(x, y)?;
                            Ok(())
                        });
                    }
                })?;
                owner
                    .native
                    .borrow()
                    .add_gesture_ref(&gesture, None, None)?;
                *self.long_press.borrow_mut() = Some((delay, gesture));
            }
        }
        Ok(())
    }
    pub(crate) fn props(&self, owner: &Rc<Node>, props: ViewProps) -> Result<()> {
        let old_props = self.props.borrow().clone();
        let selection_changed = old_props.selectable_view != props.selectable_view;
        let pointer_changed = old_props.pointer_events != props.pointer_events;
        let window_control_changed = old_props.window_control != props.window_control;
        let focusable_changed = old_props.focusable != props.focusable
            || old_props.accessibility.state.disabled != props.accessibility.state.disabled;
        owner.wrapper_semantics_reset();
        {
            let native = owner.native.borrow();
            if old_props.test_id != props.test_id {
                if let Some(id) = &props.test_id {
                    native.id(id.as_ref())?;
                } else {
                    native.reset_attribute(A::Id)?;
                }
            }
            let next_group = props.accessibility.accessible.unwrap_or(false);
            if old_props.accessibility.accessible.unwrap_or(false) != next_group {
                native.set_bool_attribute(A::AccessibilityGroup, next_group)?;
            }
            let mode_for = |hidden: bool, accessible: Option<bool>| -> i32 {
                if hidden {
                    AccessibilityMode::DisabledForDescendants.into()
                } else {
                    match accessible {
                        Some(true) => AccessibilityMode::Enabled.into(),
                        Some(false) => AccessibilityMode::Disabled.into(),
                        None => AccessibilityMode::Auto.into(),
                    }
                }
            };
            let next_mode = mode_for(props.accessibility.hidden, props.accessibility.accessible);
            if mode_for(
                old_props.accessibility.hidden,
                old_props.accessibility.accessible,
            ) != next_mode
            {
                native.set_i32_attribute(A::AccessibilityMode, next_mode)?;
            }
            if old_props.accessibility.hint != props.accessibility.hint {
                native.set_string_attribute(
                    A::AccessibilityDescription,
                    props.accessibility.hint.as_deref().unwrap_or_default(),
                )?;
            }
        }
        crate::accessibility::node::configure(
            owner,
            &old_props.accessibility,
            &props.accessibility,
        )?;
        let disabled = props.accessibility.state.disabled;
        let native_id = props.native_id.clone();
        *self.props.borrow_mut() = props;
        if let Some(scroll) = owner.scroll.borrow().as_ref() {
            owner
                .scroll_watchers
                .props(owner.env, owner.id, scroll, native_id);
        }
        owner.update_wrapper()?;
        self.hit_region(owner)?;
        self.state(owner, state::DISABLED, disabled)?;
        self.apply_label(owner)?;
        self.apply_opacity(owner)?;
        if pointer_changed {
            self.pointer_internal(owner, self.inherited_block.get(), true)?;
        } else if window_control_changed || focusable_changed {
            self.events(owner, self.events.get())?;
        }
        if selection_changed {
            owner.selection_configure()?;
        }
        self.sync_key_focus(owner)
    }
    pub(crate) fn label(&self, owner: &Node, label: Option<Arc<str>>) -> Result<()> {
        self.props.borrow_mut().accessibility.label = label;
        self.apply_label(owner)
    }
    pub(crate) fn text(&self, owner: &Node, text: &Arc<str>) -> Result<()> {
        *self.label.borrow_mut() = (!text.is_empty()).then(|| text.clone());
        self.apply_label(owner)
    }
    fn apply_label(&self, owner: &Node) -> Result<()> {
        let effective = self
            .props
            .borrow()
            .accessibility
            .label
            .clone()
            .or_else(|| self.label.borrow().clone())
            .filter(|s| !s.is_empty());
        let unchanged = match (&*self.applied_label.borrow(), &effective) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
            _ => false,
        };
        if !unchanged {
            owner.native.borrow().set_string_attribute(
                A::AccessibilityText,
                effective.as_deref().unwrap_or_default(),
            )?;
            *self.applied_label.borrow_mut() = effective;
        }
        Ok(())
    }
    pub(crate) fn pointer(&self, owner: &Rc<Node>, inherited: bool) -> Result<()> {
        self.pointer_internal(owner, inherited, false)
    }
    fn pointer_internal(&self, owner: &Rc<Node>, inherited: bool, force_self: bool) -> Result<()> {
        let inherited_changed = self.inherited_block.replace(inherited) != inherited;
        let pointer = self.props.borrow().pointer_events;
        let mode: i32 = if inherited {
            HitTestMode::None.into()
        } else {
            match pointer {
                PointerEvents::Auto => HitTestMode::Default.into(),
                PointerEvents::BoxOnly => HitTestMode::Block.into(),
                PointerEvents::BoxNone | PointerEvents::None => HitTestMode::None.into(),
            }
        };
        if self.applied_hit_test.replace(mode) != mode {
            owner
                .native
                .borrow()
                .set_i32_attribute(A::HitTestBehavior, mode)?;
        }
        let internal =
            if inherited || matches!(pointer, PointerEvents::None | PointerEvents::BoxOnly) {
                HitTestMode::None
            } else {
                HitTestMode::Default
            };
        let internal_i32: i32 = internal.into();
        if self.applied_internal_hit_test.replace(internal_i32) != internal_i32 {
            if let Some(input) = owner.input.borrow().as_ref() {
                input.hit_test(internal)?;
            }
            if let Some(scroll) = owner.scroll.borrow().as_ref() {
                scroll
                    .native
                    .borrow()
                    .set_i32_attribute(A::HitTestBehavior, internal_i32)?;
            }
        }
        if force_self || inherited_changed || owner.raster.borrow().is_some() {
            self.events(owner, self.events.get())?;
        }
        let next_child_inherited =
            inherited || matches!(pointer, PointerEvents::None | PointerEvents::BoxOnly);
        for child in owner
            .children
            .borrow()
            .iter()
            .filter_map(|child| child.upgrade())
        {
            if child.interaction.inherited_block.get() != next_child_inherited {
                child
                    .interaction
                    .pointer_internal(&child, next_child_inherited, false)?;
            }
        }
        Ok(())
    }
    pub(crate) fn blocks_children(&self) -> bool {
        self.inherited_block.get()
            || matches!(
                self.props.borrow().pointer_events,
                PointerEvents::None | PointerEvents::BoxOnly
            )
    }
    pub(crate) fn gestures(&self, owner: &Rc<Node>, values: GestureValues) -> Result<()> {
        if self.gesture_values.replace(values) == values {
            return Ok(());
        }
        for gesture in self.gestures.borrow_mut().drain(..) {
            owner.native.borrow().remove_gesture(&gesture)?;
            gesture.dispose()?;
        }
        for kind in 0..3 {
            let enabled = match kind {
                0 => values.pan_x.is_some() || values.pan_y.is_some(),
                1 => values.pinch.is_some(),
                _ => values.rotation.is_some(),
            };
            if !enabled {
                continue;
            }
            let gesture = match kind {
                0 => Gesture::create_pan_gesture(
                    1,
                    GestureDirection::All,
                    10.0 * owner.scale.get() as f64,
                )?,
                1 => Gesture::create_pinch_gesture(2, 5.0 * owner.scale.get() as f64)?,
                _ => Gesture::create_rotation_gesture(2, 1.0)?,
            };
            let weak = Rc::downgrade(owner);
            gesture.on_gesture(
                GestureEventAction::Accept
                    | GestureEventAction::Update
                    | GestureEventAction::End
                    | GestureEventAction::Cancel,
                move |event| {
                    callback(|| {
                        let Some(owner) = weak.upgrade() else {
                            return Ok(());
                        };
                        let values = owner.interaction.gesture_values.get();
                        let accept = event.event_action_type == GestureEventAction::Accept;
                        let ended = event
                            .event_action_type
                            .intersects(GestureEventAction::End | GestureEventAction::Cancel);
                        if accept {
                            owner.interaction.gesture_start.set([
                                values.pan_x.map_or(0.0, |id| owner.animator.value(id)),
                                values.pan_y.map_or(0.0, |id| owner.animator.value(id)),
                                values.pinch.map_or(1.0, |id| owner.animator.value(id)),
                                values.rotation.map_or(0.0, |id| owner.animator.value(id)),
                            ]);
                        }
                        let start = owner.interaction.gesture_start.get();
                        match event.event_action_data {
                            GestureData::Pan(pan) => {
                                if let Some(id) = values.pan_x {
                                    owner
                                        .animator
                                        .set(id, start[0] + pan.offset_x / owner.scale.get())?;
                                }
                                if let Some(id) = values.pan_y {
                                    owner
                                        .animator
                                        .set(id, start[1] + pan.offset_y / owner.scale.get())?;
                                }
                                if accept || ended {
                                    let translation = nsh_core::Point::new(
                                        nsh_core::Points(pan.offset_x / owner.scale.get()),
                                        nsh_core::Points(pan.offset_y / owner.scale.get()),
                                    );
                                    let velocity_x = pan.velocity_x / owner.scale.get();
                                    let velocity_y = pan.velocity_y / owner.scale.get();
                                    let value_x = values
                                        .pan_x
                                        .map(|id| nsh_core::Points(owner.animator.value(id)));
                                    let value_y = values
                                        .pan_y
                                        .map(|id| nsh_core::Points(owner.animator.value(id)));
                                    owner.emit(
                                        if accept {
                                            EventKind::PanStart
                                        } else {
                                            EventKind::PanEnd
                                        },
                                        if event.event_action_type == GestureEventAction::Cancel {
                                            EventPayload::PanCancelled {
                                                translation,
                                                velocity_x,
                                                velocity_y,
                                                value_x,
                                                value_y,
                                            }
                                        } else {
                                            EventPayload::Pan {
                                                translation,
                                                velocity_x,
                                                velocity_y,
                                                value_x,
                                                value_y,
                                            }
                                        },
                                    );
                                }
                            }
                            GestureData::Pinch(pinch) => {
                                if let Some(id) = values.pinch {
                                    owner.animator.set(id, start[2] * pinch.scale)?;
                                }
                                if accept || ended {
                                    owner.emit(
                                        if accept {
                                            EventKind::PinchStart
                                        } else {
                                            EventKind::PinchEnd
                                        },
                                        EventPayload::Map(vec![
                                            ("scale", PayloadValue::Number(pinch.scale as f64)),
                                            ("velocity", PayloadValue::Number(0.0)),
                                        ]),
                                    );
                                }
                            }
                            GestureData::Rotation(rotation) => {
                                if let Some(id) = values.rotation {
                                    owner.animator.set(id, start[3] + rotation.angle)?;
                                }
                                if accept || ended {
                                    owner.emit(
                                        if accept {
                                            EventKind::RotationStart
                                        } else {
                                            EventKind::RotationEnd
                                        },
                                        EventPayload::Map(vec![
                                            (
                                                "rotation",
                                                PayloadValue::Number(rotation.angle as f64),
                                            ),
                                            ("velocity", PayloadValue::Number(0.0)),
                                        ]),
                                    );
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                },
            )?;
            owner
                .native
                .borrow()
                .add_gesture_ref(&gesture, None, None)?;
            self.gestures.borrow_mut().push(gesture);
        }
        Ok(())
    }
    pub(crate) fn dispose(&self, owner: &Node) -> Result<()> {
        self.clear_long(owner)?;
        for gesture in self.gestures.borrow_mut().drain(..) {
            owner.native.borrow().remove_gesture(&gesture)?;
            gesture.dispose()?;
        }
        Ok(())
    }
    pub(crate) fn clear_long(&self, owner: &Node) -> Result<()> {
        if let Some((_, gesture)) = self.long_press.borrow_mut().take() {
            owner.native.borrow().remove_gesture(&gesture)?;
            gesture.dispose()?;
        }
        Ok(())
    }
    pub(crate) fn track(&self, owner: &Rc<Node>, tracked: u16) -> Result<()> {
        self.tracked.set(tracked);
        owner
            .control
            .set_states(owner.id, self.states.get() & tracked);
        self.events(owner, self.events.get())
    }
    pub(crate) fn hovered(&self) -> bool {
        self.states.get() & state::HOVERED != 0
    }
    pub(crate) fn state(&self, owner: &Node, bit: u16, enabled: bool) -> Result<()> {
        let old = self.states.get();
        let next = if enabled { old | bit } else { old & !bit };
        self.states.set(next);
        if old != next && self.tracked.get() & bit != 0 {
            owner
                .control
                .set_states(owner.id, next & self.tracked.get());
        }
        if bit == state::PRESSED {
            self.apply_opacity(owner)?;
        }
        Ok(())
    }
    pub(crate) fn opacity(&self, owner: &Node, opacity: f32) -> Result<()> {
        if self.opacity.replace(Some(opacity)) != Some(opacity) {
            self.apply_opacity(owner)?;
        }
        Ok(())
    }
    pub(crate) fn invalidate_opacity(&self) {
        self.opacity.set(None);
        self.applied_opacity.set(None);
    }
    fn apply_opacity(&self, owner: &Node) -> Result<()> {
        let opacity = if owner.native_visibility_hidden() {
            0.0
        } else {
            self.opacity.get().unwrap_or(1.0)
        };
        if self.applied_opacity.replace(Some(opacity)) != Some(opacity) {
            owner.visual_node().borrow().opacity(opacity)?;
        }
        Ok(())
    }
}

fn mouse_button(button: UIMouseEventButton) -> Option<nsh_core::MouseButton> {
    match button {
        UIMouseEventButton::Left => Some(nsh_core::MouseButton::Left),
        UIMouseEventButton::Right => Some(nsh_core::MouseButton::Right),
        UIMouseEventButton::Middle => Some(nsh_core::MouseButton::Middle),
        UIMouseEventButton::Back => Some(nsh_core::MouseButton::Back),
        UIMouseEventButton::Forward => Some(nsh_core::MouseButton::Forward),
        _ => None,
    }
}

fn pressed_mouse_button(input: &ArkUIInputEvent) -> Option<nsh_core::MouseButton> {
    let mut raw = [0_i32; 8];
    match input.mouse_pressed_buttons(&mut raw) {
        Ok(count) => raw[..count.min(raw.len())]
            .iter()
            .find_map(|&code| UIMouseEventButton::try_from_raw(code as u32).and_then(mouse_button)),
        Err(_) => mouse_button(input.mouse_button()),
    }
}

fn capture_mouse_up_targets(root: &Rc<Node>, x: f32, y: f32) -> Result<()> {
    let mut targets = Vec::new();
    if let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? {
        loop {
            if cur.interaction.mask().contains(EventKind::MouseUp) {
                targets.push(Rc::downgrade(&cur));
            }
            if cur.id == root.id {
                break;
            }
            let Some(parent) = cur.parent_node.borrow().upgrade() else {
                break;
            };
            cur = parent;
        }
    }
    root.touches.borrow_mut().capture_mouse_up(targets);
    Ok(())
}

fn dispatch_mouse_up(root: &Rc<Node>, x: f32, y: f32, button: nsh_core::MouseButton) -> Result<()> {
    let mut targets: Vec<Rc<Node>> = root
        .touches
        .borrow_mut()
        .take_mouse_up_targets()
        .into_iter()
        .filter_map(|weak| weak.upgrade())
        .filter(|node| {
            node.visible_in_tree() && node.interaction.mask().contains(EventKind::MouseUp)
        })
        .collect();
    if let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? {
        loop {
            if cur.interaction.mask().contains(EventKind::MouseUp)
                && !targets.iter().any(|existing| existing.id == cur.id)
            {
                targets.push(cur.clone());
            }
            if cur.id == root.id {
                break;
            }
            let Some(parent) = cur.parent_node.borrow().upgrade() else {
                break;
            };
            cur = parent;
        }
    }
    for target in targets {
        let (lx, ly) = if target.id == root.id {
            (x, y)
        } else {
            target
                .input_in_root(root)?
                .and_then(|matrix| crate::render::geometry::local_point(&matrix.0, (x, y)))
                .unwrap_or((x, y))
        };
        target.emit(EventKind::MouseUp, target.mouse_up_payload(lx, ly, button));
    }
    Ok(())
}

fn dispatch_window_control(
    owner: &Rc<Node>,
    control: nsh_core::desktop::WindowControl,
) -> Result<()> {
    let mut cur = Some(owner.clone());
    while let Some(node) = cur {
        if let Some(modal) = node.modal.borrow().as_ref()
            && modal.is_desktop_window()
        {
            return modal.desktop_window_control(control);
        }
        cur = node.parent_node.borrow().upgrade();
    }
    use crate::services::desktop::{DesktopEvent, WireCommand, wire_request};
    match control {
        nsh_core::desktop::WindowControl::Minimize => {
            owner
                .host
                .desktop_command(&wire_request(&nsh_core::desktop::WindowRequest::Minimize))?;
        }
        nsh_core::desktop::WindowControl::Maximize => {
            owner
                .host
                .desktop_command(&WireCommand::ToggleMaximize { window_id: None })?;
        }
        nsh_core::desktop::WindowControl::Close => {
            if let Some(route) = owner.dispatcher.upgrade() {
                route.send(crate::app::runtime::Message::HostDesktop(
                    DesktopEvent::ClosePress(nsh_core::desktop::CloseReason::CloseButton),
                ));
            }
        }
    }
    Ok(())
}

fn is_window_drag_target(root: &Rc<Node>, x: f32, y: f32) -> Result<bool> {
    let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? else {
        return Ok(false);
    };
    loop {
        if cur.interaction.window_control().is_some()
            || cur.kind == nsh_core::ViewKind::Input
            || cur.interaction.window_drag() == Some(false)
        {
            return Ok(false);
        }
        let mask = cur.interaction.mask();
        if mask.contains(EventKind::Press)
            || mask.contains(EventKind::PressDown)
            || mask.contains(EventKind::Touch)
            || cur.text_listens(EventKind::Press)
        {
            return Ok(false);
        }
        if cur.interaction.window_drag() == Some(true) {
            return Ok(true);
        }
        if cur.id == root.id {
            return Ok(false);
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            return Ok(false);
        };
        cur = parent;
    }
}

fn hit_is_window_control(root: &Rc<Node>, x: f32, y: f32) -> Result<bool> {
    let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? else {
        return Ok(false);
    };
    loop {
        if cur.interaction.window_control().is_some() {
            return Ok(true);
        }
        if cur.id == root.id {
            return Ok(false);
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            return Ok(false);
        };
        cur = parent;
    }
}

fn desktop_right_click_selection_target(
    root: &Rc<Node>,
    x: f32,
    y: f32,
) -> Result<Option<nsh_core::NodeId>> {
    let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? else {
        return Ok(None);
    };
    let mut found = None;
    loop {
        if cur.interaction.mask().contains(EventKind::ContextMenu)
            || cur.kind == nsh_core::ViewKind::Input
        {
            return Ok(None);
        }
        if found.is_none() && (cur.selectable() || cur.interaction.selectable_view()) {
            found = Some(cur.id);
        }
        if cur.id == root.id {
            return Ok(found);
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            return Ok(found);
        };
        cur = parent;
    }
}

fn hit_window_resize_edge(
    root: &Rc<Node>,
    x: f32,
    y: f32,
) -> Result<Option<nsh_core::desktop::ResizeEdge>> {
    let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? else {
        return Ok(None);
    };
    loop {
        if cur.interaction.window_control().is_some() {
            return Ok(None);
        }
        if let Some(edge) = cur.interaction.window_resize() {
            return Ok(Some(edge));
        }
        if cur.id == root.id {
            return Ok(None);
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            return Ok(None);
        };
        cur = parent;
    }
}

fn inspect_hit_window_chrome(
    hit: &Rc<Node>,
    root_id: nsh_core::NodeId,
) -> (bool, Option<nsh_core::desktop::ResizeEdge>) {
    let mut cur = hit.clone();
    let mut is_control = false;
    let mut explicit_edge = None;
    loop {
        if cur.interaction.window_control().is_some() {
            is_control = true;
            explicit_edge = None;
            break;
        }
        if explicit_edge.is_none()
            && let Some(edge) = cur.interaction.window_resize()
        {
            explicit_edge = Some(edge);
        }
        if cur.id == root_id {
            break;
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            break;
        };
        cur = parent;
    }
    (is_control, explicit_edge)
}

fn update_desktop_pointer_cursor(root: &Rc<Node>, x: f32, y: f32) -> Result<()> {
    let modal = root
        .modal
        .borrow()
        .as_ref()
        .filter(|modal| modal.is_desktop_window())
        .cloned();
    let resizable = modal.as_ref().map_or_else(
        || {
            let mode = root
                .platform
                .get()
                .window_state
                .map_or(nsh_core::desktop::WindowMode::Normal, |state| state.mode);
            root.main_window.borrow().can_resize(mode)
        },
        |modal| modal.desktop_resizable(),
    );
    let hit = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)?;
    let (is_control, explicit_edge) = hit.as_ref().map_or((false, None), |node| {
        inspect_hit_window_chrome(node, root.id)
    });
    let explicit_edge = if resizable { explicit_edge } else { None };
    let resize_edge = explicit_edge.or_else(|| {
        if let Some(modal) = modal.as_ref() {
            modal.desktop_resize_edge((x, y))
        } else if resizable && root.main_window.borrow().frame_none {
            let frame = root.frame.get();
            crate::services::desktop::resize_edge(
                x,
                y,
                frame.width,
                frame.height,
                crate::services::desktop::FRAMELESS_RESIZE_MARGIN_VP,
            )
        } else {
            None
        }
    });
    if let Some(edge) = resize_edge
        && (explicit_edge.is_some() || !is_control)
    {
        root.cursor.set_resize(
            root,
            Some(crate::services::desktop::resize_edge_cursor(edge)),
        )?;
        return Ok(());
    }
    root.cursor.set_resize(root, None)?;

    let auto_cursor = if let Some(node) = hit {
        if node.style.borrow().cursor != nsh_core::Cursor::Auto {
            None
        } else {
            let local = if node.id == root.id {
                Some((x, y))
            } else {
                node.input_in_root(root)?
                    .and_then(|matrix| crate::render::geometry::local_point(&matrix.0, (x, y)))
            };
            if let Some((lx, ly)) = local
                && node.text_link_at(EventKind::Press, lx, ly).is_some()
            {
                Some(nsh_core::Cursor::Pointer)
            } else if node.selectable() {
                Some(nsh_core::Cursor::Text)
            } else {
                None
            }
        }
    } else {
        None
    };
    root.cursor.set_pointer_auto(root, auto_cursor)
}

fn blur_desktop_focus_on_empty_press(root: &Rc<Node>, x: f32, y: f32) -> Result<()> {
    let Some(focused) = root.key_focus.borrow().upgrade() else {
        return Ok(());
    };
    if !focused.visible_in_tree() || focused.key_window_id() != root.key_window_id() {
        return Ok(());
    }
    let Some(mut cur) = crate::input::touch::hit(root, (x, y), &nsh_core::Matrix::IDENTITY)? else {
        return Ok(());
    };
    loop {
        if cur.id == focused.id
            || cur.kind == nsh_core::ViewKind::Input
            || cur.interaction.wrapper_key_focusable(&cur)
            || cur.interaction.window_control().is_some()
            || cur.interaction.window_drag() == Some(true)
        {
            return Ok(());
        }
        let mask = cur.interaction.mask();
        if [
            EventKind::Press,
            EventKind::PressDown,
            EventKind::LongPress,
            EventKind::Touch,
        ]
        .into_iter()
        .any(|event| mask.contains(event) || cur.text_listens(event))
        {
            return Ok(());
        }
        if cur.id == root.id {
            break;
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            break;
        };
        cur = parent;
    }
    if let Some(input) = focused.input.borrow().as_ref() {
        let _ = input.focus(false);
    }
    if let Some(sink) = root.focus_sink.borrow().as_ref() {
        let _ = sink.borrow().request_focus();
    }
    Ok(())
}

fn handle_window_resize_press(
    owner: &Rc<Node>,
    x: f32,
    y: f32,
    window_point: (f32, f32),
) -> Result<bool> {
    let modal = owner
        .modal
        .borrow()
        .clone()
        .filter(|modal| modal.is_desktop_window());
    if let Some(modal) = modal {
        if !modal.desktop_resizable() {
            return Ok(false);
        }
        let explicit = hit_window_resize_edge(owner, x, y)?;
        let Some(edge) = explicit.or_else(|| modal.desktop_resize_edge((x, y))) else {
            return Ok(false);
        };
        if explicit.is_none() && hit_is_window_control(owner, x, y)? {
            return Ok(false);
        }
        owner
            .touches
            .borrow_mut()
            .start_window_resize(&modal, edge, window_point);
        owner.cursor.set_resize(
            owner,
            Some(crate::services::desktop::resize_edge_cursor(edge)),
        )?;
        return Ok(true);
    }
    let is_main_window_host = owner
        .modal
        .borrow()
        .as_ref()
        .is_some_and(|modal| modal.is_main_window_host());
    if owner.id != nsh_core::ROOT_ID && !is_main_window_host {
        return Ok(false);
    }
    let Some(state) = owner.platform.get().window_state else {
        return Ok(false);
    };
    let (can_resize, frame_none) = {
        let main = owner.main_window.borrow();
        (main.can_resize(state.mode), main.frame_none)
    };
    if !can_resize {
        return Ok(false);
    }
    let explicit = hit_window_resize_edge(owner, x, y)?;
    let frame = owner.frame.get();
    let Some(edge) = explicit.or_else(|| {
        frame_none
            .then(|| {
                crate::services::desktop::resize_edge(
                    x,
                    y,
                    frame.width,
                    frame.height,
                    crate::services::desktop::FRAMELESS_RESIZE_MARGIN_VP,
                )
            })
            .flatten()
    }) else {
        return Ok(false);
    };
    if explicit.is_none() && hit_is_window_control(owner, x, y)? {
        return Ok(false);
    }
    let start_origin = (state.bounds.origin.x.0, state.bounds.origin.y.0);
    let start_size = (state.bounds.size.width.0, state.bounds.size.height.0);
    if start_size.0 <= 0.0 || start_size.1 <= 0.0 {
        return Ok(false);
    }
    owner.touches.borrow_mut().start_main_window_resize(
        edge,
        window_point,
        start_origin,
        start_size,
    );
    owner.cursor.set_resize(
        owner,
        Some(crate::services::desktop::resize_edge_cursor(edge)),
    )?;
    Ok(true)
}

fn axis_modifiers(owner: &Node, input: &ArkUIInputEvent) -> nsh_core::keymap::Modifiers {
    let mut modifiers = owner.host.modifiers();
    if let Ok(states) = input.modifier_key_states() {
        modifiers.ctrl |= states.contains(ModifierKey::Ctrl);
        modifiers.shift |= states.contains(ModifierKey::Shift);
        modifiers.alt |= states.contains(ModifierKey::Alt);
    }
    modifiers
}

fn takes_axis(owner: &Rc<Node>, x: f32, y: f32, event: EventKind) -> bool {
    if owner.scroll.borrow().is_some() {
        return false;
    }
    let Ok(Some(mut cur)) = crate::input::touch::hit(owner, (x, y), &nsh_core::Matrix::IDENTITY)
    else {
        return true;
    };
    let mut chain = Vec::new();
    loop {
        let stop = cur.id == owner.id;
        chain.push(cur.clone());
        if stop {
            break;
        }
        let Some(parent) = cur.parent_node.borrow().upgrade() else {
            break;
        };
        cur = parent;
    }
    nsh_core::input::wheel_listener(
        chain,
        |node| node.interaction.mask().contains(event),
        |node| {
            node.scroll.borrow().is_some()
                || matches!(
                    node.kind,
                    nsh_core::ViewKind::WebView | nsh_core::ViewKind::Input
                )
        },
    )
    .is_some_and(|(target, takes)| target.id == owner.id && takes)
}

fn handle_window_drag_press(
    owner: &Rc<Node>,
    x: f32,
    y: f32,
    window_point: (f32, f32),
    clicks: u32,
) -> Result<()> {
    if !is_window_drag_target(owner, x, y)? {
        return Ok(());
    }
    let double_click = clicks == 2;
    if let Some(modal) = owner.modal.borrow().as_ref()
        && modal.is_desktop_window()
    {
        if double_click {
            owner.touches.borrow_mut().clear_pending_window_drag();
            modal.desktop_toggle_maximize()?;
        } else if modal.desktop_movable() {
            owner
                .touches
                .borrow_mut()
                .start_pending_window_drag(window_point, Some(modal), false);
        }
        return Ok(());
    }
    if owner.id == nsh_core::ROOT_ID
        || owner
            .modal
            .borrow()
            .as_ref()
            .is_some_and(|modal| modal.is_main_window_host())
    {
        let (movable, maximizable) = {
            let main = owner.main_window.borrow();
            (main.movable, main.maximizable)
        };
        if double_click {
            owner.touches.borrow_mut().clear_pending_window_drag();
            if maximizable {
                owner.host.desktop_command(
                    &crate::services::desktop::WireCommand::ToggleMaximize { window_id: None },
                )?;
            }
        } else if movable
            && owner
                .platform
                .get()
                .window_state
                .is_none_or(|state| state.mode == nsh_core::desktop::WindowMode::Normal)
        {
            owner
                .touches
                .borrow_mut()
                .start_pending_window_drag(window_point, None, true);
        }
    }
    Ok(())
}

fn extract_drag_file_uris(drag: &ohos_arkui_binding::api::drag::DragEvent<'_>) -> Vec<String> {
    let Ok(mut data) = ohos_udmf_binding::UdmfData::try_new() else {
        return Vec::new();
    };
    if drag.get_udmf_data(&mut data).is_err() {
        return Vec::new();
    }
    let Ok(records) = data.records() else {
        return Vec::new();
    };
    let mut uris = Vec::new();
    for record in records {
        if let Ok(uds) = record.file_uri()
            && let Ok(uri) = uds.file_uri()
            && !uri.is_empty()
        {
            uris.push(uri);
        }
    }
    uris
}
