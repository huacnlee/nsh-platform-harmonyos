//! Example native module bridging HarmonyOS ArkTS APIs and Ability lifecycle
//! into TSX via `nsh-platform-harmonyos` extension points.
//!
//! `App.tsx` imports `HarmonyBridge` from `"extensions/harmony"` (configured
//! via `nsh.json` `"rust": "native"`). Its TypeScript declarations are
//! automatically generated into `.nsh/types/extensions/harmony.d.ts`.

use nsh_core::prelude::*;

/// Native module exported to TSX as `import { HarmonyBridge } from "extensions/harmony"`.
#[native_module("extensions/harmony")]
fn native(registry: &mut Registry) {
    registry.module::<HarmonyBridge>();
}

/// Device metadata read directly from ArkTS `@ohos.deviceInfo` on the UI thread.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct DeviceInfo {
    /// `true` on HarmonyOS / OpenHarmony; `false` on other targets.
    pub supported: bool,
    /// Host application bundle name (`UiThread::bundle_name`).
    pub bundle_name: String,
    /// Device form factor (`"phone"`, `"tablet"`, `"2in1"`, `"tv"`, ...).
    pub device_type: String,
    /// Hardware brand (e.g. `"HUAWEI"`).
    pub brand: String,
    /// Commercial model name.
    pub market_name: String,
    /// Full OS version string.
    pub os_full_name: String,
    /// HarmonyOS SDK API level.
    pub sdk_api_version: i32,
}

/// Ability sandbox directories from `cache_dir()` and `files_dir()`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct StoragePaths {
    /// Ability sandbox cache directory (`abilityContext.cacheDir`).
    pub cache_dir: Option<String>,
    /// Ability sandbox files directory (`abilityContext.filesDir`).
    pub files_dir: Option<String>,
}

/// Parameters for starting another HarmonyOS Ability via `startAbilityForResult`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct AbilityRequest {
    /// Target application bundle name (e.g. `"com.huawei.hmos.settings"`).
    pub bundle_name: Option<String>,
    /// Target UIAbility name (e.g. `"com.huawei.hmos.settings.MainAbility"`).
    pub ability_name: Option<String>,
    /// Optional implicit/explicit Want action.
    pub action: Option<String>,
    /// Optional Want URI.
    pub uri: Option<String>,
}

/// Result returned by `ability::start_for_result`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct AbilityOutcome {
    /// HarmonyOS `AbilityResult.resultCode` (`0` = `RESULT_OK`).
    pub result_code: i32,
    /// Returned `Want.uri`, if any.
    pub uri: Option<String>,
    /// Returned `Want.action`, if any.
    pub action: Option<String>,
}

/// Summary of an `EntryAbility` `onCreate` or `onNewWant` event.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct WantInfo {
    /// `"create"` or `"new-want"`.
    pub kind: String,
    /// `Want.action`, if present.
    pub action: Option<String>,
    /// `Want.uri`, if present.
    pub uri: Option<String>,
    /// Target `Want.abilityName`, if present.
    pub ability_name: Option<String>,
}

/// Output summary from `prepare_image_bytes`.
#[derive(Debug, Clone, Default, PartialEq, JsObject)]
pub struct PreparedImageInfo {
    /// Output MIME type (e.g. `"image/jpeg"` or `"image/png"`).
    pub mime: String,
    /// Output image width in pixels.
    pub width: i32,
    /// Output image height in pixels.
    pub height: i32,
    /// Encoded output byte length.
    pub bytes: u32,
}

/// HarmonyOS ArkTS & Ability bridge exposed to TSX.
#[module]
pub struct HarmonyBridge;

