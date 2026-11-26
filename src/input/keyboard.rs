//! Focus-scoped ArkUI key events; mounted bindings decide consumption before JS runs.
use std::sync::{Arc, Mutex};

use nsh_core::{
    NodeId,
    keymap::{Key, Keymap, Keystroke, Modifiers},
};
use ohos_arkui_binding::{
    component::attribute::{ArkUICommonAttribute, ArkUIEvent},
    event::{KeyCode as N, KeyEvent, KeyEventType},
};

const KEY_SCROLL_LINE_STEP_VP: f32 = 40.0;

impl crate::render::node::Node {
    pub(crate) fn dismiss_keyboard(&self) -> crate::render::node::Result<()> {
        let Some(id) = self
            .dispatcher
            .upgrade()
            .and_then(|route| route.keyboard.lock().unwrap().input)
        else {
            return Ok(());
        };
        let mut root = self.parent_node.borrow().upgrade();
        while let Some(parent) = root
            .as_ref()
            .and_then(|node| node.parent_node.borrow().upgrade())
        {
            root = Some(parent);
        }
        let input = root.as_deref().unwrap_or(self).find_input(id);
        if let Some(input) = input {
            input.focus(false)?;
        }
        Ok(())
    }
    pub(crate) fn key_window_id(&self) -> NodeId {
        if let Some(modal) = self.modal.borrow().as_ref() {
            if modal.is_desktop_window() {
                return self.id;
            }
            if let Some(host_id) = modal.host_sub_window_id() {
                return host_id;
            }
        }
        let mut parent = self.parent_node.borrow().upgrade();
        while let Some(node) = parent {
            if let Some(modal) = node.modal.borrow().as_ref() {
                if modal.is_desktop_window() {
                    return node.id;
                }
                if let Some(host_id) = modal.host_sub_window_id() {
                    return host_id;
                }
            }
            parent = node.parent_node.borrow().upgrade();
        }
        nsh_core::ROOT_ID
    }
    pub(crate) fn key_surface_root(self: &std::rc::Rc<Self>) -> std::rc::Rc<Self> {
        let mut node = self.clone();
        loop {
            if node.id == nsh_core::ROOT_ID || node.kind == nsh_core::ViewKind::Modal {
                return node;
            }
            let Some(parent) = node.parent_node.borrow().upgrade() else {
                return node;
            };
            node = parent;
        }
    }
    fn find_input(&self, id: NodeId) -> Option<std::rc::Rc<crate::input::editor::Input>> {
        if self.id == id {
            return self.input.borrow().clone();
        }
        self.children
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .find_map(|node| node.find_input(id))
    }
}
pub(crate) struct Snapshot {
    pub(crate) map: Arc<Keymap>,
    pub(crate) menu: Option<Arc<[nsh_core::desktop::MenuItem]>>,
    pub(crate) menu_labels: nsh_core::desktop::MenuLabels,
    pub(crate) menu_bar_mode: nsh_core::desktop::MenuBarMode,
    pub(crate) menu_bar_available: bool,
    pub(crate) menu_bar_revealed: bool,
    pub(crate) quit_on_request: bool,
    pub(crate) desktop: bool,
    pub(crate) focused: Option<NodeId>,
    pub(crate) input: Option<NodeId>,
    pub(crate) selection: Option<NodeId>,
    pub(crate) hosted_main_window: Option<NodeId>,
    pub(crate) composing: bool,
}
impl Snapshot {
    pub(crate) fn keymap_window(&self, window: NodeId) -> NodeId {
        if window == nsh_core::ROOT_ID {
            self.hosted_main_window.unwrap_or(nsh_core::ROOT_ID)
        } else {
            window
        }
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            map: Default::default(),
            menu: None,
            menu_labels: Default::default(),
            menu_bar_mode: nsh_core::desktop::MenuBarMode::Shown,
            menu_bar_available: false,
            menu_bar_revealed: false,
            quit_on_request: true,
            desktop: false,
            focused: None,
            input: None,
            selection: None,
            hosted_main_window: None,
            composing: false,
        }
    }
}
#[derive(Default)]
struct Held {
    keys: Vec<(N, bool)>,
    last_editor_key: Option<(i64, i32, N)>,
    alt_armed: bool,
}

struct KeyInput<'a> {
    action: i32,
    code: N,
    unicode: u32,
    caps_lock: Option<bool>,
    key_text: &'a str,
    pressed_keys: Option<&'a [i32]>,
    explicit_modifiers: Option<Modifiers>,
    timestamp: i64,
    from_editor: bool,
}

