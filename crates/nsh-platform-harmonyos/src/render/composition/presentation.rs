//! Shader frames presented by an XComponent Surface, beside the sampled subtree.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use napi_ohos::{
    JsValue,
    bindgen_prelude::{FnArgs, Function, JsObjectValue, Object, PromiseRaw, Unknown},
};
use ohos_arkui_binding::{
    api::{attribute_option::DrawableDescriptor, node_custom_event::NodeCustomEvent},
    common::node_registry,
    component::{attribute::ArkUICommonAttribute, built_in_component::Image},
    types::{
        advanced::{LengthMetricUnit, NodeDirtyFlag},
        attribute::ArkUINodeAttributeType as Attribute,
        view::{AccessibilityMode, HitTestMode},
    },
};
use ohos_image_native_binding::PixelMap;

use crate::render::node::{NativeNode, Node, Result, callback};

/// Holds the last native frame while the root changes its producer destination.
struct NativeFrame {
    node: NativeNode,
    descriptor: DrawableDescriptor,
    _pixels: ohos_image_native_binding::PixelMap,
}
impl NativeFrame {
    fn from_pixels(parent: &NativeNode, owner: &Node, pixels: PixelMap) -> Result<Self> {
        let descriptor = DrawableDescriptor::from_pixel_map(pixels.handle())?;
        let frame = Self {
            node: Image::new()?.into_node(),
            descriptor,
            _pixels: pixels,
        };
        {
            let image = frame.node.borrow();
            image.set_length_metric_unit(LengthMetricUnit::Vp)?;
            image.set_i32_attribute(Attribute::HitTestBehavior, HitTestMode::None.into())?;
            image.set_i32_attribute(
                Attribute::AccessibilityMode,
                AccessibilityMode::DisabledForDescendants.into(),
            )?;
            image.set_bool_attribute(Attribute::ImageSyncLoad, true)?;
            image.set_i32_attribute(Attribute::ImageObjectFit, 3)?;
            image.set_attribute(Attribute::ImageSrc, (&frame.descriptor).into())?;
            let size = owner.frame.get();
            image.width(size.width)?;
            image.height(size.height)?;
            image.z_index(2)?;
        }
        parent.borrow_mut().add_child(frame.node.clone())?;
        Ok(frame)
    }
}

impl Drop for NativeFrame {
    fn drop(&mut self) {
        callback(|| {
            let mut node = self.node.borrow_mut();
            if node.is_owned() {
                node_registry::unregister(&node);
                node.dispose()?;
            }
            Ok(())
        });
    }
}

/// Keeps the original native subtree on screen until its replacement image is attached.
/// Only this presentation owns the request. Retiring it invalidates delayed SDK callbacks.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum CaptureState {
    #[default]
    Waiting,
    Pending,
    Finished,
}

#[derive(Default)]
struct FrameCapture {
    state: Cell<CaptureState>,
    animation: RefCell<Option<crate::render::animation::PresentedAnimation>>,
    frame: RefCell<Option<NativeFrame>>,
}

impl FrameCapture {
    fn start(self: &Rc<Self>, owner: &Node, parent: &NativeNode, screen_effect: bool) {
        if self.state.get() != CaptureState::Waiting {
            return;
        }
        self.state.set(CaptureState::Pending);
        if screen_effect {
            *self.animation.borrow_mut() = owner.animator.hold_presented_animation();
        }
        if let Err(error) = self.request(owner, parent) {
            log::debug!("OHOS asynchronous root snapshot unavailable: {error}");
            self.install(owner, parent, None);
        }
    }

