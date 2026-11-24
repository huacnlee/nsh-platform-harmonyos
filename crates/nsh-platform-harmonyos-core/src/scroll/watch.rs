//! Host scroll watching (`NshSurface.watchScroll` and `NshScrollHandle`).
//!
//! Native UI of the host that moves with the app's content in every frame
//! follows a `<Scroll>` by its `nativeID` on the UI thread without a round trip
//! through JS.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};

use napi_derive_ohos::napi;
use napi_ohos::{
    Env, Error, Result,
    bindgen_prelude::{FnArgs, Function, FunctionRef},
};
use nsh_core::NodeId;
use rustc_hash::FxHashMap;

use crate::scroll::controller::Scroller;

enum Listener {
    Scroll(FunctionRef<FnArgs<(f64, f64, f64, f64)>, ()>),
    ContentSize(FunctionRef<FnArgs<(f64, f64)>, ()>),
}

struct HandleEntry {
    env: Env,
    scroll: Weak<Scroller>,
    alive: Cell<bool>,
    offset: Cell<(f64, f64)>,
    size: Cell<(f64, f64)>,
    listeners: RefCell<Vec<(u32, Listener)>>,
    next_listener: Cell<u32>,
}

static NEXT_HANDLE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    static HANDLES: RefCell<FxHashMap<u64, Rc<HandleEntry>>> = RefCell::default();
}

/// A `<Scroll>` of the app watched by the host on the UI thread.
#[napi]
#[derive(Clone, Debug)]
pub struct NshScrollHandle {
    id: u64,
}

#[napi]
impl NshScrollHandle {
    /// Whether the scroll view is still mounted with the watched `nativeID`.
    #[napi(getter)]
    pub fn is_alive(&self) -> bool {
        self.with_scroller(|_| true).unwrap_or(false)
    }

    /// The horizontal scroll offset in vp.
    #[napi(getter)]
    pub fn scroll_x(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_offset().0))
            .unwrap_or(0.0)
    }

    /// The vertical scroll offset in vp.
    #[napi(getter)]
    pub fn scroll_y(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_offset().1))
            .unwrap_or(0.0)
    }

    /// The content's width in vp (at least the viewport's).
    #[napi(getter)]
    pub fn content_width(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_content_size().0))
            .unwrap_or(0.0)
    }

    /// The content's height in vp (at least the viewport's).
    #[napi(getter)]
    pub fn content_height(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_content_size().1))
            .unwrap_or(0.0)
    }

    /// The visible width in vp.
    #[napi(getter)]
    pub fn viewport_width(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_viewport().0))
            .unwrap_or(0.0)
    }

    /// The visible height in vp.
    #[napi(getter)]
    pub fn viewport_height(&self) -> f64 {
        self.with_scroller(|s| f64::from(s.current_viewport().1))
            .unwrap_or(0.0)
    }

    /// Scrolls by `dx`, `dy`, clamped to the content.
    #[napi]
    pub fn scroll_by(&self, dx: f64, dy: f64, animated: Option<bool>) {
        let _ = self.with_scroller(|s| {
            let _ = s.host_scroll_by(dx as f32, dy as f32, animated.unwrap_or(false));
        });
    }

    /// Ends a fling or an animated scroll where it is.
    #[napi]
    pub fn stop_scrolling(&self) {
        let _ = self.with_scroller(|s| {
            let _ = s.host_stop_scrolling();
        });
    }

    /// Calls `listener(scrollX, scrollY, dx, dy)` on every offset change.
    /// Returns a listener id for `removeListener`.
    #[napi]
    pub fn on_scroll(&self, listener: Function<FnArgs<(f64, f64, f64, f64)>, ()>) -> Result<u32> {
        let reference = listener.create_ref()?;
        Ok(self.add_listener(Listener::Scroll(reference)))
    }

    /// Calls `listener(contentWidth, contentHeight)` when the content size changes.
    /// Returns a listener id for `removeListener`.
    #[napi]
    pub fn on_content_size_change(
        &self,
        listener: Function<FnArgs<(f64, f64)>, ()>,
    ) -> Result<u32> {
        let reference = listener.create_ref()?;
        Ok(self.add_listener(Listener::ContentSize(reference)))
    }

    /// Stops calling listener `id`.
    #[napi]
    pub fn remove_listener(&self, id: u32) {
        HANDLES.with(|handles| {
            if let Some(entry) = handles.borrow().get(&self.id) {
                entry.listeners.borrow_mut().retain(|(i, _)| *i != id);
            }
        });
    }
}