/// Per-window modifier bookkeeping, with no window-global native registration.
pub(crate) struct KeyboardWindow {
    id: i32,
    held: Mutex<Held>,
}
impl KeyboardWindow {
    pub(crate) fn id(&self) -> i32 {
        self.id
    }
    pub(crate) fn clear(&self) {
        let mut held = self.held.lock().unwrap();
        held.keys.clear();
        held.last_editor_key = None;
        held.alt_armed = false;
    }
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.held.lock().unwrap().modifiers()
    }
    pub(crate) fn new(id: i32) -> Self {
        Self {
            id,
            held: Mutex::default(),
        }
    }

    pub(crate) fn event(
        &self,
        event: KeyEvent<'_>,
        route: &crate::app::runtime::Dispatcher,
        owner: &std::rc::Rc<crate::render::node::Node>,
    ) -> bool {
        let action = match event.event_type() {
            KeyEventType::Down => 1,
            KeyEventType::Up => 2,
            _ => return false,
        };
        let mut pressed = [0; 64];
        let pressed_keys = event
            .pressed_keys(&mut pressed)
            .ok()
            .map(|count| &pressed[..count.min(pressed.len())]);
        let key_text = event.key_text();
        self.dispatch(
            KeyInput {
                action,
                code: event.key_code(),
                unicode: event.unicode(),
                caps_lock: event.caps_lock_on().ok(),
                key_text: &key_text,
                pressed_keys,
                explicit_modifiers: None,
                timestamp: event.event_time(),
                from_editor: false,
            },
            route,
            owner,
        )
    }

    pub(crate) fn editor_event(
        &self,
        event: &crate::interop::sdk::EditorKeyEvent,
        route: &crate::app::runtime::Dispatcher,
        owner: &std::rc::Rc<crate::render::node::Node>,
    ) -> bool {
        if event.action != 1 && event.action != 2 {
            return false;
        }
        let code = N::try_from_raw(event.key_code).unwrap_or(N::Unknown);
        self.dispatch(
            KeyInput {
                action: event.action,
                code,
                unicode: event.unicode,
                caps_lock: event.caps_lock,
                key_text: &event.key_text,
                pressed_keys: None,
                explicit_modifiers: Some(Modifiers {
                    ctrl: event.ctrl,
                    alt: event.alt,
                    shift: event.shift,
                    cmd: event.cmd,
                    ..Modifiers::default()
                }),
                timestamp: event.timestamp,
                from_editor: true,
            },
            route,
            owner,
        )
    }

    fn dispatch(
        &self,
        input: KeyInput<'_>,
        route: &crate::app::runtime::Dispatcher,
        owner: &std::rc::Rc<crate::render::node::Node>,
    ) -> bool {
        let action = input.action;
        let code = input.code;
        let mut held = self.held.lock().unwrap();
        if input.from_editor {
            held.last_editor_key = Some((input.timestamp, action, code));
        } else if held.last_editor_key.take() == Some((input.timestamp, action, code)) {
            return false;
        }
        if action == 1
            && !matches!(
                code,
                N::ShiftLeft
                    | N::ShiftRight
                    | N::CtrlLeft
                    | N::CtrlRight
                    | N::AltLeft
                    | N::AltRight
                    | N::MetaLeft
                    | N::MetaRight
            )
        {
            owner
                .control
                .set_input_modality(nsh_core::platform::InputModality::Keyboard);
        }
        // Focus may have entered while modifiers were held in the host. Sync from
        // the event's pressed set or modifier state instead of relying solely on
        // prior keydowns inside our tree.
        if let Some(keys) = input.pressed_keys {
            held.keys.retain(|(key, _)| {
                *key == code || keys.iter().any(|raw| N::try_from_raw(*raw) == Some(*key))
            });
            for raw in keys {
                if let Some(key) = N::try_from_raw(*raw)
                    && key != code
                    && !held.keys.iter().any(|(old, _)| *old == key)
                {
                    held.keys.push((key, false));
                }
            }
        } else if let Some(modifiers) = input.explicit_modifiers {
            held.sync_modifiers(code, modifiers);
        }
        let held_before = held.keys.iter().any(|(old, _)| *old == code);
        let modifiers = held.modifiers();
        let stroke = key(code).map(|key| Keystroke { key, modifiers });
        let repeat = action == 1 && held_before;
        let kind = if action == 1 {
            nsh_core::EventKind::KeyDown
        } else {
            nsh_core::EventKind::KeyUp
        };
        let window = owner.key_window_id();
        let focus = owner
            .key_focus
            .borrow()
            .upgrade()
            .filter(|node| node.visible_in_tree() && node.key_window_id() == window);
        let scope_modal = owner.focus_scope_modal(window);
        let covered = scope_modal.as_ref().is_some_and(|modal| {
            focus
                .as_ref()
                .is_some_and(|node| !node.is_descendant_of(modal.id))
        });
        let (composing, snapshot_editing) = {
            let snapshot = route.keyboard.lock().unwrap();
            let editing_in_window = snapshot.focused.is_some_and(|id| {
                owner
                    .node_by_id(id)
                    .is_none_or(|node| node.key_window_id() == window)
            });
            (snapshot.composing, editing_in_window)
        };
        let editing = input.from_editor
            || (!covered
                && (focus.as_ref().is_some_and(|node| native_owns(node)) || snapshot_editing));
        // Native inputs, web views, video players and hosted native views own
        // their own editing keys: ancestor `captureKeys` and raw key listeners
        // must not intercept them; only keymap bindings allowed while editing
        // (`KeyFocus::editing = true`) apply.
        let native_owned = input.from_editor || editing;
        let focused_control = (!native_owned && !covered)
            .then(|| {
                focus.as_ref().filter(|node| {
                    !native_owns(node)
                        && !node.accessibility_state.spec.borrow().state.disabled
                        && node.interaction.mask().contains(nsh_core::EventKind::Press)
                })
            })
            .flatten()
            .cloned();
        let mut key_focus = nsh_core::keymap::KeyFocus::default();
        key_focus.editing = editing;
        key_focus.control = focused_control.is_some();
        key_focus.covered = covered;

        let raw_target = if native_owned {
            None
        } else if covered {
            Some(key_start(scope_modal.as_ref().unwrap_or(owner), window))
        } else {
            Some(key_start(focus.as_ref().unwrap_or(owner), window))
        };

        let build_key_event = |default_prevented: bool, held: &Held| {
            let mut details = nsh_core::KeyEvent::default();
            details.key = logical_key(
                code,
                input.unicode,
                modifiers.shift,
                input.caps_lock,
                input.key_text,
            )
            .into();
            // ArkUI exposes a logical key identifier but no hardware scan code.
            details.modifiers = modifiers;
            match code {
                N::ShiftLeft | N::ShiftRight => {
                    details.modifiers.shift = action == 1
                        || held
                            .keys
                            .iter()
                            .any(|(k, _)| *k != code && matches!(k, N::ShiftLeft | N::ShiftRight))
                }
                N::CtrlLeft | N::CtrlRight => {
                    details.modifiers.ctrl = action == 1
                        || held
                            .keys
                            .iter()
                            .any(|(k, _)| *k != code && matches!(k, N::CtrlLeft | N::CtrlRight))
                }
                N::AltLeft | N::AltRight => {
                    details.modifiers.alt = action == 1
                        || held
                            .keys
                            .iter()
                            .any(|(k, _)| *k != code && matches!(k, N::AltLeft | N::AltRight))
                }
                N::MetaLeft | N::MetaRight => {
                    details.modifiers.cmd = action == 1
                        || held
                            .keys
                            .iter()
                            .any(|(k, _)| *k != code && matches!(k, N::MetaLeft | N::MetaRight))
                }
                _ => {}
            }
            details.repeat = repeat;
            details.default_prevented = default_prevented;
            details
        };

        let alt_released_alone = held.track_alt_alone(action, code, held_before);

        if action == 2 {
            let (mut observes, mut captures) = (false, false);
            if !composing && let Some(target) = &raw_target {
                walk_keyboard_ancestors(
                    target.clone(),
                    |node| keyboard_parent(node, window),
                    |node| {
                        let (listens, prevents) =
                            node.interaction.key_interest(kind, stroke.as_ref());
                        observes |= listens;
                        captures |= prevents;
                        false
                    },
                );
                if observes || captures {
                    let details = build_key_event(captures, &held);
                    target.control.dispatch_event(
                        target.id,
                        kind,
                        nsh_core::EventPayload::Key(details),
                    );
                } else {
                    captures = false;
                }
            }
            let down_consumed = held.release(code);
            if alt_released_alone && !captures && window == nsh_core::ROOT_ID {
                let snapshot = route.keyboard.lock().unwrap();
                if snapshot.desktop
                    && snapshot.menu_bar_available
                    && !snapshot.composing
                    && snapshot.menu_bar_mode == nsh_core::desktop::MenuBarMode::AutoHide
                    && snapshot.menu.as_ref().is_some_and(|menu| !menu.is_empty())
                {
                    route.send(crate::app::runtime::Message::DesktopMenuBar(
                        crate::services::desktop::MenuBarKeyAction::ToggleAlt,
                    ));
                    return true;
                }
            }
            return down_consumed || captures;
        }

        let mut consumed = false;
        let mut observes = false;
        let mut captures = false;

        if !composing {
            // 1. App/system bindings (`Keymap::find_with`, menu bar toggles, text selection).
            if let Some(stroke) = stroke {
                let snapshot = route.keyboard.lock().unwrap();
                if !repeat
                    && snapshot.desktop
                    && window == nsh_core::ROOT_ID
                    && stroke.key == Key::Escape
                    && stroke.modifiers == Modifiers::default()
                    && snapshot.menu_bar_mode == nsh_core::desktop::MenuBarMode::AutoHide
                    && snapshot.menu_bar_revealed
                {
                    route.send(crate::app::runtime::Message::DesktopMenuBar(
                        crate::services::desktop::MenuBarKeyAction::Hide,
                    ));
                    consumed = true;
                } else if !repeat
                    && snapshot.desktop
                    && snapshot.menu_bar_available
                    && window == nsh_core::ROOT_ID
                    && stroke.key == Key::F(10)
                    && stroke.modifiers == Modifiers::default()
                    && snapshot.menu.as_ref().is_some_and(|menu| !menu.is_empty())
                {
                    route.send(crate::app::runtime::Message::DesktopMenuBar(
                        crate::services::desktop::MenuBarKeyAction::ToggleF10,
                    ));
                    consumed = true;
                } else if !editing
                    && !covered
                    && let Some(id) = snapshot.selection
                {
                    use crate::text::selection::controller::Command;
                    let command = match stroke.key {
                        Key::Escape if stroke.modifiers == Modifiers::default() => {
                            Some(Command::Clear)
                        }
                        Key::Char('c')
                            if stroke.modifiers.ctrl
                                && !stroke.modifiers.alt
                                && !stroke.modifiers.shift
                                && !stroke.modifiers.cmd =>
                        {
                            Some(Command::Copy)
                        }
                        Key::Char('a')
                            if stroke.modifiers.ctrl
                                && !stroke.modifiers.alt
                                && !stroke.modifiers.shift
                                && !stroke.modifiers.cmd =>
                        {
                            Some(Command::SelectAll)
                        }
                        _ => None,
                    };
                    if let Some(command) = command {
                        if !repeat {
                            route.send(crate::app::runtime::Message::TextSelection(id, command));
                        }
                        consumed = true;
                    }
                }
                if !consumed {
                    let keymap_window = snapshot.keymap_window(window);
                    let matched = snapshot
                        .map
                        .find_with(keymap_window, stroke, key_focus, repeat);
                    if let Some(id) = matched.fired() {
                        route.send(crate::app::runtime::Message::HostEvent(
                            nsh_core::EventKind::KeyCommand,
                            nsh_core::EventPayload::Value(nsh_core::PayloadValue::Number(
                                id as f64,
                            )),
                        ));
                    }
                    if matched.consumed() {
                        consumed = true;
                    }
                }
            }

            // 2. Raw key listeners & `captureKeys` on non-native views (following `key_parent`).
            if !consumed && let Some(target) = &raw_target {
                walk_keyboard_ancestors(
                    target.clone(),
                    |node| keyboard_parent(node, window),
                    |node| {
                        let (listens, prevents) =
                            node.interaction.key_interest(kind, stroke.as_ref());
                        observes |= listens;
                        captures |= prevents;
                        false
                    },
                );
                if observes || captures {
                    let details = build_key_event(captures, &held);
                    target.control.dispatch_event(
                        target.id,
                        kind,
                        nsh_core::EventPayload::Key(details),
                    );
                } else {
                    captures = false;
                }
                if captures {
                    consumed = true;
                }
            }

            // 3. Return/Space press on a focused control (once keymap & captureKeys passed).
            if !consumed
                && !repeat
                && let Some(control) = &focused_control
                && let Some(stroke) = stroke
                && nsh_core::input::presses_focused_control(stroke.key, stroke.modifiers)
            {
                control.control.dispatch_event(
                    control.id,
                    nsh_core::EventKind::Press,
                    nsh_core::EventPayload::None,
                );
                consumed = true;
            }

            // 4. Desktop menu shortcuts, Back navigation, and keyboard scrolling.
            if !consumed && let Some(stroke) = stroke {
                let allow_key_scroll = !observes
                    && focus
                        .as_ref()
                        .is_none_or(|node| node.scroll.borrow().is_some());
                let snapshot = route.keyboard.lock().unwrap();
                if !repeat
                    && snapshot.desktop
                    && let Some(menu) = snapshot.menu.as_deref()
                    && let Some(shortcut) =
                        crate::services::desktop::shortcut_from_keystroke(stroke)
                    && let Some(action) =
                        nsh_core::desktop::MenuItem::find_shortcut(menu, &shortcut)
                    && (!editing || action.fires_while_editing())
                {
                    route.send(crate::app::runtime::Message::DesktopMenuAction {
                        id: action.id,
                        role: action.role,
                    });
                    consumed = true;
                } else if !repeat
                    && snapshot.desktop
                    && snapshot.menu_bar_available
                    && window == nsh_core::ROOT_ID
                    && !editing
                    && stroke.modifiers
                        == (Modifiers {
                            alt: true,
                            ..Modifiers::default()
                        })
                    && let Key::Char(ch) = stroke.key
                    && ch.is_ascii_alphabetic()
                    && let Some(menu) = snapshot.menu.as_deref()
                    && crate::services::desktop::menu_bar_has_initial(
                        menu,
                        ch,
                        &snapshot.menu_labels,
                    )
                {
                    route.send(crate::app::runtime::Message::DesktopMenuBar(
                        crate::services::desktop::MenuBarKeyAction::OpenInitial(ch),
                    ));
                    consumed = true;
                } else if !repeat
                    && snapshot.desktop
                    && stroke.key == Key::Char('q')
                    && stroke.modifiers
                        == (Modifiers {
                            ctrl: true,
                            ..Modifiers::default()
                        })
                {
                    route.send(crate::app::runtime::Message::DesktopMenuAction {
                        id: 0,
                        role: Some(nsh_core::desktop::MenuRole::Quit),
                    });
                    consumed = true;
                } else if !repeat
                    && stroke.key == Key::Escape
                    && stroke.modifiers == Modifiers::default()
                {
                    if snapshot.input.is_some() && !snapshot.desktop {
                        route.send(crate::app::runtime::Message::DismissKeyboard);
                        consumed = true;
                    } else {
                        drop(snapshot);
                        consumed = back(route);
                    }
                } else if !repeat
                    && !editing
                    && ((stroke.key == Key::Left
                        && stroke.modifiers
                            == Modifiers {
                                alt: true,
                                ..Modifiers::default()
                            })
                        || (stroke.key == Key::Char('[')
                            && stroke.modifiers
                                == Modifiers {
                                    cmd: true,
                                    ..Modifiers::default()
                                }))
                {
                    drop(snapshot);
                    consumed = back(route);
                } else if snapshot.desktop
                    && !editing
                    && !covered
                    && snapshot.selection.is_none()
                    && allow_key_scroll
                {
                    use nsh_core::scroll::KeyMove;
                    let scroll_move = if stroke.modifiers == Modifiers::default() {
                        match stroke.key {
                            Key::PageUp => Some((KeyMove::Pages(-1.0), false)),
                            Key::PageDown => Some((KeyMove::Pages(1.0), false)),
                            Key::Home => Some((KeyMove::Start, false)),
                            Key::End => Some((KeyMove::End, false)),
                            Key::Up => Some((
                                KeyMove::Points(nsh_core::Points(-KEY_SCROLL_LINE_STEP_VP)),
                                false,
                            )),
                            Key::Down => Some((
                                KeyMove::Points(nsh_core::Points(KEY_SCROLL_LINE_STEP_VP)),
                                false,
                            )),
                            Key::Char(' ') => Some((KeyMove::Pages(1.0), false)),
                            _ => None,
                        }
                    } else if stroke.modifiers
                        == (Modifiers {
                            shift: true,
                            ..Modifiers::default()
                        })
                        && stroke.key == Key::Char(' ')
                    {
                        Some((KeyMove::Pages(-1.0), false))
                    } else {
                        None
                    };
                    if let Some((key_move, horizontal)) = scroll_move {
                        route.send(crate::app::runtime::Message::KeyScroll {
                            window,
                            key_move,
                            horizontal,
                        });
                        consumed = true;
                    }
                }
            }

            // 5. `captureKeys="text"` typing redirect into the first enabled Input.
            let is_modifier_or_lock = matches!(
                code,
                N::ShiftLeft
                    | N::ShiftRight
                    | N::CtrlLeft
                    | N::CtrlRight
                    | N::AltLeft
                    | N::AltRight
                    | N::MetaLeft
                    | N::MetaRight
                    | N::CapsLock
                    | N::NumLock
                    | N::ScrollLock
            );
            if !consumed
                && !is_modifier_or_lock
                && let Some(target) = &raw_target
                && let Some(container) = text_capture_container(target, window)
                && let Some(text) = crate::input::intercept::typing_text(
                    stroke.as_ref().map(|s| s.key),
                    input.unicode,
                    modifiers,
                    input.caps_lock,
                    (!input.key_text.is_empty()).then_some(input.key_text),
                )
                && let Some(dest_input) = first_enabled_input(&container)
            {
                let _ = dest_input.focus(true);
                let _ = dest_input.insert_text(text);
                consumed = true;
            }
        }

        held.record_down(code, consumed);
        consumed
    }
}

