//! Host-owned native widgets. Factories run on the ArkTS thread before their
//! first property batch; their FrameNode owner stays retained until detachment.
//! Instance handles survive source replacement without reusing node identifiers.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use napi_derive_ohos::napi;
use napi_ohos::{
    Env,
    bindgen_prelude::{FnArgs, Function, FunctionRef, Object, ObjectRef},
};
use nsh_core::{JsonValue, NodeId};
use nsh_engine::EngineControl;
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::{Stack, Toggle},
    },
    types::{
        advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A,
        event::NodeEventType as E,
    },
};
use rustc_hash::FxHashMap;

use crate::{
    interop::attachment::NativeAttachment,
    render::node::{NativeNode, Result},
};

/// One changed property of a host native component. A component receives
/// one array per batch; values cross Node-API directly without JSON strings.
#[napi(object)]
pub struct NshNativeProperty {
    /// Property name.
    pub name: String,
    /// Property value; `removed` distinguishes a removed property from null.
    #[napi(ts_type = "string | number | boolean | object | null")]
    pub value: JsonValue,
    /// The property was removed.
    pub removed: bool,
}
type Factory = FunctionRef<u32, ObjectRef>;
pub(crate) type TabFactory = FunctionRef<TabBarContext, ObjectRef>;

/// Native callback ownership for the system tab bar, scoped to its surface load.
#[napi]
pub(crate) struct TabBarContext {
    context: Context,
    name: String,
}
#[napi]
#[allow(unreachable_pub)] // Node-API requires public methods on its internal callback object.
impl TabBarContext {
    /// The built-in NSH component requested by the engine.
    #[napi(getter)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reports the controlled value selected in a built-in mobile control.
    #[napi]
    pub fn value_changed(&self, value: serde_json::Value) {
        if self.context.active.get() {
            self.context.control.dispatch_event(
                self.context.node,
                nsh_core::EventKind::NativeEvent,
                nsh_core::EventPayload::Native {
                    name: "change".into(),
                    data: value,
                },
            );
        }
    }

    /// Supplies the intrinsic size in logical points.
    #[napi]
    pub fn size(&self, width: f64, height: f64) {
        if self.context.active.get() && [width, height].iter().all(|v| (0.0..=16384.0).contains(v))
        {
            self.context.control.set_component_size(
                self.context.node,
                nsh_core::Size::new(
                    nsh_core::Points(width as f32),
                    nsh_core::Points(height as f32),
                ),
            );
        }
    }

    /// Reports the native selection while the component remains mounted.
    #[napi]
    pub fn change(&self, index: u32) {
        if self.context.active.get() {
            self.context.control.dispatch_event(
                self.context.node,
                nsh_core::EventKind::NativeEvent,
                nsh_core::EventPayload::Native {
                    name: "change".into(),
                    data: index.into(),
                },
            );
        }
    }
    /// Supplies the tab bar's intrinsic height in vp.
    #[napi]
    pub fn height(&self, height: f64) {
        if self.context.active.get() && height.is_finite() && (0.0..=16384.0).contains(&height) {
            self.context.control.set_component_size(
                self.context.node,
                nsh_core::Size::new(nsh_core::Points::ZERO, nsh_core::Points(height as f32)),
            );
        }
    }
}

