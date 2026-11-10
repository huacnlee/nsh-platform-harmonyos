//! CPU scene composition for paint planes requiring masks or ordered backdrop
//! roots. Ordinary exported filters and shaders stay in the GLES pipeline.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use super::draw::{Plan, Sources};
use crate::app::runtime::{Dispatcher, Message};

pub(crate) struct Pixels {
    pub(crate) id: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) output: Mutex<Option<Vec<Pixels>>>,
    pub(crate) posted: AtomicBool,
}
pub(crate) struct Scene {
    pub(crate) plan: Arc<Plan>,
    pub(crate) shared: Arc<Shared>,
}
impl Scene {
    pub(super) fn render(&self, source: &mut impl Sources, route: &Dispatcher) {
        match self.plan.render(source) {
            Ok(Some(frames)) => {
                *self.shared.output.lock().unwrap() = Some(
                    frames
                        .into_iter()
                        .map(|frame| Pixels {
                            id: frame.id,
                            width: frame.image.bitmap.width(),
                            height: frame.image.bitmap.height(),
                            pixels: frame.image.bitmap.pixels().to_vec(),
                        })
                        .collect(),
                );
                if !self.shared.posted.swap(true, Ordering::AcqRel) {
                    route.send(Message::Scene);
                }
            }
            Ok(None) => {}
            Err(error) => route.send(Message::Error(format!("scene compositor: {error}"))),
        }
    }
}