#[module]
impl HarmonyBridge {
    /// Reads HarmonyOS device information by calling ArkTS `@ohos.deviceInfo`
    /// inside `run_on_ui_thread` (zero ArkTS glue code needed).
    ///
    /// Third-party ohpm HAR packages declared under
    /// `[package.metadata.nsh.harmonyos.dependencies]` can be loaded the exact
    /// same way via `ui.load_module("@acme/my-kit")`.
    pub async fn get_device_info() -> Result<DeviceInfo, String> {
        #[cfg(target_env = "ohos")]
        {
            nsh_platform_harmonyos::run_on_ui_thread(|ui| {
                let module = ui.load_module("@ohos.deviceInfo")?;
                Ok(DeviceInfo {
                    supported: true,
                    bundle_name: ui.bundle_name().to_owned(),
                    device_type: module.get("deviceType").unwrap_or_default(),
                    brand: module.get("brand").unwrap_or_default(),
                    market_name: module.get("marketName").unwrap_or_default(),
                    os_full_name: module.get("osFullName").unwrap_or_default(),
                    sdk_api_version: module.get("sdkApiVersion").unwrap_or_default(),
                })
            })
            .await
            .map_err(|err| err.to_string())
        }
        #[cfg(not(target_env = "ohos"))]
        {
            Ok(DeviceInfo {
                supported: false,
                device_type: "host".into(),
                ..Default::default()
            })
        }
    }

    /// Displays a native HarmonyOS toast by calling ArkTS
    /// `uiContext.getPromptAction().showToast({ message, duration })` from Rust.
    pub async fn show_toast(message: String, duration_ms: Option<u32>) -> Result<(), String> {
        let duration = duration_ms.unwrap_or(2000).clamp(500, 10_000);
        #[cfg(target_env = "ohos")]
        {
            nsh_platform_harmonyos::run_on_ui_thread(move |ui| {
                use nsh_platform_harmonyos::napi_ohos::bindgen_prelude::{
                    Function, JsObjectValue as _, Object,
                };
                let get_prompt: Function<'_, (), Object<'_>> =
                    ui.ui_context().get_named_property("getPromptAction")?;
                let prompt_action = get_prompt.apply(*ui.ui_context(), ())?;
                let show: Function<'_, Object<'_>, ()> =
                    prompt_action.get_named_property("showToast")?;
                let mut opts = Object::new(ui.env())?;
                opts.set("message", message.as_str())?;
                opts.set("duration", duration)?;
                show.apply(prompt_action, opts)?;
                Ok(())
            })
            .await
            .map_err(|err| err.to_string())
        }
        #[cfg(not(target_env = "ohos"))]
        {
            let _ = (message, duration);
            Ok(())
        }
    }

    /// Starts another HarmonyOS Ability via `ability::start_for_result` and
    /// resolves with its `AbilityResult`.
    pub async fn start_ability(request: AbilityRequest) -> Result<AbilityOutcome, String> {
        #[cfg(target_env = "ohos")]
        {
            let mut want = nsh_platform_harmonyos::Want::default();
            want.bundle_name = request.bundle_name;
            want.ability_name = request.ability_name;
            want.action = request.action;
            want.uri = request.uri;
            let result = nsh_platform_harmonyos::ability::start_for_result(want)
                .await
                .map_err(|err| err.to_string())?;
            Ok(AbilityOutcome {
                result_code: result.result_code,
                uri: result.want.as_ref().and_then(|w| w.uri.clone()),
                action: result.want.as_ref().and_then(|w| w.action.clone()),
            })
        }
        #[cfg(not(target_env = "ohos"))]
        {
            let _ = request;
            Err("startAbilityForResult is only available on HarmonyOS".into())
        }
    }

    /// Returns the `Want` that launched the host `UIAbility` (`onCreate`), if any.
    pub fn get_launch_want() -> Option<WantInfo> {
        #[cfg(target_env = "ohos")]
        {
            nsh_platform_harmonyos::launch_want().map(|want| WantInfo {
                kind: "create".into(),
                action: want.action,
                uri: want.uri,
                ability_name: want.ability_name,
            })
        }
        #[cfg(not(target_env = "ohos"))]
        {
            None
        }
    }

    /// Subscribes to `EntryAbility` `onCreate` / `onNewWant` events and returns
    /// an unsubscribe function.
    pub fn on_want(
        #[args(event)] listener: Callback<WantInfo>,
        cx: CallCx<'_>,
    ) -> nsh_core::element::Stop {
        #[cfg(target_env = "ohos")]
        {
            ohos_want_bridge::subscribe(listener, cx)
        }
        #[cfg(not(target_env = "ohos"))]
        {
            let _ = (listener, cx);
            nsh_core::element::Stop::new(|| {})
        }
    }

