//! Retained custom ink. Unlike C node draw events, a ContentModifier keeps its
//! draw callback when Rosen moves the node between the window and texture trees.
//! Leaf hosts attach the RenderNode directly; container hosts use a lightweight
//! child Custom carrier so FrameNode and RenderNode siblings never mix.
use ohos_arkui_binding::{
    common::node::ArkUINode,
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    render_node::{FloatProperty, RenderContentModifier, RenderNode},
    types::{
        advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A, view::HitTestMode,
    },
};
pub(crate) use ohos_drawing_binding::Canvas as PaintCanvas;
use ohos_drawing_binding::{CornerPosition, Rect, RoundRect};
use std::{
    cell::{Cell, RefCell},
    ops::Deref,
    rc::{Rc, Weak},
};

use super::node::{NativeNode, Result, callback};

pub(crate) struct NativeRoundRect {
    rect: RoundRect,
    bounds: [f32; 4],
}
impl Deref for NativeRoundRect {
    type Target = RoundRect;
    fn deref(&self) -> &RoundRect {
        &self.rect
    }
}
impl NativeRoundRect {
    pub(crate) fn new(
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        corners: [(f32, f32); 4],
    ) -> Self {
        use CornerPosition::{BottomLeft, BottomRight, TopLeft, TopRight};
        let mut rect = RoundRect::new(&Rect::new(left, top, right, bottom), 0., 0.);
        for (corner, (x, y)) in [TopLeft, TopRight, BottomRight, BottomLeft]
            .into_iter()
            .zip(corners)
        {
            rect.set_corner(corner, x, y);
        }
        Self {
            rect,
            bounds: [left, top, right, bottom],
        }
    }
    /// Zero-radius enclosing box guaranteed to satisfy `SkRRect::contains(inner)`
    /// across `self` and `other`, allowing `(enclosing \ inner) ∩ clip` to draw
    /// shifted shadow crescents via `drawDRRect` + `Intersect` without `Difference`.
    pub(crate) fn enclosing(&self, other: &Self, margin: f32) -> Self {
        let m = margin.max(1.0);
        Self::new(
            self.bounds[0].min(other.bounds[0]) - m,
            self.bounds[1].min(other.bounds[1]) - m,
            self.bounds[2].max(other.bounds[2]) + m,
            self.bounds[3].max(other.bounds[3]) + m,
            [(0.0, 0.0); 4],
        )
    }
}

enum Attachment {
    Direct(Weak<RefCell<ArkUINode>>),
    Carrier(NativeNode),
}

fn make_carrier(z_index: i32) -> Result<NativeNode> {
    let carrier = Custom::new()?.into_node();
    carrier
        .borrow()
        .set_length_metric_unit(LengthMetricUnit::Px)?;
    carrier
        .borrow()
        .set_attribute(A::HitTestBehavior, i32::from(HitTestMode::None).into())?;
    carrier.borrow().set_bool_attribute(A::Clip, false)?;
    carrier.borrow().set_i32_attribute(A::ZIndex, z_index)?;
    Ok(carrier)
}

