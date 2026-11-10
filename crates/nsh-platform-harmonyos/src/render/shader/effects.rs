//! UI ownership of shader effects; native texture exports retain live input.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use nsh_core::{
    NodeId,
    animation::AnimationValue,
    shader::{ShaderLayer, ShaderRole},
};
use rustc_hash::FxHashMap;

use crate::render::node::{Node, Result};

#[derive(Default)]
pub(super) struct Effects {
    root: RefCell<Weak<Node>>,
    views: RefCell<FxHashMap<NodeId, State>>,
}

struct State {
    owner: Weak<Node>,
    target: Weak<Node>,
    layer: Arc<ShaderLayer>,
    active: Cell<bool>,
    navigation: Option<Rc<super::navigation::Navigation>>,
}

impl Effects {
    pub(super) fn wake(&self) {
        if let Some(root) = self.root.borrow().upgrade()
            && let Some(worker) = root.compositor.borrow().as_ref()
        {
            worker.wake();
        }
    }
    pub(super) fn root(&self, root: &Rc<Node>) {
        *self.root.borrow_mut() = Rc::downgrade(root);
    }

    pub(super) fn set(&self, owner: &Rc<Node>, layer: Arc<ShaderLayer>) -> Result<()> {
        let target = if matches!(
            layer.role,
            ShaderRole::Screen | ShaderRole::ScreenTransition
        ) {
            self.root.borrow().upgrade()
        } else {
            Some(owner.clone())
        };
        let Some(target) = target else {
            return Ok(());
        };
        self.set_on(owner, &target, layer, None)
    }
    fn set_on(
        &self,
        owner: &Rc<Node>,
        target: &Rc<Node>,
        layer: Arc<ShaderLayer>,
        navigation: Option<Rc<super::navigation::Navigation>>,
    ) -> Result<()> {
        let moved = self.views.borrow().get(&owner.id).is_some_and(|state| {
            state
                .target
                .upgrade()
                .is_none_or(|old| !Rc::ptr_eq(&old, target))
        });
        if moved {
            self.remove(owner.id)?;
        }
        self.views.borrow_mut().insert(
            owner.id,
            State {
                owner: Rc::downgrade(owner),
                target: Rc::downgrade(target),
                layer: layer.clone(),
                active: Cell::new(true),
                navigation,
            },
        );
        {
            let mut layers = target.shader_layers.borrow_mut();
            if let Some((_, old)) = layers.iter_mut().find(|(id, _)| *id == owner.id) {
                *old = layer;
            } else {
                layers.push((owner.id, layer));
            }
        }
        target.invalidate_composites();
        if let Some(scene) = target.scene.borrow().as_ref() {
            scene.require(target);
        }
        target.ensure_wrapper()?;
        target.restore_style()?;
        target.animator.request_frame();
        Ok(())
    }

