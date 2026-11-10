//! One serialized native font collection for the engine and its Canvas drawings.
//! Registered buffers remain alive for both shaped paragraphs and glyph blobs.
use std::sync::{Arc, Mutex};

use ohos_drawing_binding::FontCollection;

pub(crate) struct Fonts {
    pub(crate) collection: FontCollection,
    pub(crate) data: Vec<Arc<[u8]>>,
    pub(crate) generation: u64,
}
// SAFETY: FontCollection is a standalone drawing resource, not an ArkUI object.
// Every native access, including registration and builder creation, holds the
// shared Mutex. Paragraphs and text blobs retain their own immutable typefaces.
unsafe impl Send for Fonts {}
impl Fonts {
    pub(crate) fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            collection: FontCollection::shared(),
            data: Vec::new(),
            generation: 0,
        }))
    }
}

pub(crate) fn families(value: &str) -> Vec<&str> {
    use nsh_core::FamilyName;
    nsh_core::font_families(value)
        .filter_map(|family| match family {
            FamilyName::Named(name) => (!name.contains('\0')).then_some(name),
            FamilyName::Serif => Some("serif"),
            FamilyName::Monospace => Some("monospace"),
            _ => Some("sans-serif"),
        })
        .collect()
}
