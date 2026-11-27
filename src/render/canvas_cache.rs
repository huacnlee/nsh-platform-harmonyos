//! Bounded cross-frame shaped-text LRU cache and active redraw frame-rate
//! tracker for ArkUI `CanvasView`.
//!
//! Pure-Rust logic kept separate from OHOS drawing bindings so host tests can
//! verify LRU eviction, key discrimination, and 120 Hz voting/idle release.

use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use nsh_core::canvas::CanvasFont;
use rustc_hash::FxHashMap;

/// Default maximum number of shaped text runs cached per `CanvasView`.
///
/// Sized for ~10–25 full terminal screens (40 rows × multi-span colored runs)
/// so unchanged rows hit the cache in O(1) across frames without unbounded
/// memory growth.
pub(crate) const DEFAULT_TEXT_CACHE_CAPACITY: usize = 1024;

/// Maximum interval between consecutive `CanvasView::set()` redraws to classify
/// a canvas as actively streaming/animating (≥ 20 Hz, safely below 530 ms
/// cursor blink intervals).
pub(crate) const CANVAS_BURST_WINDOW: Duration = Duration::from_millis(50);

/// Duration after the last `CanvasView::set()` redraw before releasing the
/// high-refresh `FrameRate` vote and letting the display return to idle.
pub(crate) const CANVAS_IDLE_HOLD: Duration = Duration::from_millis(200);

/// Canonicalizes `f32` to deterministic hashable bits (`-0.0` → `+0.0`, any
/// `NaN` → quiet canonical `NaN`).
#[inline]
fn canonical_f32_bits(value: f32) -> u32 {
    if value == 0.0 {
        0.0f32.to_bits()
    } else if value.is_nan() {
        f32::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

/// Borrowed lookup key for [`ShapedTextCache`], capturing every parameter that
/// influences `ShapedText` glyph shaping and metrics (`OH_Drawing_Typography`).
///
/// Paint color, stroke/fill mode, alignment, baseline, `max_width`, transforms,
/// alpha, blend mode, and shadows are applied live at draw time (`Replay::text`)
/// and therefore do not invalidate cached glyph blobs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextCacheKeyRef<'a> {
    pub(crate) text: &'a str,
    pub(crate) font_size_bits: u32,
    pub(crate) font_weight: u16,
    pub(crate) font_italic: bool,
    pub(crate) font_family: Option<&'a str>,
    pub(crate) font_variant: u8,
    pub(crate) rtl: bool,
    pub(crate) font_generation: u64,
}

impl<'a> TextCacheKeyRef<'a> {
    /// Builds a zero-allocation lookup key from live canvas shaping inputs.
    #[inline]
    pub(crate) fn new(
        text: &'a str,
        font: &'a CanvasFont,
        rtl: bool,
        font_generation: u64,
    ) -> Self {
        Self {
            text,
            font_size_bits: canonical_f32_bits(font.size.0),
            font_weight: font.weight,
            font_italic: font.italic,
            font_family: font.family.as_deref(),
            font_variant: font.variant.0,
            rtl,
            font_generation,
        }
    }

    /// Converts this borrowed lookup key into an owned key on a cache miss.
    #[inline]
    pub(crate) fn to_owned_key(self, font_family: Option<Arc<str>>) -> TextCacheKey {
        TextCacheKey {
            text: Arc::from(self.text),
            font_size_bits: self.font_size_bits,
            font_weight: self.font_weight,
            font_italic: self.font_italic,
            font_family,
            font_variant: self.font_variant,
            rtl: self.rtl,
            font_generation: self.font_generation,
        }
    }
}

/// Owned cache key stored inside [`ShapedTextCache`].
#[derive(Clone, Debug)]
pub(crate) struct TextCacheKey {
    text: Arc<str>,
    font_size_bits: u32,
    font_weight: u16,
    font_italic: bool,
    font_family: Option<Arc<str>>,
    font_variant: u8,
    rtl: bool,
    font_generation: u64,
}

impl TextCacheKey {
    #[inline]
    fn as_ref(&self) -> TextCacheKeyRef<'_> {
        TextCacheKeyRef {
            text: &self.text,
            font_size_bits: self.font_size_bits,
            font_weight: self.font_weight,
            font_italic: self.font_italic,
            font_family: self.font_family.as_deref(),
            font_variant: self.font_variant,
            rtl: self.rtl,
            font_generation: self.font_generation,
        }
    }
}

