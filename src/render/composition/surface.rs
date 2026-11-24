//! XComponent ownership shared by shader fills and exported composites.
//! The UI retains a node until the compositor has released its EGL window.

use std::{cell::Cell, ptr::NonNull};

use ohos_arkui_binding::{
    common::{attribute::ArkUINodeAttributeNumber as Number, node_registry},
    component::{attribute::ArkUICommonAttribute, built_in_component::XComponent},
    types::{
        advanced::LengthMetricUnit,
        attribute::ArkUINodeAttributeType as A,
        view::{AccessibilityMode, HitTestMode},
    },
};
use ohos_native_window_binding::NativeWindow;
use ohos_xcomponent_binding::NativeXComponent;

use super::worker::WindowEvents;
use crate::render::node::{NativeNode, Result};

/// A box: its size in points and its corner radii (top-left first).
type Placement = ((f32, f32), [f32; 4]);

pub(crate) struct Surface {
    pub(crate) native: NativeNode,
    xcomponent: NativeXComponent,
    /// The box last given it.
    applied: Cell<Option<Placement>>,
}

impl Surface {
    pub(crate) fn new(name: String, events: WindowEvents, clip: bool) -> Result<Surface> {
        let native = XComponent::new()?.into_node();
        {
            let node = native.borrow();
            node.set_length_metric_unit(LengthMetricUnit::Vp)?;
            node.set_string_attribute(A::XComponentId, name)?;
            // A picture: presses and screen readers go to the view it is in.
            node.set_i32_attribute(A::HitTestBehavior, HitTestMode::None.into())?;
            node.set_i32_attribute(A::AccessibilityMode, AccessibilityMode::Disabled.into())?;
            node.set_bool_attribute(A::Clip, clip)?;
            node.background_color(0)?;
        }
        let xcomponent = native
            .borrow()
            .native_xcomponent()
            .ok_or_else(|| napi_ohos::Error::from_reason("shader XComponent is unavailable"))?;
        let created = events.clone();
        xcomponent.on_surface_created(move |_, window| {
            let raw = NonNull::new(window.0)
                .ok_or_else(|| napi_ohos::Error::from_reason("shader window is null"))?;
            // SAFETY: the SDK lends a live window for this callback; the
            // XComponent that produces it is kept until the compositor worker has
            // dropped its surface of it (`Shaders::collect`).
            let window = unsafe { NativeWindow::retain_from_ptr(raw) }
                .map_err(|error| napi_ohos::Error::from_reason(error.to_string()))?;
            created.created(window);
            Ok(())
        });
        xcomponent.on_surface_destroyed(move |_, window| {
            events.destroyed(window.0 as usize);
            Ok(())
        });
        xcomponent.register_callback()?;
        Ok(Surface {
            native,
            xcomponent,
            applied: Cell::new(None),
        })
    }

    /// Gives it the view's size in points and its corner radii.
    pub(crate) fn place(&self, size: (f32, f32), radii: [f32; 4]) -> Result<bool> {
        if self.applied.get() == Some((size, radii)) {
            return Ok(false);
        }
        self.applied.set(Some((size, radii)));
        let node = self.native.borrow();
        node.width(size.0)?;
        node.height(size.1)?;
        node.set_number_slice_attribute(A::BorderRadius, &radii.map(Number::Float))?;
        Ok(true)
    }

    /// Takes it out of `parent` and releases it: once the compositor worker no
    /// longer draws into its window.
    pub(crate) fn dispose(&self, parent: Option<&NativeNode>) -> Result<()> {
        self.xcomponent.unregister_callbacks();
        if !self.native.borrow().is_owned() {
            return Ok(());
        }
        node_registry::unregister(&self.native.borrow());
        let index = parent.and_then(|parent| {
            parent
                .borrow()
                .children()
                .iter()
                .position(|child| std::rc::Rc::ptr_eq(child, &self.native))
        });
        match (parent, index) {
            (Some(parent), Some(index)) => {
                if self.native.borrow().parent()?.is_some() {
                    parent.borrow_mut().remove_child(index)?;
                } else {
                    parent.borrow_mut().children_mut().remove(index);
                }
            }
            _ => self.native.borrow().remove_from_parent()?,
        }
        self.native.borrow_mut().dispose()?;
        Ok(())
    }
}
