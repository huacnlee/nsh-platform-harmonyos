//! UI ownership of shader effects and their native input textures.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use nsh_core::{
    NavShader, NodeId, ViewKind,
    animation::AnimationValue,
    shader::{ShaderLayer, ShaderRole},
};
use rustc_hash::FxHashMap;

use super::navigation::{CaptureDemands, Event, HeldViews, Key, Navigation};
use crate::render::node::{Node, Result};

pub(super) fn unhidden_chain(node: &Rc<Node>) -> bool {
    let mut at = Some(node.clone());
    while let Some(current) = at {
        if !current.active.get()
            || current.retained.get()
            || current.is_settled_popped_screen()
            || current.style.borrow().hidden
        {
            return false;
        }
        at = current.parent_node.borrow().upgrade();
    }
    true
}

#[derive(Default)]
pub(super) struct Effects {
    root: RefCell<Weak<Node>>,
    views: RefCell<FxHashMap<NodeId, State>>,
    pending: RefCell<FxHashMap<NodeId, PendingNavigation>>,
    demands: RefCell<CaptureDemands>,
    held: RefCell<HeldViews>,
    // Reuse the root exporter for reversals while its presenting screen is mounted.
    capture_screen: RefCell<Option<Weak<Node>>>,
    // Native dialogs have independent output surfaces, ordered by presentation.
    modals: RefCell<Vec<Weak<Node>>>,
    capture_nodes: RefCell<FxHashMap<NodeId, Weak<Node>>>,
    logged_state: Cell<Option<(usize, usize)>>,
}

struct State {
    owner: Weak<Node>,
    target: Weak<Node>,
    layer: Arc<ShaderLayer>,
    active: Cell<bool>,
    navigation: Option<Rc<Navigation>>,
    #[cfg(feature = "dev")]
    transition_until: Cell<Option<Instant>>,
}

struct PendingNavigation {
    owner: Weak<Node>,
    target: Weak<Node>,
    picture_id: NodeId,
    spec: NavShader,
    navigation: Rc<Navigation>,
    page: bool,
}

impl Effects {
    pub(super) fn surface_active(&self) -> bool {
        self.root.borrow().upgrade().is_some_and(|root| {
            root.platform.get().lifecycle != nsh_engine::Lifecycle::Background
                && root
                    .dispatcher
                    .upgrade()
                    .is_some_and(|d| d.active.load(Ordering::Acquire))
        })
    }

    pub(super) fn hosts_retainable(&self) -> bool {
        self.root.borrow().upgrade().is_some_and(|root| {
            root.active.get()
                && root.host.check().is_ok()
                && root
                    .dispatcher
                    .upgrade()
                    .is_some_and(|d| d.live.load(Ordering::Acquire))
                && root
                    .wrapper
                    .try_borrow()
                    .ok()
                    .is_none_or(|wrapper| wrapper.as_ref().is_none_or(|w| w.is_context_live()))
        })
    }

    pub(super) fn root_node(&self) -> Option<Rc<Node>> {
        self.root.borrow().upgrade()
    }

    pub(super) fn max_overflow(&self, id: NodeId) -> f32 {
        self.views
            .borrow()
            .values()
            .filter(|s| s.target.upgrade().is_some_and(|t| t.id == id))
            .map(|s| s.layer.overflow.0)
            .fold(0.0, f32::max)
    }

    pub(super) fn finish_modal_close(&self, owner: &Rc<Node>, had_shader: bool) -> Result<()> {
        if had_shader {
            self.end_navigation(owner.id)?;
        }
        Ok(())
    }

    pub(super) fn navigation_key(&self, target_id: NodeId) -> Option<Key> {
        self.views
            .borrow()
            .values()
            .filter_map(|state| {
                let navigation = state.navigation.as_ref()?;
                (state.target.upgrade()?.id == target_id).then_some(navigation.key)
            })
            .max_by_key(|key| key.generation)
    }

    pub(super) fn modal_shown(&self, owner: &Rc<Node>, shown: bool) -> Result<()> {
        self.modals
            .borrow_mut()
            .retain(|weak| weak.upgrade().is_some_and(|node| !Rc::ptr_eq(&node, owner)));
        if shown {
            self.modals.borrow_mut().push(Rc::downgrade(owner));
        } else {
            return self.modal_presentation_changed(owner);
        }
        self.sync_modal_effects()
    }

    pub(super) fn modal_presentation_changed(&self, owner: &Rc<Node>) -> Result<()> {
        owner.modal_screen_layers.borrow_mut().clear();
        owner.modal_backdrop.set(None);
        owner.backdrop_layer.set(None);
        owner.invalidate_composites();
        self.sync_modal_effects()
    }

