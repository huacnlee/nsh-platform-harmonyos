//! UI-thread layout/style transitions and keyframes, delegating to
//! `nsh_core::transition` and driven by the surface's native frame clock.
pub(crate) use nsh_core::transition::Extra;
use nsh_core::{Bounds, LayoutAnimation, LayoutTransition, Points, ViewStyle, transition};

use crate::render::geometry::Rect;

#[derive(Default)]
pub(crate) struct Live {
    inner: transition::Live,
    target_style: Option<Box<ViewStyle>>,
    pub(crate) recolor: bool,
    pub(crate) text_color: Option<nsh_core::Color>,
}

pub(crate) struct Step {
    pub(crate) frame: Option<Rect>,
    pub(crate) opacity: f32,
    pub(crate) extra: Extra,
    pub(crate) active: bool,
    pub(crate) removed: bool,
    pub(crate) restyled: bool,
}

impl Live {
    pub(crate) fn text_color(&self) -> Option<nsh_core::Color> {
        self.text_color
    }

    pub(crate) fn frame_rate(&self) -> Option<f32> {
        self.inner.frame_rate()
    }

    pub(crate) fn recolor(&mut self, change: transition::Recolor) {
        self.inner.recolor(change);
        self.text_color = self.inner.text_color();
    }

    pub(crate) fn finish(&mut self, style: &mut Box<ViewStyle>) -> Step {
        let frame = self.inner.frame_target().map(Rect::from);
        if let Some(target) = self.target_style.take() {
            *style = target;
        }
        let removed = self.inner.dying();
        self.recolor = self.text_color.take().is_some();
        self.inner = transition::Live::default();
        Step {
            frame,
            opacity: 1.0,
            extra: Extra::NONE,
            active: false,
            removed,
            restyled: true,
        }
    }

    pub(crate) fn frame_target(&self) -> Option<Rect> {
        self.inner.frame_target().map(Rect::from)
    }

    pub(crate) fn frame(
        &mut self,
        from: Rect,
        to: Rect,
        animation: Option<&LayoutAnimation>,
    ) -> bool {
        if let Some(t) = animation
            .and_then(|a| a.update)
            .filter(|_| from != Rect::default() && from != to)
        {
            self.inner
                .move_frame(Bounds::<Points>::from(from), Bounds::<Points>::from(to), t);
            true
        } else {
            self.inner.stop_frame();
            false
        }
    }

    pub(crate) fn style(
        &mut self,
        out: &mut Box<ViewStyle>,
        old: ViewStyle,
        reduce: bool,
        initialized: bool,
    ) {
        if reduce {
            let _ = self.finish(&mut Box::default());
            return;
        }
        let mut boxed = out.clone();
        let prev = if initialized {
            old
        } else {
            ViewStyle::default()
        };
        if let Some(restyle) = transition::restyle(&prev, &mut boxed, false) {
            self.target_style = Some(out.clone());
            *out = boxed;
            self.inner.restyle(restyle);
        } else {
            self.target_style = None;
            self.inner.stop_style();
        }
    }

    pub(crate) fn appear(&mut self, transition: LayoutTransition) -> (f32, Extra) {
        self.inner.appear(transition);
        (
            self.inner.opacity(),
            self.inner.extra().unwrap_or(Extra::NONE),
        )
    }

    pub(crate) fn disappear(&mut self, transition: LayoutTransition) {
        self.inner.disappear(transition);
    }

    pub(crate) fn cancel_removal(&mut self) {
        self.inner.revive();
    }

    pub(crate) fn step(
        &mut self,
        now: f64,
        current_frame: Rect,
        style: &mut Box<ViewStyle>,
    ) -> Step {
        let prev_bounds = Bounds::<Points>::from(current_frame);
        let mut bounds = prev_bounds;
        let had_frame = self.inner.frame_target().is_some();
        let s = self.inner.step(now, &mut bounds, style);
        let color = self.inner.text_color();
        self.recolor = self.text_color != color;
        self.text_color = color;
        if s.restyled && self.inner.idle() {
            self.target_style = None;
        }
        let frame_done = had_frame && self.inner.frame_target().is_none();
        Step {
            frame: (had_frame && (bounds != prev_bounds || frame_done)).then(|| Rect::from(bounds)),
            opacity: self.inner.opacity(),
            extra: self.inner.extra().unwrap_or(Extra::NONE),
            active: !s.finished && !self.inner.idle(),
            removed: s.removed,
            restyled: s.restyled,
        }
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{AttributedText, Color, ResolvedTextStyle, Transition, TransitionProps};

    use super::*;

    fn text(color: Color) -> AttributedText {
        let mut text = AttributedText::default();
        text.push(
            "Color",
            ResolvedTextStyle {
                color,
                ..Default::default()
            },
        );
        text
    }

    #[test]
    fn text_color_interpolates_and_invalidates_the_terminal_frame() {
        let mut style = Box::new(ViewStyle {
            transition: Some(Transition {
                properties: TransitionProps(TransitionProps::COLORS),
                duration: 100.0,
                delay: 0.0,
                easing: [0.0, 0.0, 1.0, 1.0],
            }),
            ..Default::default()
        });
        let (red, blue) = (Color(0xff0000ff), Color(0x0000ffff));
        let mut live = Live::default();
        live.recolor(transition::recolor(&text(red), &text(blue), &style, false));
        assert_eq!(live.text_color(), Some(red));
        live.step(0.0, Rect::default(), &mut style);
        assert!(live.step(50.0, Rect::default(), &mut style).active);
        assert!(live.recolor);
        assert_eq!(live.text_color(), Some(Color(0x800080ff)));
        assert!(!live.step(100.0, Rect::default(), &mut style).active);
        assert!(live.recolor);
        assert_eq!(live.text_color(), None);

        live.recolor(transition::recolor(&text(red), &text(blue), &style, false));
        live.finish(&mut style);
        assert!(live.recolor);
        assert_eq!(live.text_color(), None);
    }
}
