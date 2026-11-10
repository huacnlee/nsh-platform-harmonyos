//! Focus-scoped ArkUI key events; mounted bindings decide consumption before JS runs.
use std::sync::{Arc, Mutex};

use nsh_core::{
    NodeId,
    keymap::{Key, KeyMatch, Keymap, Keystroke, Modifiers},
};
use ohos_arkui_binding::{
    component::attribute::{ArkUICommonAttribute, ArkUIEvent},
    event::{KeyCode as N, KeyEvent, KeyEventType},
};

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
#[derive(Default)]
pub(crate) struct Snapshot {
    pub(crate) map: Arc<Keymap>,
    pub(crate) focused: Option<NodeId>,
    pub(crate) input: Option<NodeId>,
    pub(crate) selection: Option<NodeId>,
}
#[derive(Default)]
struct Held {
    keys: Vec<(N, bool)>,
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
        self.held.lock().unwrap().keys.clear();
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
    ) -> bool {
        let action = match event.event_type() {
            KeyEventType::Down => 1,
            KeyEventType::Up => 2,
            _ => return false,
        };
        let code = event.key_code();
        let mut held = self.held.lock().unwrap();
        // Focus may have entered while modifiers were held in the host. Read the
        // event's pressed set instead of depending on keydowns outside our tree.
        let mut pressed = [0; 64];
        if let Ok(count) = event.pressed_keys(&mut pressed) {
            let keys = &pressed[..count.min(pressed.len())];
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
        }
        held.event(action, code, |stroke, repeat| {
            let snapshot = route.keyboard.lock().unwrap();
            if snapshot.focused.is_none()
                && let Some(id) = snapshot.selection
            {
                use crate::text::selection::controller::Command;
                let command = match stroke.key {
                    Key::Escape if stroke.modifiers == Modifiers::default() => Some(Command::Clear),
                    Key::Char('c')
                        if stroke.modifiers.ctrl
                            && !stroke.modifiers.alt
                            && !stroke.modifiers.cmd =>
                    {
                        Some(Command::Copy)
                    }
                    Key::Char('a')
                        if stroke.modifiers.ctrl
                            && !stroke.modifiers.alt
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
                .find(stroke, snapshot.focused.is_some(), repeat);
            if let KeyMatch::Fire(id) = matched {
                route.send(crate::app::runtime::Message::HostEvent(
                    nsh_core::EventKind::KeyCommand,
                    nsh_core::EventPayload::Value(nsh_core::PayloadValue::Number(id as f64)),
                ));
            }
            if matched.consumed() {
                return true;
            }
            if !repeat {
                if stroke.key == Key::Escape && stroke.modifiers == Modifiers::default() {
                    if snapshot.input.is_some() {
                        route.send(crate::app::runtime::Message::DismissKeyboard);
                        return true;
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
            false
        })
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
    let weak = std::rc::Rc::downgrade(owner);
    owner.native.borrow_mut().on_key_event(move |event| {
        if let Some(owner) = weak.upgrade()
            && let Some(event) = event.key_event()
            && let Some(route) = owner.dispatcher.upgrade()
            && owner.host.key_event(event, &route)
        {
            event.set_consumed(true);
            event.stop_propagation(true);
        }
    });
    Ok(())
}

impl Held {
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
            true
        } else {
            false
        }
    }
    fn event(&mut self, action: i32, code: N, fire: impl FnOnce(Keystroke, bool) -> bool) -> bool {
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
        let consumed =
            key(code).is_some_and(|key| fire(Keystroke { key, modifiers }, old.is_some()));
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
        N::Enter => K::Enter,
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
        assert!(held.event(1, N::A, |_, repeat| {
            assert!(!repeat);
            true
        }));
        assert!(!held.event(1, N::A, |_, repeat| {
            assert!(repeat);
            false
        }));
        assert!(held.event(2, N::A, |_, _| panic!("up must not dispatch")));
        assert!(!held.event(2, N::A, |_, _| false));
    }
    #[test]
    fn cancellation_does_not_require_a_decodable_key_code() {
        let mut held = Held::default();
        held.event(1, N::CtrlLeft, |_, _| false);
        assert!(held.cancel(0));
        assert!(held.keys.is_empty());
    }
    #[test]
    fn modifier_state_and_cancel_are_native_only() {
        let mut held = Held::default();
        held.event(1, N::CtrlLeft, |_, _| false);
        held.event(1, N::C, |stroke, _| {
            assert!(stroke.modifiers.ctrl);
            false
        });
        held.event(0, N::C, |_, _| false);
        held.event(1, N::C, |stroke, _| {
            assert!(!stroke.modifiers.ctrl);
            false
        });
    }
}