fn native_owns(node: &crate::render::node::Node) -> bool {
    matches!(
        node.kind,
        nsh_core::ViewKind::Input
            | nsh_core::ViewKind::WebView
            | nsh_core::ViewKind::Native
            | nsh_core::ViewKind::Video
    )
}

fn key_start(
    node: &std::rc::Rc<crate::render::node::Node>,
    window: NodeId,
) -> std::rc::Rc<crate::render::node::Node> {
    node.interaction
        .key_content()
        .and_then(|id| node.node_by_id(id))
        .filter(|content| content.key_window_id() == window)
        .unwrap_or_else(|| node.clone())
}

fn keyboard_parent(
    node: &std::rc::Rc<crate::render::node::Node>,
    window: NodeId,
) -> Option<std::rc::Rc<crate::render::node::Node>> {
    if let Some(id) = node.interaction.key_parent() {
        return node
            .node_by_id(id)
            .filter(|parent| parent.id != node.id && parent.key_window_id() == window);
    }
    if node
        .modal
        .borrow()
        .as_ref()
        .is_some_and(|modal| !modal.is_main_window_host())
    {
        return None;
    }
    node.parent_node
        .borrow()
        .upgrade()
        .filter(|parent| parent.key_window_id() == window)
}

fn walk_keyboard_ancestors<T>(
    start: T,
    mut parent: impl FnMut(&T) -> Option<T>,
    mut visit: impl FnMut(&T) -> bool,
) -> Option<T> {
    let mut current = Some(start);
    for _ in 0..1024 {
        let node = current?;
        if visit(&node) {
            return Some(node);
        }
        current = parent(&node);
    }
    None
}

