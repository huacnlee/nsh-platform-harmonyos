//! Off-thread GLES shader compilation and driver pipeline warmup.
//!
//! Huawei Maleoon/BiSheng defers 10–20 ms of backend ISA compilation until a
//! program's first draw into an RGBA8 target. Running that warmup on an
//! independent offscreen EGL context keeps `nsh-compositor` free to drain
//! `NativeImage` producers and present 120 Hz frames without stalling
//! `RSRenderThread`, while passing ES 3.0 `ProgramBinary` blobs back to the
//! compositor context when the driver supports binary retrieval.

use std::sync::{Arc, Mutex, mpsc};

use nsh_core::shader::ShaderLayer;
use rustc_hash::{FxHashMap, FxHashSet};

use super::readback::TextureContext;

struct State {
    in_flight: FxHashSet<u64>,
    attempted: FxHashSet<u64>,
    binaries: FxHashMap<u64, glow::ProgramBinary>,
    alive: bool,
}

#[derive(Clone)]
pub(super) struct BinaryStore {
    state: Arc<Mutex<State>>,
}
impl BinaryStore {
    pub(super) fn take(&self, hash: u64) -> Option<glow::ProgramBinary> {
        self.state.lock().unwrap().binaries.remove(&hash)
    }
}

pub(super) struct Precompiler {
    tx: mpsc::Sender<Arc<ShaderLayer>>,
    state: Arc<Mutex<State>>,
    _thread: std::thread::JoinHandle<()>,
}
impl Precompiler {
    pub(super) fn spawn(on_warmed: impl Fn() + Send + 'static) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Arc<ShaderLayer>>();
        let state = Arc::new(Mutex::new(State {
            in_flight: FxHashSet::default(),
            attempted: FxHashSet::default(),
            binaries: FxHashMap::default(),
            alive: true,
        }));
        let worker_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("nsh-shader-prep".into())
            .spawn(move || {
                let mut gpu: Option<TextureContext> = None;
                while let Ok(layer) = rx.recv() {
                    let hash = layer.shader.hash();
                    if gpu.is_none() {
                        match TextureContext::new() {
                            Ok(ctx) => gpu = Some(ctx),
                            Err(error) => {
                                log::warn!("shader precompile context: {error}");
                                let mut s = worker_state.lock().unwrap();
                                s.alive = false;
                                s.in_flight.clear();
                                drop(s);
                                on_warmed();
                                return;
                            }
                        }
                    }
                    let binary =
                        match super::gles::precompile_to_binary(gpu.as_ref().unwrap(), &layer) {
                            Ok(binary) => binary,
                            Err(error) => {
                                log::warn!("shader precompile: {error}");
                                None
                            }
                        };
                    {
                        let mut s = worker_state.lock().unwrap();
                        s.in_flight.remove(&hash);
                        s.attempted.insert(hash);
                        if let Some(bin) = binary
                            && !bin.buffer.is_empty()
                        {
                            s.binaries.insert(hash, bin);
                        }
                    }
                    on_warmed();
                }
                let mut s = worker_state.lock().unwrap();
                s.alive = false;
                s.in_flight.clear();
                drop(s);
                drop(gpu);
            })
            .ok()?;
        Some(Self {
            tx,
            state,
            _thread: thread,
        })
    }

    pub(super) fn binary_store(&self) -> BinaryStore {
        BinaryStore {
            state: self.state.clone(),
        }
    }

    /// Submits `layer` for background compilation if not already attempted or
    /// in flight. Returns `true` iff `layer` is currently compiling off-thread.
    pub(super) fn submit_or_compiling(&self, layer: &Arc<ShaderLayer>) -> bool {
        let hash = layer.shader.hash();
        let mut s = self.state.lock().unwrap();
        if !s.alive {
            return false;
        }
        if s.in_flight.contains(&hash) {
            return true;
        }
        if s.attempted.contains(&hash) {
            return false;
        }
        s.in_flight.insert(hash);
        drop(s);
        if self.tx.send(layer.clone()).is_err() {
            let mut s = self.state.lock().unwrap();
            s.alive = false;
            s.in_flight.remove(&hash);
            return false;
        }
        true
    }

    pub(super) fn is_compiling(&self, hash: u64) -> bool {
        let s = self.state.lock().unwrap();
        s.alive && s.in_flight.contains(&hash)
    }
}
