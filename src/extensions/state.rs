//! Pure-Rust state machines, wire types and image-sizing helpers for HarmonyOS
//! `nshm` extension points. Host unit tests exercise this module directly.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use nsh_core::picker::{PickFormat, PickRequest};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Maximum image input/output size handled by [`crate::prepare_image`] and
/// [`crate::prepare_image_bytes`] (64 MiB, matching `Picker.pickImages`).
pub(crate) const MAX_IMAGE_BYTES: usize = 64 << 20;

/// Maximum buffered `onNewWant` events retained before the first observer registers.
pub(crate) const MAX_PENDING_WANTS: usize = 16;

/// Default JPEG encoding quality (0..=100) when a caller does not specify one.
pub(crate) const DEFAULT_JPEG_QUALITY: u32 = 85;

/// Why a HarmonyOS UI-thread extension call could not run or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtensionError {
    /// No `NshSurface` session has been created yet.
    NotStarted,
    /// No active UI session is available, or the session closed before the task ran.
    NoSession(String),
    /// The ArkTS / N-API callback returned an error.
    Failed(String),
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => f.write_str("no NshSurface session has been created yet"),
            Self::NoSession(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ExtensionError {}

/// A HarmonyOS `Want` passed to [`crate::ability::start_for_result`] or
/// delivered to [`crate::ability::on_want`] / [`crate::launch_want`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Want {
    /// Target application bundle name.
    #[serde(rename = "bundleName", skip_serializing_if = "Option::is_none")]
    pub bundle_name: Option<String>,
    /// Target HAP module name.
    #[serde(rename = "moduleName", skip_serializing_if = "Option::is_none")]
    pub module_name: Option<String>,
    /// Target UIAbility name.
    #[serde(rename = "abilityName", skip_serializing_if = "Option::is_none")]
    pub ability_name: Option<String>,
    /// Implicit or explicit action (e.g. `"ohos.want.action.viewData"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Target URI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// MIME type (`Want.type`).
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Target entities (`Want.entities`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    /// JSON-compatible entries of `Want.parameters`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub parameters: Map<String, Value>,
}

/// The result answered by `UIAbilityContext.startAbilityForResult`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AbilityResult {
    /// Result code returned by the started Ability (`0` is [`crate::ability::RESULT_OK`]).
    #[serde(rename = "resultCode", default)]
    pub result_code: i32,
    /// Optional result `Want` carrying returned parameters or URI.
    #[serde(default)]
    pub want: Option<Want>,
}

/// Why [`crate::ability::start_for_result`] could not start or complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbilityError {
    /// No `NshSurface` session has been created yet.
    NotStarted,
    /// No foreground Ability is available, or the session closed before a result arrived.
    NoAbility(String),
    /// Starting the Ability failed; `code` carries the HarmonyOS `BusinessError.code` when present.
    Failed {
        /// HarmonyOS `BusinessError.code` when reported by ArkTS.
        code: Option<i32>,
        /// Error description returned by ArkTS or serialization.
        message: String,
    },
}

impl fmt::Display for AbilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotStarted => f.write_str("no NshSurface session has been created yet"),
            Self::NoAbility(why) => f.write_str(why),
            Self::Failed {
                code: Some(code),
                message,
            } => write!(f, "{message} (code {code})"),
            Self::Failed {
                code: None,
                message,
            } => f.write_str(message),
        }
    }
}

impl std::error::Error for AbilityError {}

/// Which `UIAbility` lifecycle callback delivered a [`Want`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbilityWantKind {
    /// Cold launch (`UIAbility.onCreate`).
    Create,
    /// Warm re-entry (`UIAbility.onNewWant`).
    NewWant,
}

impl AbilityWantKind {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "create" => Some(Self::Create),
            "new-want" => Some(Self::NewWant),
            _ => None,
        }
    }
}

/// A `Want` delivered to the host `UIAbility`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AbilityWantEvent {
    /// Whether this `Want` arrived in `onCreate` or `onNewWant`.
    pub kind: AbilityWantKind,
    /// Sanitized `Want` payload.
    pub want: Want,
}

