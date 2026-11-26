//! Native attachment ownership, creation, retirement and disposal.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use nsh_core::{NodeId, ViewKind};
use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};

use super::{AppliedStyle, NativeNode, Node, Result, callback};
use crate::{
    input::{editor::Input, events::Interaction},
    render::geometry::Rect,
    scroll::controller::Scroller,
};

impl Node {
    pub(crate) fn new(
        id: NodeId,
        kind: ViewKind,
        context: Rc<crate::render::mount::UiContext>,
    ) -> Result<Rc<Self>> {
        let native = Custom::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        let visual_ptr = Rc::as_ptr(&native) as usize;
        let node = Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            accessibility_state: crate::accessibility::state::State::default(),
            #[cfg(feature = "api-23")]
            accessibility_provider: crate::accessibility::provider::State::default(),
            native,
            focus_sink: RefCell::new(None),
            wrapper: RefCell::default(),
            mount_parent: RefCell::default(),
            id,
            kind,
            pending_delete: Cell::new(false),
            scene_prepared: Cell::new(false),
            scene_root: Cell::new(false),
            scene_ancestor_captured: Cell::new(false),
            shader_layers: RefCell::default(),
            modal_screen_layers: RefCell::default(),
            backdrop_layer: Cell::new(None),
            modal_backdrop: Cell::new(None),
            navigation_capture: Cell::new(false),
            composite_dirty: Cell::new(true),
            capture_cache: Cell::new(None),
            navigation_shader: RefCell::default(),
            modal: RefCell::new(None),
            screen: RefCell::new(None),
            raw_frame: Cell::new(Rect::default()),
            frame: Cell::new(Rect::default()),
            parent: Cell::new(None),
            parent_node: RefCell::default(),
            children: RefCell::new(Vec::new()),
            context,
            input: RefCell::new(None),
            scroll: RefCell::new(None),
            image: RefCell::new(None),
            canvas: RefCell::new(None),
            component: RefCell::new(None),
            component_slots: RefCell::default(),
            component_animated: RefCell::default(),
            web: RefCell::default(),
            video: RefCell::default(),
            active: Cell::new(true),
            retained: Cell::new(false),
            retained_owner: RefCell::default(),
            retained_children: RefCell::default(),
            backface_away: Cell::new(false),
            interaction: Interaction::default(),
            style: RefCell::new(Box::default()),
            animated_opacity: Cell::new(None),
            applied: Cell::new(AppliedStyle {
                visual_ptr,
                ..AppliedStyle::default()
            }),
            styled: Cell::new(false),
            live: RefCell::default(),
            leaving: Cell::new(false),
            extra: Cell::new((1.0, crate::render::transition::Extra::NONE)),
            navigation_presentation: Cell::new((1.0, 0.0)),
            decoration: crate::render::decor::Decoration::default(),
            layout_callbacks: Cell::new(false),
            drawing: RefCell::new(None),
            drawing_bounds: Cell::new([0; 4]),
            outlined: RefCell::new(None),
            text_drawing: RefCell::new(None),
            selection_area: RefCell::new(None),
            raster: RefCell::new(None),
            selection: Default::default(),
            tint: Cell::new(None),
            content: Cell::new(Rect::default()),
            overflow: Cell::new(None),
            raster_origin_y: Cell::new(0.0),
            presentation: Cell::new((1.0, 0.0)),
            pinned: Cell::new((0.0, 0.0)),
            input_transform: Cell::new(nsh_core::Matrix::IDENTITY),
            applied_layout_size: Cell::new((0, 0, 0)),
            applied_position: Cell::new((0, 0, 0)),
            has_text_links: Cell::new(false),
            batch_created: Cell::new(false),
            culled_by_scroll: Cell::new(false),
            in_culled_subtree: Cell::new(false),
            style_deferred: Cell::new(false),
            bounds_deferred: Cell::new(false),
            paint_deferred: Cell::new(false),
            anim_deferred: Cell::new(false),
        });
        if id == nsh_core::ROOT_ID || kind == ViewKind::Modal {
            crate::input::keyboard::install(&node)?;
        }
        match kind {
            ViewKind::Modal => {
                *node.modal.borrow_mut() =
                    Some(crate::navigation::modal::Modal::new(Rc::downgrade(&node))?)
            }
            ViewKind::Image => {
                *node.image.borrow_mut() = Some(crate::media::image::ImageView::new(
                    Rc::downgrade(&node),
                    node.images.clone(),
                )?)
            }
            ViewKind::Input => *node.input.borrow_mut() = Some(Input::new(Rc::downgrade(&node))?),
            ViewKind::Scroll => {
                *node.scroll.borrow_mut() = Some(Scroller::new(Rc::downgrade(&node))?)
            }
            ViewKind::Canvas => {
                *node.canvas.borrow_mut() = Some(crate::render::canvas::CanvasView::new(
                    Rc::downgrade(&node),
                    node.canvas_bytes.clone(),
                ));
            }
            ViewKind::Video => {
                *node.video.borrow_mut() =
                    Some(crate::media::video::Video::new(Rc::downgrade(&node))?);
            }
            ViewKind::WebView => {
                *node.web.borrow_mut() =
                    Some(crate::media::webview::WebView::new(Rc::downgrade(&node))?);
            }
            ViewKind::Screen => {
                let screen = crate::navigation::stack::Screen::new(Rc::downgrade(&node))?;
                screen.install(&node)?;
                *node.screen.borrow_mut() = Some(screen);
            }
            ViewKind::View
            | ViewKind::Text
            | ViewKind::Native
            | ViewKind::Stack
            | ViewKind::HeaderItem
            | ViewKind::InputAccessory => {}
        }
        if !matches!(
            kind,
            ViewKind::View
                | ViewKind::Text
                | ViewKind::Stack
                | ViewKind::HeaderItem
                | ViewKind::InputAccessory
        ) {
            node.enable_layout();
        }
        if node.canvas.borrow().is_some() {
            node.enable_drawing()?;
        }
        if kind == ViewKind::HeaderItem {
            node.apply_z_index()?;
        }
        if matches!(kind, ViewKind::Modal | ViewKind::Screen) {
            node.ensure_wrapper()?;
        }
        Ok(node)
    }
    /// Whether it is leaving (an exit transition, a popped screen).
    pub(crate) fn is_leaving(&self) -> bool {
        self.leaving.get()
    }
    /// Marks this node leaving or not, keeping the count of leaving nodes
    /// the mounter skips its ancestor walks by. Returns whether it was.
    pub(crate) fn set_leaving(&self, leaving: bool) -> bool {
        let was = self.leaving.replace(leaving);
        if was != leaving {
            let count = self.leaving_nodes.get();
            debug_assert!(leaving || count > 0, "leaving count underflow");
            self.leaving_nodes.set(if leaving {
                count + 1
            } else {
                count.saturating_sub(1)
            });
        }
        was
    }
    pub(crate) fn self_ref(&self) -> Option<Rc<Self>> {
        self.weak.upgrade()
    }
    pub(crate) fn is_descendant_of(&self, ancestor_id: NodeId) -> bool {
        if self.id == ancestor_id {
            return true;
        }
        let mut parent = self.parent_node.borrow().upgrade();
        for _ in 0..1024 {
            let Some(node) = parent else {
                return false;
            };
            if node.id == ancestor_id {
                return true;
            }
            parent = node.parent_node.borrow().upgrade();
        }
        false
    }
    pub(crate) fn flush_deferred_native(self: &Rc<Self>) -> Result<()> {
        if self.style_deferred.replace(false) {
            if (self.style.borrow().effects.is_some() || !self.shader_layers.borrow().is_empty())
                && let Some(scene) = self.scene.borrow().as_ref()
            {
                scene.require(self);
            }
            self.ensure_wrapper()?;
            self.restore_style()?;
        }
        if self.bounds_deferred.replace(false) || self.applied_layout_size.get().0 == 0 {
            self.update_layout_bounds()?;
        }
        if self.paint_deferred.replace(false) {
            if self.text_drawing.borrow().is_some() && self.drawing.borrow().is_none() {
                self.enable_drawing()?;
            } else {
                self.invalidate_paint()?;
            }
        }
        Ok(())
    }
    pub(crate) fn allows_scroll_group_walk(&self) -> bool {
        if self.kind != ViewKind::View
            || self.pinned.get() != (0.0, 0.0)
            || self.extra.get().1 != crate::render::transition::Extra::NONE
            || self.scene_root.get()
            || self.scene_active()
            || self.scene_ancestor_captured.get()
            || self.navigation_capture.get()
            || self.shaders.contains(self.id)
            || self.wrapper.borrow().is_some()
            || self.live.borrow().is_some()
        {
            return false;
        }
        let style = self.style.borrow();
        !style.hidden
            && style.transform.is_identity()
            && style
                .effects
                .as_ref()
                .is_none_or(|e| e.filters.is_empty() && e.backdrop_filters.is_empty())
    }
    pub(crate) fn sync_culled_subtree(self: &Rc<Self>, ancestor_culled: bool) -> Result<()> {
        let effective =
            ancestor_culled || (self.culled_by_scroll.get() && !self.navigation_capture.get());
        let changed = self.in_culled_subtree.replace(effective) != effective;
        if !effective
            && changed
            && self.anim_deferred.replace(false)
            && !self.style_deferred.get()
            && self.mount_parent.borrow().strong_count() > 0
        {
            self.restore_style()?;
        }
        if changed {
            let children: Vec<Rc<Node>> = self
                .children
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for child in children {
                child.sync_culled_subtree(effective)?;
            }
        }
        Ok(())
    }
    pub(crate) fn set_culled_by_scroll(self: &Rc<Self>, culled: bool) -> Result<()> {
        if self.culled_by_scroll.replace(culled) != culled {
            let ancestor_culled = self
                .parent_node
                .borrow()
                .upgrade()
                .is_some_and(|parent| parent.in_culled_subtree.get());
            self.sync_culled_subtree(ancestor_culled)?;
            if !self.style_deferred.get() {
                self.apply_opacity(self.style.borrow().opacity)?;
            }
        }
        Ok(())
    }
    pub(crate) fn self_transform_allows_scroll_cull(&self) -> bool {
        self.pinned.get() == (0.0, 0.0)
            && self.extra.get().1 == crate::render::transition::Extra::NONE
            && self.live.borrow().is_none()
            && self.style.borrow().transform.is_identity()
    }
    pub(crate) fn self_allows_scroll_cull(&self) -> bool {
        self.input.borrow().is_none()
            && self.web.borrow().is_none()
            && self.video.borrow().is_none()
            && self.modal.borrow().is_none()
            && self.component.borrow().is_none()
            && !self.shaders.contains(self.id)
            && self.shader_layers.borrow().is_empty()
            && self
                .wrapper
                .borrow()
                .as_ref()
                .is_none_or(|wrapper| wrapper.composite.borrow().is_none())
            && !self.navigation_capture.get()
            && !self.scene_root.get()
            && !self.scene_active()
            && !self.has_selection()
            && !self.accessibility_state.has_live()
    }
    pub(crate) fn can_cull_subtree_inner(&self) -> bool {
        if !self.self_allows_scroll_cull() {
            return false;
        }
        self.children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .all(|child| child.can_cull_subtree_inner())
    }
    pub(crate) fn scroll_ink_bounds(&self) -> (f32, f32, f32, f32) {
        let f = self.frame.get();
        let style = self.style.borrow();
        let mut outset = self.applied.get().ink_outset;
        if style.outline.is_visible() {
            outset = outset.max((style.outline.width.0 + style.outline.offset.0).max(0.0));
        }
        if let Some(effects) = style.effects.as_ref() {
            for shadow in &effects.box_shadows {
                if !shadow.inset && shadow.is_visible() {
                    let reach = shadow.offset_x.0.abs().max(shadow.offset_y.0.abs())
                        + shadow.blur.0.max(0.0) * 1.5
                        + shadow.spread.0.max(0.0);
                    outset = outset.max(reach);
                }
            }
        }
        drop(style);
        let pad = outset + 4.0;
        let (mut x0, mut y0, mut x1, mut y1) = (
            f.x - pad,
            f.y - pad,
            f.x + f.width + pad,
            f.y + f.height + pad,
        );
        if let Some(ov) = self.overflow.get() {
            x0 = x0.min(f.x + ov.x - pad);
            y0 = y0.min(f.y + ov.y - pad);
            x1 = x1.max(f.x + ov.x + ov.width + pad);
            y1 = y1.max(f.y + ov.y + ov.height + pad);
        }
        (x0, y0, x1, y1)
    }
    pub(crate) fn ancestors_allow_scroll_cull(&self) -> bool {
        let mut cur = self.self_ref();
        while let Some(node) = cur {
            if node.id == nsh_core::ROOT_ID {
                break;
            }
            if node.scene_active()
                || node.navigation_capture.get()
                || node.shaders.contains(node.id)
                || node.effects_plan().requires_texture
            {
                return false;
            }
            cur = node.parent_node.borrow().upgrade();
        }
        true
    }
    pub(crate) fn deactivate(&self) {
        if !self.active.replace(false) {
            return;
        }
        self.animated_opacity.set(None);
        callback(|| self.cursor.update(self, false));
        callback(|| self.selection_clear());
        if self.scene_active() {
            self.invalidate_scene();
        }
        self.accessibility_state.deactivate(self);
        #[cfg(feature = "api-23")]
        self.accessibility_provider.clear();
        if let Some(input) = self.input.borrow().as_ref() {
            callback(|| input.close());
        }
        if let Some(scroll) = self.scroll.borrow().as_ref() {
            self.scroll_watchers.remove(self.id);
            if let Some(plane) = scroll.overlay.borrow().as_ref() {
                callback(|| plane.close(self));
            }
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            self.accessibility_wrappers
                .set(self.accessibility_wrappers.get().saturating_sub(1));
            callback(|| wrapper.close(self));
        }
        if let Some(web) = self.web.borrow().as_ref() {
            callback(|| web.close());
        }
        if let Some(video) = self.video.borrow().as_ref() {
            callback(|| video.close());
        }
        if self
            .key_focus
            .borrow()
            .upgrade()
            .is_some_and(|node| node.id == self.id)
        {
            *self.key_focus.borrow_mut() = Weak::new();
        }
        if let Some(route) = self.dispatcher.upgrade() {
            let mut keyboard = route.keyboard.lock().unwrap();
            if keyboard.focused == Some(self.id) {
                keyboard.focused = None;
            }
            if keyboard.input == Some(self.id) {
                keyboard.input = None;
                keyboard.composing = false;
            }
        }
        if let Some(component) = self.component.borrow().as_ref() {
            component.deactivate();
        }
    }
    pub(crate) fn child_container(&self) -> NativeNode {
        self.scroll
            .borrow()
            .as_ref()
            .map_or_else(|| self.native.clone(), |scroll| scroll.content.clone())
    }
    pub(crate) fn child_index(&self, index: usize) -> usize {
        let children = self.children.borrow();
        let index = index.min(children.len());
        let container = self.child_container();
        let native = container.borrow();
        // Retained removed nodes and platform overlays are not logical siblings.
        // Locate a real sibling instead of counting assumed native child slots.
        for child in children[index..].iter().filter_map(Weak::upgrade) {
            let mounted = child.mounted();
            if let Some(at) = native
                .children()
                .iter()
                .position(|entry| Rc::ptr_eq(entry, &mounted))
            {
                return at;
            }
        }
        for child in children[..index].iter().rev().filter_map(Weak::upgrade) {
            let mounted = child.mounted();
            // Appending usually follows the final native sibling. Searching
            // from that end avoids rescanning the growing prefix per child.
            if let Some(at) = native
                .children()
                .iter()
                .rposition(|entry| Rc::ptr_eq(entry, &mounted))
            {
                return at + 1;
            }
        }
        let anchors = [
            self.focus_sink.borrow().clone(),
            self.component
                .borrow()
                .as_ref()
                .map(|component| component.native.clone()),
            self.screen
                .borrow()
                .as_ref()
                .map(|screen| screen.header().clone()),
            self.web.borrow().as_ref().map(|web| web.native().clone()),
            self.video
                .borrow()
                .as_ref()
                .filter(|video| !video.is_fullscreen())
                .map(|video| video.native().clone()),
            self.shaders.native(self.id),
        ];
        anchors
            .iter()
            .flatten()
            .filter_map(|anchor| {
                native
                    .children()
                    .iter()
                    .position(|child| Rc::ptr_eq(child, anchor))
            })
            .max()
            .map_or(0, |index| index + 1)
    }
    fn detach_component(&self, component: &crate::interop::component::Instance) -> Result<()> {
        let index = self
            .native
            .borrow()
            .children()
            .iter()
            .position(|child| Rc::ptr_eq(child, &component.native));
        if let Some(index) = index {
            self.native.borrow_mut().remove_child(index)?;
        }
        Ok(())
    }
    pub(crate) fn attach_component(
        &self,
        component: crate::interop::component::Instance,
    ) -> Result<()> {
        if let Some(old) = self.component.borrow_mut().take() {
            self.detach_component(&old)?;
            old.dispose()?;
        }
        let frame = self.frame.get();
        component.native.borrow().width(frame.width)?;
        component.native.borrow().height(frame.height)?;
        self.promote_direct_paint()?;
        self.native
            .borrow_mut()
            .insert_child(component.native.clone(), 0)?;
        *self.component.borrow_mut() = Some(component);
        self.native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.set_leaving(false);
        // Removed children still belong to their engine slots. Release their
        // native edges before disposing this parent's platform-owned children.
        for child in self
            .retained_children
            .take()
            .into_iter()
            .filter_map(|child| child.upgrade())
        {
            if std::ptr::eq(child.retained_owner.borrow().as_ptr(), self) {
                callback(|| {
                    let mounted = child.mounted();
                    if let Some(parent) = child.mount_parent.borrow().upgrade() {
                        let index = parent
                            .borrow()
                            .children()
                            .iter()
                            .position(|node| Rc::ptr_eq(node, &mounted));
                        if let Some(index) = index {
                            parent.borrow_mut().remove_child(index)?;
                        }
                    }
                    *child.mount_parent.borrow_mut() = Weak::new();
                    *child.retained_owner.borrow_mut() = Weak::new();
                    Ok(())
                });
            }
        }
        self.drawing.borrow_mut().take();
        self.outlined.borrow_mut().take();
        // Struct fields otherwise outlive this Drop body. Release controller
        // handles before recursively disposing their mounted native descendants.
        #[cfg(feature = "api-23")]
        self.accessibility_provider.clear();
        if let Some(input) = self.input.borrow_mut().take() {
            callback(|| input.close());
        }
        if let Some(web) = self.web.borrow_mut().take() {
            callback(|| web.close());
            let native = web.native().clone();
            drop(web);
            if native.borrow().is_owned() {
                callback(|| {
                    let index = self
                        .native
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &native));
                    if let Some(index) = index {
                        if native.borrow().parent()?.is_some() {
                            self.native.borrow_mut().remove_child(index)?;
                        } else {
                            self.native.borrow_mut().children_mut().remove(index);
                        }
                    } else {
                        native.borrow().remove_from_parent()?;
                    }
                    if Rc::strong_count(&native) == 1 {
                        native.borrow_mut().dispose()?;
                    }
                    Ok(())
                });
            }
        }
        if let Some(video) = self.video.borrow_mut().take() {
            callback(|| video.close());
        }
        callback(|| self.interaction.dispose(self));
        if let Some(scroll) = self.scroll.borrow_mut().take() {
            callback(|| scroll.dispose());
            if let Some(plane) = scroll.overlay.borrow_mut().take() {
                callback(|| plane.close(self));
                // Its outer node remains in self.native; dropping Plane
                // releases its duplicate handle and its inner wrapper handle.
            }
            let wrapper = scroll.wrapper.clone();
            drop(scroll);
            // Once the overlay attachment releases this wrapper, its detached
            // tree needs its own disposal.
            if wrapper.borrow().is_owned() && Rc::strong_count(&wrapper) == 1 {
                callback(|| {
                    wrapper.borrow().remove_from_parent()?;
                    wrapper.borrow_mut().dispose()?;
                    Ok(())
                });
            }
        }
        self.screen.borrow_mut().take();
        self.image.borrow_mut().take();
        if let Some(modal) = self.modal.borrow_mut().take() {
            callback(|| modal.close(false));
        }
        if let Some(component) = self.component.borrow_mut().take() {
            callback(|| {
                self.detach_component(&component)?;
                component.dispose()
            });
        }
        if let Some(sink) = self.focus_sink.borrow_mut().take() {
            callback(|| {
                let index = self
                    .native
                    .borrow()
                    .children()
                    .iter()
                    .position(|child| Rc::ptr_eq(child, &sink));
                if let Some(index) = index {
                    self.native.borrow_mut().remove_child(index)?;
                }
                if Rc::strong_count(&sink) == 1 {
                    sink.borrow_mut().dispose()?;
                }
                Ok(())
            });
        }
        if let Some(wrapper) = self.wrapper.borrow_mut().take() {
            callback(|| wrapper.close(self));
            if wrapper.outer.borrow().is_owned() && Rc::strong_count(&wrapper.outer) == 1 {
                callback(|| {
                    wrapper.outer.borrow().remove_from_parent()?;
                    wrapper.outer.borrow_mut().dispose()?;
                    Ok(())
                });
            }
        }
        // A retiring Video retains this parent until its worker acknowledges
        // release. That UI owner disposes the parent after detaching its child.
        if Rc::strong_count(&self.native) == 1
            && let Err(error) = self.native.borrow_mut().dispose()
        {
            log::error!("OHOS dispose: {error}");
        }
    }
}
