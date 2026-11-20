//! The deepest hovered NSH view selects the owning window's pointer. Exiting or
//! retiring its subtree restores the parent/host choice without a global filter.
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use nsh_core::Cursor;

use crate::render::node::{Node, Result};

#[derive(Default)]
pub(crate) struct State {
    hovered: RefCell<Vec<Weak<Node>>>,
    resize: Cell<Option<Cursor>>,
    pointer_auto: Cell<Option<Cursor>>,
    applied: Cell<Cursor>,
}

impl State {
    pub(crate) fn update(&self, owner: &Node, hovered: bool) -> Result<()> {
        {
            let mut nodes = self.hovered.borrow_mut();
            nodes.retain(|entry| {
                entry
                    .upgrade()
                    .is_some_and(|node| node.id != owner.id && node.input_visible())
            });
            if hovered && let Some(node) = owner.self_ref() {
                nodes.push(Rc::downgrade(&node));
            }
        }
        self.apply(owner)
    }

    pub(crate) fn set_resize(&self, owner: &Node, cursor: Option<Cursor>) -> Result<()> {
        if self.resize.replace(cursor) != cursor {
            self.apply(owner)?;
        }
        Ok(())
    }

    pub(crate) fn set_pointer_auto(&self, owner: &Node, cursor: Option<Cursor>) -> Result<()> {
        if self.pointer_auto.replace(cursor) != cursor {
            self.apply(owner)?;
        }
        Ok(())
    }

    pub(crate) fn clear_dynamic(&self, owner: &Node) -> Result<()> {
        let changed = self.resize.take().is_some() | self.pointer_auto.take().is_some();
        if changed {
            self.apply(owner)?;
        }
        Ok(())
    }

    fn apply(&self, owner: &Node) -> Result<()> {
        let cursor = if let Some(resize) = self.resize.get() {
            resize
        } else {
            let explicit = self
                .hovered
                .borrow()
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
            explicit
                .or_else(|| self.pointer_auto.get())
                .unwrap_or(Cursor::Auto)
        };
        if self.applied.replace(cursor) != cursor {
            owner.host.cursor(cursor)?;
        }
        Ok(())
    }
}