fn back(route: &crate::app::runtime::Dispatcher) -> bool {
    if route.wants_back.load(std::sync::atomic::Ordering::Acquire) {
        route.send(crate::app::runtime::Message::Back);
        true
    } else {
        false
    }
}

fn text_capture_container(
    target: &std::rc::Rc<crate::render::node::Node>,
    window: NodeId,
) -> Option<std::rc::Rc<crate::render::node::Node>> {
    if native_owns(target) || target.input.borrow().is_some() {
        return None;
    }
    walk_keyboard_ancestors(
        target.clone(),
        |node| keyboard_parent(node, window),
        |node| node.interaction.types_into_input(),
    )
}

fn first_enabled_input(
    root: &std::rc::Rc<crate::render::node::Node>,
) -> Option<std::rc::Rc<crate::input::editor::Input>> {
    fn visit(
        node: &std::rc::Rc<crate::render::node::Node>,
        is_root: bool,
    ) -> Option<std::rc::Rc<crate::input::editor::Input>> {
        if !node.visible_in_tree() || node.accessibility_state.spec.borrow().state.disabled {
            return None;
        }
        if !is_root
            && node
                .modal
                .borrow()
                .as_ref()
                .is_some_and(|modal| !modal.is_main_window_host())
        {
            return None;
        }
        if node.kind == nsh_core::ViewKind::Input
            && let Some(input) = node.input.borrow().as_ref()
            && input.is_editable()
        {
            return Some(input.clone());
        }
        let children = node.children.borrow();
        for weak in children.iter() {
            if let Some(child) = weak.upgrade()
                && let Some(found) = visit(&child, false)
            {
                return Some(found);
            }
        }
        None
    }
    visit(root, true)
}

