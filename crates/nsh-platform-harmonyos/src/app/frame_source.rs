//! One UIContext DisplaySync consumer per engine generation.

use napi_ohos::{
    Env, Result,
    bindgen_prelude::{Function, FunctionRef, Object, ObjectRef},
};

use super::{frame_rate::RateRange, session::Services};

pub(crate) type Factory = FunctionRef<ObjectRef, FunctionRef<Request, ()>>;

#[napi_derive_ohos::napi(object)]
pub(crate) struct Request {
    pub enabled: bool,
    pub close: bool,
    pub min: i32,
    pub max: i32,
    pub expected: i32,
}

pub(crate) struct FrameSource {
    env: Env,
    control: FunctionRef<Request, ()>,
}

impl FrameSource {
    pub(crate) fn new(
        env: Env,
        host: &Services,
        on_frame: Function<'_, f64, bool>,
    ) -> Result<Self> {
        let mut callbacks = Object::new(&env)?;
        callbacks.set("frame", on_frame)?;
        let control = host
            .frame_factory()?
            .borrow_back(&env)?
            .call(callbacks.create_ref()?)?;
        Ok(Self { env, control })
    }

    pub(crate) fn request(&self, rate: RateRange) -> Result<()> {
        self.control.borrow_back(&self.env)?.call(Request {
            enabled: true,
            close: false,
            min: rate.min,
            max: rate.max,
            expected: rate.expected,
        })
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.control.borrow_back(&self.env)?.call(Request {
            enabled: false,
            close: false,
            min: 0,
            max: 0,
            expected: 0,
        })
    }
}

impl Drop for FrameSource {
    fn drop(&mut self) {
        let result = self.control.borrow_back(&self.env).and_then(|control| {
            control.call(Request {
                enabled: false,
                close: true,
                min: 0,
                max: 0,
                expected: 0,
            })
        });
        if let Err(error) = result {
            log::warn!("OHOS frame source disposal: {error}");
        }
    }
}
