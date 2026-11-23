//! Public extension points for HarmonyOS `nshm` crates and app Rust code.
//!
//! Mirrors the generic hooks `nsh-platform-ios` (`presenting_controller`,
//! `on_application_event`, `prepare_image`) and `nsh-platform-android`
//! (`with_env`, `activity::start_for_result`, `picker::prepare_image`) expose,
//! keeping platform-specific capabilities out of the core runtime.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use nsh_core::{
    geometry::{DevicePixels, Size},
    picker::{Encoding, PickRequest},
};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_IMAGE_BYTES: usize = 64 << 20;
const MAX_IMAGE_PIXELS: u64 = 64_000_000;
const MAX_PENDING_WANTS: usize = 16;

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
    /// Ability launch flags (`Want.flags`, e.g. `wantConstant.Flags`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,
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
    fn parse(raw: &str) -> Option<Self> {
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

struct ObserverSlot {
    id: u64,
    callback: WantObserver,
    /// Pending events (buffered replay followed by live deliveries) drained in
    /// strict FIFO order by one thread at a time so live wants never overtake
    /// replayed buffered wants.
    queue: Mutex<(VecDeque<AbilityWantEvent>, bool)>,
}

struct DrainReset<'a>(&'a Mutex<(VecDeque<AbilityWantEvent>, bool)>);

impl Drop for DrainReset<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
    }
}

impl ObserverSlot {
    fn new(id: u64, callback: WantObserver, initial: VecDeque<AbilityWantEvent>) -> Self {
        Self {
            id,
            callback,
            queue: Mutex::new((initial, false)),
        }
    }

    fn enqueue(&self, event: AbilityWantEvent) {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .push_back(event);
        self.drain();
    }

    fn drain(&self) {
        loop {
            let next = {
                let mut state = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                if state.1 {
                    return;
                }
                let Some(event) = state.0.pop_front() else {
                    return;
                };
                state.1 = true;
                event
            };
            let reset = DrainReset(&self.queue);
            (self.callback)(&next);
            drop(reset);
        }
    }
}

struct WantHubInner {
    next_id: u64,
    launch: Option<Want>,
    pending_new_wants: VecDeque<AbilityWantEvent>,
    observers: Vec<Arc<ObserverSlot>>,
}

struct WantHub(Mutex<WantHubInner>);

impl WantHub {
    fn new() -> Self {
        Self(Mutex::new(WantHubInner {
            next_id: 1,
            launch: None,
            pending_new_wants: VecDeque::new(),
            observers: Vec::new(),
        }))
    }

    fn launch_want(&self) -> Option<Want> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .launch
            .clone()
    }

    fn deliver(&self, kind: AbilityWantKind, want: Want) {
        let event = AbilityWantEvent { kind, want };
        let slots: Vec<Arc<ObserverSlot>> = {
            let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
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
            inner.observers.clone()
        };
        for slot in slots {
            slot.enqueue(event.clone());
        }
    }

    fn subscribe(&self, observer: WantObserver) -> u64 {
        let slot = {
            let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
            let replay = if inner.observers.is_empty() {
                std::mem::take(&mut inner.pending_new_wants)
            } else {
                VecDeque::new()
            };
            let id = inner.next_id;
            inner.next_id = inner.next_id.wrapping_add(1).max(1);
            let slot = Arc::new(ObserverSlot::new(id, observer, replay));
            inner.observers.push(slot.clone());
            slot
        };
        slot.drain();
        slot.id
    }

    fn unsubscribe(&self, id: u64) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observers
            .retain(|slot| slot.id != id);
    }
}

/// Subscription handle returned by [`crate::on_ability_want`]. Dropping it
/// unregisters the observer; call [`Self::detach`] to keep it active for the
/// lifetime of the process (the usual choice inside `#[native_module]`).
pub struct WantObserverGuard {
    unsubscribe: Option<Box<dyn FnOnce() + Send>>,
}