    pub(super) fn remove(&self, id: NodeId) -> Result<()> {
        let state = self.views.borrow_mut().remove(&id);
        if let Some(state) = state
            && let Some(target) = state.target.upgrade()
        {
            target
                .shader_layers
                .borrow_mut()
                .retain(|(owner, _)| *owner != id);
            target.invalidate_composites();
            if let Some(scene) = target.scene.borrow().as_ref() {
                scene.require(&target);
            }
            target.ensure_wrapper()?;
            target.restore_style()?;
        }
        Ok(())
    }
    pub(super) fn begin_navigation(
        &self,
        owner: &Rc<Node>,
        lower: Option<Rc<Node>>,
        backwards: bool,
    ) -> Result<Option<Rc<super::navigation::Navigation>>> {
        let Some(spec) = owner
            .navigation_shader
            .borrow()
            .clone()
            .filter(|_| !owner.platform.get().reduce_motion)
        else {
            return Ok(None);
        };
        let Some(mut root) = self.root.borrow().upgrade() else {
            return Ok(None);
        };
        let mut ancestor = owner.parent_node.borrow().upgrade();
        while let Some(node) = ancestor {
            if node.kind == nsh_core::ViewKind::Modal {
                root = node;
                break;
            }
            ancestor = node.parent_node.borrow().upgrade();
        }
        let page = owner.kind == nsh_core::ViewKind::Screen;
        // Pages compose on the root, where the native tree supplies the live
        // destination after push/pop. Dialogs own a separate native surface.
        let target = if page { root.clone() } else { owner.clone() };
        let picture = if page {
            root.clone()
        } else {
            lower.unwrap_or(root)
        };
        let has_picture = picture.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper
                .composite
                .borrow()
                .as_ref()
                .is_some_and(|composite| composite.surface().is_some())
        });
        if !has_picture {
            return Ok(None);
        }
        let navigation = Rc::new(
            super::navigation::Navigation::new(owner.id, backwards).ok_or(
                crate::render::node::Error::Unsupported("navigation generation exhausted"),
            )?,
        );
        // Capture is queued before layer/visibility changes, so interruptions keep the displayed side.
        if let Some(wrapper) = picture.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.snapshot(navigation.key);
        }
        let interrupted: Vec<_> = self
            .views
            .borrow()
            .values()
            .filter(|state| {
                state
                    .target
                    .upgrade()
                    .is_some_and(|old| Rc::ptr_eq(&old, &target))
            })
            .filter_map(|state| state.navigation.as_ref().map(|n| n.key))
            .collect();
        for key in &interrupted {
            self.navigation_complete(*key);
        }
        self.set_on(owner, &target, spec.layer, Some(navigation.clone()))?;
        for key in interrupted {
            if key.owner != owner.id {
                self.remove(key.owner)?;
            }
        }
        if let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.shader_navigation(
                navigation.key,
                spec.duration,
                backwards,
                page && backwards,
            );
            return Ok(Some(navigation));
        }
        self.remove(owner.id)?;
        Ok(None)
    }
    pub(super) fn navigation_complete(&self, key: super::navigation::Key) {
        let owner = {
            let views = self.views.borrow();
            let Some(state) = views.get(&key.owner) else {
                return;
            };
            let Some(navigation) = state.navigation.as_ref() else {
                return;
            };
            if !navigation.complete(key) {
                return;
            }
            state.owner.upgrade()
        };
        if let Some(owner) = owner {
            owner.animator.request_frame();
        }
    }

    pub(super) fn slot(&self, id: NodeId, slot: u16, value: AnimationValue) {
        let target = self
            .views
            .borrow()
            .get(&id)
            .and_then(|state| state.target.upgrade());
        if let Some(target) = target
            && let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.shader_slot(id, slot, value);
        }
    }

    pub(super) fn transition(&self, id: NodeId, duration: Duration) {
        let target = self
            .views
            .borrow()
            .get(&id)
            .and_then(|state| state.target.upgrade());
        if let Some(target) = target
            && !target.platform.get().reduce_motion
            && let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.shader_transition(id, duration);
            target.animator.request_frame();
        }
    }

    pub(super) fn frame(&self) -> Result<bool> {
        let mut due = false;
        let mut targets: FxHashMap<NodeId, (Rc<Node>, bool)> = FxHashMap::default();
        // Restoring a native style reapplies animated slots, which reads this
        // table again. Keep membership immutable throughout the frame.
        for (&id, state) in self.views.borrow().iter() {
            let Some(owner) = state.owner.upgrade() else {
                continue;
            };
            let Some(target) = state.target.upgrade() else {
                continue;
            };
            let attached = owner.shaders.visibility(&owner).0;
            let active = if state.navigation.is_some() {
                // Closing dialogs can already be deactivated; their retained
                // native picture must keep ticking until navigation ends.
                true
            } else {
                // A screen shader belongs to the surface until unmounted,
                // even when its declaring page is behind another page. This
                // matches UIKit/AppKit and keeps the root's render ownership
                // stable throughout a navigation.
                attached
            };
            if active != state.active.replace(active) {
                target
                    .shader_layers
                    .borrow_mut()
                    .retain(|(owner, _)| *owner != id);
                if active {
                    target
                        .shader_layers
                        .borrow_mut()
                        .push((id, state.layer.clone()));
                }
                target.invalidate_composites();
                if let Some(scene) = target.scene.borrow().as_ref() {
                    scene.require(&target);
                }
                target.ensure_wrapper()?;
                target.restore_style()?;
            }
            let shown = active && (state.navigation.is_some() || super::in_sight(&target));
            let entry = targets.entry(target.id).or_insert((target, false));
            entry.1 |= shown;
        }
        for (_, (target, shown)) in targets {
            due |= target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                wrapper
                    .composite
                    .borrow()
                    .as_ref()
                    .is_some_and(|composite| composite.shader_frame(shown))
            });
        }
        Ok(due)
    }
}
