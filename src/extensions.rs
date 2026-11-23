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

use nsh_core::picker::{PickFormat, PickRequest};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_IMAGE_BYTES: usize = 64 << 20;
const MAX_PENDING_WANTS: usize = 16;
const DEFAULT_JPEG_QUALITY: u32 = 85;

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

struct WantHubInner {
    next_id: u64,
    launch: Option<Want>,
    pending_new_wants: VecDeque<AbilityWantEvent>,
    observers: Vec<(u64, WantObserver)>,
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
        let observers: Vec<WantObserver> = {
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
            inner.observers.iter().map(|(_, cb)| cb.clone()).collect()
        };
        for observer in observers {
            observer(&event);
        }
    }

    fn subscribe(&self, observer: WantObserver) -> (u64, Vec<AbilityWantEvent>) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let replay = if inner.observers.is_empty() {
            inner.pending_new_wants.drain(..).collect()
        } else {
            Vec::new()
        };
        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1).max(1);
        inner.observers.push((id, observer));
        (id, replay)
    }

    fn unsubscribe(&self, id: u64) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .observers
            .retain(|(entry_id, _)| *entry_id != id);
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

fn target_dimensions(width: u32, height: u32, max_dimension: Option<i32>) -> Option<(u32, u32)> {
    let limit = max_dimension.filter(|&d| d > 0)? as u32;
    let longest = width.max(height);
    if width == 0 || height == 0 || longest <= limit {
        return None;
    }
    let scale = f64::from(limit) / f64::from(longest);
    Some((
        ((f64::from(width) * scale).round() as u32).clamp(1, limit),
        ((f64::from(height) * scale).round() as u32).clamp(1, limit),
    ))
}

fn image_needs_encode(source_mime: &str, width: u32, height: u32, request: &PickRequest) -> bool {
    target_dimensions(width, height, request.max_dimension.map(|d| d.0)).is_some()
        || request.quality.is_some()
        || (matches!(request.format, PickFormat::Compatible)
            && !matches!(
                source_mime,
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ))
}

fn encode_mime_and_quality(source_mime: &str, request: &PickRequest) -> (&'static str, u32) {
    if source_mime == "image/png" && request.quality.is_none() {
        return ("image/png", 100);
    }
    let quality = request
        .quality
        .map(|q| ((q.0.clamp(0.0, 1.0) * 100.0).round() as u32).clamp(1, 100))
        .unwrap_or(DEFAULT_JPEG_QUALITY);
    ("image/jpeg", quality)
}

#[cfg(target_env = "ohos")]
mod ohos {
    use std::{
        fs::File,
        future::Future,
        io::Read as _,
        path::{Path, PathBuf},
        sync::{Arc, LazyLock, Mutex},
    };

    use napi_ohos::{
        Env,
        bindgen_prelude::{Function, JsObjectValue as _, Object},
        module::Module,
    };
    use nsh_core::{
        ImageData,
        geometry::{DevicePixels, Size},
        picker::{PickRequest, PickedFile, file_name, sniff},
    };
    use ohos_image_native_binding::{
        DecodingOptions, ImagePacker, ImageSource, ImageString, PackingOptions, types::ImageSize,
    };

    use super::{
        AbilityError, AbilityResult, AbilityWantEvent, AbilityWantKind, ExtensionError,
        MAX_IMAGE_BYTES, OnceFuture, OnceSender, PendingTable, Want, WantHub, WantObserverGuard,
        encode_mime_and_quality, image_needs_encode, target_dimensions,
    };
    use crate::app::session::{self, HostCommand};

    type UiTask = Box<dyn FnOnce(&UiThread<'_>) + Send + 'static>;
    type AbilitySender = OnceSender<Result<AbilityResult, AbilityError>>;

    static UI_TASKS: LazyLock<Mutex<PendingTable<UiTask>>> = LazyLock::new(Mutex::default);
    static ABILITY_REQUESTS: LazyLock<Mutex<PendingTable<AbilitySender>>> =
        LazyLock::new(Mutex::default);
    static WANT_HUB: LazyLock<WantHub> = LazyLock::new(WantHub::new);

    /// Access to the active HarmonyOS ArkTS UI thread passed to closures scheduled
    /// with [`run_on_ui_thread`].
    pub struct UiThread<'env> {
        env: &'env Env,
        ui_context: Object<'env>,
        bundle_name: &'env str,
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
            let getter: Function<'_, (), Object<'_>> =
                self.ui_context.get_named_property("getHostContext")?;
            getter.apply(self.ui_context, ())
        }