impl WantObserverGuard {
    fn new(unsubscribe: impl FnOnce() + Send + 'static) -> Self {
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

struct OnceSender<T>(Arc<Mutex<OnceState<T>>>);

impl<T> OnceSender<T> {
    fn send(self, value: T) {
        let waker = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            s.value = Some(value);
            s.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Drop for OnceSender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if s.value.is_none() {
                s.closed = true;
                s.waker.take()
            } else {
                None
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

struct OnceFuture<T> {
    state: Arc<Mutex<OnceState<T>>>,
    on_closed: Option<fn() -> T>,
}

impl<T> Unpin for OnceFuture<T> {}

impl<T> OnceFuture<T> {
    fn channel(on_closed: fn() -> T) -> (OnceSender<T>, Self) {
        let state = Arc::new(Mutex::new(OnceState {
            value: None,
            closed: false,
            waker: None,
        }));
        (
            OnceSender(state.clone()),
            Self {
                state,
                on_closed: Some(on_closed),
            },
        )
    }

    fn ready(value: T) -> Self {
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
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = s.value.take() {
            return Poll::Ready(v);
        }
        if s.closed {
            drop(s);
            return Poll::Ready(self
                .on_closed
                .take()
                .expect("OnceFuture polled after completion")(
            ));
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

struct PendingTable<T> {
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
    fn insert(&mut self, session_id: u32, value: T) -> Option<u32> {
        for _ in 0..1024 {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if let std::collections::hash_map::Entry::Vacant(e) = self.entries.entry(id) {
                e.insert((session_id, value));
                return Some(id);
            }
        }
        None
    }

    fn take(&mut self, id: u32) -> Option<T> {
        self.entries.remove(&id).map(|(_, value)| value)
    }

    fn drain_session(&mut self, session_id: u32) -> Vec<T> {
        let keys: Vec<u32> = self
            .entries
            .iter()
            .filter_map(|(&id, (sid, _))| (*sid == session_id).then_some(id))
            .collect();
        keys.into_iter()
            .filter_map(|id| self.entries.remove(&id).map(|(_, v)| v))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ExifOrientation {
    #[default]
    Normal,
    FlipHorizontal,
    Rotate180,
    FlipVertical,
    Transpose,
    Rotate90,
    Transverse,
    Rotate270,
}

impl ExifOrientation {
    fn from_tag(tag: u16) -> Option<Self> {
        Some(match tag {
            1 => Self::Normal,
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            _ => return None,
        })
    }

    fn parse_property(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if let Ok(tag) = trimmed.parse::<u16>() {
            return Self::from_tag(tag);
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "top-left" | "horizontal" | "normal" => Some(Self::Normal),
            "top-right" | "mirror horizontal" => Some(Self::FlipHorizontal),
            "bottom-right" | "rotate 180" => Some(Self::Rotate180),
            "bottom-left" | "mirror vertical" => Some(Self::FlipVertical),
            "left-top" | "mirror horizontal and rotate 270 cw" => Some(Self::Transpose),
            "right-top" | "rotate 90 cw" => Some(Self::Rotate90),
            "right-bottom" | "mirror horizontal and rotate 90 cw" => Some(Self::Transverse),
            "left-bottom" | "rotate 270 cw" => Some(Self::Rotate270),
            _ => None,
        }
    }

    fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Rotate90 | Self::Transverse | Self::Rotate270
        )
    }

    fn display_dimensions(self, raw_width: u32, raw_height: u32) -> (u32, u32) {
        if self.swaps_axes() {
            (raw_height, raw_width)
        } else {
            (raw_width, raw_height)
        }
    }
}

/// Parses EXIF Orientation (TIFF IFD0 tag `0x0112`) from a JPEG APP1 segment.
/// Used as a fallback when `ImageSource::get_image_property("Orientation")`
/// does not report a value.
fn parse_jpeg_exif_orientation(bytes: &[u8]) -> Option<ExifOrientation> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 1 < bytes.len() {
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let marker = bytes[pos];
        pos += 1;
        // SOS or EOI terminates metadata headers.
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        // Standalone markers carry no length field.
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if pos + 2 > bytes.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if seg_len < 2 || pos + seg_len > bytes.len() {
            break;
        }
        let payload = &bytes[pos + 2..pos + seg_len];
        pos += seg_len;
        if marker != 0xE1 || !payload.starts_with(b"Exif\0\0") {
            continue;
        }
        let tiff = &payload[6..];
        if tiff.len() < 8 {
            continue;
        }
        let little_endian = match &tiff[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => continue,
        };
        let read_u16 = |off: usize| -> Option<u16> {
            let slice: [u8; 2] = tiff.get(off..off + 2)?.try_into().ok()?;
            Some(if little_endian {
                u16::from_le_bytes(slice)
            } else {
                u16::from_be_bytes(slice)
            })
        };
        let read_u32 = |off: usize| -> Option<u32> {
            let slice: [u8; 4] = tiff.get(off..off + 4)?.try_into().ok()?;
            Some(if little_endian {
                u32::from_le_bytes(slice)
            } else {
                u32::from_be_bytes(slice)
            })
        };
        if read_u16(2)? != 0x002A {
            continue;
        }
        let ifd0 = read_u32(4)? as usize;
        let count = read_u16(ifd0)? as usize;
        let entries_start = ifd0.checked_add(2)?;
        for i in 0..count.min(256) {
            let entry = entries_start.checked_add(i.checked_mul(12)?)?;
            if entry.checked_add(12)? > tiff.len() {
                break;
            }
            let tag = read_u16(entry)?;
            let field_type = read_u16(entry + 2)?;
            let num_values = read_u32(entry + 4)?;
            if tag == 0x0112 && field_type == 3 && num_values >= 1 {
                let value = read_u16(entry + 8)?;
                return ExifOrientation::from_tag(value);
            }
        }
    }
    None
}

fn decode_target_dimensions(
    raw_width: u32,
    raw_height: u32,
    orientation: ExifOrientation,
    request: &PickRequest,
) -> Option<(u32, u32)> {
    let (display_w, display_h) = orientation.display_dimensions(raw_width, raw_height);
    let display = Size::new(
        DevicePixels(i32::try_from(display_w).ok()?),
        DevicePixels(i32::try_from(display_h).ok()?),
    );
    let scaled = request.scaled_size(display);
    if scaled == display {
        return None;
    }
    let scaled_w = u32::try_from(scaled.width.0).ok()?;
    let scaled_h = u32::try_from(scaled.height.0).ok()?;
    if orientation.swaps_axes() {
        Some((scaled_h, scaled_w))
    } else {
        Some((scaled_w, scaled_h))
    }
}

fn encode_mime_and_quality(
    encoding: Encoding,
    request: &PickRequest,
) -> Option<(&'static str, u32)> {
    match encoding {
        Encoding::Keep => None,
        Encoding::Png => Some(("image/png", 100)),
        Encoding::Jpeg => {
            let quality = ((request.jpeg_quality().0 * 100.0).round() as u32).clamp(1, 100);
            Some(("image/jpeg", quality))
        }
    }
}

#[cfg(target_env = "ohos")]
mod ohos {
    use std::{
        fs::File,
        future::Future,
        io::Read as _,
        panic::{AssertUnwindSafe, catch_unwind},
        path::{Path, PathBuf},
        sync::{Arc, LazyLock, Mutex},
    };

    use napi_ohos::{
        Env, JsValue as _, ValueType,
        bindgen_prelude::{Function, JsObjectValue as _, Object, Unknown},
        module::Module,
    };
    use nsh_core::{
        ImageData,
        geometry::{DevicePixels, Size},
        picker::{PickRequest, PickedFile, file_name, sniff},
    };
    use ohos_image_native_binding::{
        DecodingOptions, ImagePacker, ImageSource, ImageString, PackingOptions, PixelMap,
        types::{ImageSize, PixelFormat},
    };

    use super::{
        AbilityError, AbilityResult, AbilityWantEvent, AbilityWantKind, ExifOrientation,
        ExtensionError, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, OnceFuture, OnceSender, PendingTable,
        Want, WantHub, WantObserverGuard, decode_target_dimensions, encode_mime_and_quality,
        parse_jpeg_exif_orientation,
    };
    use crate::app::session::{self, HostCommand};

    /// `IMAGE_DYNAMIC_RANGE_SDR` from `<multimedia/image_framework/image/image_source_native.h>`.
    const IMAGE_DYNAMIC_RANGE_SDR: i32 = 1;

    type UiTask = Box<dyn FnOnce(&UiThread<'_>) + Send + 'static>;
    type AbilitySender = OnceSender<Result<AbilityResult, AbilityError>>;

    static UI_TASKS: LazyLock<Mutex<PendingTable<UiTask>>> = LazyLock::new(Mutex::default);
    static ABILITY_REQUESTS: LazyLock<Mutex<PendingTable<AbilitySender>>> =
        LazyLock::new(Mutex::default);
    static WANT_HUB: LazyLock<WantHub> = LazyLock::new(WantHub::new);

    fn take_pending_exception(env: &Env) -> Option<String> {
        let raw_env = env.raw();
        let mut pending = false;
        let check = unsafe { napi_ohos::sys::napi_is_exception_pending(raw_env, &mut pending) };
        if check != napi_ohos::sys::Status::napi_ok || !pending {
            return None;
        }
        let mut exc = std::ptr::null_mut();
        let cleared =
            unsafe { napi_ohos::sys::napi_get_and_clear_last_exception(raw_env, &mut exc) };
        if cleared != napi_ohos::sys::Status::napi_ok || exc.is_null() {
            return Some("pending JS exception".into());
        }
        let mut str_val = std::ptr::null_mut();
        let coerced = unsafe { napi_ohos::sys::napi_coerce_to_string(raw_env, exc, &mut str_val) };
        if coerced == napi_ohos::sys::Status::napi_ok && !str_val.is_null() {
            let mut len = 0usize;
            if unsafe {
                napi_ohos::sys::napi_get_value_string_utf8(
                    raw_env,
                    str_val,
                    std::ptr::null_mut(),
                    0,
                    &mut len,
                )
            } == napi_ohos::sys::Status::napi_ok
                && len > 0
            {
                let mut buf = vec![0u8; len + 1];
                let mut copied = 0usize;
                if unsafe {
                    napi_ohos::sys::napi_get_value_string_utf8(
                        raw_env,
                        str_val,
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                        &mut copied,
                    )
                } == napi_ohos::sys::Status::napi_ok
                {
                    buf.truncate(copied);
                    if let Ok(text) = String::from_utf8(buf) {
                        return Some(text);
                    }
                }
            }
        }
        // Clear any secondary exception that string coercion might have raised.
        let mut secondary = false;
        if unsafe { napi_ohos::sys::napi_is_exception_pending(raw_env, &mut secondary) }
            == napi_ohos::sys::Status::napi_ok
            && secondary
        {
            let mut ignored = std::ptr::null_mut();
            let _ =
                unsafe { napi_ohos::sys::napi_get_and_clear_last_exception(raw_env, &mut ignored) };
        }
        Some("pending JS exception".into())
    }

    fn read_exif_orientation(source: &ImageSource, bytes: &[u8]) -> ExifOrientation {
        if let Ok(mut key) = ImageString::from_str("Orientation")
            && let Ok(prop) = source.get_image_property(&mut key)
            && let Some(raw) = prop.to_string_lossy()
            && let Some(orientation) = ExifOrientation::parse_property(&raw)
        {
            return orientation;
        }
        parse_jpeg_exif_orientation(bytes).unwrap_or(ExifOrientation::Normal)
    }

    fn apply_orientation(
        pixelmap: &mut PixelMap,
        orientation: ExifOrientation,
        raw_width: u32,
        raw_height: u32,
        decoded_width: u32,
        decoded_height: u32,
    ) -> Result<(), ohos_image_native_binding::ImageNativeError> {
        if orientation == ExifOrientation::Normal {
            return Ok(());
        }
        // Guard against any decoder that already applied axis-swapping EXIF orientation.
        if orientation.swaps_axes()
            && raw_width != raw_height
            && (raw_width > raw_height) != (decoded_width > decoded_height)
        {
            return Ok(());
        }
        match orientation {
            ExifOrientation::Normal => Ok(()),
            ExifOrientation::FlipHorizontal => pixelmap.flip(true, false),
            ExifOrientation::Rotate180 => pixelmap.rotate(180.0),
            ExifOrientation::FlipVertical => pixelmap.flip(false, true),
            ExifOrientation::Transpose => {
                pixelmap.flip(true, false)?;
                pixelmap.rotate(270.0)
            }
            ExifOrientation::Rotate90 => pixelmap.rotate(90.0),
            ExifOrientation::Transverse => {
                pixelmap.flip(true, false)?;
                pixelmap.rotate(90.0)
            }
            ExifOrientation::Rotate270 => pixelmap.rotate(270.0),
        }
    }

    /// Access to the active HarmonyOS ArkTS UI thread passed to closures scheduled
    /// with [`run_on_ui_thread`].
    pub struct UiThread<'env> {
        env: &'env Env,
        ui_context: Object<'env>,
        bundle_name: &'env str,
        module_name: &'env str,
    }

    impl<'env> UiThread<'env> {
        /// Returns the ArkTS N-API environment for the UI thread.
        pub fn env(&self) -> &'env Env {
            self.env
        }

        /// Returns the host window's ArkUI `UIContext`.
        pub fn ui_context(&self) -> &Object<'env> {
            &self.ui_context
        }

        /// Returns the `UIAbilityContext` owning this window (`uiContext.getHostContext()`).
        pub fn ability_context(&self) -> napi_ohos::Result<Object<'env>> {
            let getter: Function<'_, (), Unknown<'_>> =
                self.ui_context.get_named_property("getHostContext")?;
            let raw = getter.apply(self.ui_context, ())?;
            if raw.get_type()? != ValueType::Object {
                return Err(napi_ohos::Error::from_reason(
                    "uiContext.getHostContext() did not return an object",
                ));
            }
            raw.coerce_to_object()
        }

        /// Returns the host application's bundle name (e.g. `"com.example.app"`).
        pub fn bundle_name(&self) -> &str {
            self.bundle_name
        }

        /// Returns the host HAP module name (e.g. `"entry"`).
        pub fn module_name(&self) -> &str {
            self.module_name
        }

        /// Returns the `<bundleName>/<moduleName>` module-info string expected by
        /// `napi_load_module_with_info` for packages declared under
        /// `build-profile.json5` `arkOptions.runtimeOnly.packages`.
        pub fn module_info(&self) -> String {
            if self.bundle_name.is_empty() {
                String::new()
            } else {
                format!("{}/{}", self.bundle_name, self.module_name)
            }
        }

        /// Loads an ArkTS system module (`@ohos.*`), ohpm HAR dependency,
        /// or native `.so` module on the UI thread.
        ///
        /// ArkTS `@kit.*` aliases are compile-time only and cannot be resolved
        /// dynamically by N-API; pass the underlying `@ohos.*` module or an ohpm
        /// HAR instead.
        pub fn load_module(&self, specifier: &str) -> napi_ohos::Result<Module> {
            if specifier.is_empty() {
                return Err(napi_ohos::Error::from_reason(
                    "module specifier must not be empty",
                ));
            }
            if specifier.starts_with("@kit.") {
                return Err(napi_ohos::Error::from_reason(format!(
                    "cannot dynamically load ArkTS kit alias {specifier:?} at runtime; \
                     load the underlying `@ohos.*` module or an ohpm HAR instead"
                )));
            }
            if self.bundle_name.is_empty() || specifier.starts_with("@ohos.") {
                self.env.load(specifier)
            } else {
                let info = self.module_info();
                self.env.load_with_info(specifier, info.as_str())
            }
        }
    }

    /// Runs `f` on the ArkTS UI thread of the active host session (preferring the
    /// focused foreground window) and resolves with its return value.
    ///
    /// The returned `Future` dispatches `f` through the ArkTS UI event loop;
    /// `.await` it asynchronously and never block the ArkTS UI thread waiting on
    /// it synchronously.
    pub fn run_on_ui_thread<F, R>(
        f: F,
    ) -> impl Future<Output = Result<R, ExtensionError>> + Send + 'static
    where
        F: FnOnce(&UiThread<'_>) -> napi_ohos::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let host = match session::any_active_session(false) {
            Ok(host) => host,
            Err(false) => return OnceFuture::ready(Err(ExtensionError::NotStarted)),
            Err(true) => {
                return OnceFuture::ready(Err(ExtensionError::NoSession(
                    "no active OHOS host session".into(),
                )));
            }
        };
        let (sender, future) = OnceFuture::channel(|| {
            Err(ExtensionError::NoSession(
                "OHOS host session closed before UI task ran".into(),
            ))
        });
        let task: UiTask = Box::new(move |ui| {
            let outcome = catch_unwind(AssertUnwindSafe(|| f(ui)));
            let pending_exc = take_pending_exception(ui.env());
            let result = match outcome {
                Ok(Ok(value)) => match pending_exc {
                    Some(exc) => Err(ExtensionError::Failed(exc)),
                    None => Ok(value),
                },
                Ok(Err(err)) => {
                    let message = match pending_exc {
                        Some(exc) if !err.reason.contains(&exc) => format!("{err}: {exc}"),
                        _ => err.to_string(),
                    };
                    Err(ExtensionError::Failed(message))
                }
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "UI task panicked".to_owned());
                    Err(ExtensionError::Failed(format!("UI task panicked: {msg}")))
                }
            };
            sender.send(result);
        });
        let Some(task_id) = UI_TASKS.lock().unwrap().insert(host.session_id(), task) else {
            return OnceFuture::ready(Err(ExtensionError::Failed(
                "OHOS UI task queue exhausted".into(),
            )));
        };
        if let Err(err) = host.send_host_command(HostCommand {
            kind: "ui-task".into(),
            open_id: Some(task_id),
            ..Default::default()
        }) {
            UI_TASKS.lock().unwrap().take(task_id);
            return OnceFuture::ready(Err(ExtensionError::NoSession(err.to_string())));
        }
        future
    }

    /// Returns the active Ability's sandboxed cache directory (`abilityContext.cacheDir`),
    /// or `None` when no `NshSurface` session is active.
    pub fn cache_dir() -> Option<PathBuf> {
        session::any_active_session(false)
            .ok()
            .map(|h| h.raw_cache_dir().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// Returns the active Ability's sandboxed files directory (`abilityContext.filesDir`),
    /// or `None` when no `NshSurface` session is active.
    pub fn files_dir() -> Option<PathBuf> {
        session::any_active_session(false)
            .ok()
            .map(|h| h.raw_files_dir().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// Returns the `Want` that launched the host `UIAbility` (`onCreate`), if any.
    pub fn launch_want() -> Option<Want> {
        WANT_HUB.launch_want()
    }

    /// Registers an observer for `UIAbility.onCreate` / `UIAbility.onNewWant` events.
    pub fn on_ability_want(
        observer: impl Fn(&AbilityWantEvent) + Send + Sync + 'static,
    ) -> WantObserverGuard {
        let id = WANT_HUB.subscribe(Arc::new(observer));
        WantObserverGuard::new(move || WANT_HUB.unsubscribe(id))
    }

    /// Starting another HarmonyOS Ability for a result and observing host `Want`s.
    pub mod ability {
        use super::*;

        /// Standard HarmonyOS `AbilityResult.resultCode` for success.
        pub const RESULT_OK: i32 = 0;

        /// Starts `want` from the foreground `UIAbilityContext` via
        /// `startAbilityForResult` and resolves with the returned [`AbilityResult`].
        pub fn start_for_result(
            want: Want,
        ) -> impl Future<Output = Result<AbilityResult, AbilityError>> + Send + 'static {
            let host = match session::any_active_session(true) {
                Ok(host) => host,
                Err(false) => return OnceFuture::ready(Err(AbilityError::NotStarted)),
                Err(true) => {
                    return OnceFuture::ready(Err(AbilityError::NoAbility(
                        "no foreground OHOS Ability is active".into(),
                    )));
                }
            };
            let want_json = match serde_json::to_string(&want) {
                Ok(json) => json,
                Err(err) => {
                    return OnceFuture::ready(Err(AbilityError::Failed {
                        code: None,
                        message: format!("serializing Want: {err}"),
                    }));
                }
            };
            let (sender, future) = OnceFuture::channel(|| {
                Err(AbilityError::NoAbility(
                    "OHOS host session closed before startAbilityForResult completed".into(),
                ))
            });
            let Some(request_id) = ABILITY_REQUESTS
                .lock()
                .unwrap()
                .insert(host.session_id(), sender)
            else {
                return OnceFuture::ready(Err(AbilityError::Failed {
                    code: None,
                    message: "OHOS ability request queue exhausted".into(),
                }));
            };
            if let Err(err) = host.send_host_command(HostCommand {
                kind: "ability-start-for-result".into(),
                open_id: Some(request_id),
                text: Some(want_json),
                ..Default::default()
            }) {
                ABILITY_REQUESTS.lock().unwrap().take(request_id);
                return OnceFuture::ready(Err(AbilityError::NoAbility(err.to_string())));
            }
            future
        }

        /// Returns the `Want` that launched the host `UIAbility` (`onCreate`), if any.
        pub fn launch_want() -> Option<Want> {
            super::launch_want()
        }

        /// Registers an observer for `UIAbility.onCreate` / `UIAbility.onNewWant` events.
        pub fn on_want(
            observer: impl Fn(&AbilityWantEvent) + Send + Sync + 'static,
        ) -> WantObserverGuard {
            super::on_ability_want(observer)
        }
    }

    /// Prepares the image file at `path` as `request` specifies (EXIF upright,
    /// downscaled to `max_dimension`, converted to a compatible format, re-encoded
    /// at `quality` with EXIF/camera metadata stripped), named `name`.
    pub fn prepare_image(path: &Path, name: &str, request: &PickRequest) -> Option<PickedFile> {
        let file = File::open(path).ok()?;
        let metadata = file.metadata().ok();
        if metadata
            .as_ref()
            .is_some_and(|m| m.len() > MAX_IMAGE_BYTES as u64)
        {
            return None;
        }
        let modified = metadata.and_then(|m| m.modified().ok());
        let mut bytes = Vec::new();
        file.take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        let mut prepared = prepare_image_bytes(&bytes, name, request)?;
        prepared.last_modified = modified;
        Some(prepared)
    }

    /// Prepares encoded image `bytes` as `request` specifies (EXIF upright,
    /// downscaled to `max_dimension`, converted to a compatible format, re-encoded
    /// at `quality` with EXIF/camera metadata stripped), named `name`.
    pub fn prepare_image_bytes(
        bytes: &[u8],
        name: &str,
        request: &PickRequest,
    ) -> Option<PickedFile> {
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return None;
        }
        let source = ImageSource::create_from_data(bytes).ok()?;
        let info = source.image_info(0).ok()?;
        let (raw_w, raw_h) = (info.width().ok()?, info.height().ok()?);
        if raw_w == 0 || raw_h == 0 {
            return None;
        }
        let orientation = read_exif_orientation(&source, bytes);
        let (display_w, display_h) = orientation.display_dimensions(raw_w, raw_h);
        let display_size = Size::new(
            DevicePixels(i32::try_from(display_w).ok()?),
            DevicePixels(i32::try_from(display_h).ok()?),
        );
        let source_mime = sniff(bytes).unwrap_or("application/octet-stream");
        let encoding = request.needs_encoding(source_mime, display_size);
        let Some((out_mime, quality)) = encode_mime_and_quality(encoding, request) else {
            let mut picked = PickedFile::new(name, source_mime, ImageData::from(bytes.to_vec()));
            picked.size = display_size;
            return Some(picked);
        };

        if u64::from(raw_w).saturating_mul(u64::from(raw_h)) > MAX_IMAGE_PIXELS {
            return None;
        }
        let mut decode_opts = DecodingOptions::new().ok()?;
        let _ = decode_opts.set_pixel_format(PixelFormat::Rgba8888);
        let _ = decode_opts.set_desired_dynamic_range(IMAGE_DYNAMIC_RANGE_SDR);
        if let Some((width, height)) = decode_target_dimensions(raw_w, raw_h, orientation, request)
        {
            decode_opts
                .set_desired_size(ImageSize { width, height })
                .ok()?;
        }
        let mut pixelmap = source.create_pixelmap(&mut decode_opts).ok()?;
        let decoded_info = pixelmap.image_info().ok()?;
        let (decoded_w, decoded_h) = (decoded_info.width().ok()?, decoded_info.height().ok()?);
        if decoded_w == 0 || decoded_h == 0 {
            return None;
        }
        if decoded_info.dynamic_range().unwrap_or(false) {
            let _ = pixelmap.to_sdr();
        }
        apply_orientation(
            &mut pixelmap,
            orientation,
            raw_w,
            raw_h,
            decoded_w,
            decoded_h,
        )
        .ok()?;
        let pm_info = pixelmap.image_info().ok()?;
        let (out_w, out_h) = (pm_info.width().ok()?, pm_info.height().ok()?);
        if out_w == 0 || out_h == 0 {
            return None;
        }
        let out_size = Size::new(
            DevicePixels(i32::try_from(out_w).ok()?),
            DevicePixels(i32::try_from(out_h).ok()?),
        );

        let mut pack_opts = PackingOptions::new().ok()?;
        let mut mime_str = ImageString::from_str(out_mime).ok()?;
        pack_opts.set_mime_type(&mut mime_str).ok()?;
        pack_opts.set_quality(quality).ok()?;
        pack_opts.set_needs_pack_properties(false).ok()?;
        let _ = pack_opts.set_desired_dynamic_range(IMAGE_DYNAMIC_RANGE_SDR);

        let mut packer = ImagePacker::new().ok()?;
        let initial_cap =
            ((out_w as usize).saturating_mul(out_h as usize)).clamp(256 * 1024, 8 << 20);
        let mut encoded = vec![0u8; initial_cap];
        let written =
            match packer.pack_to_data_from_pixelmap(&mut pack_opts, &pixelmap, &mut encoded) {
                Ok(size) => size,
                Err(err) if matches!(err.code, 7600103 | 7600301 | 7800301) => {
                    let max_cap = (out_w as usize)
                        .saturating_mul(out_h as usize)
                        .saturating_mul(4)
                        .saturating_add(65_536)
                        .clamp(1 << 20, MAX_IMAGE_BYTES);
                    if max_cap <= initial_cap {
                        return None;
                    }
                    encoded.resize(max_cap, 0);
                    packer
                        .pack_to_data_from_pixelmap(&mut pack_opts, &pixelmap, &mut encoded)
                        .ok()?
                }
                Err(_) => return None,
            };
        encoded.truncate(written);
        if encoded.is_empty() || encoded.len() > MAX_IMAGE_BYTES {
            return None;
        }

        let mut picked = PickedFile::new(
            file_name(name, out_mime),
            out_mime,
            ImageData::from(encoded),
        );
        picked.size = out_size;
        Some(picked)
    }

    pub(crate) fn run_ui_task(
        env: &Env,
        task_id: u32,
        ui_context: Object<'_>,
        bundle_name: &str,
        module_name: &str,
    ) {
        if let Some(task) = UI_TASKS.lock().unwrap().take(task_id) {
            task(&UiThread {
                env,
                ui_context,
                bundle_name,
                module_name,
            });
            let _ = take_pending_exception(env);
        }
    }

    pub(crate) fn complete_ability_result(
        request_id: u32,
        ok: bool,
        payload: Option<String>,
        code: Option<i32>,
        message: Option<String>,
    ) {
        let Some(sender) = ABILITY_REQUESTS.lock().unwrap().take(request_id) else {
            return;
        };
        let outcome = if ok {
            payload
                .as_deref()
                .ok_or_else(|| "missing AbilityResult payload".to_owned())
                .and_then(|raw| {
                    serde_json::from_str::<AbilityResult>(raw)
                        .map_err(|err| format!("invalid AbilityResult payload: {err}"))
                })
                .map_err(|message| AbilityError::Failed {
                    code: None,
                    message,
                })
        } else {
            Err(AbilityError::Failed {
                code,
                message: message.unwrap_or_else(|| "startAbilityForResult failed".into()),
            })
        };
        sender.send(outcome);
    }

    pub(crate) fn deliver_ability_want(kind: &str, payload: &str) {
        let Some(kind) = AbilityWantKind::parse(kind) else {
            log::warn!("OHOS ability want: unknown kind {kind:?}");
            return;
        };
        match serde_json::from_str::<Want>(payload) {
            Ok(want) => WANT_HUB.deliver(kind, want),
            Err(err) => log::warn!("OHOS ability want: invalid JSON payload: {err}"),
        }
    }

    pub(crate) fn on_session_closed(session_id: u32) {
        let ui_tasks = UI_TASKS.lock().unwrap().drain_session(session_id);
        drop(ui_tasks);
        let ability_requests = ABILITY_REQUESTS.lock().unwrap().drain_session(session_id);
        drop(ability_requests);
    }
}

#[cfg(target_env = "ohos")]
pub use ohos::{
    UiThread, ability, cache_dir, files_dir, launch_want, on_ability_want, prepare_image,
    prepare_image_bytes, run_on_ui_thread,
};
#[cfg(target_env = "ohos")]
pub(crate) use ohos::{
    complete_ability_result, deliver_ability_want, on_session_closed, run_ui_task,
};

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Wake, Waker},
    };