/// Requests ArkUI focus for a live native node without going through
/// `ohos_arkui_binding::ArkUINode::request_focus()`, whose `ArkUIErrorCode`
/// conversion panics on ArkUI focus status codes (`150001..=150003`, e.g.
/// when a node is not yet attached to a window tree or its window is inactive).
pub(crate) fn request_node_focus(
    node: &ohos_arkui_binding::common::node::ArkUINode,
) -> std::result::Result<(), i32> {
    unsafe extern "C" {
        fn OH_ArkUI_FocusRequest(node: *mut std::ffi::c_void) -> i32;
    }
    let raw = node.raw_handle();
    if raw.is_null() {
        return Err(-1);
    }
    // SAFETY: `raw` is a live ArkUI node handle borrowed on the UI thread.
    let status = unsafe { OH_ArkUI_FocusRequest(raw.cast()) };
    if status == 0 { Ok(()) } else { Err(status) }
}

/// Root and native dialog boundaries receive bubbling keys only from their own
/// focus subtree. ArkUI retires the handler with its node on the UI thread.
pub(crate) fn install(
    owner: &std::rc::Rc<crate::render::node::Node>,
) -> crate::render::node::Result<()> {
    owner.native.borrow().set_focusable(true)?;
    if owner.platform.get().os.is_desktop() {
        // ArkUI delegates focus requested on a container to its first focusable
        // leaf descendant (e.g. a search TextInput), while a childless Stack
        // (`FocusType::SCOPE`) cannot hold focus itself. Use an invisible
        // `FocusType::NODE` leaf (`Image` has no built-in Enter/Space/IME
        // handlers) inside `owner.native` so surface-level focus stays inside
        // this window/modal and keys bubble cleanly to `owner.native`.
        let sink = ohos_arkui_binding::component::built_in_component::Image::new()?.into_node();
        {
            use ohos_arkui_binding::{
                common::attribute::ArkUINodeAttributeNumber,
                types::{
                    advanced::LengthMetricUnit,
                    attribute::ArkUINodeAttributeType as A,
                    view::{AccessibilityMode, HitTestMode},
                },
            };
            let node = sink.borrow();
            node.set_length_metric_unit(LengthMetricUnit::Vp)?;
            node.width(1.0)?;
            node.height(1.0)?;
            node.opacity(0.0)?;
            node.set_i32_attribute(A::HitTestBehavior, HitTestMode::None.into())?;
            node.set_i32_attribute(
                A::AccessibilityMode,
                AccessibilityMode::DisabledForDescendants.into(),
            )?;
            node.set_focusable(true)?;
            let _ = node.set_default_focus(true);
            let _ = node.set_focus_box([
                ArkUINodeAttributeNumber::Float(0.0),
                ArkUINodeAttributeNumber::Float(0.0),
                ArkUINodeAttributeNumber::Uint(0),
            ]);
        }
        owner.native.borrow_mut().insert_child(sink.clone(), 0)?;
        *owner.focus_sink.borrow_mut() = Some(sink);
    }
    let weak = std::rc::Rc::downgrade(owner);
    // Use post-IME key delivery so active IMEs (including desktop candidate
    // windows that do not populate PreviewText) consume editing and navigation
    // keys while composing. Focused editors route post-IME keys before ArkUI's
    // built-in TextArea/TextInput handler via `nsh_editor_key_event`.
    owner.native.borrow_mut().on_key_event(move |event| {
        if let Some(owner) = weak.upgrade()
            && let Some(event) = event.key_event()
            && let Some(route) = owner.dispatcher.upgrade()
            && owner.host.key_event(event, &route, &owner)
        {
            event.set_consumed(true);
            event.stop_propagation(true);
        }
    });
    Ok(())
}

