//! Shader frames presented by an XComponent Surface, outside the exported subtree.

use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent, component::attribute::ArkUICommonAttribute,
};

use crate::render::node::{NativeNode, Node, Result, callback};

pub(super) struct Presentation {
    parent: NativeNode,
    surface: super::surface::Surface,
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
        parent.borrow_mut().add_child(native.clone())?;
        let mut layer = Self {
            parent,
            surface,
            scale: owner.scale.get(),
            origin: (0, 0),
            size: (0, 0),
            revealed: false,
        };
        let frame = owner.frame.get();
        layer.update(
            (frame.width * owner.scale.get()).ceil().max(1.0) as u32,
            (frame.height * owner.scale.get()).ceil().max(1.0) as u32,
            0.0,
            0.0,
        )?;
        Ok(layer)
    }

    pub(super) fn reveal(&mut self) -> Result<()> {
        if !self.revealed {
            self.surface.native.borrow().opacity(1.0)?;
            self.revealed = true;
        }
        Ok(())
    }

    pub(super) fn hide(&self) {
        let _ = self.surface.native.borrow().opacity(0.0);
    }

    pub(super) fn update(&mut self, width: u32, height: u32, x: f32, y: f32) -> Result<()> {
        let node = self.surface.native.borrow();
        let origin = (x.round() as i32, y.round() as i32);
        let origin_changed = self.origin != origin;
        if origin_changed {
            node.set_position([x / self.scale, y / self.scale])?;
            self.origin = origin;
        }
        if self.size != (width, height) || origin_changed {
            if self.size != (width, height) {
                node.width(width as f32 / self.scale)?;
                node.height(height as f32 / self.scale)?;
                self.size = (width, height);
            }
            node.set_layout_rect([self.origin.0, self.origin.1, width as i32, height as i32])?;
        }
        Ok(())
    }

    pub(super) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        crate::render::layout::ChildMeasure::new(event).child(&self.surface.native.borrow())
    }

    pub(super) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        event.layout_child(&self.surface.native.borrow(), self.origin.0, self.origin.1)?;
        Ok(())
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        callback(|| self.surface.dispose(Some(&self.parent)));
    }
}