pub(crate) struct PaintNode {
    attachment: RefCell<Attachment>,
    // Drop order matters: dispose node before modifier, and modifier before property.
    node: RenderNode,
    modifier: RenderContentModifier,
    property: FloatProperty,
    bounds: Cell<[i32; 4]>,
    revision: Cell<bool>,
}
impl PaintNode {
    pub(crate) fn new_attached(
        host: &NativeNode,
        z_index: i32,
        allow_direct: bool,
        paint: impl Fn(&PaintCanvas) + 'static,
    ) -> Result<Self> {
        // Declare in reverse drop order so early errors dispose node -> modifier -> property.
        let property = FloatProperty::new(0.0)?;
        let mut modifier = RenderContentModifier::new()?;
        let node = RenderNode::new()?;
        node.set_clip_to_frame(false)?;
        node.set_clip_to_bounds(false)?;
        modifier.set_on_draw_canvas(paint)?;
        node.attach_content_modifier(&modifier)?;
        // Properties acquire their target from the modifier's attached node.
        modifier.attach_float_property(&property)?;
        let attachment = if allow_direct
            && host.borrow().children().is_empty()
            && node.attach_to(&host.borrow()).is_ok()
        {
            Attachment::Direct(Rc::downgrade(host))
        } else {
            let carrier = make_carrier(z_index)?;
            let index = host.borrow().children().len();
            let attached = node.attach_to(&carrier.borrow());
            if let Err(error) = attached {
                let _ = carrier.borrow_mut().dispose();
                return Err(error.into());
            }
            if let Err(error) = host.borrow_mut().insert_child(carrier.clone(), index) {
                let _ = node.detach_from(&carrier.borrow());
                let _ = carrier.borrow_mut().dispose();
                return Err(error.into());
            }
            Attachment::Carrier(carrier)
        };
        Ok(Self {
            attachment: RefCell::new(attachment),
            node,
            modifier,
            property,
            bounds: Cell::new([i32::MIN; 4]),
            revision: Cell::new(false),
        })
    }
    pub(crate) fn promote_to_carrier(&self, z_index: i32) -> Result<()> {
        let host = match &*self.attachment.borrow() {
            Attachment::Direct(host) => host.upgrade(),
            Attachment::Carrier(_) => return Ok(()),
        };
        let Some(host) = host else {
            return Ok(());
        };
        let carrier = make_carrier(z_index)?;
        let bounds = self.bounds.get();
        if bounds != [i32::MIN; 4] {
            let [x, y, w, h] = bounds;
            carrier
                .borrow()
                .set_layout_rect([0, 0, w.max(0), h.max(0)])?;
            carrier.borrow().set_position([x as f32, y as f32])?;
        }
        self.node.detach_from(&host.borrow())?;
        let attach = self
            .node
            .set_position(0, 0)
            .and_then(|()| self.node.attach_to(&carrier.borrow()));
        if let Err(error) = attach {
            let _ = self.node.attach_to(&host.borrow());
            if bounds != [i32::MIN; 4] {
                let _ = self.node.set_position(bounds[0], bounds[1]);
            }
            let _ = carrier.borrow_mut().dispose();
            return Err(error.into());
        }
        let index = host.borrow().children().len();
        if let Err(error) = host.borrow_mut().insert_child(carrier.clone(), index) {
            let _ = self.node.detach_from(&carrier.borrow());
            let _ = self.node.attach_to(&host.borrow());
            if bounds != [i32::MIN; 4] {
                let _ = self.node.set_position(bounds[0], bounds[1]);
            }
            let _ = carrier.borrow_mut().dispose();
            return Err(error.into());
        }
        *self.attachment.borrow_mut() = Attachment::Carrier(carrier);
        Ok(())
    }

    pub(crate) fn bounds(&self, bounds: [i32; 4]) -> Result<()> {
        if self.bounds.get() == bounds {
            return Ok(());
        }
        let [x, y, w, h] = bounds;
        let (w, h) = (w.max(0), h.max(0));
        let pos = match &*self.attachment.borrow() {
            Attachment::Direct(_) => (x, y),
            Attachment::Carrier(carrier) => {
                carrier.borrow().set_layout_rect([0, 0, w, h])?;
                carrier.borrow().set_position([x as f32, y as f32])?;
                (0, 0)
            }
        };
        self.node.set_size(w, h)?;
        self.node.set_position(pos.0, pos.1)?;
        self.invalidate()?;
        self.bounds.set(bounds);
        Ok(())
    }
    pub(crate) fn invalidate(&self) -> Result<()> {
        let next = !self.revision.get();
        self.property.set(if next { 1.0 } else { 0.0 })?;
        self.revision.set(next);
        Ok(())
    }
}

impl Drop for PaintNode {
    fn drop(&mut self) {
        match self.attachment.get_mut() {
            Attachment::Direct(host) => {
                if let Some(host) = host.upgrade()
                    && host.borrow().is_owned()
                {
                    let _ = self.node.detach_from(&host.borrow());
                }
            }
            Attachment::Carrier(carrier) => {
                if carrier.borrow().is_owned() {
                    let _ = self.node.detach_from(&carrier.borrow());
                }
                if Rc::strong_count(carrier) == 1 {
                    callback(|| {
                        carrier.borrow_mut().dispose()?;
                        Ok(())
                    });
                }
            }
        }
        let _ = self.modifier.clear_on_draw();
    }
}