#[derive(Clone)]
pub(crate) struct Context {
    pub(crate) node: NodeId,
    pub(crate) control: EngineControl,
    active: Rc<Cell<bool>>,
}
#[derive(Default)]
pub(crate) struct Registry {
    factories: FxHashMap<Arc<str>, Rc<Factory>>,
    contexts: FxHashMap<u32, Context>,
    gpu_surfaces: Vec<std::rc::Weak<crate::interop::gpu_surface::GpuSurfaceInstance>>,
    next: u32,
}
#[derive(Clone, Default)]
pub(crate) struct Components(Rc<RefCell<Registry>>);
impl Components {
    pub(crate) fn register(&self, name: String, factory: Factory) -> napi_ohos::Result<()> {
        check_name(&name)?;
        self.0
            .borrow_mut()
            .factories
            .insert(name.into(), Rc::new(factory));
        Ok(())
    }
    pub(crate) fn unregister(&self, name: &str) -> napi_ohos::Result<()> {
        check_name(name)?;
        self.0.borrow_mut().factories.remove(name);
        Ok(())
    }
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().factories.clear();
    }
    fn live_gpu_surfaces(&self) -> Vec<Rc<crate::interop::gpu_surface::GpuSurfaceInstance>> {
        let mut registry = self.0.borrow_mut();
        let mut live = Vec::new();
        registry.gpu_surfaces.retain(|weak| {
            if let Some(surface) = weak.upgrade() {
                live.push(surface);
                true
            } else {
                false
            }
        });
        live
    }
    pub(crate) fn tick_gpu_surfaces(&self, timestamp_ns: u64) {
        for surface in self.live_gpu_surfaces() {
            surface.on_vsync(timestamp_ns);
        }
    }
    pub(crate) fn set_gpu_surfaces_active(&self, active: bool) {
        for surface in self.live_gpu_surfaces() {
            surface.set_host_active(active);
            surface.sync_xcomponent_rate();
        }
    }
    pub(crate) fn sync_gpu_surface_rates(&self) {
        for surface in self.live_gpu_surfaces() {
            surface.sync_xcomponent_rate();
        }
    }
    pub(crate) fn context(&self, handle: u32) -> napi_ohos::Result<Context> {
        self.0
            .borrow()
            .contexts
            .get(&handle)
            .filter(|context| context.active.get())
            .cloned()
            .ok_or_else(|| napi_ohos::Error::from_reason("native component is disposed"))
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn make(
        &self,
        env: Env,
        name: &str,
        node: NodeId,
        control: EngineControl,
        host: &crate::app::session::Services,
        scale: f32,
        frame_rate: Arc<crate::app::frame_rate::FrameRate>,
        dispatcher: std::sync::Weak<crate::app::runtime::Dispatcher>,
    ) -> Result<Option<Instance>> {
        let (factory, handle, context) = {
            let mut registry = self.0.borrow_mut();
            let factory = registry.factories.get(name).cloned();
            if factory.is_none() && name == nsh_core::platform::SWITCH_COMPONENT {
                drop(registry);
                return Ok(Some(Instance::native_switch(node, control)?));
            }
            if factory.is_none()
                && let Some(registered) = crate::interop::gpu_surface::lookup_gpu_surface(name)
            {
                drop(registry);
                let surface = crate::interop::gpu_surface::GpuSurfaceInstance::new(
                    registered,
                    node,
                    control,
                    scale,
                    frame_rate,
                    dispatcher,
                    host.max_refresh_rate(),
                )?;
                let native = surface.native.clone();
                self.0
                    .borrow_mut()
                    .gpu_surfaces
                    .push(Rc::downgrade(&surface));
                return Ok(Some(Instance {
                    native,
                    kind: InstanceKind::GpuSurface(surface),
                    disposed: Cell::new(false),
                }));
            }
            if factory.is_none()
                && !matches!(
                    name,
                    nsh_core::platform::TAB_BAR_COMPONENT
                        | nsh_core::platform::DATE_TIME_PICKER_COMPONENT
                )
            {
                log::warn!("OHOS native component is not registered: {name}");
                return Ok(None);
            }
            registry.next = registry.next.checked_add(1).ok_or_else(|| {
                napi_ohos::Error::from_reason("native component handles exhausted")
            })?;
            let handle = registry.next;
            let context = Context {
                node,
                control,
                active: Rc::new(Cell::new(true)),
            };
            registry.contexts.insert(handle, context.clone());
            (factory, handle, context)
        };
        // Release the registry borrow before calling the host: it can register
        // another factory, emit events or report intrinsic size synchronously.
        let registration = Registration {
            components: self.clone(),
            handle,
            context: context.clone(),
        };
        let value = if let Some(factory) = factory {
            factory.borrow_back(&env)?.call(handle)?
        } else {
            host.tab_bar_factory()?
                .borrow_back(&env)?
                .call(TabBarContext {
                    context,
                    name: name.to_owned(),
                })?
        };
        let retained = RetainedObject {
            value: Some(value),
            env,
            disposed: Cell::new(false),
        };
        let object = retained.object()?;
        let node: Object<'_> = object.get("node")?.ok_or_else(|| {
            napi_ohos::Error::from_reason(
                "native component factory must return an object with node",
            )
        })?;
        // Ability retains the callback-scoped FrameNode until its native
        // borrowing edge is retired. The business object remains independently
        // retained for props, commands and the host's disposal callback.
        let native = Stack::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        let target = NativeAttachment::new(&native)?;
        NativeAttachment::attach(&env, target.id(), &node)?;
        Ok(Some(Instance {
            native,
            kind: InstanceKind::Host {
                target,
                retained: Some(retained),
                registration,
            },
            disposed: Cell::new(false),
        }))
    }
}
struct Registration {
    components: Components,
    handle: u32,
    context: Context,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.context.active.set(false);
        self.components.0.borrow_mut().contexts.remove(&self.handle);
    }
}
struct RetainedObject {
    value: Option<ObjectRef>,
    env: Env,
    disposed: Cell<bool>,
}
impl RetainedObject {
    fn object(&self) -> napi_ohos::Result<Object<'_>> {
        self.value
            .as_ref()
            .ok_or_else(|| napi_ohos::Error::from_reason("native component is disposed"))?
            .get_value(&self.env)
    }
    fn dispose(&self) -> napi_ohos::Result<()> {
        if self.disposed.replace(true) {
            return Ok(());
        }
        self.env.run_in_scope(|| {
            let object = self.object()?;
            let dispose: Option<Function<'_, (), ()>> = object.get("dispose")?;
            if let Some(dispose) = dispose {
                dispose.apply(object, ())?;
            }
            Ok(())
        })
    }
}
impl Drop for RetainedObject {
    fn drop(&mut self) {
        if let Err(error) = self.dispose() {
            log::error!("OHOS component disposal: {error}");
        }
        if let Some(value) = self.value.take()
            && let Err(error) = value.unref(&self.env)
        {
            log::error!("OHOS component release: {error}");
        }
    }
}
struct NativeSwitchState {
    active: Rc<Cell<bool>>,
    checked: Rc<Cell<bool>>,
    disabled: Rc<Cell<bool>>,
    updating: Rc<Cell<bool>>,
}

