# The HarmonyOS backend (ArkUI)

`crates/nsh-platform-harmonyos` renders NSH apps on HarmonyOS / OpenHarmony (API 20+, `arm64-v8a`) with ArkUI custom nodes
and lays text out with native `OH_Drawing_Typography`. Each `NshSurface` owns one `NodeContent`, one engine thread and
one QuickJS VM. How apps use it: [the README](../README.md), [embedding.md](embedding.md).

## Building and checking

The backend builds for `aarch64-unknown-linux-ohos` with the OpenHarmony NDK (API 20+); off `target_env = "ohos"` the
crate is empty. The environment cargo needs (the NDK's clang as `CC` and linker, its sysroot for bindgen, its
libclang) is the one `nsh-platform-harmonyos-cli`'s `sdk.rs` sets:

```sh
OHOS_NDK_HOME=… nsh package examples/counter --target harmonyos -v   # the whole path: runner, library, project
cargo check -p nsh-platform-harmonyos --target aarch64-unknown-linux-ohos --features dev   # with the NDK's CC_* / linker set
```

Features:
- Default / `dev`: API 20 baseline (`OH_ArkUI_*` custom nodes, `OH_Drawing_Typography`, `NativeVSync`, `AVPlayer`,
  system `TextInput` / `TextArea`, and `ArkWeb`); `dev` connects to `nsh dev`.
- `api-23`: native accessibility providers, state descriptions and reduced-motion observation.
- `api-26`: includes `api-23` plus named custom accessibility actions.

## On NSH's extension points

- **The environment**: `Environment { os: Os::Android, platform: Some("harmonyos"), .. }` (`app/runtime.rs`). The
  platform is not one of NSH's `Os`; Android's props and behavior (a system back gesture) are the closest, and
  `platform.name` says where the app runs.
- **The linked app**: the runner, a `cdylib` named `nsh_platform_harmonyos` (the N-API module registers under the
  backend's crate name, which the library's name must match), defines `__nsh_platform_harmonyos_app` with the
  backend's `app!` macro (`linked.rs`): the UI sets and, for an app written in Rust, its root from NSH's generated
  `nsh_rust_app`. `NshSurface.runApp()` starts it; the UI sets serve TSX bundles too.
- **Mount items** this backend does not apply yet (shader transitions, `NavShader`, `ShaderInput`) and the opt-in ones
  it does not ask for (`TreeOrder`, `ProjectStates`) are skipped (`render/mount.rs`).
- **Streaming assets**: the video player and web views read large files through `services::assets::StreamAssets`,
  this backend's trait over `nsh_engine::Assets`.

## Architecture

- **One `NshSurface` per embedded view** (`src/app/`): the host creates `new NshSurface(this.getUIContext())` and places
  `surface.content` (`NodeContent`) inside a standard ArkUI `ContentSlot`. `@nsh/harmonyos` binds each window session
  explicitly from the provided `UIContext` (`UIAbilityContext` and `Window`) in a standard `UIAbility` without a custom
  base class or process-global context guessing.
- **Commits and frames** (`src/render/`): the engine submits each `MountBatch` through a N-API threadsafe function to
  the ArkUI UI thread. Per-surface `NativeVSync` callbacks drive animations on the UI thread; idle surfaces request no
  frames.
- **Device tests** (`nsh-platform-harmonyos-cli/src/test/`, `src/app/testing.rs`, `src/render/mount/testing.rs`): the
  CLI runs NSH's public `TestRun` and `Agent`, with a `RemoteScene` over the dev protocol. The device's `Model`
  observes mount batches and provides selectors, storage, network mocks and the manual clock. ArkUI answers native
  geometry queries on its UI thread; the CLI injects system input through hdc (`uitest` / `uinput`) and captures the
  marked surface. A separate `.nshtest` bundle and per-open data directories isolate tests from the normal app.
  Quit retires the engine before removing its files; the CLI stops the host and removes its reverse forward.
- **Shaders** (`src/render/shader/`): wgpu runs on GLES only, which waits for the GPU on the calling thread, so each
  shader view renders into a native surface of its own (an XComponent first among its children) from a thread of the
  surface's (`nsh-shader`), paced by the surface's `NativeVSync`, skipping views out of sight; nothing is read back
  (`docs/dev/shaders.md`).
- **Typography** (`src/text/`): the engine thread shapes and measures paragraphs with `ohos-drawing-binding`
  (`FontCollection::shared()`, `Typography`) and hands owned RGBA bitmaps and selection geometry to the UI thread.
- **Controls and media** (`src/input/`, `src/scroll/`, `src/media/`, `src/interop/`): `Input` uses the system
  `TextInput` / `TextArea` BuilderNode; `Scroll` and `List` use native ArkUI `Scroll` (with on-demand `Refresh`);
  `WebView` uses `ArkWeb`; `Video` uses a C `XComponent` surface and an `AVPlayer` worker.
- **Navigation** (`src/navigation/`): HarmonyOS is a native-header platform, as iOS and Android: each `Screen` draws its own
  bar (back button, title) and places only the `headerLeft` / `headerTitle` / `headerRight` items in it; a screen's
  `header` slot (the bar on platforms without a native header) is never shown, as UIKit's and the Android toolbar's
  bars never place it. Screens and modals report `onInsets` as the other backends do: a screen's top inset is 0 below
  a visible, opaque header, else the safe area's top.
- **Hit areas**: `hitSlop` and overflowing children widen `NODE_RESPONSE_REGION`, whose width and height are
  percentages of the node's size (`100` is the node itself), not ratios.
