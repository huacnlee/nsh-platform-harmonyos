//! Public extension points for HarmonyOS `nshm` crates and app Rust code.
//!
//! Mirrors the generic hooks `nsh-platform-ios` (`presenting_controller`,
//! `on_application_event`, `prepare_image`) and `nsh-platform-android`
//! (`with_env`, `activity::start_for_result`, `picker::prepare_image`) expose,
//! keeping platform-specific capabilities out of the core runtime.

pub(crate) mod state;

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

pub use self::state::{
    AbilityError, AbilityResult, AbilityWantEvent, AbilityWantKind, ExtensionError, Want,
    WantObserverGuard,
};
use self::state::{
    MAX_IMAGE_BYTES, OnceFuture, OnceSender, PendingTable, WantHub, encode_mime_and_quality,
    image_needs_encode, target_dimensions,
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
    ///
    /// Third-party ohpm packages declared via `[package.metadata.nsh.harmonyos.dependencies]`
    /// are automatically added to `entry/build-profile.json5`'s
    /// `arkOptions.runtimeOnly.packages`, so `load_module("@acme/my-kit")`
    /// resolves them without static ArkTS imports.
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
///
/// Safe to call from any thread (including NSH engine threads and `tokio`
/// workers). If the session closes before the task executes, the returned
/// future resolves with [`ExtensionError::NoSession`].
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
        let outcome = f(ui).map_err(|err| ExtensionError::Failed(err.to_string()));
        sender.send(outcome);
    });
    let session_id = host.session_id();
    let Some(task_id) = UI_TASKS.lock().unwrap().insert(session_id, task) else {
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
        .map(|host| host.raw_cache_dir().to_path_buf())
}

/// Returns the active Ability's sandboxed files directory (`abilityContext.filesDir`),
/// or `None` when no `NshSurface` session is active.
pub fn files_dir() -> Option<PathBuf> {
    session::any_active_session(false)
        .ok()
        .map(|host| host.raw_files_dir().to_path_buf())
}

/// Returns the `Want` that launched the host `UIAbility` (`onCreate`), if any.
pub fn launch_want() -> Option<Want> {
    WANT_HUB.launch_want()
}

/// Registers an observer for `UIAbility.onCreate` / `UIAbility.onNewWant` events.
///
/// Any `onNewWant` events that arrived before the first observer registered are
/// replayed in order. Call [`launch_want`] to inspect the cold-launch `Want`
/// regardless of registration timing, and call [`WantObserverGuard::detach`]
/// inside `#[native_module]` to keep the observer alive for the process lifetime.
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
    ///
    /// Safe to call from any thread. Fails with [`AbilityError::NotStarted`]
    /// before any `NshSurface` exists, or [`AbilityError::NoAbility`] when the
    /// app is in the background or its session closes before completion.
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
        let session_id = host.session_id();
        let Some(request_id) = ABILITY_REQUESTS.lock().unwrap().insert(session_id, sender) else {
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
///
/// Reads and decodes the file synchronously: call it off the UI thread.
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
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mut prepared = prepare_image_bytes(&bytes, name, request)?;
    prepared.last_modified = modified;
    Some(prepared)
}

/// Prepares encoded image `bytes` as `request` specifies (EXIF upright,
/// downscaled to `max_dimension`, converted to a compatible format, re-encoded
/// at `quality` with EXIF/camera metadata stripped), named `name`.
///
/// Decodes and encodes synchronously: call it off the UI thread.
pub fn prepare_image_bytes(bytes: &[u8], name: &str, request: &PickRequest) -> Option<PickedFile> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let source = ImageSource::create_from_data(bytes).ok()?;
    let info = source.image_info(0).ok()?;
    let width = info.width().ok()?;
    let height = info.height().ok()?;
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
    if let Some((target_w, target_h)) =
        target_dimensions(width, height, request.max_dimension.map(|d| d.0))
    {
        decode_opts
            .set_desired_size(ImageSize {
                width: target_w,
                height: target_h,
            })
            .ok()?;
    }
    let pixelmap = source.create_pixelmap(&mut decode_opts).ok()?;
    let pm_info = pixelmap.image_info().ok()?;
    let out_w = pm_info.width().ok()?;
    let out_h = pm_info.height().ok()?;
    if out_w == 0 || out_h == 0 {
        return None;
    }

    let (out_mime, quality) = encode_mime_and_quality(source_mime, request);
    let mut pack_opts = PackingOptions::new().ok()?;
    let mut mime_str = ImageString::from_str(out_mime).ok()?;
    pack_opts.set_mime_type(&mut mime_str).ok()?;
    pack_opts.set_quality(quality).ok()?;
    // Strip camera EXIF/GPS metadata on re-encode.
    pack_opts.set_needs_pack_properties(false).ok()?;

    let mut packer = ImagePacker::new().ok()?;
    let initial_cap = ((out_w as usize).saturating_mul(out_h as usize)).clamp(256 * 1024, 8 << 20);
    let mut encoded = vec![0u8; initial_cap];
    let written = match packer.pack_to_data_from_pixelmap(&mut pack_opts, &pixelmap, &mut encoded) {
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
    let Some(task) = UI_TASKS.lock().unwrap().take(task_id) else {
        return;
    };
    let ui = UiThread {
        env,
        ui_context,
        bundle_name,
    };
    task(&ui);
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
    if ok {
        let parsed = payload
            .as_deref()
            .ok_or_else(|| "missing AbilityResult payload".to_owned())
            .and_then(|raw| {
                serde_json::from_str::<AbilityResult>(raw)
                    .map_err(|err| format!("invalid AbilityResult payload: {err}"))
            });
        match parsed {
            Ok(result) => sender.send(Ok(result)),
            Err(message) => sender.send(Err(AbilityError::Failed {
                code: None,
                message,
            })),
        }
    } else {
        sender.send(Err(AbilityError::Failed {
            code,
            message: message.unwrap_or_else(|| "startAbilityForResult failed".into()),
        }));
    }
}

pub(crate) fn deliver_ability_want(kind: &str, payload: &str) {
    let Some(kind) = AbilityWantKind::parse(kind) else {
        log::warn!("OHOS ability want: unknown kind {kind:?}");
        return;
    };
    let want: Want = match serde_json::from_str(payload) {
        Ok(want) => want,
        Err(err) => {
            log::warn!("OHOS ability want: invalid JSON payload: {err}");
            return;
        }
    };
    WANT_HUB.deliver(kind, want);
}

pub(crate) fn on_session_closed(session_id: u32) {
    // Dropping each drained UiTask / OnceSender invokes OnceSender::drop and
    // wakes the waiter with NoSession / NoAbility outside the table locks.
    let ui_tasks = UI_TASKS.lock().unwrap().drain_session(session_id);
    drop(ui_tasks);
    let ability_requests = ABILITY_REQUESTS.lock().unwrap().drain_session(session_id);
    drop(ability_requests);
}