/// Trait enabling zero-allocation `FxHashMap` lookups with [`TextCacheKeyRef`].
trait TextKey {
    fn as_key_ref(&self) -> TextCacheKeyRef<'_>;
}

impl TextKey for TextCacheKey {
    #[inline]
    fn as_key_ref(&self) -> TextCacheKeyRef<'_> {
        self.as_ref()
    }
}

impl TextKey for TextCacheKeyRef<'_> {
    #[inline]
    fn as_key_ref(&self) -> TextCacheKeyRef<'_> {
        *self
    }
}

impl PartialEq for dyn TextKey + '_ {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        let a = self.as_key_ref();
        let b = other.as_key_ref();
        a.font_generation == b.font_generation
            && a.font_size_bits == b.font_size_bits
            && a.font_weight == b.font_weight
            && a.font_italic == b.font_italic
            && a.font_variant == b.font_variant
            && a.rtl == b.rtl
            && a.font_family == b.font_family
            && a.text == b.text
    }
}

impl Eq for dyn TextKey + '_ {}

impl Hash for dyn TextKey + '_ {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        let k = self.as_key_ref();
        k.text.hash(state);
        k.font_size_bits.hash(state);
        k.font_weight.hash(state);
        k.font_italic.hash(state);
        k.font_family.hash(state);
        k.font_variant.hash(state);
        k.rtl.hash(state);
        k.font_generation.hash(state);
    }
}

impl PartialEq for TextCacheKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        (self as &dyn TextKey) == (other as &dyn TextKey)
    }
}

impl Eq for TextCacheKey {}

impl Hash for TextCacheKey {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self as &dyn TextKey).hash(state);
    }
}

impl<'a> Borrow<dyn TextKey + 'a> for TextCacheKey {
    #[inline]
    fn borrow(&self) -> &(dyn TextKey + 'a) {
        self
    }
}

struct LruSlot<V> {
    key: TextCacheKey,
    value: V,
    prev: Option<usize>,
    next: Option<usize>,
}

/// Hit/miss diagnostic counters for [`ShapedTextCache`].
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextCacheStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

/// Bounded O(1) LRU cache mapping [`TextCacheKey`] to shaped text entries.
///
/// Cache hits perform zero heap allocations and promote entries in O(1). When
/// the font collection's generation changes, stale entries are discarded
/// immediately so updated typefaces take effect without leaking old glyphs.
pub(crate) struct ShapedTextCache<V> {
    capacity: usize,
    font_generation: u64,
    index: FxHashMap<TextCacheKey, usize>,
    slots: Vec<LruSlot<V>>,
    head: Option<usize>,
    tail: Option<usize>,
    hits: u64,
    misses: u64,
}

impl<V> Default for ShapedTextCache<V> {
    fn default() -> Self {
        Self::new(DEFAULT_TEXT_CACHE_CAPACITY)
    }
}