    fn sync_modal_effects(&self) -> Result<()> {
        let Some(root) = self.root_node() else {
            return Ok(());
        };
        if self.modals.borrow().is_empty() && root.backdrop_layer.get().is_none() {
            return Ok(());
        }
        let screen_layers: Vec<_> = root
            .shader_layers
            .borrow()
            .iter()
            .filter(|(id, layer)| {
                matches!(
                    layer.role,
                    ShaderRole::Screen | ShaderRole::ScreenTransition
                ) && self
                    .views
                    .borrow()
                    .get(id)
                    .is_some_and(|state| state.navigation.is_none())
            })
            .cloned()
            .collect();
        let modals: Vec<_> = self
            .modals
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|node| {
                node.modal.borrow().as_ref().is_some_and(|modal| {
                    modal.presentation() == nsh_core::ModalPresentation::FullScreen
                })
            })
            .collect();
        let screen_id = screen_layers.first().map(|(id, _)| *id);
        let source_layer = screen_id.filter(|_| !modals.is_empty());
        if root.backdrop_layer.replace(source_layer) != source_layer {
            root.invalidate_composites();
            root.ensure_wrapper()?;
            root.update_wrapper()?;
        }
        let mut lower = root.id;
        for (index, modal) in modals.iter().enumerate() {
            let backdrop = screen_id.map(|_| lower);
            lower = modal.id;
            let source_layer = screen_id.filter(|_| index + 1 < modals.len());
            if modal.backdrop_layer.replace(source_layer) == source_layer
                && modal.modal_backdrop.get() == backdrop
                && *modal.modal_screen_layers.borrow() == screen_layers
            {
                continue;
            }
            modal.modal_backdrop.set(backdrop);
            modal
                .modal_screen_layers
                .borrow_mut()
                .clone_from(&screen_layers);
            modal.invalidate_composites();
            modal.ensure_wrapper()?;
            modal.restore_style()?;
            modal.animator.request_frame();
        }
        Ok(())
    }

    pub(super) fn reconcile_hosts(&self) -> Result<()> {
        self.release_screen_input()?;
        let mut targets = Vec::new();
        for (&id, state) in self.views.borrow().iter() {
            if state.navigation.is_some() {
                continue;
            }
            let Some(owner) = state.owner.upgrade() else {
                continue;
            };
            let Some(target) = state.target.upgrade() else {
                continue;
            };
            let active = owner.shaders.visibility(&owner).0 && unhidden_chain(&owner);
            if active != state.active.replace(active) {
                let mut layers = target.shader_layers.borrow_mut();
                layers.retain(|(owner, _)| *owner != id);
                if active {
                    layers.push((id, state.layer.clone()));
                }
                drop(layers);
                targets.push(target);
            }
        }
        for target in targets {
            target.invalidate_composites();
            target.ensure_wrapper()?;
            target.restore_style()?;
        }
        self.sync_modal_effects()
    }

    fn cancel_on_worker(&self, keys: &[Key]) {
        if keys.is_empty() {
            return;
        }
        if let Some(root) = self.root.borrow().upgrade()
            && let Some(worker) = root.compositor.borrow().as_ref()
        {
            for &key in keys {
                worker.cancel_capture(key);
            }
        }
    }

    fn retain_root_input(&self, mut ancestor: Option<Rc<Node>>) {
        while let Some(node) = ancestor {
            if node.kind == ViewKind::Screen {
                *self.capture_screen.borrow_mut() = Some(Rc::downgrade(&node));
                break;
            }
            ancestor = node.parent_node.borrow().upgrade();
        }
    }

    pub(super) fn holds_root_input(&self) -> bool {
        self.capture_screen
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|screen| screen.active.get() && !screen.is_settled_popped_screen())
    }

    fn release_screen_input(&self) -> Result<()> {
        if self.capture_screen.borrow().is_some() && !self.holds_root_input() {
            self.capture_screen.borrow_mut().take();
            if let Some(root) = self.root_node() {
                root.invalidate_composites();
                root.ensure_wrapper()?;
                root.restore_style()?;
            }
        }
        Ok(())
    }

    fn sync_capture_roots(&self, reason: &str, force_log: bool) -> Result<()> {
        let root_node = self.root.borrow().upgrade();
        let allow_capture = root_node
            .as_ref()
            .is_some_and(|root| !root.platform.get().reduce_motion && self.surface_active());
        let active_demands = self.demands.borrow().len();

        let mut candidates: Vec<(NodeId, Rc<Node>, bool, bool)> = Vec::new();
        {
            let demands = self.demands.borrow();
            let mut nodes = self.capture_nodes.borrow_mut();
            let mut stale = Vec::new();
            for (&id, weak) in nodes.iter() {
                if let Some(node) = weak.upgrade() {
                    let wanted = allow_capture && demands.contains_picture(id);
                    candidates.push((id, node, wanted, true));
                    if !wanted {
                        stale.push(id);
                    }
                } else {
                    stale.push(id);
                }
            }
            for id in stale {
                nodes.remove(&id);
            }
            if let Some(root) = &root_node
                && !candidates.iter().any(|(id, _, _, _)| *id == root.id)
            {
                let wanted = allow_capture && demands.contains_picture(root.id);
                candidates.push((root.id, root.clone(), wanted, false));
            }
        }

        let mut failure = None;
        for (_, node, wanted, tracked) in &candidates {
            let capture = *wanted && node.active.get();
            let changed = node.navigation_capture.replace(capture) != capture;
            if node.active.get() && (changed || (*tracked && node.composite_dirty.get())) {
                let result = (|| -> Result<()> {
                    if let Some(scene) = node.scene.borrow().as_ref() {
                        scene.require(node);
                    }
                    node.invalidate_composites();
                    node.ensure_wrapper()?;
                    node.restore_style()
                })();
                if let Err(error) = result {
                    // Keep the failed host discoverable for the next actual
                    // reconciliation, even after its capture demand has ended.
                    node.composite_dirty.set(true);
                    self.capture_nodes
                        .borrow_mut()
                        .insert(node.id, Rc::downgrade(node));
                    failure.get_or_insert(error);
                }
            }
        }

        let nav_exported = {
            let mut count = self
                .capture_nodes
                .borrow()
                .values()
                .filter_map(Weak::upgrade)
                .filter(|node| node.active.get() && node.navigation_capture.get())
                .count();
            if let Some(root) = &root_node
                && !self.capture_nodes.borrow().contains_key(&root.id)
                && root.active.get()
                && root.navigation_capture.get()
            {
                count += 1;
            }
            count
        };
        let total_composites = root_node.as_ref().map_or(0, |root| {
            root.composite_nodes
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|node| {
                    node.active.get()
                        && node
                            .wrapper
                            .borrow()
                            .as_ref()
                            .is_some_and(|wrapper| wrapper.composite.borrow().is_some())
                })
                .count()
        });

        let snapshot_state = (active_demands, nav_exported);
        if force_log || self.logged_state.replace(Some(snapshot_state)) != Some(snapshot_state) {
            log::info!(
                "[NSH] nav-capture demand={reason} active={active_demands} exported={nav_exported} composites={total_composites}"
            );
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn log_idle_configured(&self) {
        if self.demands.borrow().is_empty() {
            let _ = self.sync_capture_roots("idle-configured", true);
        }
    }

    pub(super) fn reconcile_navigation_capture(&self, reduce_motion: bool) -> Result<()> {
        if reduce_motion {
            return self.cancel_navigation("reduce-motion");
        }
        self.sync_capture_roots("reconcile", false)
    }

    pub(super) fn cancel_navigation(&self, reason: &str) -> Result<()> {
        let fallback = reason == "reduce-motion";
        self.held.borrow_mut().clear();
        let pending: Vec<PendingNavigation> =
            self.pending.borrow_mut().drain().map(|(_, p)| p).collect();
        let cleared = self.demands.borrow_mut().clear();
        self.cancel_on_worker(&cleared);
        for item in pending {
            if fallback {
                item.navigation.mark_fallback(item.navigation.key);
            } else {
                item.navigation.complete(item.navigation.key);
            }
            if let Some(owner) = item.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }

        let nav_owners: Vec<(NodeId, Option<Rc<Node>>)> = self
            .views
            .borrow()
            .iter()
            .filter_map(|(&id, state)| {
                let nav = state.navigation.as_ref()?;
                nav.complete(nav.key);
                Some((id, state.owner.upgrade()))
            })
            .collect();
        let mut result = Ok(());
        for (id, owner) in nav_owners {
            result = result.and(self.detach_view(id));
            if let Some(owner) = owner {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        result.and(self.sync_capture_roots(reason, false))
    }

    pub(super) fn wake(&self) {
        if !self.surface_active() {
            let _ = self.cancel_navigation("background");
            return;
        }
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
        if target.id == nsh_core::ROOT_ID && owner.id != target.id {
            self.retain_root_input(Some(owner.clone()));
        }
        self.set_on(owner, &target, layer, None)
    }

    fn set_on(
        &self,
        owner: &Rc<Node>,
        target: &Rc<Node>,
        layer: Arc<ShaderLayer>,
        navigation: Option<Rc<Navigation>>,
    ) -> Result<()> {
        let is_non_nav = navigation.is_none();
        let precompile_layer = is_non_nav.then(|| layer.clone());
        if is_non_nav {
            self.held.borrow_mut().remove_view(owner.id);
        }
        let moved = self.views.borrow().get(&owner.id).is_some_and(|state| {
            state
                .target
                .upgrade()
                .is_none_or(|old| !Rc::ptr_eq(&old, target))
        });
        if moved {
            self.detach_view(owner.id)?;
        }
        self.views.borrow_mut().insert(
            owner.id,
            State {
                owner: Rc::downgrade(owner),
                target: Rc::downgrade(target),
                layer: layer.clone(),
                active: Cell::new(true),
                navigation,
                #[cfg(feature = "dev")]
                transition_until: Cell::new(None),
            },
        );
        let already_revealed = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper
                .composite
                .borrow()
                .as_ref()
                .is_some_and(|composite| composite.is_revealed())
        });
        let (changed_layers, layer_count) = {
            let mut layers = target.shader_layers.borrow_mut();
            let changed = if let Some((_, old)) = layers.iter_mut().find(|(id, _)| *id == owner.id)
            {
                if *old != layer {
                    *old = layer;
                    true
                } else {
                    false
                }
            } else {
                layers.push((owner.id, layer));
                true
            };
            (changed, layers.len())
        };
        if is_non_nav && already_revealed && changed_layers {
            log::info!(
                "[NSH] shader-layer-swap node={} layers={} passthrough=false",
                target.id,
                layer_count,
            );
        }
        target.invalidate_composites();
        if let Some(scene) = target.scene.borrow().as_ref() {
            scene.require(target);
        }
        target.ensure_wrapper()?;
        target.restore_style()?;
        // Enqueue precompile after wrapper/Composite creation so Task::Create
        // reaches the compositor before any shader compilation work.
        if let Some(layer) = precompile_layer
            && let Ok(worker) = crate::render::composition::worker::Worker::for_owner(target)
        {
            worker.precompile(layer);
        }
        target.animator.request_frame();
        Ok(())
    }

    fn detach_view(&self, id: NodeId) -> Result<()> {
        self.held.borrow_mut().remove_view(id);
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
            if target.active.get() {
                target.ensure_wrapper()?;
                target.restore_style()?;
            }
        }
        Ok(())
    }

    fn detach_navigation_views(&self, ids: &[NodeId]) -> Result<()> {
        let mut failure = None;
        for &id in ids {
            let is_nav = self
                .views
                .borrow()
                .get(&id)
                .is_some_and(|state| state.navigation.is_some());
            if is_nav {
                if let Err(error) = self.detach_view(id) {
                    failure.get_or_insert(error);
                }
            } else {
                self.held.borrow_mut().remove_view(id);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn end_navigation(&self, id: NodeId) -> Result<()> {
        let pending = self.pending.borrow_mut().remove(&id);
        let mut cancel_keys = Vec::new();
        let mut detach_held = Vec::new();
        if let Some(pending) = pending {
            pending.navigation.complete(pending.navigation.key);
            cancel_keys.push(pending.navigation.key);
            detach_held.extend(
                self.held
                    .borrow_mut()
                    .release_holder(pending.navigation.key, None),
            );
        }
        let released = self.demands.borrow_mut().release_node(id);
        for key in released {
            if !cancel_keys.contains(&key) {
                cancel_keys.push(key);
            }
            let matching_other = if key.owner != id {
                let mut map = self.pending.borrow_mut();
                if map.get(&key.owner).is_some_and(|p| p.navigation.key == key) {
                    map.remove(&key.owner)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(other) = matching_other {
                other.navigation.complete(other.navigation.key);
                detach_held.extend(self.held.borrow_mut().release_holder(key, None));
                if let Some(owner) = other.owner.upgrade() {
                    owner.animator.track(&owner);
                    owner.animator.request_frame();
                }
            }
        }
        self.cancel_on_worker(&cancel_keys);
        let mut result = self.detach_navigation_views(&detach_held);
        let should_detach_self = !self.held.borrow().is_held(id)
            && self
                .views
                .borrow()
                .get(&id)
                .is_some_and(|state| state.navigation.is_some());
        if should_detach_self {
            result = result.and(self.detach_view(id));
        }
        result.and(self.sync_capture_roots("end", false))
    }

    pub(super) fn remove(&self, id: NodeId) -> Result<()> {
        let mut removed_pending = Vec::new();
        {
            let mut pending = self.pending.borrow_mut();
            let matching: Vec<NodeId> = pending
                .iter()
                .filter_map(|(&owner_id, item)| {
                    let target_matches = item.target.upgrade().is_some_and(|t| t.id == id);
                    (owner_id == id || item.picture_id == id || target_matches).then_some(owner_id)
                })
                .collect();
            for owner_id in matching {
                if let Some(item) = pending.remove(&owner_id) {
                    removed_pending.push(item);
                }
            }
        }
        let mut cancel_keys = self.demands.borrow_mut().release_node(id);
        let mut detach_held = Vec::new();
        for item in removed_pending {
            item.navigation.complete(item.navigation.key);
            self.demands.borrow_mut().release(item.navigation.key);
            detach_held.extend(
                self.held
                    .borrow_mut()
                    .release_holder(item.navigation.key, None),
            );
            if !cancel_keys.contains(&item.navigation.key) {
                cancel_keys.push(item.navigation.key);
            }
            if let Some(owner) = item.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        self.held.borrow_mut().remove_view(id);
        detach_held.extend(self.held.borrow_mut().remove_target(id));
        self.cancel_on_worker(&cancel_keys);
        let mut result = self.detach_navigation_views(&detach_held);

        let target_nav_owners: Vec<NodeId> = self
            .views
            .borrow()
            .iter()
            .filter_map(|(&owner_id, state)| {
                let target_matches = state.target.upgrade().is_some_and(|t| t.id == id);
                (owner_id != id && target_matches && state.navigation.is_some()).then(|| {
                    if let Some(nav) = &state.navigation {
                        nav.complete(nav.key);
                    }
                    owner_id
                })
            })
            .collect();
        result = result.and(self.detach_view(id));
        for owner_id in target_nav_owners {
            result = result.and(self.detach_view(owner_id));
        }
        if self
            .root_node()
            .is_some_and(|root| root.flushing_batch.get())
        {
            // Mount batches and frame retirement reconcile after all owners
            // have been detached, rather than rebuilding capture hosts per node.
            result
        } else {
            result.and(self.sync_capture_roots("remove", false))
        }
    }

    pub(super) fn begin_navigation(
        &self,
        owner: &Rc<Node>,
        lower: Option<Rc<Node>>,
        backwards: bool,
    ) -> Result<Option<Rc<Navigation>>> {
        let Some(spec) = owner
            .navigation_shader
            .borrow()
            .clone()
            .filter(|_| !owner.platform.get().reduce_motion && self.surface_active())
        else {
            return Ok(None);
        };
        let Some(mut root) = self.root.borrow().upgrade() else {
            return Ok(None);
        };
        let mut ancestor = owner.parent_node.borrow().upgrade();
        while let Some(node) = ancestor {
            if node.kind == ViewKind::Modal {
                root = node;
                break;
            }
            ancestor = node.parent_node.borrow().upgrade();
        }
        let page = owner.kind == ViewKind::Screen;
        // Pages compose on the root, where the native tree supplies the live
        // destination after push/pop. Dialogs own a separate native surface.
        let target = if page { root.clone() } else { owner.clone() };
        if root.id == nsh_core::ROOT_ID {
            self.retain_root_input(if page {
                lower.clone()
            } else {
                owner.parent_node.borrow().upgrade()
            });
        }
        let picture = if page {
            root.clone()
        } else {
            lower.unwrap_or(root)
        };
        let picture_frame = picture.frame.get();
        let target_frame = target.frame.get();
        if !picture.active.get()
            || !target.active.get()
            || picture_frame.width <= 0.0
            || picture_frame.height <= 0.0
            || target_frame.width <= 0.0
            || target_frame.height <= 0.0
        {
            return Ok(None);
        }

        let navigation = Rc::new(Navigation::new(owner.id, backwards, spec.duration).ok_or(
            crate::render::node::Error::Unsupported("navigation generation exhausted"),
        )?);
        let superseded_pending: Vec<PendingNavigation> = {
            let mut pending = self.pending.borrow_mut();
            let ids: Vec<NodeId> = pending
                .iter()
                .filter_map(|(&id, item)| {
                    let same_target = item
                        .target
                        .upgrade()
                        .is_some_and(|old| Rc::ptr_eq(&old, &target));
                    (id == owner.id || same_target).then_some(id)
                })
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        let mut cancel_keys = Vec::new();
        let mut stale_held = Vec::new();
        for old in superseded_pending {
            old.navigation.complete(old.navigation.key);
            stale_held.extend(self.held.borrow_mut().transfer_or_release(
                old.navigation.key,
                navigation.key,
                target.id,
            ));
            if self.demands.borrow_mut().release(old.navigation.key) {
                cancel_keys.push(old.navigation.key);
            }
            if let Some(old_owner) = old.owner.upgrade()
                && old_owner.id != owner.id
            {
                old_owner.animator.track(&old_owner);
                old_owner.animator.request_frame();
            }
        }
        self.cancel_on_worker(&cancel_keys);

        // Hold interrupted shader views on `target` until `navigation.key` resolves;
        // mark their Navigation completed so stale `Event::Completed(old_key)` is ignored.
        let interrupted: Vec<(NodeId, Rc<Navigation>, Option<Rc<Node>>)> = self
            .views
            .borrow()
            .iter()
            .filter(|(_, state)| {
                state
                    .target
                    .upgrade()
                    .is_some_and(|old| Rc::ptr_eq(&old, &target))
            })
            .filter_map(|(&view_id, state)| {
                state
                    .navigation
                    .as_ref()
                    .map(|nav| (view_id, nav.clone(), state.owner.upgrade()))
            })
            .collect();
        for (view_id, old_nav, old_owner) in interrupted {
            old_nav.complete(old_nav.key);
            self.held
                .borrow_mut()
                .hold(view_id, target.id, navigation.key);
            if let Some(old_owner) = old_owner
                && old_owner.id != owner.id
            {
                old_owner.animator.track(&old_owner);
                old_owner.animator.request_frame();
            }
        }

        self.capture_nodes
            .borrow_mut()
            .insert(picture.id, Rc::downgrade(&picture));
        self.demands
            .borrow_mut()
            .request(navigation.key, picture.id);
        self.pending.borrow_mut().insert(
            owner.id,
            PendingNavigation {
                owner: Rc::downgrade(owner),
                target: Rc::downgrade(&target),
                picture_id: picture.id,
                spec,
                navigation: navigation.clone(),
                page,
            },
        );
        // Publish the owner before any fallible native operation. Every failure
        // below can now retire the whole attempt by its existing generation.
        let started = (|| -> Result<bool> {
            self.detach_navigation_views(&stale_held)?;
            self.sync_capture_roots("begin", false)?;
            Ok(picture.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                wrapper
                    .composite
                    .borrow()
                    .as_ref()
                    .filter(|composite| !composite.is_failed())
                    .is_some_and(|composite| {
                        composite.capture(
                            navigation.key,
                            !page && owner.modal_backdrop.get().is_some(),
                        );
                        true
                    })
            }))
        })();
        match started {
            Ok(true) => {
                picture.animator.request_frame();
                owner.animator.request_frame();
                Ok(Some(navigation))
            }
            other => {
                if let Err(error) = other {
                    log::warn!("OHOS navigation capture could not start: {error}");
                }
                self.fail_navigation(navigation.key, Some((&navigation, &Rc::downgrade(owner))));
                Ok(None)
            }
        }
    }

    /// Close logical ownership first, then attempt every native cleanup even if
    /// one fails. No timer or extra phase is needed to recover a partial handoff.
    fn fail_navigation(&self, key: Key, attempt: Option<(&Navigation, &Weak<Node>)>) {
        let pending = {
            let mut pending = self.pending.borrow_mut();
            if pending
                .get(&key.owner)
                .is_some_and(|p| p.navigation.key == key)
            {
                pending.remove(&key.owner)
            } else {
                None
            }
        };
        let running = self.views.borrow().get(&key.owner).and_then(|state| {
            state
                .navigation
                .as_ref()
                .filter(|nav| nav.key == key)
                .map(|nav| (nav.clone(), state.owner.clone()))
        });
        let attempt = attempt
            .or_else(|| pending.as_ref().map(|p| (p.navigation.as_ref(), &p.owner)))
            .or_else(|| running.as_ref().map(|(nav, owner)| (nav.as_ref(), owner)));
        if let Some((navigation, owner)) = attempt {
            navigation.mark_fallback(key);
            if let Some(owner) = owner.upgrade() {
                owner.animator.track(&owner);
            }
        }
        self.demands.borrow_mut().release(key);
        self.cancel_on_worker(&[key]);
        let mut detach = self.held.borrow_mut().release_holder(key, None);
        if running.is_some() && !detach.contains(&key.owner) {
            detach.push(key.owner);
        }
        if let Err(error) = self.detach_navigation_views(&detach) {
            log::warn!("OHOS navigation fallback cleanup: {error}");
        }
        if let Err(error) = self.sync_capture_roots("fallback", false) {
            log::warn!("OHOS navigation fallback restore: {error}");
        }
    }

    pub(super) fn navigation_event(&self, event: Event) -> Result<()> {
        let key = event.key();
        let attempt = self
            .pending
            .borrow()
            .get(&key.owner)
            .filter(|pending| pending.navigation.key == key)
            .map(|pending| (pending.navigation.clone(), pending.owner.clone()))
            .or_else(|| {
                self.views.borrow().get(&key.owner).and_then(|state| {
                    state
                        .navigation
                        .as_ref()
                        .filter(|nav| nav.key == key)
                        .map(|nav| (nav.clone(), state.owner.clone()))
                })
            });
        if matches!(event, Event::Failed(_)) {
            self.fail_navigation(
                key,
                attempt.as_ref().map(|(nav, owner)| (nav.as_ref(), owner)),
            );
            return Ok(());
        }
        let result = self.apply_navigation_event(event);
        if result.is_err() {
            self.fail_navigation(
                key,
                attempt.as_ref().map(|(nav, owner)| (nav.as_ref(), owner)),
            );
        }
        result
    }

    fn apply_navigation_event(&self, event: Event) -> Result<()> {
        let key = event.key();
        let pending = {
            let mut map = self.pending.borrow_mut();
            if map
                .get(&key.owner)
                .is_some_and(|entry| entry.navigation.key == key)
            {
                map.remove(&key.owner)
            } else {
                None
            }
        };
        match event {
            Event::Captured(_) => {
                let Some(pending) = pending else {
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
                    if self.demands.borrow_mut().release(key) || !held.is_empty() {
                        self.sync_capture_roots("stale-capture", false)?;
                    }
                    return Ok(());
                };
                let owner = pending.owner.upgrade();
                let target = pending.target.upgrade();
                let valid = owner.as_ref().is_some_and(|o| {
                    o.active.get() && !o.platform.get().reduce_motion && self.surface_active()
                }) && target.as_ref().is_some_and(|t| t.active.get());
                if !valid {
                    if owner
                        .as_ref()
                        .is_some_and(|o| o.platform.get().reduce_motion)
                    {
                        pending.navigation.mark_fallback(key);
                    } else {
                        pending.navigation.complete(key);
                    }
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
                    self.demands.borrow_mut().release(key);
                    self.sync_capture_roots("cancelled", false)?;
                    if let Some(owner) = owner {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                    return Ok(());
                }
                let owner = owner.unwrap();
                let target = target.unwrap();
                if !pending.navigation.mark_running(key) {
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
                    self.demands.borrow_mut().release(key);
                    self.sync_capture_roots("cancelled", false)?;
                    return Ok(());
                }

                // Commit the live destination side now that the leaving snapshot
                // is safely copied into its standalone GPU target on the worker.
                if let Some(screen) = owner.screen.borrow().as_ref() {
                    screen.on_shader_captured(&pending.navigation)?;
                }
                if let Some(modal) = owner.modal.borrow().as_ref() {
                    modal.on_shader_captured(&pending.navigation)?;
                }

                // Attach `owner`'s transition shader layer BEFORE detaching any
                // cross-owner held layer or releasing `picture`'s demand so `target`
                // keeps `requires_texture == true` continuously across phases.
                self.set_on(
                    &owner,
                    &target,
                    pending.spec.layer,
                    Some(pending.navigation.clone()),
                )?;
                let held = self.held.borrow_mut().release_holder(key, Some(owner.id));
                self.detach_navigation_views(&held)?;

                if let Some(modal) = owner.modal.borrow().as_ref() {
                    modal.refresh_presentation()?;
                }

                self.demands.borrow_mut().release(key);
                self.sync_capture_roots("captured", false)?;

                let started = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                    wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .filter(|composite| !composite.is_failed())
                        .is_some_and(|composite| {
                            composite.shader_navigation(
                                key,
                                pending.spec.duration,
                                pending.navigation.backwards,
                                pending.page && pending.navigation.backwards,
                            );
                            true
                        })
                });
                if !started {
                    self.detach_view(owner.id)?;
                    pending.navigation.mark_fallback(key);
                    owner.animator.track(&owner);
                    owner.animator.request_frame();
                }
                Ok(())
            }
            Event::Failed(_) => unreachable!("failures are retired before applying native updates"),
            Event::Completed(_) => {
                let released = self.demands.borrow_mut().release(key);
                let held = self.held.borrow_mut().release_holder(key, None);
                self.detach_navigation_views(&held)?;
                if let Some(pending) = pending {
                    pending.navigation.complete(key);
                    if let Some(owner) = pending.owner.upgrade() {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                }
                if released || !held.is_empty() {
                    self.sync_capture_roots("complete", false)?;
                }
                self.navigation_complete(key)?;
                Ok(())
            }
        }
    }

    fn navigation_complete(&self, key: Key) -> Result<()> {
        if self.held.borrow().is_held(key.owner) {
            return Ok(());
        }
        let owner = {
            let views = self.views.borrow();
            let Some(state) = views.get(&key.owner) else {
                return Ok(());
            };
            let Some(navigation) = state.navigation.as_ref() else {
                return Ok(());
            };
            if !navigation.complete(key) {
                return Ok(());
            }
            Some(state.owner.upgrade())
        };
        if let Some(owner) = owner {
            if let Some(owner) = owner {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            } else {
                self.detach_view(key.owner)?;
            }
        }
        Ok(())
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
            for modal in self.modals.borrow().iter().filter_map(Weak::upgrade) {
                if modal
                    .modal_screen_layers
                    .borrow()
                    .iter()
                    .any(|(owner, _)| *owner == id)
                    && let Some(wrapper) = modal.wrapper.borrow().as_ref()
                    && let Some(output) = wrapper.composite.borrow().as_ref()
                {
                    output.shader_slot(id, slot, value);
                }
            }
        }
    }

    pub(super) fn transition(&self, id: NodeId, duration: Duration) {
        let views = self.views.borrow();
        let Some(state) = views.get(&id) else {
            return;
        };
        let Some(target) = state.target.upgrade() else {
            return;
        };
        if !target.platform.get().reduce_motion
            && let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            #[cfg(feature = "dev")]
            if !duration.is_zero() {
                state.transition_until.set(Some(Instant::now() + duration));
            }
            composite.shader_transition(id, duration);
            for modal in self.modals.borrow().iter().filter_map(Weak::upgrade) {
                if modal
                    .modal_screen_layers
                    .borrow()
                    .iter()
                    .any(|(owner, _)| *owner == id)
                    && let Some(wrapper) = modal.wrapper.borrow().as_ref()
                    && let Some(output) = wrapper.composite.borrow().as_ref()
                {
                    output.shader_transition(id, duration);
                }
            }
            drop(views);
            target.animator.request_frame();
        }
    }

    #[cfg(feature = "dev")]
    pub(super) fn animating(&self) -> bool {
        let now = Instant::now();
        !self.pending.borrow().is_empty()
            || self.views.borrow().values().any(|state| {
                state.navigation.as_ref().is_some_and(|nav| nav.running())
                    || state
                        .transition_until
                        .get()
                        .is_some_and(|until| now < until)
            })
    }

    pub(super) fn frame(&self) -> Result<bool> {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending
            .borrow()
            .values()
            .map(|p| &p.navigation)
            .chain(
                self.views
                    .borrow()
                    .values()
                    .filter_map(|s| s.navigation.as_ref()),
            )
            .filter(|nav| nav.timed_out(now))
            .map(|nav| nav.key)
            .collect();
        for key in expired {
            self.fail_navigation(key, None);
        }
        let mut due = !self.demands.borrow().is_empty();
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
                due |= state.navigation.as_ref().is_some_and(|nav| nav.running());
                // Closing dialogs can already be deactivated; their retained
                // native picture must keep ticking until navigation ends.
                true
            } else {
                // A screen shader belongs to the surface until unmounted,
                // even when its declaring page is behind another page. This
                // matches UIKit/AppKit and keeps the root's render ownership
                // stable throughout a navigation.
                attached && unhidden_chain(&owner)
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
        // Keep demanded picture roots active on the compositor while awaiting
        // their initial input snapshot.
        let capture_targets: Vec<Rc<Node>> = self
            .capture_nodes
            .borrow()
            .values()
            .filter_map(Weak::upgrade)
            .filter(|node| node.active.get() && node.navigation_capture.get())
            .collect();
        for picture in capture_targets {
            let entry = targets.entry(picture.id).or_insert((picture, false));
            entry.1 = true;
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