enum InstanceKind {
    Host {
        target: NativeAttachment,
        retained: Option<RetainedObject>,
        registration: Registration,
    },
    NativeSwitch(NativeSwitchState),
    GpuSurface(Rc<crate::interop::gpu_surface::GpuSurfaceInstance>),
}

pub(crate) struct Instance {
    pub(crate) native: NativeNode,
    kind: InstanceKind,
    disposed: Cell<bool>,
}
impl Instance {
    fn native_switch(node_id: NodeId, control: EngineControl) -> Result<Self> {
        let native = Toggle::new()?.into_node();
        {
            let borrowed = native.borrow();
            borrowed.set_length_metric_unit(LengthMetricUnit::Vp)?;
            borrowed.set_i32_attribute(A::ToggleValue, 0)?;
            borrowed.set_bool_attribute(A::Enabled, true)?;
        }
        // Standard HarmonyOS ArkUI Switch intrinsic size (36 x 20 vp).
        control.set_component_size(
            node_id,
            nsh_core::Size::new(nsh_core::Points(36.0), nsh_core::Points(20.0)),
        );
        let active = Rc::new(Cell::new(true));
        let checked = Rc::new(Cell::new(false));
        let disabled = Rc::new(Cell::new(false));
        let updating = Rc::new(Cell::new(false));
        let cb_active = active.clone();
        let cb_checked = checked.clone();
        let cb_disabled = disabled.clone();
        let cb_updating = updating.clone();
        let cb_node = Rc::downgrade(&native);
        native
            .borrow_mut()
            .on_event(E::ToggleOnChange, move |event| {
                if cb_updating.get() || !cb_active.get() || cb_disabled.get() {
                    return;
                }
                let next = event
                    .component_i32(0)
                    .or_else(|| event.i32_value(0))
                    .unwrap_or_default()
                    != 0;
                let controlled = cb_checked.get();
                if next != controlled {
                    // Revert the visual state to the controlled value so rejected
                    // changes stay synchronized; an accepted change updates it in props().
                    if let Some(toggle) = cb_node.upgrade() {
                        cb_updating.set(true);
                        let _ = toggle
                            .borrow()
                            .set_i32_attribute(A::ToggleValue, i32::from(controlled));
                        cb_updating.set(false);
                    }
                    control.dispatch_event(
                        node_id,
                        nsh_core::EventKind::NativeEvent,
                        nsh_core::EventPayload::Native {
                            name: "change".into(),
                            data: next.into(),
                        },
                    );
                }
            });
        Ok(Self {
            native,
            kind: InstanceKind::NativeSwitch(NativeSwitchState {
                active,
                checked,
                disabled,
                updating,
            }),
            disposed: Cell::new(false),
        })
    }
    fn object(&self) -> napi_ohos::Result<Object<'_>> {
        match &self.kind {
            InstanceKind::Host { retained, .. } => retained
                .as_ref()
                .filter(|_| !self.disposed.get())
                .ok_or_else(|| napi_ohos::Error::from_reason("native component is disposed"))?
                .object(),
            InstanceKind::NativeSwitch(_) => Err(napi_ohos::Error::from_reason(
                "native switch has no JS object",
            )),
            InstanceKind::GpuSurface(_) => Err(napi_ohos::Error::from_reason(
                "native GPU surface has no JS object",
            )),
        }
    }
    pub(crate) fn set_frame(&self, width: f32, height: f32, scale: f32) -> Result<()> {
        match &self.kind {
            InstanceKind::GpuSurface(surface) => surface.set_frame(width, height, scale),
            InstanceKind::Host { .. } | InstanceKind::NativeSwitch(_) => {
                let node = self.native.borrow();
                node.width(width)?;
                node.height(height)?;
                Ok(())
            }
        }
    }
    pub(crate) fn density_changed(&self, scale: f32) {
        if let InstanceKind::GpuSurface(surface) = &self.kind {
            surface.density_changed(scale);
        }
    }
    pub(crate) fn deactivate(&self) {
        match &self.kind {
            InstanceKind::Host { registration, .. } => registration.context.active.set(false),
            InstanceKind::NativeSwitch(state) => state.active.set(false),
            InstanceKind::GpuSurface(surface) => surface.deactivate(),
        }
    }
    pub(crate) fn props(&self, props: Vec<(Arc<str>, Option<JsonValue>)>) -> Result<()> {
        if let InstanceKind::GpuSurface(surface) = &self.kind {
            surface.props(props);
            return Ok(());
        }
        if let InstanceKind::NativeSwitch(state) = &self.kind {
            let borrowed = self.native.borrow();
            for (name, value) in props {
                match name.as_ref() {
                    "checked" => {
                        let next = value.as_ref().and_then(JsonValue::as_bool).unwrap_or(false);
                        state.checked.set(next);
                        state.updating.set(true);
                        let res = borrowed.set_i32_attribute(A::ToggleValue, i32::from(next));
                        state.updating.set(false);
                        res?;
                    }
                    "disabled" => {
                        let next = value.as_ref().and_then(JsonValue::as_bool).unwrap_or(false);
                        state.disabled.set(next);
                        borrowed.set_bool_attribute(A::Enabled, !next)?;
                        borrowed.opacity(if next { 0.4 } else { 1.0 })?;
                    }
                    "accessibilityLabel" => {
                        let label = value
                            .as_ref()
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default();
                        borrowed.set_string_attribute(A::AccessibilityText, label)?;
                    }
                    "tint" => {
                        if let Some(argb) = value
                            .as_ref()
                            .and_then(JsonValue::as_str)
                            .and_then(parse_css_argb)
                        {
                            borrowed.set_u32_attribute(A::ToggleSelectedColor, argb)?;
                        } else {
                            borrowed.reset_attribute(A::ToggleSelectedColor)?;
                        }
                    }
                    _ => {}
                }
            }
            return Ok(());
        }
        let object = self.object()?;
        let update: Option<Function<'_, Vec<NshNativeProperty>, ()>> = object.get("updateProps")?;
        if let Some(update) = update {
            update.apply(
                object,
                props
                    .into_iter()
                    .map(|(name, value)| NshNativeProperty {
                        name: name.to_string(),
                        removed: value.is_none(),
                        value: value.unwrap_or(JsonValue::Null),
                    })
                    .collect(),
            )?;
        }
        Ok(())
    }
    pub(crate) fn command(&self, name: &str, args: &JsonValue) -> Result<()> {
        if let InstanceKind::GpuSurface(surface) = &self.kind {
            surface.command(name, args);
            return Ok(());
        }
        if matches!(self.kind, InstanceKind::NativeSwitch(_)) {
            return Ok(());
        }
        let object = self.object()?;
        let command: Option<Function<'_, FnArgs<(String, JsonValue)>, ()>> =
            object.get("command")?;
        if let Some(command) = command {
            command.apply(object, (name.to_owned(), args.clone()).into())?;
        }
        Ok(())
    }
    pub(crate) fn dispose(&self) -> Result<()> {
        self.deactivate();
        if self.disposed.get() {
            return Ok(());
        }
        match &self.kind {
            InstanceKind::Host {
                target, retained, ..
            } => {
                target.invalidate()?;
                if self.native.borrow().is_owned() {
                    self.native.borrow_mut().dispose()?;
                }
                // Detachment is committed before calling the host, which may throw.
                self.disposed.set(true);
                if let Some(retained) = retained {
                    retained.dispose()?;
                }
            }
            InstanceKind::NativeSwitch(_) => {
                if self.native.borrow().is_owned() {
                    self.native.borrow_mut().dispose()?;
                }
                self.disposed.set(true);
            }
            InstanceKind::GpuSurface(surface) => {
                self.disposed.set(true);
                surface.dispose()?;
            }
        }
        Ok(())
    }
}