    use nsh_core::geometry::{DevicePixels, Percentage};
    use serde_json::json;

    use super::*;

    struct Hits(AtomicUsize);
    impl Wake for Hits {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn once_future_delivers_value_and_recovers_if_sender_drops() {
        let hits = Arc::new(Hits(AtomicUsize::new(0)));
        let waker = Waker::from(hits.clone());
        let mut cx = Context::from_waker(&waker);

        let (tx, mut fut) = OnceFuture::channel(|| Err("closed"));
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending);
        tx.send(Ok(42));
        assert_eq!(hits.0.load(Ordering::SeqCst), 1);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(Ok(42)));

        let (tx2, mut fut2) = OnceFuture::<Result<i32, &str>>::channel(|| Err("dropped"));
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Pending);
        drop(tx2);
        assert_eq!(hits.0.load(Ordering::SeqCst), 2);
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
            assert!(hub_a.launch_want().is_some());
            seen_a
                .lock()
                .unwrap()
                .push(event.want.uri.clone().unwrap_or_default());
        });
        let id_a = hub.subscribe(obs_a);
        assert_eq!(seen.lock().unwrap().len(), MAX_PENDING_WANTS);
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
        let id_b = hub.subscribe(Arc::new(move |event| {
            seen_b_cb
                .lock()
                .unwrap()
                .push(event.want.uri.clone().unwrap_or_default());
        }));
        assert!(seen_b.lock().unwrap().is_empty());

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
        assert_eq!(table.drain_session(1), vec!["s1-b"]);
        assert_eq!(table.take(b), None);
        assert_eq!(table.take(c), Some("s2-c"));
    }

    #[test]
    fn want_and_ability_result_roundtrip_json() {
        let mut want = Want {
            bundle_name: Some("com.example.cam".into()),
            ability_name: Some("EntryAbility".into()),
            action: Some("ohos.want.action.imageCapture".into()),
            flags: Some(0x1000_0001),
            ..Default::default()
        };
        want.parameters
            .insert("supportMultiMode".into(), json!(false));

        let serialized = serde_json::to_value(&want).unwrap();
        assert_eq!(serialized["bundleName"], "com.example.cam");
        assert_eq!(serialized["abilityName"], "EntryAbility");
        assert_eq!(serialized["action"], "ohos.want.action.imageCapture");
        assert_eq!(serialized["flags"], 0x1000_0001u32);
        assert_eq!(serialized["parameters"]["supportMultiMode"], false);
        assert!(serialized.get("uri").is_none());

        let result: AbilityResult = serde_json::from_value(json!({
            "resultCode": 0,
            "want": {
                "uri": "file://media/Photo/1/IMG_001.jpg",
                "flags": 1,
                "parameters": { "resourceUri": "file://media/Photo/1/IMG_001.jpg" }
            }
        }))
        .unwrap();
        assert_eq!(result.result_code, 0);
        let returned = result.want.unwrap();
        assert_eq!(returned.flags, Some(1));
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
    fn want_hub_preserves_fifo_order_when_live_want_arrives_during_replay() {
        let hub = Arc::new(WantHub::new());
        hub.deliver(
            AbilityWantKind::NewWant,
            Want {
                uri: Some("myapp://buffered/1".into()),
                ..Default::default()
            },
        );
        hub.deliver(
            AbilityWantKind::NewWant,
            Want {
                uri: Some("myapp://buffered/2".into()),
                ..Default::default()
            },
        );

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_cb = seen.clone();
        let hub_cb = hub.clone();
        let injected = Arc::new(AtomicUsize::new(0));
        let id = hub.subscribe(Arc::new(move |event| {
            let uri = event.want.uri.clone().unwrap_or_default();
            seen_cb.lock().unwrap().push(uri);
            if injected.fetch_add(1, Ordering::SeqCst) == 0 {
                // Live want arriving mid-replay must queue behind buffered/2.
                hub_cb.deliver(
                    AbilityWantKind::NewWant,
                    Want {
                        uri: Some("myapp://live/3".into()),
                        ..Default::default()
                    },
                );
            }
        }));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "myapp://buffered/1".to_owned(),
                "myapp://buffered/2".to_owned(),
                "myapp://live/3".to_owned(),
            ]
        );
        hub.unsubscribe(id);
    }

    #[test]
    fn exif_orientation_parsing_and_jpeg_app1_extraction() {
        assert_eq!(
            ExifOrientation::parse_property("Right-top"),
            Some(ExifOrientation::Rotate90)
        );
        assert_eq!(
            ExifOrientation::parse_property("6"),
            Some(ExifOrientation::Rotate90)
        );
        assert_eq!(
            ExifOrientation::parse_property("Left-bottom"),
            Some(ExifOrientation::Rotate270)
        );
        assert_eq!(
            ExifOrientation::parse_property("Top-left"),
            Some(ExifOrientation::Normal)
        );
        assert_eq!(ExifOrientation::parse_property("default_exif_value"), None);
        assert_eq!(
            ExifOrientation::Rotate90.display_dimensions(40, 20),
            (20, 40)
        );
        assert_eq!(
            ExifOrientation::Rotate180.display_dimensions(40, 20),
            (40, 20)
        );

        // Minimal JPEG with big-endian (MM) APP1 EXIF Orientation = 6.
        let mut jpeg_mm = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x22];
        jpeg_mm.extend_from_slice(b"Exif\0\0");
        jpeg_mm.extend_from_slice(b"MM");
        jpeg_mm.extend_from_slice(&0x002Au16.to_be_bytes());
        jpeg_mm.extend_from_slice(&8u32.to_be_bytes());
        jpeg_mm.extend_from_slice(&1u16.to_be_bytes());
        jpeg_mm.extend_from_slice(&0x0112u16.to_be_bytes());
        jpeg_mm.extend_from_slice(&3u16.to_be_bytes());
        jpeg_mm.extend_from_slice(&1u32.to_be_bytes());
        jpeg_mm.extend_from_slice(&6u16.to_be_bytes());
        jpeg_mm.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        jpeg_mm.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(
            parse_jpeg_exif_orientation(&jpeg_mm),
            Some(ExifOrientation::Rotate90)
        );

        // Minimal JPEG with little-endian (II) APP1 EXIF Orientation = 8.
        let mut jpeg_ii = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x22];
        jpeg_ii.extend_from_slice(b"Exif\0\0");
        jpeg_ii.extend_from_slice(b"II");
        jpeg_ii.extend_from_slice(&0x002Au16.to_le_bytes());
        jpeg_ii.extend_from_slice(&8u32.to_le_bytes());
        jpeg_ii.extend_from_slice(&1u16.to_le_bytes());
        jpeg_ii.extend_from_slice(&0x0112u16.to_le_bytes());
        jpeg_ii.extend_from_slice(&3u16.to_le_bytes());
        jpeg_ii.extend_from_slice(&1u32.to_le_bytes());
        jpeg_ii.extend_from_slice(&8u16.to_le_bytes());
        jpeg_ii.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        jpeg_ii.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(
            parse_jpeg_exif_orientation(&jpeg_ii),
            Some(ExifOrientation::Rotate270)
        );
    }

    #[test]
    fn image_preparation_sizing_and_encoding_decisions() {
        use nsh_core::picker::PickFormat;

        let mut req = PickRequest::new(false, 1);
        let s_800x600 = Size::new(DevicePixels(800), DevicePixels(600));
        assert_eq!(req.needs_encoding("image/jpeg", s_800x600), Encoding::Keep);
        assert_eq!(req.needs_encoding("image/heic", s_800x600), Encoding::Jpeg);
        // Default JPEG quality matches nsh_core::picker::DEFAULT_QUALITY (90).
        assert_eq!(
            encode_mime_and_quality(Encoding::Jpeg, &req),
            Some(("image/jpeg", 90))
        );

        // Quality set on a PNG does NOT force lossy JPEG conversion.
        req.quality = Some(Percentage(0.7));
        assert_eq!(req.needs_encoding("image/png", s_800x600), Encoding::Keep);
        assert_eq!(req.needs_encoding("image/jpeg", s_800x600), Encoding::Jpeg);
        assert_eq!(
            encode_mime_and_quality(Encoding::Jpeg, &req),
            Some(("image/jpeg", 70))
        );

        // Resized PNG stays PNG even when quality is set.
        req.max_dimension = Some(DevicePixels(512));
        assert_eq!(req.needs_encoding("image/png", s_800x600), Encoding::Png);
        assert_eq!(
            encode_mime_and_quality(Encoding::Png, &req),
            Some(("image/png", 100))
        );

        // EXIF Rotate90 40x20 raw -> 20x40 display -> scaled to max 32 -> 16x32 display -> 32x16 raw decode.
        let mut scale_req = PickRequest::new(false, 1);
        scale_req.max_dimension = Some(DevicePixels(32));
        assert_eq!(
            decode_target_dimensions(40, 20, ExifOrientation::Rotate90, &scale_req),
            Some((32, 16))
        );
        assert_eq!(
            decode_target_dimensions(40, 20, ExifOrientation::Normal, &scale_req),
            Some((32, 16))
        );
        scale_req.max_dimension = Some(DevicePixels(100));
        assert_eq!(
            decode_target_dimensions(40, 20, ExifOrientation::Rotate90, &scale_req),
            None
        );

        // Current format leaves HEIC untouched when not scaled.
        let mut cur = PickRequest::new(false, 1);
        cur.format = PickFormat::Current;
        assert_eq!(cur.needs_encoding("image/heic", s_800x600), Encoding::Keep);
    }
}