impl NshScrollHandle {
    fn new(env: Env, scroll: &Rc<Scroller>) -> Self {
        let id = NEXT_HANDLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (ox, oy) = scroll.current_offset();
        let (cw, ch) = scroll.current_content_size();
        let entry = Rc::new(HandleEntry {
            env,
            scroll: Rc::downgrade(scroll),
            alive: Cell::new(true),
            offset: Cell::new((f64::from(ox), f64::from(oy))),
            size: Cell::new((f64::from(cw), f64::from(ch))),
            listeners: RefCell::default(),
            next_listener: Cell::new(1),
        });
        HANDLES.with(|handles| {
            handles.borrow_mut().insert(id, entry);
        });
        Self { id }
    }

    fn with_scroller<R>(&self, f: impl FnOnce(&Rc<Scroller>) -> R) -> Option<R> {
        let entry = HANDLES.with(|handles| handles.borrow().get(&self.id).cloned())?;
        if !entry.alive.get() {
            return None;
        }
        let scroll = entry.scroll.upgrade()?;
        Some(f(&scroll))
    }

    fn add_listener(&self, listener: Listener) -> u32 {
        HANDLES.with(|handles| {
            let handles = handles.borrow();
            let Some(entry) = handles.get(&self.id) else {
                return 0;
            };
            if !entry.alive.get() || entry.scroll.strong_count() == 0 {
                return 0;
            }
            let id = entry.next_listener.get();
            entry.next_listener.set(id.wrapping_add(1).max(1));
            entry.listeners.borrow_mut().push((id, listener));
            id
        })
    }

    fn kill(&self) {
        if let Some(entry) = HANDLES.with(|handles| handles.borrow_mut().remove(&self.id)) {
            entry.alive.set(false);
            entry.listeners.borrow_mut().clear();
        }
    }

    fn scrolled(&self) {
        let Some(entry) = HANDLES.with(|handles| handles.borrow().get(&self.id).cloned()) else {
            return;
        };
        if !entry.alive.get() {
            return;
        }
        let Some(scroll) = entry.scroll.upgrade() else {
            return;
        };
        let (ox, oy) = scroll.current_offset();
        let offset = (f64::from(ox), f64::from(oy));
        let old = entry.offset.replace(offset);
        let dx = offset.0 - old.0;
        let dy = offset.1 - old.1;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let ids: Vec<u32> = entry
            .listeners
            .borrow()
            .iter()
            .filter_map(|(id, l)| matches!(l, Listener::Scroll(_)).then_some(*id))
            .collect();
        for id in ids {
            let cb = entry.listeners.borrow().iter().find_map(|(i, l)| {
                (*i == id)
                    .then(|| match l {
                        Listener::Scroll(f) => f.borrow_back(&entry.env).ok(),
                        Listener::ContentSize(_) => None,
                    })
                    .flatten()
            });
            if let Some(cb) = cb {
                let _ = cb.call((offset.0, offset.1, dx, dy).into());
            }
        }
    }

    fn resized(&self) {
        let Some(entry) = HANDLES.with(|handles| handles.borrow().get(&self.id).cloned()) else {
            return;
        };
        if !entry.alive.get() {
            return;
        };
        let Some(scroll) = entry.scroll.upgrade() else {
            return;
        };
        let (cw, ch) = scroll.current_content_size();
        let size = (f64::from(cw), f64::from(ch));
        if entry.size.replace(size) == size {
            return;
        }
        let ids: Vec<u32> = entry
            .listeners
            .borrow()
            .iter()
            .filter_map(|(id, l)| matches!(l, Listener::ContentSize(_)).then_some(*id))
            .collect();
        for id in ids {
            let cb = entry.listeners.borrow().iter().find_map(|(i, l)| {
                (*i == id)
                    .then(|| match l {
                        Listener::ContentSize(f) => f.borrow_back(&entry.env).ok(),
                        Listener::Scroll(_) => None,
                    })
                    .flatten()
            });
            if let Some(cb) = cb {
                let _ = cb.call((size.0, size.1).into());
            }
        }
    }
}

struct WatcherCallback {
    env: Env,
    callback: FunctionRef<Option<NshScrollHandle>, ()>,
}