    fn request(self: &Rc<Self>, owner: &Node, parent: &NativeNode) -> Result<()> {
        let id = parent.borrow().unique_id()?;
        let node = Rc::downgrade(
            &owner
                .self_ref()
                .ok_or(crate::render::node::Error::InvalidNode(owner.id))?,
        );
        let env = &owner.env;
        env.run_in_scope(|| {
            let ui = owner.host.ui_context(env)?;
            let service: Function<'_, (), Object<'_>> =
                ui.get_named_property("getComponentSnapshot")?;
            let service = service.apply(ui, ())?;
            let get: Function<'_, i32, PromiseRaw<'_, Object<'_>>> =
                service.get_named_property("getWithUniqueId")?;
            let capture = get.apply(service, id)?;
            let success = Rc::downgrade(self);
            let failure = success.clone();
            let timeout = success.clone();
            let timeout_owner = node.clone();
            let success_owner = node.clone();
            capture
                .then(move |ctx| {
                    Self::complete(&success, &success_owner, || {
                        PixelMap::from_napi(ctx.env.raw().cast(), ctx.value.raw().cast()).ok()
                    });
                    Ok(())
                })?
                .catch::<Unknown<'_>, (), _>(move |_| {
                    Self::complete(&failure, &node, || None);
                    Ok(())
                })?;
            // A suspended or detached SDK snapshot can outlive its request. Give a
            // still-active root its synchronous fallback if no callback arrives.
            let handler = env.create_function_from_closure("rootSnapshotDeadline", move |_| {
                Self::complete(&timeout, &timeout_owner, || None);
                Ok(())
            })?;
            let global = env.get_global()?;
            type TimerArgs<'env> = FnArgs<(Function<'env, (), ()>, u32)>;
            let schedule: Function<'_, TimerArgs<'_>, Unknown<'_>> =
                global.get_named_property("setTimeout")?;
            schedule.call((handler, 500).into())?;
            Ok(())
        })?;
        Ok(())
    }

    fn complete(
        capture: &Weak<Self>,
        node: &Weak<Node>,
        pixels: impl FnOnce() -> Option<PixelMap>,
    ) {
        let (Some(capture), Some(owner)) = (capture.upgrade(), node.upgrade()) else {
            return;
        };
        if capture.state.get() != CaptureState::Pending
            || !owner.active.get()
            || owner.host.check().is_err()
        {
            return;
        }
        callback(|| {
            capture.install(&owner, &owner.mounted(), pixels());
            owner.update_wrapper()?;
            owner.flush_arkui()
        });
    }

    fn install(&self, owner: &Node, parent: &NativeNode, pixels: Option<PixelMap>) {
        let result = match pixels {
            Some(pixels) => Ok(pixels),
            None => parent.borrow().snapshot(),
        };
        match result
            .map_err(crate::render::node::Error::from)
            .and_then(|pixels| NativeFrame::from_pixels(parent, owner, pixels))
        {
            Ok(frame) => *self.frame.borrow_mut() = Some(frame),
            Err(error) => log::warn!("OHOS root snapshot unavailable: {error}"),
        }
        self.state.set(CaptureState::Finished);
    }
}

pub(super) struct Presentation {
    parent: NativeNode,
    surface: super::surface::Surface,
    native_frame: Option<Rc<FrameCapture>>,
    scale: f32,
    origin: (i32, i32),
    size: (u32, u32),
    revealed: bool,
}

impl Presentation {
    pub(super) fn new(
        owner: &Node,
        key: super::worker::Key,
        events: super::worker::WindowEvents,
    ) -> Result<Self> {
        let surface = super::surface::Surface::new(
            format!("nsh-effect-{}-{}", key.id, key.generation),
            events,
            false,
        )?;
        surface.native.borrow().z_index(1)?;
        surface.native.borrow().opacity(0.0)?;
        let native = surface.native.clone();
        let parent = owner.mounted();
        parent.borrow_mut().add_child(native)?;
        let native_frame = if key.id == nsh_core::ROOT_ID && !key.is_overlay() {
            Some(Rc::new(FrameCapture::default()))
        } else {
            None
        };
        let mut layer = Self {
            parent,
            surface,
            native_frame,
            scale: owner.scale.get(),
            origin: (0, 0),
            size: (0, 0),
            revealed: false,
        };
        let frame = owner.frame.get();
        let scale = owner.scale.get();
        layer.update(
            (frame.width * scale).ceil().max(1.0) as u32,
            (frame.height * scale).ceil().max(1.0) as u32,
            0.0,
            0.0,
            scale,
        )?;
        Ok(layer)
    }