    /// Returns the active Ability's sandbox `cacheDir` and `filesDir`.
    pub fn get_storage_paths() -> StoragePaths {
        #[cfg(target_env = "ohos")]
        {
            StoragePaths {
                cache_dir: nsh_platform_harmonyos::cache_dir()
                    .map(|p| p.to_string_lossy().into_owned()),
                files_dir: nsh_platform_harmonyos::files_dir()
                    .map(|p| p.to_string_lossy().into_owned()),
            }
        }
        #[cfg(not(target_env = "ohos"))]
        {
            StoragePaths::default()
        }
    }

    /// Re-encodes a tiny embedded sample PNG through HarmonyOS's native
    /// `prepare_image_bytes` pipeline (EXIF upright, downscale, metadata strip).
    pub async fn prepare_sample_image(
        max_dimension: Option<i32>,
        quality: Option<f32>,
    ) -> Result<PreparedImageInfo, String> {
        // Minimal valid 1x1 red PNG (68 bytes).
        const SAMPLE_PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        #[cfg(target_env = "ohos")]
        {
            let mut req = nsh_core::picker::PickRequest::new(false, 1);
            req.max_dimension = max_dimension.filter(|&d| d > 0).map(DevicePixels);
            req.quality = quality
                .filter(|q| q.is_finite())
                .map(|q| nsh_core::geometry::Percentage(q.clamp(0.0, 1.0)));
            let prepared =
                nsh_platform_harmonyos::prepare_image_bytes(SAMPLE_PNG, "sample.png", &req)
                    .ok_or_else(|| "prepare_image_bytes failed".to_owned())?;
            Ok(PreparedImageInfo {
                mime: prepared.mime,
                width: prepared.size.width.0,
                height: prepared.size.height.0,
                bytes: prepared.data.len() as u32,
            })
        }
        #[cfg(not(target_env = "ohos"))]
        {
            let _ = (max_dimension, quality);
            Ok(PreparedImageInfo {
                mime: "image/png".into(),
                width: 1,
                height: 1,
                bytes: SAMPLE_PNG.len() as u32,
            })
        }
    }
}

/// Bridges the UI-thread `Send + Sync` `on_ability_want` callback onto the
/// NSH engine thread where `Callback<WantInfo>` runs.
#[cfg(target_env = "ohos")]
mod ohos_want_bridge {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use nsh_core::prelude::*;

    use crate::WantInfo;

    #[derive(Default)]
    struct Queue {
        items: VecDeque<WantInfo>,
        waker: Option<Waker>,
        closed: bool,
    }

    struct NextWant(Arc<Mutex<Queue>>);

    impl Future for NextWant {
        type Output = Option<WantInfo>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut q = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(item) = q.items.pop_front() {
                return Poll::Ready(Some(item));
            }
            if q.closed {
                return Poll::Ready(None);
            }
            q.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    pub(crate) fn subscribe(
        listener: Callback<WantInfo>,
        cx: CallCx<'_>,
    ) -> nsh_core::element::Stop {
        let queue = Arc::new(Mutex::new(Queue::default()));
        let push_q = queue.clone();
        let guard = nsh_platform_harmonyos::on_ability_want(move |event| {
            let info = WantInfo {
                kind: match event.kind {
                    nsh_platform_harmonyos::AbilityWantKind::Create => "create".into(),
                    nsh_platform_harmonyos::AbilityWantKind::NewWant => "new-want".into(),
                },
                action: event.want.action.clone(),
                uri: event.want.uri.clone(),
                ability_name: event.want.ability_name.clone(),
            };
            let waker = {
                let mut q = push_q.lock().unwrap_or_else(|e| e.into_inner());
                if q.closed {
                    return;
                }
                q.items.push_back(info);
                q.waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
        });

        let task_q = queue.clone();
        cx.start(async move {
            while let Some(info) = NextWant(task_q.clone()).await {
                listener.post(info);
            }
        });

        let guard = Mutex::new(Some(guard));
        let stop_q = queue;
        nsh_core::element::Stop::new(move || {
            drop(guard.lock().ok().and_then(|mut g| g.take()));
            let waker = {
                let mut q = stop_q.lock().unwrap_or_else(|e| e.into_inner());
                q.closed = true;
                q.waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exports_harmony_bridge_module() {
        let set = registry();
        assert_eq!(set.specifier(), Some("extensions/harmony"));
    }
}
