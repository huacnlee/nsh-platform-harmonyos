//! Surface-local composition policy. ArkUI retains every native input/scroll node.
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{Arc, atomic::Ordering},
};

use nsh_core::{Matrix, ViewKind};
use ohos_arkui_binding::types::advanced::NodeDirtyFlag;
use ohos_drawing_binding::{Bitmap, BitmapFormat, Canvas};
use rustc_hash::FxHashMap;

use crate::render::{
    composition::{
        draw::{PaintNode, Plan, Root},
        geometry::{Bounds, target_size},
        raster::Shared as SceneShared,
    },
    effects::has_active_filters,
    geometry::Size,
    node::{Error, Node, Result},
};

struct RootBitmap {
    bitmap: Bitmap,
    x: f32,
    y: f32,
    scale: f32,
}

pub(crate) struct Scene {
    root: Weak<Node>,
    active: Cell<bool>,
    dirty: Cell<bool>,
    preparing: Cell<bool>,
    requeued: Cell<bool>,
    shared: Arc<SceneShared>,
    bitmaps: RefCell<FxHashMap<u32, RootBitmap>>,
    roots: RefCell<Vec<Weak<Node>>>,
    requirements: RefCell<Vec<Weak<Node>>>,
}
impl Scene {
    pub(crate) fn new(root: &Rc<Node>) -> Self {
        Self {
            root: Rc::downgrade(root),
            active: Cell::new(false),
            dirty: Cell::new(false),
            preparing: Cell::new(false),
            requeued: Cell::new(false),
            shared: Arc::default(),
            bitmaps: RefCell::default(),
            roots: RefCell::default(),
            requirements: RefCell::default(),
        }
    }
    pub(crate) fn has_output(&self) -> bool {
        !self.bitmaps.borrow().is_empty()
    }
    pub(crate) fn require(&self, node: &Rc<Node>) {
        let required = node.effects_plan().requires_scene;
        let mut nodes = self.requirements.borrow_mut();
        nodes.retain(|weak| weak.upgrade().is_some_and(|owner| owner.id != node.id));
        if required {
            nodes.push(Rc::downgrade(node));
        }
        drop(nodes);
        if self.preparing.get() {
            self.requeued.set(true);
        } else {
            self.invalidate();
        }
    }
    pub(crate) fn invalidate(&self) {
        if (self.active.get() || !self.requirements.borrow().is_empty())
            && !self.preparing.get()
            && !self.dirty.replace(true)
            && let Some(root) = self.root.upgrade()
            && let Some(route) = root.dispatcher.upgrade()
        {
            route.send(crate::app::runtime::Message::Arkui);
        }
    }
    pub(crate) fn prepare(&self) -> Result<()> {
        if !self.dirty.replace(false) {
            return Ok(());
        }
        self.preparing.set(true);
        let result = self.reconcile();
        self.preparing.set(false);
        if self.requeued.replace(false) {
            self.invalidate();
        }
        result
    }
    fn reconcile(&self) -> Result<()> {
        let Some(root) = self.root.upgrade() else {
            return Ok(());
        };
        let mut requirements = self.requirements.borrow_mut();
        requirements.retain(|node| node.strong_count() > 0);
        // Most surfaces have no scene effects. Check only current trigger nodes;
        // ordinary mount/animation turns do not scan the whole native tree.
        let mut candidates = Vec::new();
        for node in requirements.iter().filter_map(Weak::upgrade) {
            if !node.paint_visible() {
                continue;
            }
            let mut connected = false;
            let mut ancestor = Some(node.clone());
            while let Some(current) = ancestor {
                if Rc::ptr_eq(&current, &root) {
                    connected = true;
                    break;
                }
                ancestor = current.parent_node.borrow().upgrade();
            }
            if connected {
                let candidate = scene_root_for(&node, &root);
                if !candidates
                    .iter()
                    .any(|existing: &Rc<Node>| Rc::ptr_eq(existing, &candidate))
                {
                    candidates.push(candidate);
                }
            }
        }
        drop(requirements);
        let initial_roots: Vec<Rc<Node>> = candidates
            .iter()
            .filter(|candidate| {
                !candidates.iter().any(|other| {
                    !Rc::ptr_eq(candidate, other) && is_scene_ancestor(other, candidate)
                })
            })
            .cloned()
            .collect();
        let active = !initial_roots.is_empty();
        let previous = self.active.replace(active);
        if active {
            return self.build(&root, initial_roots);
        }
        if previous {
            if let Some(worker) = root.compositor.borrow().as_ref() {
                worker.clear_plan();
            }
            self.bitmaps.borrow_mut().clear();
            let old_roots = std::mem::take(&mut *self.roots.borrow_mut());
            self.shared.output.lock().unwrap().take();
            for old in old_roots.into_iter().filter_map(|w| w.upgrade()) {
                restore(&old)?;
            }
            restore(&root)?;
        }
        Ok(())
    }
    fn build(&self, root: &Rc<Node>, initial_roots: Vec<Rc<Node>>) -> Result<()> {
        for old in self.roots.borrow().iter().filter_map(Weak::upgrade) {
            if !initial_roots
                .iter()
                .any(|next| Rc::ptr_eq(&old, next) || is_scene_ancestor(next, &old))
            {
                restore(&old)?;
            }
        }
        fn prepare(node: &Rc<Node>, roots: &mut Vec<Rc<Node>>, depth: usize) -> Result<()> {
            if depth > 256 {
                return Err(Error::Unsupported("scene hierarchy exceeds 256 levels"));
            }
            if !node.paint_visible() {
                return Ok(());
            }
            let was_prepared = node.scene_prepared.replace(true);
            node.ensure_wrapper()?;
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                scroll.prepare_scene(node)?;
            }
            if !was_prepared {
                node.restore_style()?;
            }
            if node.kind == ViewKind::Modal
                && !roots.iter().any(|existing| Rc::ptr_eq(existing, node))
            {
                node.scene_root.set(true);
                roots.push(node.clone());
            }
            for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
                prepare(&child, roots, depth + 1)?;
            }
            Ok(())
        }
        let mut roots = initial_roots.clone();
        let build_result = (|| -> Result<Plan> {
            for scene_root in &initial_roots {
                scene_root.scene_root.set(true);
                prepare(scene_root, &mut roots, 0)?;
            }
            let mut plan = Plan { roots: Vec::new() };
            for scene_root in &roots {
                if !scene_root.paint_visible() {
                    continue;
                }
                let node = snapshot(scene_root, None, 0)?;
                let own_filters = node
                    .style
                    .effects
                    .as_ref()
                    .map_or(&[][..], |effects| effects.filters.as_slice());
                let root_bounds = node.bounds.filters(own_filters);
                let logical_w = (root_bounds.0[2] - root_bounds.0[0]).max(1.0);
                let logical_h = (root_bounds.0[3] - root_bounds.0[1]).max(1.0);
                let (_, _, root_scale) = target_size(logical_w, logical_h, scene_root.scale.get());
                if root_scale <= 0.0 {
                    return Err(Error::Unsupported("invalid scene root dimensions"));
                }
                let origin_x = (root_bounds.0[0] * root_scale).floor();
                let origin_y = (root_bounds.0[1] * root_scale).floor();
                let width =
                    ((root_bounds.0[2] * root_scale).ceil() - origin_x).clamp(1.0, 4096.0) as u32;
                let height =
                    ((root_bounds.0[3] * root_scale).ceil() - origin_y).clamp(1.0, 4096.0) as u32;
                plan.roots.push(Root {
                    id: scene_root.id,
                    width,
                    height,
                    origin: (origin_x, origin_y),
                    scale: root_scale,
                    node,
                });
            }
            Ok(plan)
        })();
        let plan = match build_result {
            Ok(plan) => plan,
            Err(error) => {
                // Roll back any nodes touched during the failed build so no node
                // remains stranded with `scene_prepared = true` outside `self.roots`.
                self.active.set(false);
                if let Some(worker) = root.compositor.borrow().as_ref() {
                    worker.clear_plan();
                }
                self.bitmaps.borrow_mut().clear();
                self.shared.output.lock().unwrap().take();
                let old_roots = std::mem::take(&mut *self.roots.borrow_mut());
                for old in old_roots.into_iter().filter_map(|w| w.upgrade()) {
                    let _ = restore(&old);
                }
                for touched in &roots {
                    let _ = restore(touched);
                }
                let _ = restore(root);
                return Err(error);
            }
        };
        *self.roots.borrow_mut() = roots.iter().map(Rc::downgrade).collect();
        if let Some(worker) = root.compositor.borrow().as_ref() {
            worker.plan(Arc::new(plan), self.shared.clone());
        }
        Ok(())
    }
    pub(crate) fn ready(&self) -> Result<()> {
        self.shared.posted.store(false, Ordering::Release);
        let frames = self.shared.output.lock().unwrap().take();
        if !self.active.get() {
            return Ok(());
        }
        if let Some(frames) = frames {
            let was_empty = !self.has_output();
            let mut bitmaps = self.bitmaps.borrow_mut();
            let mut next_ids = rustc_hash::FxHashSet::default();
            for frame in frames {
                next_ids.insert(frame.id);
                if let Some(existing) = bitmaps.get_mut(&frame.id)
                    && existing.bitmap.width() == frame.width
                    && existing.bitmap.height() == frame.height
                {
                    existing.bitmap.pixels_mut().copy_from_slice(&frame.pixels);
                    existing.x = frame.x;
                    existing.y = frame.y;
                    existing.scale = frame.scale;
                } else {
                    let mut bitmap =
                        Bitmap::new(frame.width, frame.height, BitmapFormat::default());
                    bitmap.pixels_mut().copy_from_slice(&frame.pixels);
                    bitmaps.insert(
                        frame.id,
                        RootBitmap {
                            bitmap,
                            x: frame.x,
                            y: frame.y,
                            scale: frame.scale,
                        },
                    );
                }
            }
            bitmaps.retain(|id, _| next_ids.contains(id));
            let has_output = !bitmaps.is_empty();
            drop(bitmaps);
            for root in self.roots.borrow().iter().filter_map(Weak::upgrade) {
                root.mounted()
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
            if was_empty
                && has_output
                && let Some(root) = self.root.upgrade()
            {
                let composites = root.composite_nodes.borrow().clone();
                for node in composites.into_iter().filter_map(|w| w.upgrade()) {
                    node.update_wrapper()?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn draw(&self, id: u32, canvas: &Canvas, display_scale: f32) {
        if let Some(entry) = self.bitmaps.borrow().get(&id) {
            let scale = entry.scale.max(0.01);
            if (display_scale - scale).abs() <= 1e-4 {
                canvas.draw_bitmap(&entry.bitmap, entry.x, entry.y);
            } else {
                let ratio = display_scale / scale;
                canvas.save();
                canvas.scale(ratio, ratio);
                canvas.draw_bitmap(&entry.bitmap, entry.x, entry.y);
                canvas.restore();
            }
        }
    }
}
fn restore(node: &Rc<Node>) -> Result<()> {
    let was_prepared = node.scene_prepared.replace(false);
    let was_root = node.scene_root.replace(false);
    if was_prepared || was_root {
        if let Some(scroll) = node.scroll.borrow().as_ref() {
            scroll.restore_scene(node)?;
        }
        node.ensure_wrapper()?;
        node.restore_style()?;
        node.native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        node.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        node.mounted()
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedRender)?;
    }
    for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
        restore(&child)?;
    }
    Ok(())
}
fn is_scene_ancestor(ancestor: &Rc<Node>, node: &Rc<Node>) -> bool {
    let mut current = node.parent_node.borrow().upgrade();
    while let Some(parent) = current {
        if parent.kind == ViewKind::Modal {
            return false;
        }
        if Rc::ptr_eq(&parent, ancestor) {
            return true;
        }
        current = parent.parent_node.borrow().upgrade();
    }
    false
}
fn scene_root_for(node: &Rc<Node>, root: &Rc<Node>) -> Rc<Node> {
    let samples_backdrop = {
        let style = node.style.borrow();
        style.blend_mode != nsh_core::BlendMode::Normal
            || style
                .effects
                .as_ref()
                .is_some_and(|e| has_active_filters(&e.backdrop_filters))
            || node.shader_layers.borrow().iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            })
    };
    if !samples_backdrop {
        let overflows = {
            let style = node.style.borrow();
            style.outline.is_visible()
                || style
                    .effects
                    .as_ref()
                    .is_some_and(|e| !e.box_shadows.is_empty() || has_active_filters(&e.filters))
                || !node.shader_layers.borrow().is_empty()
        };
        if !overflows {
            return node.clone();
        }
        if let Some(parent) = node.parent_node.borrow().upgrade() {
            return parent;
        }
        return node.clone();
    }
    let mut current = node.parent_node.borrow().upgrade();
    while let Some(ancestor) = current {
        let backdrop_root = {
            let style = ancestor.style.borrow();
            let effects = style.effects.as_ref();
            style.opacity < 1.0
                || style.blend_mode != nsh_core::BlendMode::Normal
                || effects.is_some_and(|e| {
                    !e.mask_images.is_empty()
                        || has_active_filters(&e.filters)
                        || has_active_filters(&e.backdrop_filters)
                })
        };
        if Rc::ptr_eq(&ancestor, root)
            || ancestor.kind == ViewKind::Modal
            || ancestor.kind == ViewKind::Screen
            || backdrop_root
            || !ancestor.shader_layers.borrow().is_empty()
            || ancestor.navigation_capture.get()
        {
            return ancestor;
        }
        if ancestor.clips_children() {
            let opaque_bg = !ancestor.style.borrow().background_color.is_transparent()
                || ancestor
                    .children
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .find(|child| child.paint_visible())
                    .is_some_and(|first| !first.style.borrow().background_color.is_transparent());
            if opaque_bg {
                return ancestor;
            }
        }
        current = ancestor.parent_node.borrow().upgrade();
    }
    root.clone()
}
fn snapshot(owner: &Rc<Node>, parent: Option<&Node>, depth: usize) -> Result<PaintNode> {
    if depth > 256 {
        return Err(Error::Unsupported("scene hierarchy exceeds 256 levels"));
    }
    let frame = owner.frame.get();
    let size = Size::new(frame.width, frame.height);
    let style = owner.style.borrow().clone();
    let source = owner.wrapper.borrow().as_ref().and_then(|wrapper| {
        wrapper
            .composite
            .borrow()
            .as_ref()
            .map(|composite| composite.key())
    });
    let mut children: Vec<_> = owner
        .children
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|child| child.kind != ViewKind::Modal && child.paint_visible())
        .collect();
    children.sort_by_key(|child| child.input_priority());
    let mut bounds = Bounds::new(size.width, size.height)
        .shaders(
            owner.shaders.overflow(owner.id),
            &owner.shader_layers.borrow(),
        )
        .own(&style);
    let children = children
        .iter()
        .map(|child| snapshot(child, Some(owner), depth + 1))
        .collect::<Result<Vec<_>>>()?;
    if !owner.clips_children() {
        for child in &children {
            let effects = child
                .style
                .effects
                .as_ref()
                .map_or(&[][..], |effects| effects.filters.as_slice());
            if let Some(child) = child.bounds.filters(effects).transformed(child.transform) {
                bounds.union(child);
            } else {
                return Err(Error::Unsupported("unbounded scene projection"));
            }
        }
    }
    if let Some(effects) = &style.effects {
        bounds.union(Bounds::new(size.width, size.height).filters(&effects.backdrop_filters));
    }
    let transform = parent.map_or(Ok(Matrix::IDENTITY), |parent| owner.input_to_parent(parent))?;
    Ok(PaintNode {
        source,
        shaders: !owner.shader_layers.borrow().is_empty(),
        shader_backdrop: owner.id != nsh_core::ROOT_ID
            && owner.shader_layers.borrow().iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            }),
        capture: owner.navigation_capture.get(),
        overlay: owner.scroll.borrow().as_ref().and_then(|scroll| {
            scroll
                .overlay
                .borrow()
                .as_ref()
                .and_then(|plane| plane.key())
        }),
        transform,
        size,
        opacity: owner.scene_opacity(),
        clip: owner.clips_children(),
        bounds,
        style: Arc::from(style),
        children,
    })
}