fn parse_css_argb(raw: &str) -> Option<u32> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                Some(0xFF00_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
            }
            4 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                let a = u8::from_str_radix(&hex[3..4], 16).ok()? * 17;
                Some(
                    (u32::from(a) << 24)
                        | (u32::from(r) << 16)
                        | (u32::from(g) << 8)
                        | u32::from(b),
                )
            }
            6 => {
                let rgb = u32::from_str_radix(hex, 16).ok()?;
                Some(0xFF00_0000 | rgb)
            }
            8 => {
                // CSS hex is #RRGGBBAA; ArkUI u32 colors are 0xAARRGGBB.
                let rgba = u32::from_str_radix(hex, 16).ok()?;
                Some(rgba.rotate_right(8))
            }
            _ => None,
        };
    }
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 3 && parts.len() != 4 {
        return None;
    }
    let r = parts[0].parse::<f32>().ok()?.round().clamp(0.0, 255.0) as u8;
    let g = parts[1].parse::<f32>().ok()?.round().clamp(0.0, 255.0) as u8;
    let b = parts[2].parse::<f32>().ok()?.round().clamp(0.0, 255.0) as u8;
    let a = if parts.len() == 4 {
        (parts[3].parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        255
    };
    Some((u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
}

impl Drop for Instance {
    fn drop(&mut self) {
        if let Err(error) = self.dispose() {
            if !self.disposed.get() {
                if let InstanceKind::Host { retained, .. } = &mut self.kind {
                    // The C container still borrows the ArkTS node. Keep both owners
                    // alive permanently when the final native detach retry fails.
                    std::mem::forget((retained.take(), self.native.clone()));
                }
                log::error!(
                    "OHOS native component detachment failed; retaining the native container \
                     and ArkTS owner until process exit: {error}"
                );
            } else {
                log::error!("OHOS native component disposal: {error}");
            }
        }
    }
}

pub(crate) fn check_name(name: &str) -> napi_ohos::Result<()> {
    if name.starts_with("Nsh") {
        return Err(napi_ohos::Error::from_reason(
            "Component names starting with 'Nsh' are reserved",
        ));
    }
    Ok(())
}