type WantObserver = Arc<dyn Fn(&AbilityWantEvent) + Send + Sync>;

struct WantHubInner {
    next_id: u64,
    launch: Option<Want>,
    pending_new_wants: VecDeque<AbilityWantEvent>,
    observers: Vec<(u64, WantObserver)>,
}

/// Process-wide hub for `UIAbility` `onCreate` / `onNewWant` events.
pub(crate) struct WantHub {
    inner: Mutex<WantHubInner>,
}

impl WantHub {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(WantHubInner {
                next_id: 1,
                launch: None,
                pending_new_wants: VecDeque::new(),
                observers: Vec::new(),
            }),
        }
    }

    pub(crate) fn launch_want(&self) -> Option<Want> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .launch
            .clone()
    }

    pub(crate) fn deliver(&self, kind: AbilityWantKind, want: Want) {
        let event = AbilityWantEvent { kind, want };
        let observers: Vec<WantObserver> = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if kind == AbilityWantKind::Create {
                inner.launch = Some(event.want.clone());
            }
            if inner.observers.is_empty() {
                if kind == AbilityWantKind::NewWant {
                    if inner.pending_new_wants.len() >= MAX_PENDING_WANTS {
                        inner.pending_new_wants.pop_front();
                    }
                    inner.pending_new_wants.push_back(event);
                }
                return;
            }
            inner.observers.iter().map(|(_, cb)| cb.clone()).collect()
        };
        for observer in observers {
            observer(&event);
        }
    }

    pub(crate) fn subscribe(&self, observer: WantObserver) -> (u64, Vec<AbilityWantEvent>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let was_empty = inner.observers.is_empty();
        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        inner.observers.push((id, observer));
        let replay = if was_empty {
            inner.pending_new_wants.drain(..).collect()
        } else {
            Vec::new()
        };
        (id, replay)
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.observers.retain(|(entry_id, _)| *entry_id != id);
    }
}

/// Subscription handle returned by [`crate::on_ability_want`]. Dropping it
/// unregisters the observer; call [`Self::detach`] to keep it active for the
/// lifetime of the process (the usual choice inside `#[native_module]`).
pub struct WantObserverGuard {
    unsubscribe: Option<Box<dyn FnOnce() + Send>>,
}

impl WantObserverGuard {
    pub(crate) fn new(unsubscribe: impl FnOnce() + Send + 'static) -> Self {
        Self {
            unsubscribe: Some(Box::new(unsubscribe)),
        }
    }

    /// Keeps this observer registered for the lifetime of the process.
    pub fn detach(mut self) {
        self.unsubscribe = None;
    }
}

impl Drop for WantObserverGuard {
    fn drop(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
    }
}

impl fmt::Debug for WantObserverGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WantObserverGuard")
            .field("active", &self.unsubscribe.is_some())
            .finish()
    }
}

struct OnceState<T> {
    value: Option<T>,
    closed: bool,
    waker: Option<Waker>,
}

/// One-shot sender paired with [`OnceFuture`].
pub(crate) struct OnceSender<T> {
    state: Arc<Mutex<OnceState<T>>>,
}

