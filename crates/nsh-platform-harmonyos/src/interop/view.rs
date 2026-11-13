//! On-demand ArkTS wrappers retain visual and accessibility composition around C subtrees.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use nsh_core::{AccessibilityRole as R, Checked, EventKind};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Custom,
    },
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};

use crate::{
    accessibility::value::owns_semantics,
    interop::{
        attachment::NativeAttachment,
        sdk::{MenuItem, ViewOptions, ViewRoute},
    },
    render::node::{NativeNode, Node, Result, callback},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirePhase {
    Exporting,
    Settling,
}

pub(crate) struct Wrapper {
    closed: std::cell::Cell<bool>,
    pub(crate) outer: NativeNode,
    target: RefCell<Option<NativeAttachment>>,
    replaces: Cell<Option<u32>>,
    route: RefCell<Option<ViewRoute>>,
    context_live: Cell<bool>,
    owner: std::rc::Weak<Node>,
    settings: std::cell::RefCell<Option<ViewOptions>>,
    pub(crate) composite: std::cell::RefCell<Option<crate::render::composition::worker::Composite>>,
    retiring: Cell<Option<(u32, RetirePhase)>>,
    lingering: std::cell::RefCell<Vec<crate::render::composition::worker::Composite>>,
    /// The accessibility mode and grouping last given the wrapped node
    /// (`None`: to give again, after the node's own props reset them).
    semantics: Cell<Option<(i32, bool)>>,
    /// The frame size the outer node was last asked to measure at.
    measured: Cell<Option<(f32, f32)>>,
}
impl Wrapper {
    pub(crate) fn retiring_export(&self) -> bool {
        matches!(self.retiring.get(), Some((_, RetirePhase::Exporting)))
    }
    fn retire_composite(owner: std::rc::Weak<Node>, generation: u32, remaining: u8) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        let Some((outer, active, closed)) = node.wrapper.borrow().as_ref().map(|w| {
            (
                w.outer.clone(),
                w.retiring.get().map(|(g, _)| g) == Some(generation)
                    && w.composite
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c.generation == generation),
                w.closed.get(),
            )
        }) else {
            return;
        };
        if closed || !active {
            return;
        }
        if node.needs_texture() {
            if let Some(wrapper) = node.wrapper.borrow().as_ref()
                && wrapper.retiring.get().map(|(g, _)| g) == Some(generation)
            {
                wrapper.retiring.set(None);
            }
            return;
        }
        if remaining == 2 {
            callback(|| {
                node.composite_dirty.set(true);
                if let Some(wrapper) = node.wrapper.borrow().as_ref()
                    && wrapper.retiring.get().map(|(g, _)| g) == Some(generation)
                {
                    wrapper
                        .retiring
                        .set(Some((generation, RetirePhase::Settling)));
                    if let Some(composite) = wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .filter(|c| c.generation == generation)
                    {
                        let _ = composite.update(&node);
                    }
                }
                Ok(())
            });
        }
        if remaining > 0 {
            let next = owner.clone();
            if outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::retire_composite(next, generation, remaining - 1);
                })
                .is_ok()
            {
                return;
            }
        }
        callback(|| {
            let retired = node.wrapper.borrow().as_ref().and_then(|wrapper| {
                if wrapper.retiring.get().map(|(g, _)| g) != Some(generation) {
                    return None;
                }
                wrapper.retiring.set(None);
                if wrapper
                    .composite
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.generation == generation)
                {
                    wrapper.composite.borrow_mut().take()
                } else {
                    None
                }
            });
            if let Some(old) = retired {
                let old_generation = old.generation;
                node.composite_dirty.set(true);
                let _ = old.update(&node);
                if let Some(wrapper) = node.wrapper.borrow().as_ref() {
                    wrapper.lingering.borrow_mut().push(old);
                    Self::retire_lingering(Rc::downgrade(&node), old_generation, 12);
                    wrapper
                        .outer
                        .borrow()
                        .mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
                node.schedule_subtree_paint(6)?;
                node.update_wrapper()?;
                node.restore_style()?;
                node.flush_arkui()?;
            }
            Ok(())
        });
    }
    fn commit_presentation(owner: std::rc::Weak<Node>, generation: u32, remaining: u8) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        let Some((outer, active, closed)) = node.wrapper.borrow().as_ref().map(|w| {
            (
                w.outer.clone(),
                w.composite
                    .borrow()
                    .as_ref()
                    .is_some_and(|composite| composite.generation == generation),
                w.closed.get(),
            )
        }) else {
            return;
        };
        if closed || !active {
            return;
        }
        if remaining > 0 {
            let next = owner.clone();
            if outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::commit_presentation(next, generation, remaining - 1);
                })
                .is_ok()
            {
                return;
            }
        }
        callback(|| {
            let committed = node
                .wrapper
                .borrow()
                .as_ref()
                .filter(|w| !w.closed.get())
                .and_then(|w| {
                    w.composite
                        .borrow()
                        .as_ref()
                        .filter(|composite| composite.generation == generation)
                        .map(|composite| composite.commit_presentation())
                })
                .unwrap_or(false);
            if committed {
                node.update_wrapper()?;
                node.flush_arkui()?;
            }
            Ok(())
        });
    }
    fn retire_lingering(owner: std::rc::Weak<Node>, generation: u32, remaining: u8) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        let Some((outer, present)) = node.wrapper.borrow().as_ref().map(|w| {
            (
                w.outer.clone(),
                !w.closed.get()
                    && w.lingering
                        .borrow()
                        .iter()
                        .any(|c| c.generation == generation),
            )
        }) else {
            return;
        };
        if !present {
            return;
        }
        if remaining > 0 {
            let next = owner.clone();
            if outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::retire_lingering(next, generation, remaining - 1);
                })
                .is_ok()
            {
                return;
            }
        }
        if let Some(wrapper) = node.wrapper.borrow().as_ref() {
            wrapper
                .lingering
                .borrow_mut()
                .retain(|c| c.generation != generation);
            let _ = wrapper.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender);
        }
    }
    pub(crate) fn invalidate_context(&self) {
        self.settings.borrow_mut().take();
        self.replaces.set(None);
        self.route.borrow_mut().take();
        self.context_live.set(false);
        // Keep the old target through SDK lifecycle cleanup; replacement queues its final owner.
    }
    fn replace_target(&self, owner: &Node) -> Result<u32> {
        let target = NativeAttachment::for_subtree(&self.outer, &owner.native)?;
        let id = target.id();
        let old_attached = self
            .target
            .replace(Some(target))
            .and_then(|old| owner.view_remove(old));
        if old_attached.is_some() {
            self.replaces.set(old_attached);
        }
        // The native target is the unique, nonreused callback generation.
        let weak = self.owner.clone();
        *self.route.borrow_mut() = Some(ViewRoute::new(id, weak, &owner.native));
        Ok(id)
    }
    fn ensure_context(&self, owner: &Node) -> Result<()> {
        let target_retired = self
            .target
            .borrow()
            .as_ref()
            .is_none_or(NativeAttachment::is_retired);
        if !self.context_live.get() || target_retired {
            self.settings.borrow_mut().take();
            self.replace_target(owner)?;
            if !self.context_live.get() && self.composite.borrow_mut().take().is_some() {
                let composite = crate::render::composition::worker::Composite::new(
                    &self
                        .owner
                        .upgrade()
                        .ok_or(crate::render::node::Error::InvalidNode(owner.id))?,
                )?;
                *self.composite.borrow_mut() = Some(composite);
            }
            self.context_live.set(true);
        }
        Ok(())
    }
    fn with_child<T>(
        &self,
        f: impl FnOnce(&ohos_arkui_binding::common::node::ArkUINode) -> T,
    ) -> Result<Option<T>> {
        if self.closed.get()
            || !self.context_live.get()
            || !self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.host.check().is_ok())
        {
            return Ok(None);
        }
        self.target
            .borrow()
            .as_ref()
            .map_or(Ok(None), |target| Ok(target.with_child(f)?))
    }
    fn abort(self) {
        let cleanup = || -> Result<()> {
            if let Some(target) = self.target.borrow().as_ref() {
                target.invalidate()?;
            }
            self.outer.borrow().remove_from_parent()?;
            self.outer.borrow_mut().dispose()?;
            Ok(())
        };
        if let Err(error) = cleanup() {
            log::error!("Failed to release an unmounted OHOS wrapper: {error}");
            // A failed native cleanup must retain the complete owner, not just its target ID.
            std::mem::forget(self);
        }
    }
    fn new(owner: &Rc<Node>) -> Result<Self> {
        let outer = Custom::new()?.into_node();
        let setup = || -> Result<NativeAttachment> {
            outer
                .borrow()
                .set_length_metric_unit(LengthMetricUnit::Vp)?;
            Ok(NativeAttachment::for_subtree(&outer, &owner.native)?)
        };
        let target = match setup() {
            Ok(target) => target,
            Err(error) => {
                if let Err(cleanup) = outer.borrow_mut().dispose() {
                    log::error!("Failed to release a new OHOS wrapper: {cleanup}");
                    std::mem::forget(outer.clone());
                }
                return Err(error);
            }
        };
        let route = ViewRoute::new(target.id(), Rc::downgrade(owner), &owner.native);
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_draw(move |event| {
            callback(|| {
                if let Some(owner) = weak.upgrade()
                    && owner.scene_root.get()
                    && let Some(raw) = event
                        .draw_context_in_draw()
                        .and_then(|context| context.canvas())
                {
                    // SAFETY: ArkUI owns this canvas until this draw callback returns.
                    let canvas = unsafe { ohos_drawing_binding::Canvas::from_raw_borrowed(raw) };
                    if let Some(scene) = owner.scene.borrow().as_ref() {
                        scene.draw(owner.id, &canvas, owner.scale.get());
                    }
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                if let Some(owner) = weak.upgrade() {
                    if let Some(modal) = owner.modal.borrow().as_ref() {
                        let size = modal.measure(event)?;
                        let mut raw = owner.raw_frame.get();
                        raw.width = size.width;
                        raw.height = size.height;
                        owner.set_frame(raw)?;
                    }
                    let frame = owner.frame.get();
                    let mut width = (frame.width * owner.scale.get()).round() as i32;
                    let mut height = (frame.height * owner.scale.get()).round() as i32;
                    if !owner.clips_children() && !owner.children.borrow().is_empty() {
                        width = width.max(1);
                        height = height.max(1);
                    }
                    event.set_measured_size(width, height)?;
                    if let Some(wrapper) = owner.wrapper.borrow().as_ref()
                        && let Some(result) = wrapper.with_child(|node| {
                            crate::render::layout::ChildMeasure::new(event).child(node)
                        })?
                    {
                        result?;
                    }
                    if let Some(wrapper) = owner.wrapper.borrow().as_ref() {
                        if let Some(composite) = wrapper.composite.borrow().as_ref() {
                            composite.measure(event)?;
                        }
                        for composite in wrapper.lingering.borrow().iter() {
                            composite.measure(event)?;
                        }
                    }
                }
                Ok(())
            })
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_layout(move |event| {
            callback(|| {
                let position = event.position_in_layout();
                event.set_layout_position(position.x, position.y)?;
                if let Some(owner) = weak.upgrade()
                    && let Some(wrapper) = owner.wrapper.borrow().as_ref()
                    && let Some(result) = wrapper.with_child(|node| {
                        let (x, y) = wrapper
                            .composite
                            .borrow()
                            .as_ref()
                            .filter(|composite| composite.surface().is_some())
                            .map_or((0.0, 0.0), |composite| composite.origin());
                        event.layout_child(
                            node,
                            (x * owner.scale.get()).round() as i32,
                            (y * owner.scale.get()).round() as i32,
                        )
                    })?
                {
                    result?;
                }
                if let Some(owner) = weak.upgrade()
                    && let Some(wrapper) = owner.wrapper.borrow().as_ref()
                {
                    if let Some(composite) = wrapper.composite.borrow().as_ref() {
                        composite.layout(event)?;
                    }
                    for composite in wrapper.lingering.borrow().iter() {
                        composite.layout(event)?;
                    }
                }
                Ok(())
            })
        });
        Ok(Self {
            closed: std::cell::Cell::new(false),
            outer,
            target: RefCell::new(Some(target)),
            replaces: Cell::new(None),
            route: RefCell::new(Some(route)),
            context_live: Cell::new(true),
            owner: Rc::downgrade(owner),
            settings: std::cell::RefCell::default(),
            composite: std::cell::RefCell::default(),
            retiring: Cell::new(None),
            lingering: std::cell::RefCell::default(),
            semantics: Cell::new(None),
            measured: Cell::new(None),
        })
    }
    /// Sends the ArkTS view its settings; whether they changed.
    fn update(&self, owner: &Node) -> Result<bool> {
        if self.closed.get() || owner.host.check().is_err() {
            return Ok(false);
        }
        self.ensure_context(owner)?;
        let texture_surface = self
            .composite
            .borrow()
            .as_ref()
            .and_then(|composite| composite.surface());
        if self
            .settings
            .borrow()
            .as_ref()
            .is_some_and(|previous| previous.texture_surface != texture_surface)
        {
            self.replace_target(owner)?;
        }
        if let Some(route) = self.route.borrow().as_ref() {
            route.texture(
                texture_surface.is_some()
                    || self
                        .settings
                        .borrow()
                        .as_ref()
                        .is_some_and(|previous| previous.texture_surface.is_some()),
            );
        }
        let Some(target_id) = self.target.borrow().as_ref().map(NativeAttachment::id) else {
            return Ok(false);
        };
        let replaces = self.replaces.take();
        let spec = owner.accessibility_state.spec.borrow();
        let frame = owner.frame.get();
        let value = crate::accessibility::value::spoken_value(&spec).unwrap_or_default();
        let actions: Vec<MenuItem> = if cfg!(feature = "api-26") {
            spec.actions
                .iter()
                .take(16)
                .map(|action| MenuItem {
                    kind: "action".into(),
                    name: action.name.to_string(),
                    label: action.label.as_deref().unwrap_or(&action.name).to_owned(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut settings = ViewOptions {
            texture_insets: if texture_surface.is_some() {
                self.composite
                    .borrow()
                    .as_ref()
                    .map(|composite| composite.insets())
            } else {
                None
            },
            texture_surface,
            texture_ready: self
                .composite
                .borrow()
                .as_ref()
                .is_some_and(|composite| composite.is_ready(owner)),
            target_id,
            replaces: None,
            test_id: owner.interaction.test_id(),
            width: frame.width as f64,
            height: frame.height as f64,
            hue: 0.0,
            semantic: owns_semantics(&spec),
            clickable: owner.interaction.mask().contains(EventKind::Press)
                || spec
                    .actions
                    .iter()
                    .any(|action| action.name.as_ref() == "activate"),
            disabled: spec.state.disabled,
            accessible: spec.accessible,
            hidden: spec.hidden,
            label: owner.accessibility_state.label(owner),
            hint: spec.hint.as_deref().unwrap_or("").to_owned(),
            api23: cfg!(feature = "api-23") && !value.is_empty(),
            api26: !actions.is_empty(),
            value,
            role: role(spec.role).to_owned(),
            checked: spec.state.checked.map(|value| value == Checked::True),
            selected: spec.state.selected,
            actions,
            menu: owner.selection_menu(),
        };
        let mut previous = self.settings.borrow_mut();
        if replaces.is_none() && previous.as_ref() == Some(&settings) {
            return Ok(false);
        }
        *previous = Some(settings.clone());
        settings.replaces = replaces;
        owner.view_update(settings);
        Ok(true)
    }
    pub(crate) fn close(&self, owner: &Node) -> Result<()> {
        self.closed.set(true);
        self.retiring.set(None);
        self.replaces.set(None);
        self.route.borrow_mut().take();
        if let Some(old) = self.target.borrow_mut().take() {
            owner.view_remove(old);
        }
        self.composite.borrow_mut().take();
        self.lingering.borrow_mut().clear();
        Ok(())
    }
}
impl Node {
    pub(crate) fn needs_texture(&self) -> bool {
        self.effects_plan().requires_texture
    }
    pub(crate) fn wrapper_owns_semantics(&self) -> bool {
        self.wrapper.borrow().is_some() && owns_semantics(&self.accessibility_state.spec.borrow())
    }
    pub(crate) fn accessibility_id(&self) -> Result<i32> {
        use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
        let spec = self.accessibility_state.spec.borrow();
        if owns_semantics(&spec)
            && let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(id) =
                wrapper.with_child(|node| node.i32_attribute(A::AccessibilityId, 0))?
        {
            return Ok(id?);
        }
        if let Some(input) = self.input.borrow().as_ref()
            && let Some(id) = input.accessibility_id()?
        {
            return Ok(id);
        }
        Ok(self.native.borrow().i32_attribute(A::AccessibilityId, 0)?)
    }
    pub(crate) fn mounted(&self) -> NativeNode {
        self.wrapper
            .borrow()
            .as_ref()
            .map_or_else(|| self.native.clone(), |wrapper| wrapper.outer.clone())
    }
    pub(crate) fn ensure_wrapper(self: &Rc<Self>) -> Result<()> {
        if self
            .wrapper
            .borrow()
            .as_ref()
            .is_some_and(|wrapper| wrapper.closed.get())
        {
            return Ok(());
        }
        let spec = self.accessibility_state.spec.borrow();
        if cfg!(feature = "api-26") && spec.actions.len() > 16 {
            log::warn!("OHOS accessibility: showing the first 16 custom actions");
        }
        let has_semantics = owns_semantics(&spec);
        drop(spec);
        let requires_texture = self.effects_plan().requires_texture;
        // Establish the root's layout host before it has scroll descendants.
        // Adding it around an already laid-out tree resets native scroll offsets.
        let needed = self.id == nsh_core::ROOT_ID
            || self.kind == nsh_core::ViewKind::Modal
            || self.selectable()
            || has_semantics
            || requires_texture;
        let mut created_wrapper = false;
        if needed && self.wrapper.borrow().is_none() {
            self.enable_layout();
            self.reset_visual_baseline(&self.native)?;
            let parent = self.mount_parent.borrow().upgrade();
            let index = parent
                .as_ref()
                .map(|parent| {
                    parent
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &self.native))
                        .ok_or(crate::render::node::Error::InvalidNode(self.id))
                })
                .transpose()?;
            let wrapper = Wrapper::new(self)?;
            if let Some((parent, index)) = parent.zip(index) {
                let mut parent = parent.borrow_mut();
                if let Err(error) = parent.remove_child(index) {
                    wrapper.abort();
                    return Err(error.into());
                }
                if let Err(error) = parent.insert_child(wrapper.outer.clone(), index) {
                    let restored = parent.insert_child(self.native.clone(), index);
                    wrapper.abort();
                    restored?;
                    return Err(error.into());
                }
            }
            *self.wrapper.borrow_mut() = Some(wrapper);
            created_wrapper = true;
            self.accessibility_wrappers
                .set(self.accessibility_wrappers.get() + 1);
            self.invalidate_mounted_geometry();
            self.update_layout_bounds()?;
            // The wrapper owns this node's semantics; native descendants keep their own entries.
            #[cfg(feature = "api-23")]
            self.accessibility_provider.clear();
        }
        let mut created_composite = false;
        if !requires_texture {
            let start_generation = self.wrapper.borrow().as_ref().and_then(|wrapper| {
                let generation = wrapper.composite.borrow().as_ref()?.generation;
                if wrapper.retiring.get().is_none() {
                    wrapper
                        .retiring
                        .set(Some((generation, RetirePhase::Exporting)));
                    Some(generation)
                } else {
                    None
                }
            });
            if let Some(generation) = start_generation {
                self.composite_dirty.set(true);
                self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
                if let Some(wrapper) = self.wrapper.borrow().as_ref() {
                    wrapper
                        .outer
                        .borrow()
                        .mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
                Wrapper::retire_composite(Rc::downgrade(self), generation, 3);
            }
        } else if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.retiring.set(None);
            if wrapper.composite.borrow().is_none() {
                if wrapper.target.borrow().is_none() {
                    return Err(crate::render::node::Error::Unsupported(
                        "wrapper target retired",
                    ));
                }
                let composite = crate::render::composition::worker::Composite::new(self)?;
                *wrapper.composite.borrow_mut() = Some(composite);
                created_composite = true;
            }
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            let spec = self.accessibility_state.spec.borrow();
            let semantic = owns_semantics(&spec);
            use ohos_arkui_binding::types::view::AccessibilityMode as Mode;
            let mode = if semantic && spec.accessible == Some(true) {
                // The ArkTS group is the single semantic entry for this subtree.
                Mode::DisabledForDescendants
            } else if semantic {
                Mode::Disabled
            } else if spec.hidden {
                Mode::DisabledForDescendants
            } else {
                match spec.accessible {
                    Some(true) => Mode::Enabled,
                    Some(false) => Mode::Disabled,
                    None => Mode::Auto,
                }
            };
            let next = (i32::from(mode), !semantic && spec.accessible == Some(true));
            // Style and text updates come here too: give the wrapped node
            // its semantics only when they change or its own props reset them.
            if wrapper.semantics.replace(Some(next)) != Some(next) {
                use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
                let native = self.native.borrow();
                // The wrapper carries the test ID.
                native.reset_attribute(A::Id)?;
                native.set_i32_attribute(A::AccessibilityMode, next.0)?;
                native.set_bool_attribute(A::AccessibilityGroup, next.1)?;
            }
        }
        if created_wrapper || created_composite {
            self.apply_visual_style(&self.style.borrow())?;
        }
        self.update_wrapper()
    }
    pub(crate) fn update_wrapper(&self) -> Result<()> {
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.update(self)?;
        }
        if let Some(wrapper) = self
            .wrapper
            .borrow()
            .as_ref()
            .filter(|wrapper| !wrapper.closed.get())
        {
            let composite = if let Some(composite) = wrapper.composite.borrow().as_ref() {
                composite.update(self)?
            } else {
                false
            };
            let changed = wrapper.update(self)?;
            // The outer node measures at the frame's size (a modal at its
            // content's) and places the ArkTS view at the composite's origin:
            // measure again only when one of those, or what the view shows,
            // may have changed.
            let frame = self.frame.get();
            let size = Some((frame.width, frame.height));
            let resized = wrapper.measured.replace(size) != size;
            let modal = self.modal.borrow().is_some();
            if changed || composite || modal || resized {
                wrapper
                    .outer
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        }
        self.composite_dirty.set(false);
        Ok(())
    }
    /// The wrapped node's own props rewrote its test ID and accessibility
    /// mode: the wrapper gives them again at the next update.
    pub(crate) fn wrapper_semantics_reset(&self) {
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.semantics.set(None);
        }
    }
    pub(crate) fn visual_node(&self) -> NativeNode {
        self.mounted()
    }
    pub(crate) fn composite_ready(&self, generation: u32) -> Result<()> {
        let mut initialized = false;
        let mut revealed = false;
        let mut drop_failed = false;
        let mut weak = None;
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.ready(self, generation)?;
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            for composite in wrapper
                .lingering
                .borrow()
                .iter()
                .filter(|composite| composite.generation == generation)
            {
                composite.ready();
            }
            if let Some(composite) = wrapper
                .composite
                .borrow()
                .as_ref()
                .filter(|composite| composite.generation == generation)
            {
                (initialized, revealed) = composite.ready();
                drop_failed = composite.is_failed();
                weak = Some(wrapper.owner.clone());
                if composite.needs_shader_frame() {
                    self.animator.request_frame();
                }
                wrapper
                    .outer
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
            if drop_failed {
                wrapper.composite.borrow_mut().take();
            }
        }
        if initialized || revealed || drop_failed {
            self.restore_style()?;
            self.update_wrapper()?;
            self.invalidate_scene();
            if !drop_failed {
                self.schedule_subtree_paint(4)?;
            }
        }
        if revealed && let Some(weak) = weak {
            Wrapper::commit_presentation(weak, generation, 6);
        }
        Ok(())
    }
}
fn role(role: R) -> &'static str {
    match role {
        R::None => "",
        R::Button | R::ImageButton | R::KeyboardKey => "button",
        R::Checkbox => "checkbox",
        R::Switch => "switch",
        R::Radio => "radio",
        R::Adjustable | R::Scrollbar => "slider",
        R::Image => "image",
        R::Link => "link",
        R::List => "list",
        R::Grid => "grid",
        R::Progressbar => "progress",
        R::Search => "search",
        R::Menuitem => "menuitem",
        _ => "text",
    }
}
