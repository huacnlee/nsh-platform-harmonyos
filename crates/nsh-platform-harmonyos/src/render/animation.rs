//! One native animation graph per surface, evaluated on the UI thread.

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    time::Instant,
};

use nsh_core::animation::{AnimationOp, AnimationProp, AnimationValue, Finished, Graph};
use nsh_engine::EngineControl;

use crate::{
    app::frame_rate::UiDemand,
    render::{
        frame_clock::Active,
        node::{Node, Result},
    },
};

type Bindings = (Weak<Node>, Vec<(AnimationProp, u32)>);

#[derive(Default)]
struct State {
    graph: Graph,
    finished: Vec<Finished>,
}

pub(crate) struct Animator {
    state: RefCell<State>,
    dirty: std::cell::Cell<bool>,
    bindings: RefCell<Active<Bindings>>,
    motion: RefCell<Active<Weak<Node>>>,
    scrolls: RefCell<Active<Weak<Node>>>,
    sticky: RefCell<Active<Weak<Node>>>,
    selection: RefCell<Weak<Node>>,
    removals: RefCell<Vec<u32>>,
    control: EngineControl,
    dispatcher: std::sync::Weak<crate::app::runtime::Dispatcher>,
    clock: Instant,
    frame_clock: RefCell<super::frame_clock::FrameClock>,
    frame_time: std::cell::Cell<f64>,
    graph_clock: RefCell<super::frame_clock::GraphClock>,
}

