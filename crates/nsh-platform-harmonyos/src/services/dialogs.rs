//! System AlertDialog/ActionSheet, called from Rust on the owning UI thread.
//! ArkUI owns all content, styling, input and exit animations. The queue advances
//! only after onDidDisappear, never while a system button is on the native stack.
use std::{cell::Cell, collections::VecDeque, rc::Rc};

use napi_ohos::bindgen_prelude::{Function, JsObjectValue, Object};
use nsh_core::{DialogButtonStyle, EventKind, EventPayload, PayloadValue};

use crate::{
    app::runtime::Message,
    render::{mount::UiContext, node::Result},
};

struct Alert {
    request: nsh_core::Dialog,
    active: Rc<Cell<bool>>,
    #[cfg(feature = "dev")]
    appeared: Rc<Cell<bool>>,
}
impl Drop for Alert {
    fn drop(&mut self) {
        // System dialogs have window lifetime and no programmatic close API.
        // Retiring a surface must not let its later callbacks reach a new engine.
        self.active.set(false);
    }
}

pub(crate) struct Dialogs {
    context: Rc<UiContext>,
    alerts: VecDeque<Alert>,
    overlay: Option<crate::app::session::Overlay>,
}
impl Dialogs {
    #[cfg(feature = "dev")]
    pub(crate) fn animating(&self) -> bool {
        self.alerts
            .front()
            .is_some_and(|alert| !alert.appeared.get())
    }

    #[cfg(feature = "dev")]
    pub(crate) fn test_point(
        &self,
        button: Option<&str>,
    ) -> std::result::Result<Option<[f32; 2]>, String> {
        let alert = self.alerts.front().ok_or("no dialog is open")?;
        match button {
            None if !alert.request.cancelable => {
                Err("the dialog cannot be dismissed without choosing a button".into())
            }
            Some(label)
                if !alert
                    .request
                    .buttons
                    .iter()
                    .any(|button| button.text == label) =>
            {
                Err(format!("no dialog button {label:?}"))
            }
            // System content is outside the NSH surface. The device test runner
            // resolves its real button through the system accessibility tree.
            _ => Ok(None),
        }
    }

    pub(crate) fn new(context: Rc<UiContext>) -> Self {
        Self {
            context,
            alerts: VecDeque::new(),
            overlay: None,
        }
    }
    pub(crate) fn visible(&self) -> bool {
        !self.alerts.is_empty()
    }
    pub(crate) fn show(&mut self, request: nsh_core::Dialog) -> Result<()> {
        if request.buttons.is_empty() || self.alerts.len() >= 32 {
            self.reply(request.id, -1);
            return Ok(());
        }
        if self.alerts.is_empty() {
            self.overlay = Some(self.context.host.overlay(self.context.dispatcher.clone())?);
        }
        self.alerts.push_back(Alert {
            request,
            active: Rc::new(Cell::new(true)),
            #[cfg(feature = "dev")]
            appeared: Rc::new(Cell::new(false)),
        });
        if self.alerts.len() == 1 {
            self.show_next();
        }
        Ok(())
    }
    fn show_next(&mut self) {
        while let Some(alert) = self.alerts.front() {
            match self.present(alert) {
                Ok(()) => return,
                Err(error) => {
                    log::error!("OHOS system dialog: {error}");
                    let id = alert.request.id;
                    self.alerts.pop_front();
                    self.reply(id, -1);
                }
            }
        }
        self.overlay = None;
    }
    fn present(&self, alert: &Alert) -> Result<()> {
        let env = &self.context.env;
        env.run_in_scope(|| {
            let request = &alert.request;
            let ui = self.context.host.ui_context(env)?;
            let mut options = Object::new(env)?;
            if let Some(title) = &request.title {
                options.set("title", title.as_str())?;
            } else if request.sheet {
                options.set("title", "")?;
            }
            options.set("message", request.message.as_deref().unwrap_or(""))?;
            options.set("autoCancel", request.cancelable)?;
            #[cfg(feature = "dev")]
            for (event, visible) in [("onDidAppear", true), ("onWillDisappear", false)] {
                let appeared = alert.appeared.clone();
                options.set(
                    event,
                    env.create_function_from_closure::<(), (), _>(event, move |_| {
                        appeared.set(visible);
                        Ok(())
                    })?,
                )?;
            }
            let result = Rc::new(Cell::new(None));
            let mut buttons = Vec::new();
            let cancel = request
                .sheet
                .then(|| {
                    request
                        .buttons
                        .iter()
                        .position(|button| button.style == DialogButtonStyle::Cancel)
                })
                .flatten();
            for (index, button) in request.buttons.iter().enumerate() {
                let mut option = Object::new(env)?;
                option.set(
                    if request.sheet && cancel != Some(index) {
                        "title"
                    } else {
                        "value"
                    },
                    button.text.as_str(),
                )?;
                let selected = result.clone();
                let active = alert.active.clone();
                option.set(
                    "action",
                    env.create_function_from_closure::<(), (), _>("nshDialogAction", move |_| {
                        if active.get() && selected.get().is_none() {
                            selected.set(Some(index as i32));
                        }
                        Ok(())
                    })?,
                )?;
                if cancel == Some(index) {
                    options.set("confirm", option)?;
                } else {
                    buttons.push(option);
                }
            }
            options.set(if request.sheet { "sheets" } else { "buttons" }, buttons)?;
            let active = alert.active.clone();
            let cancelable = request.cancelable;
            options.set(
                "onWillDismiss",
                env.create_function_from_closure::<Object<'_>, (), _>(
                    "nshDialogDismiss",
                    move |ctx| {
                        // An orphaned window-owned dialog must always be dismissible.
                        if cancelable || !active.get() {
                            let action: Object<'_> = ctx.first_arg()?;
                            let dismiss: Function<'_, (), ()> =
                                action.get_named_property("dismiss")?;
                            dismiss.apply(action, ())?;
                        }
                        Ok(())
                    },
                )?,
            )?;
            let active = alert.active.clone();
            let route = self.context.dispatcher.clone();
            let id = request.id;
            options.set(
                "onDidDisappear",
                env.create_function_from_closure::<(), (), _>("nshDialogClosed", move |_| {
                    if active.replace(false)
                        && let Some(route) = route.upgrade()
                    {
                        route.send(Message::DialogResult {
                            id,
                            index: result.get().unwrap_or(-1),
                        });
                    }
                    Ok(())
                })?,
            )?;
            let show: Function<'_, Object<'_>, ()> = ui.get_named_property(if request.sheet {
                "showActionSheet"
            } else {
                "showAlertDialog"
            })?;
            show.apply(ui, options)
        })?;
        Ok(())
    }
    pub(crate) fn finish(&mut self, id: u32, index: i32) -> Result<()> {
        if self
            .alerts
            .front()
            .is_none_or(|alert| alert.request.id != id)
        {
            return Ok(());
        }
        self.alerts.pop_front();
        self.reply(id, index);
        self.show_next();
        Ok(())
    }
    fn reply(&self, id: u32, index: i32) {
        self.context.control.dispatch_event(
            nsh_core::ROOT_ID,
            EventKind::Dialog,
            EventPayload::Map(vec![
                ("id", PayloadValue::Number(id as f64)),
                ("index", PayloadValue::Number(index as f64)),
            ]),
        );
    }
    pub(crate) fn clear(&mut self) {
        self.alerts.clear();
        self.overlay = None;
    }
}