impl<V> ShapedTextCache<V> {
    /// Creates an empty cache with at least 1 entry of capacity.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            font_generation: 0,
            index: FxHashMap::default(),
            slots: Vec::new(),
            head: None,
            tail: None,
            hits: 0,
            misses: 0,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn stats(&self) -> TextCacheStats {
        TextCacheStats {
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// Drops all cached entries.
    pub(crate) fn clear(&mut self) {
        self.index.clear();
        self.slots.clear();
        self.head = None;
        self.tail = None;
    }

    /// Ensures cached entries belong to `generation`, clearing stale shaped
    /// runs immediately when new custom fonts are registered.
    pub(crate) fn sync_font_generation(&mut self, generation: u64) {
        if self.font_generation != generation {
            self.clear();
            self.font_generation = generation;
        }
    }

    /// Looks up a cached entry by borrowed key, promoting it to most-recently-used
    /// on a hit without allocating.
    #[allow(dead_code)]
    pub(crate) fn get(&mut self, lookup: TextCacheKeyRef<'_>) -> Option<&V> {
        let key: &dyn TextKey = &lookup;
        let idx = *self.index.get(key)?;
        self.hits = self.hits.saturating_add(1);
        self.move_to_front(idx);
        Some(&self.slots[idx].value)
    }

    /// Looks up `lookup` (promoting on hit) or builds and inserts on a miss,
    /// returning the slot index so callers can release the mutable borrow of
    /// the cache before reading via [`Self::entry`].
    pub(crate) fn ensure_with(
        &mut self,
        lookup: TextCacheKeyRef<'_>,
        family: Option<Arc<str>>,
        build: impl FnOnce() -> V,
    ) -> usize {
        self.sync_font_generation(lookup.font_generation);
        let key: &dyn TextKey = &lookup;
        if let Some(&idx) = self.index.get(key) {
            self.hits = self.hits.saturating_add(1);
            self.move_to_front(idx);
            return idx;
        }
        self.misses = self.misses.saturating_add(1);
        let value = build();
        let owned_key = lookup.to_owned_key(family);
        self.insert_owned(owned_key, value)
    }

    /// Returns the cached value at slot index `idx` returned by [`Self::ensure_with`].
    #[inline]
    pub(crate) fn entry(&self, idx: usize) -> &V {
        &self.slots[idx].value
    }

    /// Looks up `lookup`, or invokes `build` on a miss, inserting the newly
    /// constructed value and evicting the least-recently-used entry if full.
    #[allow(dead_code)]
    pub(crate) fn get_or_insert_with(
        &mut self,
        lookup: TextCacheKeyRef<'_>,
        family: Option<Arc<str>>,
        build: impl FnOnce() -> V,
    ) -> &V {
        let idx = self.ensure_with(lookup, family, build);
        self.entry(idx)
    }

    fn insert_owned(&mut self, key: TextCacheKey, value: V) -> usize {
        if let Some(&idx) = self.index.get(&key) {
            self.slots[idx].value = value;
            self.move_to_front(idx);
            return idx;
        }
        if self.slots.len() >= self.capacity {
            let evict_idx = self.tail.expect("non-empty LRU cache has a tail");
            self.detach(evict_idx);
            let old_key = std::mem::replace(&mut self.slots[evict_idx].key, key.clone());
            self.index.remove(&old_key);
            self.slots[evict_idx].value = value;
            self.index.insert(key, evict_idx);
            self.attach_front(evict_idx);
            evict_idx
        } else {
            let idx = self.slots.len();
            self.slots.push(LruSlot {
                key: key.clone(),
                value,
                prev: None,
                next: None,
            });
            self.index.insert(key, idx);
            self.attach_front(idx);
            idx
        }
    }

    fn detach(&mut self, idx: usize) {
        let prev = self.slots[idx].prev;
        let next = self.slots[idx].next;
        match prev {
            Some(p) => self.slots[p].next = next,
            None => self.head = next,
        }
        match next {
            Some(n) => self.slots[n].prev = prev,
            None => self.tail = prev,
        }
        self.slots[idx].prev = None;
        self.slots[idx].next = None;
    }

    fn attach_front(&mut self, idx: usize) {
        self.slots[idx].prev = None;
        self.slots[idx].next = self.head;
        match self.head {
            Some(old_head) => self.slots[old_head].prev = Some(idx),
            None => self.tail = Some(idx),
        }
        self.head = Some(idx);
    }

    fn move_to_front(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        self.detach(idx);
        self.attach_front(idx);
    }
}

/// Tracks whether a `CanvasView` is receiving sustained redraws (e.g. terminal
/// streaming, live charts, interactive drawing) so it votes for the display's
/// high refresh rate (e.g. 120 Hz) while active and automatically releases the
/// vote when idle.
#[derive(Debug)]
pub(crate) struct CanvasActivity {
    burst_window: Duration,
    idle_hold: Duration,
    last_redraw: Option<Instant>,
    active: bool,
}

impl Default for CanvasActivity {
    fn default() -> Self {
        Self::new(CANVAS_BURST_WINDOW, CANVAS_IDLE_HOLD)
    }
}

impl CanvasActivity {
    /// Creates an idle tracker with custom burst and idle-hold thresholds.
    pub(crate) fn new(burst_window: Duration, idle_hold: Duration) -> Self {
        Self {
            burst_window,
            idle_hold,
            last_redraw: None,
            active: false,
        }
    }

    /// Returns whether this canvas currently holds a high-refresh vote.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Records a `CanvasView::set()` redraw at `now`.
    ///
    /// Returns `true` on the transition from idle → active (when two redraws
    /// arrive within `burst_window`), signaling the caller to engage its
    /// `FrameRate::Vote`.
    pub(crate) fn record_redraw(&mut self, now: Instant) -> bool {
        let prev = self.last_redraw.replace(now);
        if self.active {
            return false;
        }
        if let Some(prev) = prev
            && now.saturating_duration_since(prev) <= self.burst_window
        {
            self.active = true;
            return true;
        }
        false
    }

    /// Checks on a frame tick whether the active vote should remain held.
    ///
    /// Returns `true` while still active; returns `false` when `idle_hold` has
    /// elapsed since the last redraw (transitioning back to idle so the caller
    /// releases its `FrameRate::Vote`).
    pub(crate) fn check_idle(&mut self, now: Instant) -> bool {
        if !self.active {
            return false;
        }
        let expired = self
            .last_redraw
            .is_none_or(|last| now.saturating_duration_since(last) >= self.idle_hold);
        if expired {
            self.active = false;
            self.last_redraw = None;
            return false;
        }
        true
    }

    /// Immediately resets to idle (e.g. on backgrounding or node deactivation).
    /// Returns `true` if a high-refresh vote was previously active.
    pub(crate) fn reset(&mut self) -> bool {
        self.last_redraw = None;
        std::mem::replace(&mut self.active, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::{FontVariant, Points};

    fn font(size: f32, weight: u16, italic: bool, family: Option<&str>, variant: u8) -> CanvasFont {
        CanvasFont {
            size: Points(size),
            weight,
            italic,
            family: family.map(Arc::from),
            variant: FontVariant(variant),
        }
    }

    #[test]
    fn terminal_streaming_achieves_high_cross_frame_hit_rate() {
        let mut cache = ShapedTextCache::<usize>::new(512);
        let mono = font(14.0, 400, false, Some("JetBrains Mono"), 0);
        let mut build_calls = 0usize;

        // Frame 1: 40 unique terminal rows -> 40 misses.
        let mut rows: Vec<String> = (0..40)
            .map(|r| format!("row {r:02}: prompt$ cargo build --release"))
            .collect();
        for row in &rows {
            let key = TextCacheKeyRef::new(row, &mono, false, 1);
            cache.get_or_insert_with(key, mono.family.clone(), || {
                build_calls += 1;
                build_calls
            });
        }
        assert_eq!(cache.len(), 40);
        assert_eq!(build_calls, 40);

        // Frames 2..=11: only 1 row changes per frame -> 39/40 cache hits per frame!
        for frame in 1..=10 {
            rows[39] = format!("row 39: progress {frame}/10 [####]");
            for row in &rows {
                let key = TextCacheKeyRef::new(row, &mono, false, 1);
                cache.get_or_insert_with(key, mono.family.clone(), || {
                    build_calls += 1;
                    build_calls
                });
            }
        }

        // 40 initial + 10 changed rows = 50 shapes total out of 440 text draws.
        assert_eq!(build_calls, 50);
        let stats = cache.stats();
        assert_eq!(stats.misses, 50);
        assert_eq!(stats.hits, 390);
        assert_eq!(cache.len(), 50);
    }

    #[test]
    fn lru_evicts_least_recently_used_and_respects_capacity_bound() {
        let mut cache = ShapedTextCache::<u32>::new(3);
        let f = font(12.0, 400, false, None, 0);

        for (i, text) in ["a", "b", "c"].into_iter().enumerate() {
            let key = TextCacheKeyRef::new(text, &f, false, 1);
            cache.get_or_insert_with(key, None, || i as u32);
        }
        assert_eq!(cache.len(), 3);

        // Touch "a" so "b" becomes the least recently used entry.
        assert_eq!(cache.get(TextCacheKeyRef::new("a", &f, false, 1)), Some(&0));

        // Inserting "d" must evict "b", keeping "a", "c", "d".
        cache.get_or_insert_with(TextCacheKeyRef::new("d", &f, false, 1), None, || 3);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get(TextCacheKeyRef::new("b", &f, false, 1)), None);
        assert_eq!(cache.get(TextCacheKeyRef::new("a", &f, false, 1)), Some(&0));
        assert_eq!(cache.get(TextCacheKeyRef::new("c", &f, false, 1)), Some(&2));
        assert_eq!(cache.get(TextCacheKeyRef::new("d", &f, false, 1)), Some(&3));

        // Filling well past capacity never exceeds the configured bound.
        for i in 0..100 {
            let s = format!("line-{i}");
            cache.get_or_insert_with(TextCacheKeyRef::new(&s, &f, false, 1), None, || i);
            assert!(cache.len() <= 3);
        }
    }

    #[test]
    fn key_discriminates_all_shaping_parameters_and_clears_on_font_generation() {
        let mut cache = ShapedTextCache::<u32>::new(64);
        let base = font(14.0, 400, false, Some("Inter"), 0);
        let variants = [
            ("hello", base.clone(), false, 1),
            ("world", base.clone(), false, 2),
            ("hello", font(16.0, 400, false, Some("Inter"), 0), false, 3),
            ("hello", font(14.0, 700, false, Some("Inter"), 0), false, 4),
            ("hello", font(14.0, 400, true, Some("Inter"), 0), false, 5),
            ("hello", font(14.0, 400, false, Some("Menlo"), 0), false, 6),
            ("hello", font(14.0, 400, false, None, 0), false, 7),
            (
                "hello",
                font(14.0, 400, false, Some("Inter"), FontVariant::TABULAR_NUMS),
                false,
                8,
            ),
            ("hello", base.clone(), true, 9),
        ];

        for (text, f, rtl, id) in &variants {
            let key = TextCacheKeyRef::new(text, f, *rtl, 1);
            let got = *cache.get_or_insert_with(key, f.family.clone(), || *id);
            assert_eq!(got, *id);
        }
        assert_eq!(cache.len(), variants.len());

        // Re-querying every variant returns its exact distinct value with 0 misses.
        let misses_before = cache.stats().misses;
        for (text, f, rtl, id) in &variants {
            let key = TextCacheKeyRef::new(text, f, *rtl, 1);
            assert_eq!(cache.get(key), Some(id));
        }
        assert_eq!(cache.stats().misses, misses_before);

        // -0.0 and +0.0 font sizes canonicalize identically.
        let neg_zero = font(-0.0, 400, false, None, 0);
        let pos_zero = font(0.0, 400, false, None, 0);
        cache.get_or_insert_with(TextCacheKeyRef::new("z", &neg_zero, false, 1), None, || 99);
        assert_eq!(
            cache.get(TextCacheKeyRef::new("z", &pos_zero, false, 1)),
            Some(&99)
        );

        // Advancing font generation drops stale entries immediately.
        let got = *cache.get_or_insert_with(
            TextCacheKeyRef::new("hello", &base, false, 2),
            base.family.clone(),
            || 777,
        );
        assert_eq!(got, 777);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn canvas_activity_ignores_static_and_slow_blink_but_engages_120hz_for_streaming() {
        let t0 = Instant::now();
        let mut activity = CanvasActivity::default();
        assert!(!activity.is_active());

        // Single initial draw does not request 120 Hz.
        assert!(!activity.record_redraw(t0));
        assert!(!activity.is_active());

        // Slow 530 ms cursor blink does not trigger 120 Hz.
        assert!(!activity.record_redraw(t0 + Duration::from_millis(530)));
        assert!(!activity.is_active());
        assert!(!activity.record_redraw(t0 + Duration::from_millis(1060)));
        assert!(!activity.is_active());

        // Streaming frames every 16 ms engage 120 Hz on the second consecutive frame.
        let stream_start = t0 + Duration::from_millis(2000);
        assert!(!activity.record_redraw(stream_start));
        assert!(activity.record_redraw(stream_start + Duration::from_millis(16)));
        assert!(activity.is_active());

        // Subsequent 8 ms frames stay active without re-firing transition.
        let mut t = stream_start + Duration::from_millis(16);
        for _ in 0..30 {
            t += Duration::from_millis(8);
            assert!(!activity.record_redraw(t));
            assert!(activity.check_idle(t));
        }

        // Still active shortly after streaming pauses (< 200 ms hold).
        assert!(activity.check_idle(t + Duration::from_millis(150)));
        assert!(activity.is_active());

        // Releases automatically once idle hold (200 ms) elapses.
        assert!(!activity.check_idle(t + Duration::from_millis(200)));
        assert!(!activity.is_active());

        // Reset immediately releases an active vote (e.g. backgrounding).
        let t2 = t + Duration::from_millis(500);
        assert!(!activity.record_redraw(t2));
        assert!(activity.record_redraw(t2 + Duration::from_millis(10)));
        assert!(activity.reset());
        assert!(!activity.is_active());
        assert!(!activity.reset());
    }
}
