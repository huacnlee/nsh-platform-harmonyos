//! Native alert/action-sheet queue. Results and reclamation run in a later
//! UI turn so a button callback never destroys its own native stack.
use std::{collections::VecDeque, rc::Rc};

use nsh_core::{DialogButtonStyle, EventKind, EventPayload, PayloadValue};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::{Column, Scroll, Text},
    },
    dialog::Dialog,
    types::{
        advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A,
        node_type::ArkUINodeType,
    },
};

use crate::{
    app::runtime::Message,
    render::{
        mount::UiContext,
        node::{NativeNode, Result},
    },
};
struct Alert {
    id: u32,
    dialog: Dialog,
    root: NativeNode,
    cancelable: bool,
}
impl Drop for Alert {
    fn drop(&mut self) {
        crate::render::node::callback(|| {
            let close = self.dialog.close();
            let remove = self.dialog.remove_content();
            let dispose = self.root.borrow_mut().dispose();
            // Queued alerts have never been shown; close may reject that state.
            let _ = close;
            remove?;
            dispose?;
            self.dialog.dispose()?;
            Ok(())
        });
    }
}
pub(crate) struct Dialogs {
    context: Rc<UiContext>,
    alerts: VecDeque<Alert>,
    overlay: Option<crate::app::session::Overlay>,
}
impl Dialogs {
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
    pub(crate) fn request_close(&mut self) -> Result<()> {
        if let Some(alert) = self.alerts.front()
            && alert.cancelable
        {
            self.finish(alert.id, -1)?;
        }
        Ok(())
    }
    pub(crate) fn show(&mut self, request: nsh_core::Dialog) -> Result<()> {
        if request.buttons.is_empty() || self.alerts.len() >= 32 {
            self.reply(request.id, -1);
            return Ok(());
        }
        let dialog = Dialog::new()?;
        // The system card's blur can override its background on device. Own the
        // content decoration so an app's dark palette also applies in a light host.
        dialog.custom_style(true)?;
        if request.sheet {
            dialog.content_alignment(ohos_arkui_binding::types::alignment::Alignment::Bottom)?;
        }
        let root = Scroll::new()?.into_node();
        let column = Column::new()?.into_node();
        for node in [&root, &column] {
            node.borrow().set_length_metric_unit(LengthMetricUnit::Vp)?;
        }
        let dark = self
            .context
            .appearance
            .get()
            .resolve(self.context.platform.get().color_scheme)
            == nsh_engine::ColorScheme::Dark;
        let width = (self.context.platform.get().window.width.0 - 32.0).clamp(120.0, 360.0);
        let fg = self
            .context
            .theme
            .borrow()
            .color("popover-foreground", dark);
        let font_scale = self.context.platform.get().font_scale.max(0.1);
        let mut height = 16.0;
        let make_text = |text: &str, size: f32, h: f32, color: u32| -> Result<NativeNode> {
            let node = Text::new()?.into_node();
            node.borrow().set_length_metric_unit(LengthMetricUnit::Vp)?;
            node.borrow()
                .set_string_attribute(A::TextContent, text.replace('\0', "\u{fffd}"))?;
            node.borrow()
                .set_f32_attribute(A::FontSize, size * font_scale)?;
            node.borrow().set_u32_attribute(A::FontColor, color)?;
            node.borrow().set_i32_attribute(A::TextAlign, 1)?;
            node.borrow().width(width)?;
            node.borrow().height(h)?;
            Ok(node)
        };
        for (text, size) in [
            (request.title.as_deref(), 20.0),
            (request.message.as_deref(), 16.0),
        ] {
            if let Some(text) = text {
                use ohos_drawing_binding::{
                    FontCollection, TextStyle, TypographyBuilder, TypographyStyle,
                };
                let mut fonts = FontCollection::shared();
                let mut paragraph_style = TypographyStyle::new();
                let mut builder = TypographyBuilder::new(&mut paragraph_style, &mut fonts);
                let mut style = TextStyle::new();
                style.set_font_size(f64::from(size * font_scale));
                builder.push_text_style(&mut style);
                builder.add_text(text);
                builder.pop_text_style();
                let mut paragraph = builder.build();
                paragraph.layout(f64::from(width));
                let h = paragraph.height().ceil() as f32 + size * font_scale + 4.0;
                column
                    .borrow_mut()
                    .add_child(make_text(text, size, h, fg)?)?;
                height += h;
            }
        }
        let mut ordered: Vec<_> = request.buttons.iter().enumerate().collect();
        ordered.sort_by_key(|(_, button)| button.style == DialogButtonStyle::Cancel);
        for (index, button) in ordered {
            if request.sheet && button.style == DialogButtonStyle::Cancel {
                let gap = Column::new()?.into_node();
                gap.borrow().set_length_metric_unit(LengthMetricUnit::Vp)?;
                gap.borrow().height(8.0)?;
                column.borrow_mut().add_child(gap)?;
                height += 8.0;
            }
            let color = if button.style == DialogButtonStyle::Destructive {
                self.context.theme.borrow().color("destructive", dark)
            } else {
                self.context.theme.borrow().color("primary", dark)
            };
            let action_height = (48.0 * font_scale).max(48.0);
            let node = make_text(&button.text, 18.0, action_height, color)?;
            node.borrow()
                .set_u32_attribute(A::AccessibilityRole, ArkUINodeType::Button.into())?;
            node.borrow().set_i32_attribute(
                A::AccessibilityMode,
                ohos_arkui_binding::types::view::AccessibilityMode::Enabled.into(),
            )?;
            node.borrow()
                .id(format!("nsh-dialog-{}-{index}", request.id))?;
            let route = self.context.dispatcher.clone();
            let id = request.id;
            node.borrow_mut().on_click(move || {
                if let Some(route) = route.upgrade() {
                    route.send(Message::DialogResult {
                        id,
                        index: index as i32,
                    });
                }
            });
            column.borrow_mut().add_child(node)?;
            height += action_height;
        }
        column.borrow().width(width)?;
        column.borrow().height(height)?;
        root.borrow().width(width)?;
        root.borrow()
            .height(height.min((self.context.platform.get().window.height.0 * 0.8).max(96.0)))?;
        root.borrow_mut().add_child(column)?;
        root.borrow().set_u32_attribute(
            A::BackgroundColor,
            self.context.theme.borrow().color("popover", dark),
        )?;
        root.borrow().set_f32_attribute(A::BorderRadius, 16.0)?;
        root.borrow().set_i32_attribute(A::Clip, 1)?;
        dialog.content_node(&root)?;
        let route = self.context.dispatcher.clone();
        let id = request.id;
        dialog.dismiss_handler(move |_| {
            if request.cancelable
                && let Some(route) = route.upgrade()
            {
                route.send(Message::DialogResult { id, index: -1 });
            }
            true
        })?;
        self.alerts.push_back(Alert {
            id,
            dialog,
            root,
            cancelable: request.cancelable,
        });
        if self.alerts.len() == 1 {
            let overlay = self.context.host.overlay(self.context.dispatcher.clone())?;
            if let Err(error) = self.alerts.front().unwrap().dialog.show() {
                self.alerts.pop_front();
                return Err(error.into());
            }
            self.overlay = Some(overlay);
        }
        Ok(())
    }
    pub(crate) fn finish(&mut self, id: u32, index: i32) -> Result<()> {
        if self.alerts.front().is_none_or(|alert| alert.id != id) {
            return Ok(());
        }
        self.alerts.pop_front();
        self.reply(id, index);
        while let Some(next) = self.alerts.front() {
            match next.dialog.show() {
                Ok(()) => return Ok(()),
                Err(error) => {
                    log::error!("OHOS queued dialog: {error}");
                    let id = next.id;
                    self.alerts.pop_front();
                    self.reply(id, -1);
                }
            }
        }
        self.overlay = None;
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
