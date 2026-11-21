//! DisplaySync is the only animation clock, in ArkUI's pre-build phase.
//! A NativeVSync connection publishes the same Rust-arbitrated range to RS;
//! its independent callback never admits, skips or advances a UI frame.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, Mutex, Weak},
};

use napi_ohos::{
    Env,
    bindgen_prelude::{FunctionRef, Object, ObjectRef},
};
use ohos_vsync_binding::Vsync;

use super::{
    frame_rate::{FrameRate, RateRange},
    runtime::{Dispatcher, Message},
    session::Services,
};
use crate::render::node::{Error, Result};

pub(crate) type Factory = FunctionRef<ObjectRef, FunctionRef<Request, ()>>;

#[napi_derive_ohos::napi(object)]
pub(crate) struct Request {
    pub enabled: bool,
    pub close: bool,
    pub min: i32,
    pub max: i32,
    pub expected: i32,
}

pub(crate) struct Phase {
    env: Env,
    control: FunctionRef<Request, ()>,
    running: Rc<Cell<bool>>,
    range: Cell<Option<RateRange>>,
}

impl Phase {
    pub(crate) fn new(
        env: Env,
        host: &Services,
        driver: Arc<RateDriver>,
        sample: impl Fn(u64) -> napi_ohos::Result<bool> + 'static,
    ) -> napi_ohos::Result<Self> {
        let running = Rc::new(Cell::new(false));
        let callback_running = running.clone();
        let on_frame =
            env.create_function_from_closure::<f64, bool, _>("nshFrame", move |ctx| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let timestamp: f64 = ctx.get(0)?;
                    if !timestamp.is_finite() || timestamp < 0.0 {
                        return Err(napi_ohos::Error::from_reason(
                            "invalid display frame timestamp",
                        ));
                    }
                    sample(timestamp as u64)
                }))
                .unwrap_or_else(|_| {
                    Err(napi_ohos::Error::from_reason(
                        "panic in OHOS display frame callback",
                    ))
                });
                // ETS stops even when the callback throws. Keep the subscription's
                // ownership in sync so a later real request can start it again.
                let active = matches!(result, Ok(true));
                callback_running.set(active);
                if !active && let Err(error) = driver.set_active(false) {
                    log::warn!("OHOS frame driver stop: {error}");
                }
                result
            })?;
        let mut callbacks = Object::new(&env)?;
        callbacks.set("frame", on_frame)?;
        let control = host
            .frame_factory()?
            .borrow_back(&env)?
            .call(callbacks.create_ref()?)?;
        Ok(Self {
            env,
            control,
            running,
            range: Cell::new(None),
        })
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.get()
    }

    pub(crate) fn set_running(&self, running: bool, range: RateRange) -> napi_ohos::Result<()> {
        let range = range.for_display_sync();
        if self.running.get() != running || (running && self.range.get() != Some(range)) {
            self.control.borrow_back(&self.env)?.call(Request {
                enabled: running,
                close: false,
                min: range.min,
                max: range.max,
                expected: range.expected,
            })?;
            self.running.set(running);
            if running {
                self.range.set(Some(range));
            }
        }
        Ok(())
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Err(error) = self.control.borrow_back(&self.env).and_then(|control| {
            control.call(Request {
                enabled: false,
                close: true,
                min: 0,
                max: 0,
                expected: 0,
            })
        }) {
            log::warn!("OHOS frame phase disposal: {error}");
        }
    }
}

#[derive(Default)]
struct Pending {
    active: bool,
    in_flight: bool,
    reclaim: Option<u64>,
}

/// One NativeVSync request in flight while there is real frame demand. This
/// keeps the RS rate vote live independently of UI delivery. Native and UI
/// callbacks have no guaranteed order, so they must never gate each other.
pub(crate) struct RateDriver {
    vsync: Arc<Vsync>,
    rate: Arc<FrameRate>,
    dispatcher: Weak<Dispatcher>,
    pending: Mutex<Pending>,
}

impl RateDriver {
    pub(crate) fn new(
        vsync: Arc<Vsync>,
        rate: Arc<FrameRate>,
        dispatcher: Weak<Dispatcher>,
    ) -> Arc<Self> {
        Arc::new(Self {
            vsync,
            rate,
            dispatcher,
            pending: Mutex::default(),
        })
    }

    pub(crate) fn set_active(self: &Arc<Self>, active: bool) -> Result<()> {
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.active != active {
                pending.active = active;
                if !active {
                    // Serialize release with publication from the native thread.
                    self.rate.release();
                }
            }
        }
        self.request()
    }

    pub(crate) fn active(&self) -> bool {
        self.pending.lock().unwrap().active
    }

    pub(crate) fn retire(self: &Arc<Self>, epoch: u64) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        pending.reclaim = Some(pending.reclaim.map_or(epoch, |old| old.max(epoch)));
        drop(pending);
        self.request()
    }

    fn request(self: &Arc<Self>) -> Result<()> {
        let mut pending = self.pending.lock().unwrap();
        if pending.in_flight || !(pending.active || pending.reclaim.is_some()) {
            return Ok(());
        }
        let Some(dispatcher) = self.dispatcher.upgrade() else {
            return Ok(());
        };
        // Capture only epochs eligible at registration. Retirements occurring
        // during this request need a following frame, including after close.
        let reclaim = pending.reclaim.take();
        pending.in_flight = true;
        drop(pending);
        let driver = self.clone();
        // The callback retains the dispatcher until the reclamation message has
        // been posted. Do not filter this finite tail by live/active generation.
        let status = self.vsync.request_frame_once(move |_timestamp| {
            {
                let mut pending = driver.pending.lock().unwrap();
                pending.in_flight = false;
                if pending.active {
                    driver.rate.publish_frame();
                }
            }
            if let Some(epoch) = reclaim {
                dispatcher.send(Message::Reclaim(epoch));
            }
            if let Err(error) = driver.request() {
                dispatcher.send(Message::Error(error.to_string()));
            }
        });
        if status != 0 {
            let mut pending = self.pending.lock().unwrap();
            pending.in_flight = false;
            pending.active = false;
            if let Some(epoch) = reclaim {
                pending.reclaim = Some(pending.reclaim.map_or(epoch, |old| old.max(epoch)));
            }
            self.rate.release();
            return Err(Error::Vsync(status));
        }
        Ok(())
    }
}
