//! Process-shared native font collection for the engine and Canvas drawings.
//! Uses HarmonyOS's global font collection so text follows the active system
//! theme font, serialized behind one Mutex with retained custom font buffers.
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use ohos_drawing_binding::FontCollection;

pub(crate) use super::family::families;

pub(crate) struct Fonts {
    pub(crate) collection: FontCollection,
    pub(crate) data: Vec<Arc<[u8]>>,
    pub(crate) generation: u64,
}

// SAFETY: FontCollection is a standalone drawing resource, not an ArkUI object.
// Every native access, including registration, cache clearing, and builder
// creation, holds the single process-wide Mutex. Paragraphs and text blobs
// retain their own immutable typefaces, and registered buffers stay live for
// the global collection's lifetime.
unsafe impl Send for Fonts {}

static SHARED: OnceLock<Arc<Mutex<Fonts>>> = OnceLock::new();
static PREWARMED: AtomicBool = AtomicBool::new(false);

impl Fonts {
    pub(crate) fn shared() -> Arc<Mutex<Self>> {
        SHARED
            .get_or_init(|| {
                Arc::new(Mutex::new(Self {
                    collection: FontCollection::global_instance()
                        .unwrap_or_else(FontCollection::shared),
                    data: Vec::new(),
                    generation: 0,
                }))
            })
            .clone()
    }

    /// Initializes the process-wide drawing font collection off the UI thread
    /// so the first text measurement does not pay cold collection setup.
    pub(crate) fn prewarm() {
        if SHARED.get().is_some() || PREWARMED.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(pool) = crate::services::executor::Executor::assets()
            && pool
                .submit(|| {
                    let _ = Self::shared();
                })
                .is_err()
        {
            PREWARMED.store(false, Ordering::Release);
        }
    }

    pub(crate) fn on_system_font_changed() {
        if let Some(shared) = SHARED.get() {
            let mut fonts = shared.lock().expect("font collection");
            fonts.collection.clear_caches();
            fonts.generation = fonts.generation.wrapping_add(1);
        }
    }
}