impl WatcherCallback {
    fn notify(&self, handle: Option<NshScrollHandle>) {
        if let Ok(func) = self.callback.borrow_back(&self.env) {
            let _ = func.call(handle);
        }
    }
}

#[derive(Default)]
struct Registry {
    watchers: FxHashMap<Arc<str>, Rc<WatcherCallback>>,
    keyed: FxHashMap<Arc<str>, NshScrollHandle>,
    by_node: FxHashMap<NodeId, (Arc<str>, NshScrollHandle)>,
}

/// Per-surface scroll watcher registry shared between `NshSurface` and `UiContext`.
#[derive(Clone, Default)]
pub(crate) struct ScrollWatchers(Rc<RefCell<Registry>>);

impl ScrollWatchers {
    pub(crate) fn watch(
        &self,
        env: Env,
        native_id: &str,
        watcher: Option<Function<Option<NshScrollHandle>, ()>>,
    ) -> Result<()> {
        if native_id.is_empty() {
            return Err(Error::from_reason("nativeId must not be empty"));
        }
        let attach = {
            let mut reg = self.0.borrow_mut();
            match watcher {
                None => {
                    reg.watchers.remove(native_id);
                    None
                }
                Some(func) => {
                    let cb = Rc::new(WatcherCallback {
                        env,
                        callback: func.create_ref()?,
                    });
                    reg.watchers.insert(native_id.into(), cb.clone());
                    reg.keyed.get(native_id).cloned().map(|h| (cb, h))
                }
            }
        };
        if let Some((cb, handle)) = attach {
            cb.notify(Some(handle));
        }
        Ok(())
    }

    pub(crate) fn props(
        &self,
        env: Env,
        id: NodeId,
        scroll: &Rc<Scroller>,
        native_id: Option<Arc<str>>,
    ) {
        let current = self.0.borrow().by_node.get(&id).map(|(k, _)| k.clone());
        if current == native_id {
            return;
        }
        self.remove(id);
        let Some(native_id) = native_id else {
            return;
        };
        let handle = NshScrollHandle::new(env, scroll);
        let (previous, watcher) = {
            let mut reg = self.0.borrow_mut();
            reg.by_node.insert(id, (native_id.clone(), handle.clone()));
            let previous = reg.keyed.insert(native_id.clone(), handle.clone());
            (previous, reg.watchers.get(&native_id).cloned())
        };
        if let Some(previous) = previous {
            previous.kill();
            if let Some(w) = &watcher {
                w.notify(None);
            }
        }
        if let Some(w) = watcher {
            w.notify(Some(handle));
        }
    }

    pub(crate) fn remove(&self, id: NodeId) {
        let removed = self.0.borrow_mut().by_node.remove(&id);
        let Some((native_id, handle)) = removed else {
            return;
        };
        handle.kill();
        let watcher = {
            let mut reg = self.0.borrow_mut();
            if !reg.keyed.get(&native_id).is_some_and(|h| h.id == handle.id) {
                return;
            }
            reg.keyed.remove(&native_id);
            reg.watchers.get(&native_id).cloned()
        };
        if let Some(w) = watcher {
            w.notify(None);
        }
    }

    pub(crate) fn clear_nodes(&self) {
        let all: Vec<_> = self.0.borrow_mut().by_node.drain().collect();
        for (_, (native_id, handle)) in all {
            handle.kill();
            let watcher = {
                let mut reg = self.0.borrow_mut();
                if reg.keyed.get(&native_id).is_some_and(|h| h.id == handle.id) {
                    reg.keyed.remove(&native_id);
                    reg.watchers.get(&native_id).cloned()
                } else {
                    None
                }
            };
            if let Some(w) = watcher {
                w.notify(None);
            }
        }
    }

    pub(crate) fn close(&self) {
        self.clear_nodes();
        self.0.borrow_mut().watchers.clear();
    }

    pub(crate) fn scrolled(&self, id: NodeId) {
        let handle = self.0.borrow().by_node.get(&id).map(|(_, h)| h.clone());
        if let Some(handle) = handle {
            handle.scrolled();
        }
    }

    pub(crate) fn resized(&self, id: NodeId) {
        let handle = self.0.borrow().by_node.get(&id).map(|(_, h)| h.clone());
        if let Some(handle) = handle {
            handle.resized();
        }
    }
}
