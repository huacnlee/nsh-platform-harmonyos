//! Focus, press dispatch and pointer payload coordinates.

use std::rc::Rc;

use nsh_core::{EventKind, EventMask, EventPayload, Points, ViewKind};

use super::{Node, Result, callback};
use crate::text::typography::NativeText;

impl Node {
    pub(crate) fn input_priority(&self) -> bool {
        self.pinned.get() != (0.0, 0.0)
    }
    pub(crate) fn input_visible(&self) -> bool {
        if !self.active.get()
            || self.retained.get()
            || self.leaving.get()
            || self.backface_away.get()
            || self.style.borrow().hidden
            || !self.header_item_visible()
        {
            return false;
        }
        if self.screen.borrow().as_ref().is_some_and(|s| !s.is_top()) {
            return false;
        }
        true
    }
    pub(crate) fn visible_in_tree(&self) -> bool {
        self.input_visible()
            && self
                .parent_node
                .borrow()
                .upgrade()
                .is_none_or(|parent| parent.visible_in_tree())
    }
    pub(crate) fn emit(&self, event: EventKind, payload: EventPayload) {
        if self.leaving.get() && event != EventKind::Dismiss {
            return;
        }
        if self.active.get() || event == EventKind::Dismiss {
            if matches!(
                event,
                EventKind::Focus | EventKind::PressDown | EventKind::Press
            ) {
                self.host.activate(self.dispatcher.clone());
            }
            if matches!(event, EventKind::Focus | EventKind::Blur) {
                if matches!(self.kind, ViewKind::Input | ViewKind::Native)
                    && let Some(route) = self.dispatcher.upgrade()
                {
                    let mut keyboard = route.keyboard.lock().unwrap();
                    if event == EventKind::Focus {
                        keyboard.focused = Some(self.id);
                        if self.kind == ViewKind::Input {
                            keyboard.input = Some(self.id);
                        }
                    } else {
                        if keyboard.focused == Some(self.id) {
                            keyboard.focused = None;
                        }
                        if keyboard.input == Some(self.id) {
                            keyboard.input = None;
                        }
                    }
                }
                callback(|| {
                    self.interaction.state(
                        self,
                        nsh_core::tailwind::state::FOCUSED,
                        event == EventKind::Focus,
                    )
                });
            }
            self.control.dispatch_event(self.id, event, payload);
        }
    }
    pub(crate) fn set_events(self: &Rc<Self>, events: EventMask) -> Result<()> {
        self.interaction.events(self, events)
    }
    pub(crate) fn text_listens(&self, event: EventKind) -> bool {
        self.raster
            .borrow()
            .as_ref()
            .and_then(|layout| layout.0.downcast_ref::<NativeText>())
            .is_some_and(|raster| {
                raster
                    .links
                    .iter()
                    .any(|(link, _)| link.events.contains(event))
            })
    }
    pub(crate) fn press(&self, event: EventKind, x: f32, y: f32) {
        if !self.touches.borrow().press_allowed(self.id) {
            return;
        }
        let target = {
            let raster = self.raster.borrow();
            raster
                .as_ref()
                .and_then(|layout| layout.0.downcast_ref::<NativeText>())
                .and_then(|raster| {
                    let content = self.content.get();
                    let x = x - content.x;
                    let y = y - content.y;
                    raster
                        .links
                        .iter()
                        .filter(|(link, rects)| {
                            link.events.contains(event)
                                && rects
                                    .iter()
                                    .any(|r| x >= r[0] && x < r[2] && y >= r[1] && y < r[3])
                        })
                        .min_by_key(|(link, _)| link.end - link.start)
                        .map(|(link, _)| link.node)
                })
        };
        let payload = self.press_payload(x, y);
        if let Some(id) = target {
            if self.active.get() {
                self.control.dispatch_event(id, event, payload);
            }
        } else if self.interaction.mask().contains(event) {
            self.emit(event, payload);
        }
    }
    pub(crate) fn pointer_positions(
        &self,
        x: f32,
        y: f32,
    ) -> (nsh_core::Point<Points>, nsh_core::Point<Points>) {
        let position = nsh_core::Point::new(Points(x), Points(y));
        let mut page_position = position;
        if self.id != nsh_core::ROOT_ID && self.kind != ViewKind::Modal {
            let mut top = self.parent_node.borrow().upgrade();
            while let Some(root) = top {
                if root.id == nsh_core::ROOT_ID || root.kind == ViewKind::Modal {
                    let mapped =
                        self.input_in_root(&root).ok().flatten().and_then(|matrix| {
                            crate::render::geometry::page_point(&matrix.0, (x, y))
                        });
                    if let Some((x, y)) = mapped {
                        page_position = nsh_core::Point::new(Points(x), Points(y));
                    } else if let (Ok(node), Ok(origin)) = (
                        self.mounted().borrow().layout_position_in_window(),
                        root.mounted().borrow().layout_position_in_window(),
                    ) {
                        page_position = nsh_core::Point::new(
                            Points((node.x - origin.x) as f32 / self.scale.get() + x),
                            Points((node.y - origin.y) as f32 / self.scale.get() + y),
                        );
                    }
                    break;
                }
                top = root.parent_node.borrow().upgrade();
            }
        }
        (position, page_position)
    }
    pub(crate) fn press_payload(&self, x: f32, y: f32) -> EventPayload {
        let (position, page_position) = self.pointer_positions(x, y);
        EventPayload::Press(nsh_core::PressEvent {
            position,
            page_position,
            modifiers: self.host.modifiers(),
            clicks: self.touches.borrow().clicks(),
        })
    }
    pub(crate) fn mouse_payload(
        &self,
        x: f32,
        y: f32,
        button: Option<nsh_core::MouseButton>,
    ) -> EventPayload {
        let (position, page_position) = self.pointer_positions(x, y);
        EventPayload::Mouse(nsh_core::MouseEvent::new(
            position,
            page_position,
            self.host.modifiers(),
            button,
        ))
    }
    pub(crate) fn context_menu_payload(&self, x: f32, y: f32) -> EventPayload {
        let (position, page_position) = self.pointer_positions(x, y);
        EventPayload::ContextMenu(nsh_core::ContextMenuEvent::new(
            position,
            page_position,
            self.host.modifiers(),
            Some(nsh_core::PointerType::Mouse),
        ))
    }
    pub(crate) fn press_feedback_allowed(&self) -> bool {
        if !self.touches.borrow().press_allowed(self.id) {
            return false;
        }
        if self.kind == ViewKind::Input {
            return true;
        }
        let mut policy = self
            .scroll
            .borrow()
            .as_ref()
            .map(|scroll| scroll.keyboard_taps());
        let mut parent = self.parent_node.borrow().upgrade();
        while policy.is_none() {
            let Some(node) = parent else { break };
            if node.kind == ViewKind::Input {
                return true;
            }
            policy = node
                .scroll
                .borrow()
                .as_ref()
                .map(|scroll| scroll.keyboard_taps());
            parent = node.parent_node.borrow().upgrade();
        }
        policy != Some(nsh_core::KeyboardTaps::Never)
            || self.platform.get().keyboard_height.0 <= 0.0
            || self
                .dispatcher
                .upgrade()
                .is_none_or(|route| route.keyboard.lock().unwrap().input.is_none())
    }
}
