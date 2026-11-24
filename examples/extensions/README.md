# HarmonyOS Extensions Bridge Example (ArkTS ↔ Rust ↔ TSX)

This example demonstrates how a HarmonyOS `nshm` crate or app-local Rust crate uses `nsh-platform-harmonyos`'s public extension points to call ArkTS kits and Ability APIs directly from Rust and expose them cleanly to TSX.

## How it works

```
App.tsx  ──import { HarmonyBridge } from "extensions/harmony"──▶  native/src/lib.rs (#[native_module])
                                                                         │
                                                      nsh-platform-harmonyos extensions
                                                                         │
                                    ┌────────────────────────┬───────────┴──────────┬────────────────────┐
                                    ▼                        ▼                      ▼                    ▼
                            run_on_ui_thread        ability::start_for_result  on_ability_want    prepare_image_bytes
                          (@ohos.deviceInfo /      (UIAbilityContext.          (EntryAbility      (ImageSource +
                           UIContext.showToast)     startAbilityForResult)      onCreate/onNewWant) ImagePacker)
```

- **`nsh.json`** sets `"rust": "native"`, telling `nsh` to link `native/` into the app's HarmonyOS runner (`libnsh_platform_harmonyos.so`) and generate `.nsh/types/extensions/harmony.d.ts` from the Rust `#[module]` and `#[derive(JsObject)]` definitions.
- **`native/Cargo.toml`** depends on `nsh-platform-harmonyos` only under `[target.'cfg(target_env = "ohos")'.dependencies]`, so host tooling (`nsh test` headless, type generation, desktop targets) compiles without OHOS NDK dependencies.
- **`native/src/lib.rs`** exposes `HarmonyBridge` with five methods covering all four extension points:
  1. `HarmonyBridge.getDeviceInfo()` — calls ArkTS `@ohos.deviceInfo` via `run_on_ui_thread` + `ui.load_module(...)` to read `deviceType` (`phone` / `tablet` / `2in1`), `brand`, `marketName`, `osFullName`, `sdkApiVersion`, plus `ui.bundle_name()`.
  2. `HarmonyBridge.showToast(message, durationMs?)` — invokes ArkUI `uiContext.getPromptAction().showToast(...)` directly through `ui.ui_context()` and re-exported `nsh_platform_harmonyos::napi_ohos` (zero custom ArkTS glue).
  3. `HarmonyBridge.startAbility(request)` — invokes `ability::start_for_result(Want)` and returns `{ resultCode, uri, action }`.
  4. `HarmonyBridge.getLaunchWant()` / `HarmonyBridge.onWant(cb)` — inspects `launch_want()` and bridges `on_ability_want(...)` UI-thread callbacks safely onto the NSH engine thread (`Callback<WantInfo>` + `Stop`).
  5. `HarmonyBridge.getStoragePaths()` / `HarmonyBridge.prepareSampleImage(...)` — reads `cache_dir()` / `files_dir()` and runs `prepare_image_bytes(...)`.

## Calling third-party ohpm HAR packages

If your crate calls a third-party ohpm package instead of a built-in `@ohos.*` / `@kit.*` kit, declare it in your crate's `Cargo.toml`:

```toml
[package.metadata.nsh.harmonyos.dependencies]
"@acme/my-kit" = "^1.0.0"
```

Then load it on the UI thread with the exact same `ui.load_module("@acme/my-kit")` call (which uses `napi_load_module_with_info` against `<bundleName>/entry`).

## Running

```sh
# Headless test (runs host fallbacks in native/src/lib.rs)
nsh test examples/extensions

# Build DevEco Studio project / HAP for HarmonyOS
OHOS_NDK_HOME=… nsh package examples/extensions --target harmonyos --no-hap

# Run directly on a connected HarmonyOS / OpenHarmony device
OHOS_NDK_HOME=… nsh run examples/extensions --target harmonyos
```