impl Animator {
    #[cfg(feature = "dev")]
    pub(crate) fn test_animating(&self) -> bool {
        self.state.borrow().graph.is_finishing()
            || self.motion.borrow().iter().any(|node| {
                node.upgrade().is_some_and(|node| {
                    node.live
                        .borrow()
                        .as_ref()
                        .is_some_and(|live| live.test_finishing(self.frame_time.get()))
                })
            })
    }
    pub(crate) fn new(
        control: EngineControl,
        dispatcher: std::sync::Weak<crate::app::runtime::Dispatcher>,
    ) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::default(),
            dirty: std::cell::Cell::new(false),
            bindings: RefCell::default(),
            motion: RefCell::default(),
            scrolls: RefCell::default(),
            sticky: RefCell::default(),
            selection: RefCell::default(),
            removals: RefCell::default(),
            control,
            dispatcher,
            clock: Instant::now(),
            frame_clock: RefCell::default(),
            frame_time: std::cell::Cell::new(0.0),
            graph_clock: RefCell::default(),
        })
    }
    pub(crate) fn hold_presented_animation(self: &Rc<Self>) -> Option<PresentedAnimation> {
        if !self.state.borrow().graph.is_finishing() {
            return None;
        }
        let elapsed = self.now();
        let last = self.frame_time.get();
        let anchor = if last > 0.0 && (elapsed - last).abs() < 50.0 {
            last
        } else {
            self.frame_clock.borrow_mut().sample(None, elapsed)
        };
        self.graph_clock.borrow_mut().hold(anchor);
        Some(PresentedAnimation(Rc::downgrade(self)))
    }
    pub(crate) fn request_frame(&self) {
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            dispatcher.request_frame();
        }
    }
    pub(crate) fn mark_dirty(&self) {
        self.dirty.set(true);
    }
    pub(crate) fn apply(&self, op: AnimationOp) {
        self.dirty.set(true);
        let mut state = self.state.borrow_mut();
        let State { graph, finished } = &mut *state;
        graph.apply(op, finished);
    }
    pub(crate) fn bind(&self, owner: &Rc<Node>, values: Vec<(AnimationProp, u32)>) -> Result<()> {
        self.dirty.set(true);
        let mut bindings = self.bindings.borrow_mut();
        if values.is_empty() {
            bindings.remove(owner.id);
        } else {
            bindings.insert(owner.id, (Rc::downgrade(owner), values));
        }
        drop(bindings);
        owner.component_animated.borrow_mut().clear();
        owner.animated_opacity.set(None);
        owner.reset_text_tint()?;
        owner.restore_style()
    }
    pub(crate) fn apply_node(&self, owner: &Node) -> Result<()> {
        let values = {
            let bindings = self.bindings.borrow();
            let Some((_, props)) = bindings.get(owner.id) else {
                return Ok(());
            };
            let state = self.state.borrow();
            props
                .iter()
                .map(|(prop, node)| (*prop, state.graph.eval(*node)))
                .collect::<Vec<_>>()
        };
        owner.animate(&values)
    }
    pub(crate) fn remove(&self, id: u32) {
        self.bindings.borrow_mut().remove(id);
        self.motion.borrow_mut().remove(id);
        self.scrolls.borrow_mut().remove(id);
        self.sticky.borrow_mut().remove(id);
    }
    pub(crate) fn set_sticky(&self, owner: &Rc<Node>, animated: bool) {
        if animated {
            self.dirty.set(true);
        }
        let mut sticky = self.sticky.borrow_mut();
        if animated {
            sticky.insert(owner.id, Rc::downgrade(owner));
        } else {
            sticky.remove(owner.id);
        }
    }
    pub(crate) fn now(&self) -> f64 {
        self.clock.elapsed().as_secs_f64() * 1000.0
    }
    pub(crate) fn track(&self, owner: &Rc<Node>) {
        let mut motion = self.motion.borrow_mut();
        motion.insert(owner.id, Rc::downgrade(owner));
        self.request_frame();
    }
    pub(crate) fn track_scroll(&self, owner: &Rc<Node>) {
        let mut scrolls = self.scrolls.borrow_mut();
        scrolls.insert(owner.id, Rc::downgrade(owner));
        self.request_frame();
    }
    pub(crate) fn track_selection(&self, root: &Rc<Node>) {
        *self.selection.borrow_mut() = Rc::downgrade(root);
        self.request_frame();
    }
    pub(crate) fn take_removals(&self) -> Vec<u32> {
        std::mem::take(&mut self.removals.borrow_mut())
    }
    pub(crate) fn set(&self, node: u32, value: f32) -> Result<()> {
        {
            let mut state = self.state.borrow_mut();
            let State { graph, finished } = &mut *state;
            graph.set(node, value, finished);
        }
        self.write()
    }
    pub(crate) fn value(&self, node: u32) -> f32 {
        self.state.borrow().graph.value(node).unwrap_or_default()
    }
    pub(crate) fn settle(&self, node: u32) {
        let started = self.state.borrow_mut().graph.settle(node);
        if started {
            self.request_frame();
        }
    }
    pub(crate) fn frame_time(&self) -> f64 {
        self.frame_time.get()
    }
    pub(crate) fn flush(&self) -> Result<()> {
        if self.dirty.get() {
            self.write()?;
        }
        Ok(())
    }
    pub(crate) fn tick(&self, timestamp: Option<u64>) -> Result<UiDemand> {
        let now = self.frame_clock.borrow_mut().sample(timestamp, self.now());
        self.frame_time.set(now);
        let (changed, mut demand) = {
            let mut state = self.state.borrow_mut();
            let State { graph, finished } = &mut *state;
            let changed = (graph.is_animating() || self.dirty.get())
                && graph.tick(self.graph_clock.borrow_mut().sample(now) / 1000.0, finished);
            (
                changed,
                UiDemand::from_animation(graph.is_animating(), graph.frame_rate()),
            )
        };
        let selection_root = self.selection.borrow().upgrade();
        if let Some(root) = selection_root {
            let mut touches = root.touches.borrow_mut();
            let active = touches.selection_tick(&root, now)?;
            demand = demand.merge(UiDemand::from_animation(active, None));
            // Keep a weak cleanup route at an edge without scheduling idle frames.
            if !touches.selection_held() {
                *self.selection.borrow_mut() = Weak::new();
            }
        }
        self.scrolls
            .borrow_mut()
            .try_retain(|weak| -> Result<bool> {
                let active = if let Some(owner) = weak.upgrade()
                    && let Some(scroll) = owner.scroll.borrow().as_ref()
                {
                    scroll.tick(now)?
                } else {
                    false
                };
                demand = demand.merge(UiDemand::from_animation(active, None));
                Ok(active)
            })?;
        self.motion
            .borrow_mut()
            .try_retain(|weak| -> Result<bool> {
                let Some(owner) = weak.upgrade() else {
                    return Ok(false);
                };
                let (motion_demand, removed) = owner.tick_transition(now)?;
                demand = demand.merge(motion_demand);
                if removed {
                    self.removals.borrow_mut().push(owner.id);
                }
                Ok(motion_demand != UiDemand::Idle)
            })?;
        if changed || self.dirty.get() {
            self.write()?;
        } else {
            // A driver can finish without changing its value. Local layout,
            // navigation and scroll motion apply their own node bindings.
            self.report_finished();
        }
        Ok(demand)
    }
    fn write(&self) -> Result<()> {
        self.dirty.set(false);
        let mut values = Vec::new();
        self.bindings
            .borrow_mut()
            .try_retain(|binding| -> Result<bool> { Ok(binding.0.strong_count() > 0) })?;
        for binding in self.bindings.borrow().iter() {
            let Some(owner) = binding.0.upgrade() else {
                continue;
            };
            values.clear();
            {
                let state = self.state.borrow();
                values.extend(
                    binding
                        .1
                        .iter()
                        .map(|(prop, node)| (*prop, state.graph.eval(*node))),
                );
            }
            owner.animate(&values)?;
        }
        self.sticky
            .borrow_mut()
            .try_retain(|owner| -> Result<bool> { Ok(owner.strong_count() > 0) })?;
        for owner in self.sticky.borrow().iter().filter_map(Weak::upgrade) {
            if let Some(scroll) = owner.scroll.borrow().as_ref() {
                scroll.pin_sticky()?;
            }
        }
        self.report_finished();
        Ok(())
    }
    fn report_finished(&self) {
        let finished = std::mem::take(&mut self.state.borrow_mut().finished);
        if !finished.is_empty() {
            self.control.animations_finished(finished);
        }
    }
}

pub(crate) fn color(value: AnimationValue) -> nsh_core::Color {
    nsh_core::Color(match value {
        AnimationValue::Color(color) => color,
        other => other.number() as u32,
    })
}

// A root snapshot can outlive its presentation's SDK callback. Dropping the
// presentation on success, failure or cancellation releases its hold.
pub(crate) struct PresentedAnimation(Weak<Animator>);

impl Drop for PresentedAnimation {
    fn drop(&mut self) {
        if let Some(animator) = self.0.upgrade() {
            animator.graph_clock.borrow_mut().release();
            animator.request_frame();
        }
    }
}