impl<T> OnceSender<T> {
    pub(crate) fn send(self, value: T) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Drop for OnceSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.value.is_none() {
                state.closed = true;
                state.waker.take()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Executor-agnostic one-shot future used by extension calls.
pub(crate) struct OnceFuture<T> {
    state: Arc<Mutex<OnceState<T>>>,
    on_closed: Option<fn() -> T>,
}

impl<T> Unpin for OnceFuture<T> {}

impl<T> OnceFuture<T> {
    pub(crate) fn channel(on_closed: fn() -> T) -> (OnceSender<T>, Self) {
        let state = Arc::new(Mutex::new(OnceState {
            value: None,
            closed: false,
            waker: None,
        }));
        (
            OnceSender {
                state: state.clone(),
            },
            Self {
                state,
                on_closed: Some(on_closed),
            },
        )
    }

    pub(crate) fn ready(value: T) -> Self {
        Self {
            state: Arc::new(Mutex::new(OnceState {
                value: Some(value),
                closed: false,
                waker: None,
            })),
            on_closed: None,
        }
    }
}

impl<T> Future for OnceFuture<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = state.value.take() {
            return Poll::Ready(value);
        }
        if state.closed {
            drop(state);
            let fallback = self
                .on_closed
                .take()
                .expect("OnceFuture polled after completion");
            return Poll::Ready(fallback());
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Session-scoped table of in-flight async requests so closing a `HostSession`
/// immediately fails and wakes every pending waiter.
pub(crate) struct PendingTable<T> {
    next_id: u32,
    entries: FxHashMap<u32, (u32, T)>,
}

impl<T> Default for PendingTable<T> {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: FxHashMap::default(),
        }
    }
}

impl<T> PendingTable<T> {
    pub(crate) fn insert(&mut self, session_id: u32, value: T) -> Option<u32> {
        // Guard against wraparound collisions if a long-lived session cycles u32.
        for _ in 0..1024 {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if let std::collections::hash_map::Entry::Vacant(entry) = self.entries.entry(id) {
                entry.insert((session_id, value));
                return Some(id);
            }
        }
        None
    }

    pub(crate) fn take(&mut self, id: u32) -> Option<T> {
        self.entries.remove(&id).map(|(_, value)| value)
    }

