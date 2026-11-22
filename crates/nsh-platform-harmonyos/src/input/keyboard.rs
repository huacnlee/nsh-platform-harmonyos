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
        if self
            .modal
            .borrow()
            .as_ref()
            .is_some_and(|modal| modal.is_desktop_window())
        {
            return self.id;
        }
        let mut parent = self.parent_node.borrow().upgrade();
        while let Some(node) = parent {
            if node
                .modal
                .borrow()
                .as_ref()
                .is_some_and(|modal| modal.is_desktop_window())
            {
                return node.id;
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
    pub(crate) quit_on_request: bool,
    pub(crate) desktop: bool,
    pub(crate) focused: Option<NodeId>,
    pub(crate) input: Option<NodeId>,
    pub(crate) selection: Option<NodeId>,
    pub(crate) composing: bool,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            map: Default::default(),
            menu: None,
            quit_on_request: true,
            desktop: false,
            focused: None,
            input: None,
            selection: None,
            composing: false,
        }
    }
}
#[derive(Default)]
struct Held {
    keys: Vec<(N, bool)>,
    last_editor_key: Option<(i64, i32, N)>,
}

struct KeyInput<'a> {
    action: i32,
    code: N,
    unicode: u32,
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
                key_text: &event.key_text,
                pressed_keys: None,
                explicit_modifiers: Some(Modifiers {
                    ctrl: event.ctrl,
                    alt: event.alt,
                    shift: event.shift,
                    cmd: event.cmd,
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
        let target = focus.as_ref().unwrap_or(owner);
        let mut ancestor = Some(target.clone());
        let (mut observes, mut captures) = (false, false);
        while let Some(node) = ancestor {
            let (listens, prevents) = node.interaction.key_interest(kind, stroke.as_ref());
            observes |= listens;
            captures |= prevents;
            if node
                .modal
                .borrow()
                .as_ref()
                .is_some_and(|modal| !modal.is_main_window_host())
            {
                break;
            }
            ancestor = node.parent_node.borrow().upgrade();
        }
        if (observes || captures) && !route.keyboard.lock().unwrap().composing {
            let mut details = nsh_core::KeyEvent::default();
            details.key = logical_key(code, input.unicode, modifiers.shift, input.key_text).into();
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
            details.repeat = action == 1 && held_before;
            details.default_prevented = captures;
            target
                .control
                .dispatch_event(target.id, kind, nsh_core::EventPayload::Key(details));
        } else {
            captures = false;
        }
        let allow_key_scroll = !observes
            && focus
                .as_ref()
                .is_none_or(|node| node.scroll.borrow().is_some());
        let shortcut = held.event(action, code, captures, |stroke, repeat| {
            let snapshot = route.keyboard.lock().unwrap();
            if snapshot.composing {
                return false;
            }
            if snapshot.focused.is_none()
                && let Some(id) = snapshot.selection
            {
                use crate::text::selection::controller::Command;
                let command = match stroke.key {
                    Key::Escape if stroke.modifiers == Modifiers::default() => Some(Command::Clear),
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
                    return true;
                }
            }
            let matched = snapshot
                .map
                .find_in(window, stroke, snapshot.focused.is_some(), repeat);
            if let Some(id) = matched.fired() {
                route.send(crate::app::runtime::Message::HostEvent(
                    nsh_core::EventKind::KeyCommand,
                    nsh_core::EventPayload::Value(nsh_core::PayloadValue::Number(id as f64)),
                ));
            }
            if matched.consumed() {
                return true;
            }
            if !repeat
                && snapshot.desktop
                && let Some(menu) = snapshot.menu.as_deref()
                && let Some(shortcut) = crate::services::desktop::shortcut_from_keystroke(stroke)
                && let Some(action) = nsh_core::desktop::MenuItem::find_shortcut(menu, &shortcut)
                && (snapshot.focused.is_none() || action.fires_while_editing())
            {
                route.send(crate::app::runtime::Message::DesktopMenuAction {
                    id: action.id,
                    role: action.role,
                });
                return true;
            }
            if !repeat
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
                return true;
            }
            if !repeat {
                if stroke.key == Key::Escape && stroke.modifiers == Modifiers::default() {
                    if snapshot.input.is_some() {
                        if !snapshot.desktop {
                            route.send(crate::app::runtime::Message::DismissKeyboard);
                            return true;
                        }
                        drop(snapshot);
                        return back(route);
                    }
                    drop(snapshot);
                    return back(route);
                }
                if snapshot.focused.is_none()
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
                    return back(route);
                }
            }
            if snapshot.desktop
                && snapshot.focused.is_none()
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
                    return true;
                }
            }
            false
        });
        shortcut || captures
    }
}

fn back(route: &crate::app::runtime::Dispatcher) -> bool {
    if route.wants_back.load(std::sync::atomic::Ordering::Acquire) {
        route.send(crate::app::runtime::Message::Back);
        true
    } else {
        false
    }
}

/// Root and native dialog boundaries receive bubbling keys only from their own
/// focus subtree. ArkUI retires the handler with its node on the UI thread.
pub(crate) fn install(
    owner: &std::rc::Rc<crate::render::node::Node>,
) -> crate::render::node::Result<()> {
    owner.native.borrow().set_focusable(true)?;
    if owner.platform.get().os.is_desktop() {
        // ArkUI delegates focus requested on a container to its first focusable
        // leaf descendant (e.g. a search TextInput). A childless leaf focus sink
        // inside the root absorbs surface-level focus so global shortcuts bubble
        // to `on_key_event` without focusing an editor or clearing selections.
        let sink = ohos_arkui_binding::component::built_in_component::Stack::new()?.into_node();
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
        }
    }
    fn cancel(&mut self, action: i32) -> bool {
        if action == 0 {
            self.keys.clear();
            self.last_editor_key = None;
            true
        } else {
            false
        }
    }
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
        if let Some(index) = old {
            self.keys[index].1 |= consumed;
        } else {
            self.keys.push((code, consumed));
        }
        consumed
    }
    fn has(&self, left: N, right: N) -> bool {
        self.keys
            .iter()
            .any(|(code, _)| *code == left || *code == right)
    }
}
fn logical_key(code: N, unicode: u32, shift: bool, key_text: &str) -> String {
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
    if let Some(ch) = char::from_u32(unicode).filter(|ch| !ch.is_control()) {
        return ch.to_string();
    }
    if let Some(mapped) = key(code) {
        match mapped {
            Key::Char(ch) => {
                let ch = if shift && ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
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
        assert_eq!(logical_key(N::Space, 0, true, "KEYCODE_SPACE"), " ");
        assert_eq!(logical_key(N::A, 0, false, "KEYCODE_A"), "a");
        assert_eq!(logical_key(N::A, 0, true, "KEYCODE_A"), "A");
        assert_eq!(logical_key(N::F5, 0, false, "KEYCODE_F5"), "F5");
        assert_eq!(logical_key(N::A, 'ä' as u32, false, "KEYCODE_A"), "ä");
        assert_eq!(logical_key(N::NumpadEnter, 0, false, ""), "Enter");
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
}