impl Held {
    fn sync_modifiers(&mut self, code: N, modifiers: Modifiers) {
        let mut sync = |left: N, right: N, active: bool| {
            if active {
                if code != left && code != right && !self.has(left, right) {
                    self.keys.push((left, false));
                }
            } else {
                self.keys.retain(|(held_code, _)| {
                    *held_code == code || (*held_code != left && *held_code != right)
                });
            }
        };
        sync(N::CtrlLeft, N::CtrlRight, modifiers.ctrl);
        sync(N::AltLeft, N::AltRight, modifiers.alt);
        sync(N::ShiftLeft, N::ShiftRight, modifiers.shift);
        sync(N::MetaLeft, N::MetaRight, modifiers.cmd);
    }
    fn modifiers(&self) -> Modifiers {
        Modifiers {
            ctrl: self.has(N::CtrlLeft, N::CtrlRight),
            alt: self.has(N::AltLeft, N::AltRight),
            shift: self.has(N::ShiftLeft, N::ShiftRight),
            cmd: self.has(N::MetaLeft, N::MetaRight),
            ..Modifiers::default()
        }
    }
    #[cfg(test)]
    fn cancel(&mut self, action: i32) -> bool {
        if action == 0 {
            self.keys.clear();
            self.last_editor_key = None;
            self.alt_armed = false;
            true
        } else {
            false
        }
    }
    fn track_alt_alone(&mut self, action: i32, code: N, held_before: bool) -> bool {
        let is_alt = matches!(code, N::AltLeft | N::AltRight);
        match action {
            1 => {
                if is_alt {
                    let modifiers = self.modifiers();
                    let others = modifiers.ctrl || modifiers.shift || modifiers.cmd;
                    if !held_before && !others && self.keys.is_empty() {
                        self.alt_armed = true;
                    } else if others {
                        self.alt_armed = false;
                    }
                } else {
                    self.alt_armed = false;
                }
                false
            }
            2 => {
                if is_alt {
                    let armed = std::mem::replace(&mut self.alt_armed, false);
                    let modifiers = self.modifiers();
                    let other_alt = self
                        .keys
                        .iter()
                        .any(|(k, _)| *k != code && matches!(k, N::AltLeft | N::AltRight));
                    armed && !other_alt && !modifiers.ctrl && !modifiers.shift && !modifiers.cmd
                } else {
                    false
                }
            }
            _ => {
                self.alt_armed = false;
                false
            }
        }
    }
    fn record_down(&mut self, code: N, consumed: bool) {
        if let Some(index) = self
            .keys
            .iter()
            .position(|(held_code, _)| *held_code == code)
        {
            self.keys[index].1 |= consumed;
        } else {
            self.keys.push((code, consumed));
        }
    }
    fn release(&mut self, code: N) -> bool {
        self.keys
            .iter()
            .position(|(held_code, _)| *held_code == code)
            .is_some_and(|index| self.keys.swap_remove(index).1)
    }
    #[cfg(test)]
    fn event(
        &mut self,
        action: i32,
        code: N,
        captured: bool,
        fire: impl FnOnce(Keystroke, bool) -> bool,
    ) -> bool {
        if self.cancel(action) {
            return false;
        }
        let old = self
            .keys
            .iter()
            .position(|(held_code, _)| *held_code == code);
        if action == 2 {
            return old.is_some_and(|index| self.keys.swap_remove(index).1);
        }
        if action != 1 {
            return false;
        }
        let modifiers = self.modifiers();
        let consumed = captured
            || key(code).is_some_and(|key| fire(Keystroke { key, modifiers }, old.is_some()));
        self.record_down(code, consumed);
        consumed
    }
    fn has(&self, left: N, right: N) -> bool {
        self.keys
            .iter()
            .any(|(code, _)| *code == left || *code == right)
    }
}
fn logical_key(
    code: N,
    unicode: u32,
    shift: bool,
    caps_lock: Option<bool>,
    key_text: &str,
) -> String {
    let named = match code {
        N::ShiftLeft | N::ShiftRight => "Shift",
        N::AltLeft | N::AltRight => "Alt",
        N::CtrlLeft | N::CtrlRight => "Control",
        N::MetaLeft | N::MetaRight => "Meta",
        N::DpadLeft => "ArrowLeft",
        N::DpadRight => "ArrowRight",
        N::DpadDown => "ArrowDown",
        N::DpadUp => "ArrowUp",
        N::MoveHome => "Home",
        N::MoveEnd => "End",
        N::Del => "Backspace",
        N::ForwardDel => "Delete",
        N::Enter | N::NumpadEnter => "Enter",
        N::Escape => "Escape",
        N::Tab => "Tab",
        N::Space => " ",
        N::PageUp => "PageUp",
        N::PageDown => "PageDown",
        N::Insert => "Insert",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_owned();
    }
    let mapped = key(code);
    if let Some(ch) = char::from_u32(unicode).filter(|ch| !ch.is_control()) {
        let ch = match mapped {
            Some(Key::Char(base))
                if base.is_ascii_alphabetic() && ch.eq_ignore_ascii_case(&base) =>
            {
                if let Some(caps_lock) = caps_lock {
                    if shift ^ caps_lock {
                        base.to_ascii_uppercase()
                    } else {
                        base.to_ascii_lowercase()
                    }
                } else {
                    // When Caps Lock state is unavailable, preserve the OS-produced
                    // character case rather than forcing lowercase.
                    ch
                }
            }
            _ => ch,
        };
        return ch.to_string();
    }
    let uppercase = shift ^ caps_lock.unwrap_or(false);
    if let Some(mapped) = mapped {
        match mapped {
            Key::Char(ch) => {
                let ch = if ch.is_ascii_alphabetic() {
                    if uppercase {
                        ch.to_ascii_uppercase()
                    } else {
                        ch.to_ascii_lowercase()
                    }
                } else {
                    ch
                };
                return ch.to_string();
            }
            Key::F(n) => return format!("F{n}"),
            _ => {}
        }
    }
    if key_text.is_empty() || key_text.starts_with("KEYCODE_") {
        "Unidentified".into()
    } else {
        key_text.to_owned()
    }
}

