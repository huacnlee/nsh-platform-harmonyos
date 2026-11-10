//! The deepest hovered NSH view selects the owning window's pointer. Exiting or
//! retiring its subtree restores the parent/host choice without a global filter.
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

use nsh_core::Cursor;

use crate::render::node::{Node, Result};
#[derive(Default)]
pub(crate) struct State {
    hovered: RefCell<Vec<Weak<Node>>>,
    applied: std::cell::Cell<Cursor>,
}
impl State {
    pub(crate) fn update(&self, owner: &Node, hovered: bool) -> Result<()> {
        let mut nodes = self.hovered.borrow_mut();
        nodes.retain(|entry| {
            entry
                .upgrade()
                .is_some_and(|node| node.id != owner.id && node.input_visible())
        });
        if hovered && let Some(node) = owner.self_ref() {
            nodes.push(Rc::downgrade(&node));
        }
        let cursor = nodes
            .iter()
            .filter_map(Weak::upgrade)
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
            .map_or(Cursor::Auto, |(_, cursor)| cursor);
        if self.applied.replace(cursor) != cursor {
            owner.host.cursor(cursor)?;
        }
        Ok(())
    }
}