    pub(crate) fn drain_session(&mut self, session_id: u32) -> Vec<T> {
        let keys: Vec<u32> = self
            .entries
            .iter()
            .filter_map(|(&id, (sid, _))| (*sid == session_id).then_some(id))
            .collect();
        let mut drained = Vec::with_capacity(keys.len());
        for id in keys {
            if let Some((_, value)) = self.entries.remove(&id) {
                drained.push(value);
            }
        }
        drained
    }
}

/// Aspect-preserving target dimensions when `max(width, height)` exceeds
/// `max_dimension`; returns `None` when no downscaling is needed.
pub(crate) fn target_dimensions(
    width: u32,
    height: u32,
    max_dimension: Option<i32>,
) -> Option<(u32, u32)> {
    let limit = max_dimension.filter(|&d| d > 0)? as u32;
    let longest = width.max(height);
    if width == 0 || height == 0 || longest <= limit {
        return None;
    }
    let scale = f64::from(limit) / f64::from(longest);
    let target_w = ((f64::from(width) * scale).round() as u32).clamp(1, limit);
    let target_h = ((f64::from(height) * scale).round() as u32).clamp(1, limit);
    Some((target_w, target_h))
}

/// Whether `prepare_image*` should decode and re-encode `bytes` through the
/// native OHOS image pipeline (to downscale, convert HEIC/TIFF/BMP to JPEG,
/// apply quality, or strip camera metadata).
pub(crate) fn image_needs_encode(
    source_mime: &str,
    width: u32,
    height: u32,
    request: &PickRequest,
) -> bool {
    if target_dimensions(width, height, request.max_dimension.map(|d| d.0)).is_some() {
        return true;
    }
    if request.quality.is_some() {
        return true;
    }
    matches!(request.format, PickFormat::Compatible)
        && !matches!(
            source_mime,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        )
}

/// Output MIME type and integer quality (`1..=100`) for OHOS `ImagePacker`.
pub(crate) fn encode_mime_and_quality(
    source_mime: &str,
    request: &PickRequest,
) -> (&'static str, u32) {
    if source_mime == "image/png" && request.quality.is_none() {
        return ("image/png", 100);
    }
    let quality = request
        .quality
        .map(|q| ((q.0.clamp(0.0, 1.0) * 100.0).round() as u32).clamp(1, 100))
        .unwrap_or(DEFAULT_JPEG_QUALITY);
    ("image/jpeg", quality)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable, Waker},
    };

    use nsh_core::geometry::{DevicePixels, Percentage};
    use serde_json::json;

    use super::*;

    fn counting_waker(hits: Arc<AtomicUsize>) -> Waker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            let cloned = arc.clone();
            std::mem::forget(arc);
            RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
        }
        unsafe fn wake(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            arc.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn wake_by_ref(ptr: *const ()) {
            let arc = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
            arc.fetch_add(1, Ordering::SeqCst);
            std::mem::forget(arc);
        }
        unsafe fn drop_waker(ptr: *const ()) {
            let _ = unsafe { Arc::from_raw(ptr.cast::<AtomicUsize>()) };
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
        // SAFETY: VTABLE upholds all RawWaker contracts for Arc<AtomicUsize>.
        unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(hits).cast(), &VTABLE)) }
    }

    #[test]
    fn once_future_delivers_value_and_recovers_if_sender_drops() {
        let hits = Arc::new(AtomicUsize::new(0));
        let waker = counting_waker(hits.clone());
        let mut cx = Context::from_waker(&waker);

        let (tx, mut fut) = OnceFuture::channel(|| Err("closed"));
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending);
        tx.send(Ok(42));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(Ok(42)));

        let (tx2, mut fut2) = OnceFuture::<Result<i32, &str>>::channel(|| Err("dropped"));
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Pending);
        drop(tx2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(
            Pin::new(&mut fut2).poll(&mut cx),
            Poll::Ready(Err("dropped"))
        );

        let mut ready = OnceFuture::ready(99);
        assert_eq!(Pin::new(&mut ready).poll(&mut cx), Poll::Ready(99));
    }

    #[test]
    fn want_hub_keeps_launch_want_buffers_new_wants_and_supports_reentrancy() {
        let hub = Arc::new(WantHub::new());
        let create_want = Want {
            uri: Some("myapp://launch".into()),
            ..Default::default()
        };
        hub.deliver(AbilityWantKind::Create, create_want.clone());
        assert_eq!(hub.launch_want(), Some(create_want));

        // Deliver more than MAX_PENDING_WANTS NewWant events before any observer subscribes.
        for i in 0..(MAX_PENDING_WANTS + 4) {
            hub.deliver(
                AbilityWantKind::NewWant,
                Want {
                    uri: Some(format!("myapp://item/{i}")),
                    ..Default::default()
                },
            );
        }

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_a = seen.clone();
        let hub_a = hub.clone();
        let obs_a: WantObserver = Arc::new(move |event| {
            // Re-entering launch_want() inside observer must not deadlock.
            assert!(hub_a.launch_want().is_some());
            seen_a
                .lock()
                .unwrap()
                .push(event.want.uri.clone().unwrap_or_default());
        });
        let (id_a, replay) = hub.subscribe(obs_a.clone());
        for ev in &replay {
            obs_a(ev);
        }
        assert_eq!(replay.len(), MAX_PENDING_WANTS);
        assert_eq!(
            seen.lock().unwrap().first().map(String::as_str),
            Some("myapp://item/4")
        );
        assert_eq!(
            seen.lock().unwrap().last().map(String::as_str),
            Some("myapp://item/19")
        );

        let seen_b = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_b_cb = seen_b.clone();
        let (id_b, replay_b) = hub.subscribe(Arc::new(move |event| {
            seen_b_cb
                .lock()
                .unwrap()
                .push(event.want.uri.clone().unwrap_or_default());
        }));
        assert!(replay_b.is_empty());

        hub.deliver(
            AbilityWantKind::NewWant,
            Want {
                uri: Some("myapp://live".into()),
                ..Default::default()
            },
        );
        assert_eq!(seen.lock().unwrap().last().unwrap(), "myapp://live");
        assert_eq!(*seen_b.lock().unwrap(), vec!["myapp://live"]);

        hub.unsubscribe(id_a);
        hub.deliver(
            AbilityWantKind::NewWant,
            Want {
                uri: Some("myapp://after".into()),
                ..Default::default()
            },
        );
        assert_eq!(seen.lock().unwrap().last().unwrap(), "myapp://live");
        assert_eq!(
            *seen_b.lock().unwrap(),
            vec!["myapp://live", "myapp://after"]
        );
        hub.unsubscribe(id_b);
    }

    #[test]
    fn want_observer_guard_unsubscribes_on_drop_unless_detached() {
        let dropped = Arc::new(AtomicUsize::new(0));
        {
            let d = dropped.clone();
            let _guard = WantObserverGuard::new(move || {
                d.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        {
            let d = dropped.clone();
            let guard = WantObserverGuard::new(move || {
                d.fetch_add(1, Ordering::SeqCst);
            });
            guard.detach();
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pending_table_drains_only_closed_session() {
        let mut table = PendingTable::default();
        let a = table.insert(1, "s1-a").unwrap();
        let b = table.insert(1, "s1-b").unwrap();
        let c = table.insert(2, "s2-c").unwrap();
        assert_eq!(table.take(a), Some("s1-a"));
        let drained = table.drain_session(1);
        assert_eq!(drained, vec!["s1-b"]);
        assert_eq!(table.take(b), None);
        assert_eq!(table.take(c), Some("s2-c"));
    }

    #[test]
    fn want_and_ability_result_roundtrip_json() {
        let mut want = Want {
            bundle_name: Some("com.example.cam".into()),
            ability_name: Some("EntryAbility".into()),
            action: Some("ohos.want.action.imageCapture".into()),
            ..Default::default()
        };
        want.parameters
            .insert("supportMultiMode".into(), json!(false));

        let serialized = serde_json::to_value(&want).unwrap();
        assert_eq!(serialized["bundleName"], "com.example.cam");
        assert_eq!(serialized["abilityName"], "EntryAbility");
        assert_eq!(serialized["action"], "ohos.want.action.imageCapture");
        assert_eq!(serialized["parameters"]["supportMultiMode"], false);
        assert!(serialized.get("uri").is_none());

        let result: AbilityResult = serde_json::from_value(json!({
            "resultCode": 0,
            "want": {
                "uri": "file://media/Photo/1/IMG_001.jpg",
                "parameters": { "resourceUri": "file://media/Photo/1/IMG_001.jpg" }
            }
        }))
        .unwrap();
        assert_eq!(result.result_code, 0);
        let returned = result.want.unwrap();
        assert_eq!(
            returned.uri.as_deref(),
            Some("file://media/Photo/1/IMG_001.jpg")
        );
        assert_eq!(
            returned.parameters["resourceUri"],
            "file://media/Photo/1/IMG_001.jpg"
        );
    }

    #[test]
    fn image_preparation_sizing_and_encoding_decisions() {
        assert_eq!(
            target_dimensions(4000, 3000, Some(2000)),
            Some((2000, 1500))
        );
        assert_eq!(target_dimensions(1500, 3000, Some(1500)), Some((750, 1500)));
        assert_eq!(target_dimensions(800, 600, Some(1024)), None);
        assert_eq!(target_dimensions(4000, 3000, None), None);
        assert_eq!(target_dimensions(4000, 1, Some(100)), Some((100, 1)));

        let mut req = PickRequest::new(false, 1);
        assert!(!image_needs_encode("image/jpeg", 800, 600, &req));
        assert!(image_needs_encode("image/heic", 800, 600, &req));

        req.format = PickFormat::Current;
        assert!(!image_needs_encode("image/heic", 800, 600, &req));

        req.max_dimension = Some(DevicePixels(512));
        assert!(image_needs_encode("image/png", 800, 600, &req));
        assert_eq!(
            encode_mime_and_quality("image/png", &req),
            ("image/png", 100)
        );

        req.quality = Some(Percentage(0.75));
        assert_eq!(
            encode_mime_and_quality("image/png", &req),
            ("image/jpeg", 75)
        );
        assert_eq!(
            encode_mime_and_quality("image/heic", &req),
            ("image/jpeg", 75)
        );
    }
}