        /// Returns the host application's bundle name (e.g. `"com.example.app"`).
        pub fn bundle_name(&self) -> &str {
            self.bundle_name
        }

        /// Returns the `<bundleName>/entry` module-info string expected by
        /// `napi_load_module_with_info` for packages declared under
        /// `entry/build-profile.json5` `arkOptions.runtimeOnly.packages`.
        pub fn module_info(&self) -> String {
            if self.bundle_name.is_empty() {
                String::new()
            } else {
                format!("{}/entry", self.bundle_name)
            }
        }

        /// Loads an ArkTS system module (`@kit.*`, `@ohos.*`), ohpm HAR dependency,
        /// or native `.so` module on the UI thread.
        pub fn load_module(&self, specifier: &str) -> napi_ohos::Result<Module> {
            if self.bundle_name.is_empty()
                || specifier.starts_with("@kit.")
                || specifier.starts_with("@ohos.")
            {
                self.env.load(specifier)
            } else {
                let info = self.module_info();
                self.env.load_with_info(specifier, info.as_str())
            }
        }
    }

    /// Runs `f` on the ArkTS UI thread of the active host session (preferring the
    /// foreground window) and resolves with its return value.
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
            sender.send(f(ui).map_err(|e| ExtensionError::Failed(e.to_string())));
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
    }

    /// Returns the active Ability's sandboxed files directory (`abilityContext.filesDir`),
    /// or `None` when no `NshSurface` session is active.
    pub fn files_dir() -> Option<PathBuf> {
        session::any_active_session(false)
            .ok()
            .map(|h| h.raw_files_dir().to_path_buf())
    }

    /// Returns the `Want` that launched the host `UIAbility` (`onCreate`), if any.
    pub fn launch_want() -> Option<Want> {
        WANT_HUB.launch_want()
    }

    /// Registers an observer for `UIAbility.onCreate` / `UIAbility.onNewWant` events.
    pub fn on_ability_want(
        observer: impl Fn(&AbilityWantEvent) + Send + Sync + 'static,
    ) -> WantObserverGuard {
        let observer = Arc::new(observer);
        let (id, replay) = WANT_HUB.subscribe(observer.clone());
        for event in &replay {
            observer(event);
        }
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
        let (width, height) = (info.width().ok()?, info.height().ok()?);
        if width == 0 || height == 0 {
            return None;
        }
        let source_mime = sniff(bytes).unwrap_or("application/octet-stream");
        if !image_needs_encode(source_mime, width, height, request) {
            let mut picked = PickedFile::new(name, source_mime, ImageData::from(bytes.to_vec()));
            picked.size = Size::new(DevicePixels(width as i32), DevicePixels(height as i32));
            return Some(picked);
        }

        let mut decode_opts = DecodingOptions::new().ok()?;
        if let Some((width, height)) =
            target_dimensions(width, height, request.max_dimension.map(|d| d.0))
        {
            decode_opts
                .set_desired_size(ImageSize { width, height })
                .ok()?;
        }
        let pixelmap = source.create_pixelmap(&mut decode_opts).ok()?;
        let pm_info = pixelmap.image_info().ok()?;
        let (out_w, out_h) = (pm_info.width().ok()?, pm_info.height().ok()?);
        if out_w == 0 || out_h == 0 {
            return None;
        }

        let (out_mime, quality) = encode_mime_and_quality(source_mime, request);
        let mut pack_opts = PackingOptions::new().ok()?;
        let mut mime_str = ImageString::from_str(out_mime).ok()?;
        pack_opts.set_mime_type(&mut mime_str).ok()?;
        pack_opts.set_quality(quality).ok()?;
        pack_opts.set_needs_pack_properties(false).ok()?;

        let mut packer = ImagePacker::new().ok()?;
        let initial_cap =
            ((out_w as usize).saturating_mul(out_h as usize)).clamp(256 * 1024, 8 << 20);
        let mut encoded = vec![0u8; initial_cap];
        let written =
            match packer.pack_to_data_from_pixelmap(&mut pack_opts, &pixelmap, &mut encoded) {
                Ok(size) => size,
                Err(_) => {
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
        picked.size = Size::new(DevicePixels(out_w as i32), DevicePixels(out_h as i32));
        Some(picked)
    }

    pub(crate) fn run_ui_task(env: &Env, task_id: u32, ui_context: Object<'_>, bundle_name: &str) {
        if let Some(task) = UI_TASKS.lock().unwrap().take(task_id) {
            task(&UiThread {
                env,
                ui_context,
                bundle_name,
            });
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
