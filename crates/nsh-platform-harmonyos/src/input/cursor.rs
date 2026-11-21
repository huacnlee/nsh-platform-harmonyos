//! The deepest hovered NSH view selects the owning window's pointer. Exiting or
//! retiring its subtree restores the parent/host choice without a global filter.
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use nsh_core::{Cursor, NodeId};
use rustc_hash::FxHashMap;

use crate::render::node::{Node, Result};

#[derive(Default)]
struct WindowCursor {
    hovered: Vec<Weak<Node>>,
    resize: Option<Cursor>,
    pointer_auto: Option<Cursor>,
    applied: Cursor,
}

#[derive(Default)]
pub(crate) struct State {
    windows: RefCell<FxHashMap<NodeId, WindowCursor>>,
}

impl State {
    pub(crate) fn update(&self, owner: &Node, hovered: bool) -> Result<()> {
        let window_id = owner.key_window_id();
        {
            let mut windows = self.windows.borrow_mut();
            let entry = windows.entry(window_id).or_default();
            entry.hovered.retain(|item| {
                item.upgrade()
                    .is_some_and(|node| node.id != owner.id && node.input_visible())
            });
            if hovered && let Some(node) = owner.self_ref() {
                entry.hovered.push(Rc::downgrade(&node));
            }
        }
        self.apply(owner, window_id)
    }

    pub(crate) fn set_resize(&self, owner: &Node, cursor: Option<Cursor>) -> Result<()> {
        let window_id = owner.key_window_id();
        let changed = {
            let mut windows = self.windows.borrow_mut();
            let entry = windows.entry(window_id).or_default();
            if entry.resize != cursor {
                entry.resize = cursor;
                true
            } else {
                false
            }
        };
        if changed {
            self.apply(owner, window_id)?;
        }
        Ok(())
    }

    pub(crate) fn set_pointer_auto(&self, owner: &Node, cursor: Option<Cursor>) -> Result<()> {
        let window_id = owner.key_window_id();
        let changed = {
            let mut windows = self.windows.borrow_mut();
            let entry = windows.entry(window_id).or_default();
            if entry.pointer_auto != cursor {
                entry.pointer_auto = cursor;
                true
            } else {
                false
            }
        };
        if changed {
            self.apply(owner, window_id)?;
        }
        Ok(())
    }

    pub(crate) fn clear_dynamic(&self, owner: &Node) -> Result<()> {
        let window_id = owner.key_window_id();
        let changed = {
            let mut windows = self.windows.borrow_mut();
            let Some(entry) = windows.get_mut(&window_id) else {
                return Ok(());
            };
            entry.resize.take().is_some() | entry.pointer_auto.take().is_some()
        };
        if changed {
            self.apply(owner, window_id)?;
        }
        Ok(())
    }

    pub(crate) fn clear_window(&self, window_id: NodeId) {
        self.windows.borrow_mut().remove(&window_id);
    }

    fn apply(&self, owner: &Node, window_id: NodeId) -> Result<()> {
        let next = {
            let mut windows = self.windows.borrow_mut();
            let entry = windows.entry(window_id).or_default();
            let cursor = if let Some(resize) = entry.resize {
                resize
            } else {
                let explicit = entry
                    .hovered
                    .iter()
                    .filter_map(Weak::upgrade)
                    .filter(|node| node.input_visible())
                    .filter_map(|node| {
                        let cursor = node.style.borrow().cursor;
                        if cursor == Cursor::Auto {
                            return None;
                        }
                        let mut depth = 0;
                        let mut parent = node.parent_node.borrow().upgrade();
                        while let Some(node) = parent {
                            depth += 1;
                            parent = node.parent_node.borrow().upgrade();
                        }
                        Some((depth, cursor))
                    })
                    .max_by_key(|(depth, _)| *depth)
                    .map(|(_, cursor)| cursor);
                explicit.or(entry.pointer_auto).unwrap_or(Cursor::Auto)
            };
            if entry.applied != cursor {
                entry.applied = cursor;
                Some(cursor)
            } else {
                None
            }
        };
        if let Some(cursor) = next {
            owner.host.cursor(
                (window_id != nsh_core::ROOT_ID).then_some(window_id),
                cursor,
            )?;
        }
        Ok(())
    }
}