    pub(super) fn is_revealed(&self) -> bool {
        self.revealed
    }

    pub(super) fn capture_before_export(&self, owner: &Node, screen_effect: bool) {
        if let Some(capture) = &self.native_frame {
            capture.start(owner, &self.parent, screen_effect);
        }
    }

    pub(super) fn awaiting_capture(&self) -> bool {
        self.native_frame
            .as_ref()
            .is_some_and(|capture| capture.state.get() != CaptureState::Finished)
    }

    fn release_native_frame(&mut self) -> Result<()> {
        if let Some(capture) = &self.native_frame
            && let Some(frame) = capture.frame.borrow().as_ref()
        {
            let index = self
                .parent
                .borrow()
                .children()
                .iter()
                .position(|node| std::rc::Rc::ptr_eq(node, &frame.node));
            if let Some(index) = index {
                self.parent.borrow_mut().remove_child(index)?;
            }
        }
        self.native_frame.take();
        Ok(())
    }

    pub(super) fn reveal(&mut self) -> Result<bool> {
        if !self.revealed {
            let node = self.surface.native.borrow();
            node.opacity(1.0)?;
            node.mark_dirty(NodeDirtyFlag::NeedRender)?;
            drop(node);
            self.release_native_frame()?;
            self.revealed = true;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn hide(&mut self) {
        if let Some(capture) = &self.native_frame {
            capture.state.set(CaptureState::Finished);
            capture.animation.borrow_mut().take();
        }
        let node = self.surface.native.borrow();
        let _ = node.opacity(0.0);
        let _ = node.mark_dirty(NodeDirtyFlag::NeedRender);
        self.revealed = false;
    }

    pub(super) fn retire_gone(&mut self) {
        self.hide();
        let node = self.surface.native.borrow();
        if node.is_owned() {
            let _ = node.set_i32_attribute(Attribute::Visibility, 2);
        }
    }

    pub(super) fn update(
        &mut self,
        width: u32,
        height: u32,
        x: f32,
        y: f32,
        scale: f32,
    ) -> Result<()> {
        let scale = scale.max(0.01);
        let scale_changed = (self.scale - scale).abs() > f32::EPSILON;
        if scale_changed {
            self.scale = scale;
        }
        let node = self.surface.native.borrow();
        let origin = (x.round() as i32, y.round() as i32);
        let origin_changed = self.origin != origin;
        if origin_changed || scale_changed {
            node.set_position([x / self.scale, y / self.scale])?;
            self.origin = origin;
        }
        if self.size != (width, height) || origin_changed || scale_changed {
            if self.size != (width, height) || scale_changed {
                node.width(width as f32 / self.scale)?;
                node.height(height as f32 / self.scale)?;
                self.size = (width, height);
            }
            node.set_layout_rect([self.origin.0, self.origin.1, width as i32, height as i32])?;
        }
        Ok(())
    }

    pub(super) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        if let Some(capture) = &self.native_frame
            && let Some(frame) = capture.frame.borrow().as_ref()
        {
            crate::render::layout::ChildMeasure::new(event).child(&frame.node.borrow())?;
        }
        crate::render::layout::ChildMeasure::new(event).child(&self.surface.native.borrow())
    }

    pub(super) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        if let Some(capture) = &self.native_frame
            && let Some(frame) = capture.frame.borrow().as_ref()
        {
            event.layout_child(&frame.node.borrow(), 0, 0)?;
        }
        event.layout_child(&self.surface.native.borrow(), self.origin.0, self.origin.1)?;
        Ok(())
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        callback(|| self.release_native_frame());
        callback(|| self.surface.dispose(Some(&self.parent)));
    }
}