fn key(code: N) -> Option<Key> {
    use Key as K;
    Some(match code {
        N::A => K::Char('a'),
        N::B => K::Char('b'),
        N::C => K::Char('c'),
        N::D => K::Char('d'),
        N::E => K::Char('e'),
        N::F => K::Char('f'),
        N::G => K::Char('g'),
        N::H => K::Char('h'),
        N::I => K::Char('i'),
        N::J => K::Char('j'),
        N::K => K::Char('k'),
        N::L => K::Char('l'),
        N::M => K::Char('m'),
        N::N => K::Char('n'),
        N::O => K::Char('o'),
        N::P => K::Char('p'),
        N::Q => K::Char('q'),
        N::R => K::Char('r'),
        N::S => K::Char('s'),
        N::T => K::Char('t'),
        N::U => K::Char('u'),
        N::V => K::Char('v'),
        N::W => K::Char('w'),
        N::X => K::Char('x'),
        N::Y => K::Char('y'),
        N::Z => K::Char('z'),
        N::Digit0 => K::Char('0'),
        N::Digit1 => K::Char('1'),
        N::Digit2 => K::Char('2'),
        N::Digit3 => K::Char('3'),
        N::Digit4 => K::Char('4'),
        N::Digit5 => K::Char('5'),
        N::Digit6 => K::Char('6'),
        N::Digit7 => K::Char('7'),
        N::Digit8 => K::Char('8'),
        N::Digit9 => K::Char('9'),
        N::F1 => K::F(1),
        N::F2 => K::F(2),
        N::F3 => K::F(3),
        N::F4 => K::F(4),
        N::F5 => K::F(5),
        N::F6 => K::F(6),
        N::F7 => K::F(7),
        N::F8 => K::F(8),
        N::F9 => K::F(9),
        N::F10 => K::F(10),
        N::F11 => K::F(11),
        N::F12 => K::F(12),
        N::Enter | N::NumpadEnter => K::Enter,
        N::Escape => K::Escape,
        N::Tab => K::Tab,
        N::Space => K::Char(' '),
        N::Del => K::Backspace,
        N::ForwardDel => K::Delete,
        N::Insert => K::Insert,
        N::DpadUp => K::Up,
        N::DpadDown => K::Down,
        N::DpadLeft => K::Left,
        N::DpadRight => K::Right,
        N::MoveHome => K::Home,
        N::MoveEnd => K::End,
        N::PageUp => K::PageUp,
        N::PageDown => K::PageDown,
        N::Comma => K::Char(','),
        N::Period => K::Char('.'),
        N::Grave => K::Char('`'),
        N::Minus => K::Char('-'),
        N::Equals => K::Char('='),
        N::LeftBracket => K::Char('['),
        N::RightBracket => K::Char(']'),
        N::Backslash => K::Char('\\'),
        N::Semicolon => K::Char(';'),
        N::Apostrophe => K::Char('\''),
        N::Slash => K::Char('/'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeat_and_consumed_release_survive_callback_borrowing() {
        let mut held = Held::default();
        assert!(held.event(1, N::A, false, |_, repeat| {
            assert!(!repeat);
            true
        }));
        assert!(!held.event(1, N::A, false, |_, repeat| {
            assert!(repeat);
            false
        }));
        assert!(held.event(2, N::A, false, |_, _| panic!("up must not dispatch")));
        assert!(!held.event(2, N::A, false, |_, _| false));
    }
    #[test]
    fn captured_key_down_consumes_matching_release() {
        let mut held = Held::default();
        assert!(held.event(1, N::Enter, true, |_, _| panic!(
            "captured key must not run shortcut callback"
        )));
        assert!(held.event(2, N::Enter, false, |_, _| false));
        assert!(!held.event(2, N::Enter, false, |_, _| false));
    }
    #[test]
    fn right_modifier_release_clears_externally_synced_left_modifier() {
        let mut held = Held::default();
        held.sync_modifiers(
            N::A,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        );
        assert!(held.modifiers().shift);
        held.sync_modifiers(N::ShiftRight, Modifiers::default());
        assert!(!held.modifiers().shift);
    }
    #[test]
    fn cancellation_does_not_require_a_decodable_key_code() {
        let mut held = Held::default();
        held.event(1, N::CtrlLeft, false, |_, _| false);
        assert!(held.cancel(0));
        assert!(held.keys.is_empty());
    }
    #[test]
    fn modifier_state_and_cancel_are_native_only() {
        let mut held = Held::default();
        held.event(1, N::CtrlLeft, false, |_, _| false);
        held.event(1, N::C, false, |stroke, _| {
            assert!(stroke.modifiers.ctrl);
            false
        });
        held.event(0, N::C, false, |_, _| false);
        held.event(1, N::C, false, |stroke, _| {
            assert!(!stroke.modifiers.ctrl);
            false
        });
    }
    #[test]
    fn logical_key_maps_space_and_letters_when_unicode_is_zero() {
        assert_eq!(
            logical_key(N::Space, 0, true, Some(false), "KEYCODE_SPACE"),
            " "
        );
        assert_eq!(logical_key(N::A, 0, false, Some(false), "KEYCODE_A"), "a");
        assert_eq!(logical_key(N::A, 0, true, Some(false), "KEYCODE_A"), "A");
        assert_eq!(
            logical_key(N::A, 'A' as u32, false, Some(false), "KEYCODE_A"),
            "a"
        );
        assert_eq!(
            logical_key(N::A, 'A' as u32, false, Some(true), "KEYCODE_A"),
            "A"
        );
        assert_eq!(
            logical_key(N::A, 'A' as u32, true, Some(true), "KEYCODE_A"),
            "a"
        );
        // When Caps Lock state is unavailable, preserve the OS-produced case.
        assert_eq!(logical_key(N::A, 'A' as u32, false, None, "KEYCODE_A"), "A");
        assert_eq!(logical_key(N::A, 'a' as u32, true, None, "KEYCODE_A"), "a");
        assert_eq!(
            logical_key(N::F5, 0, false, Some(false), "KEYCODE_F5"),
            "F5"
        );
        assert_eq!(
            logical_key(N::A, 'ä' as u32, false, Some(false), "KEYCODE_A"),
            "ä"
        );
        assert_eq!(
            logical_key(N::NumpadEnter, 0, false, Some(false), ""),
            "Enter"
        );
    }
    #[test]
    fn walk_keyboard_ancestors_follows_override_and_breaks_cycles() {
        // Portal node 5 mounts under root 1, but its logical key_parent is 3 -> 2 -> 1.
        let mut visited = Vec::new();
        walk_keyboard_ancestors(
            5u32,
            |&id| match id {
                5 => Some(3),
                3 => Some(2),
                2 => Some(1),
                _ => None,
            },
            |&id| {
                visited.push(id);
                false
            },
        );
        assert_eq!(visited, vec![5, 3, 2, 1]);

        // Cycle terminates cleanly at the 1024-step bound.
        let mut count = 0usize;
        let hit = walk_keyboard_ancestors(
            1u32,
            |&id| Some(if id == 1 { 2 } else { 1 }),
            |_| {
                count += 1;
                false
            },
        );
        assert_eq!(hit, None);
        assert_eq!(count, 1024);
    }
    #[test]
    fn key_focus_controls_and_covered_modals_match_core_rules() {
        use nsh_core::keymap::{KeyBinding, KeyFocus, KeyMatch, When};
        let enter = Keystroke {
            key: Key::Enter,
            modifiers: Modifiers::default(),
        };
        let table = Keymap::new(vec![
            KeyBinding::new(1, enter, When::NotEditing, false),
            KeyBinding::new(2, enter, When::NotEditing, false).focus_keys(true),
        ]);
        // Focused pressable control: its own `for_focus` binding wins; non-focus bindings give way.
        let mut control = KeyFocus::default();
        control.control = true;
        assert_eq!(
            table.find_with(nsh_core::ROOT_ID, enter, control, false),
            KeyMatch::Fire(2)
        );
        // Same control covered by a newer modal: `for_focus` is ignored, dialog binding wins.
        let mut covered = KeyFocus::default();
        covered.covered = true;
        assert_eq!(
            table.find_with(nsh_core::ROOT_ID, enter, covered, false),
            KeyMatch::Fire(1)
        );
        // Native editor: `When::NotEditing` is skipped so editor handles Enter natively.
        let mut editing = KeyFocus::default();
        editing.editing = true;
        assert_eq!(
            table.find_with(nsh_core::ROOT_ID, enter, editing, false),
            KeyMatch::None
        );
    }
    #[test]
    fn sync_modifiers_tracks_external_state_without_overwriting_active_modifier() {
        let mut held = Held::default();
        held.sync_modifiers(
            N::B,
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            },
        );
        assert!(held.modifiers().ctrl);
        assert!(held.modifiers().shift);
        assert!(!held.modifiers().alt);
        // Pressing Ctrl itself must not pre-populate Ctrl before held.event checks repeat.
        let mut fresh = Held::default();
        fresh.sync_modifiers(
            N::CtrlLeft,
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        );
        assert!(!fresh.modifiers().ctrl);
    }
    #[test]
    fn keymap_window_maps_root_to_hosted_main_window_only() {
        let mut snapshot = Snapshot::default();
        assert_eq!(snapshot.keymap_window(nsh_core::ROOT_ID), nsh_core::ROOT_ID);
        assert_eq!(snapshot.keymap_window(42), 42);
        snapshot.hosted_main_window = Some(7);
        assert_eq!(snapshot.keymap_window(nsh_core::ROOT_ID), 7);
        assert_eq!(snapshot.keymap_window(42), 42);
    }
}
