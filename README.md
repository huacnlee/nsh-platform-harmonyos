# nsh-platform-harmonyos

[中文](README.zh-CN.md)

Run [Native Shell](https://github.com/longbridge/nsh) (NSH) apps on **HarmonyOS** (鸿蒙) and **OpenHarmony**, with
ArkUI's native views.

<p>
  <img src="docs/screenshots/home.jpeg" width="19%" alt="Gallery">
  <img src="docs/screenshots/stocks.jpeg" width="19%" alt="Stocks">
  <img src="docs/screenshots/stock-detail.jpeg" width="19%" alt="A stock">
  <img src="docs/screenshots/ai-chat-reply.jpeg" width="19%" alt="AI Chat">
  <img src="docs/screenshots/photos.jpeg" width="19%" alt="Photos">
</p>

NSH's gallery on OpenHarmony 7.0 (API 26), in an emulator.

> **A community platform, outside NSH.** NSH ships its own backends (iOS, Android, macOS, Windows, Linux, GPUI, the
> terminal, the web); this one is built on NSH's public platform extension points, in a repository of its own, as
> [`nsh-platform-miniprogram`](https://github.com/huacnlee/nsh-platform-miniprogram) is. NSH does not ship, test or
> support it. Issues and pull requests belong here, not in `longbridge/nsh`. The ArkUI backend was written by
> [@richerfu](https://github.com/richerfu) for NSH (longbridge/nsh#475) and moved here onto the extension points.

## Usage

```sh
cargo install --git https://github.com/huacnlee/nsh-platform-harmonyos nsh-platform-harmonyos-cli
rustup target add aarch64-unknown-linux-ohos

nsh package my-app --target harmonyos              # a DevEco Studio project, and its HAP with DevEco's tools
nsh run my-app --target harmonyos                  # builds, installs and starts it on the device hdc sees
```

`nsh` runs `nsh-platform-harmonyos` from your `PATH` for `--target harmonyos`. It needs:

- **The OpenHarmony NDK, API 20 or later**, found in DevEco Studio, or `OHOS_NDK_HOME` set to an SDK's `openharmony`
  directory (the one with `native/`). It builds the native library.
- **DevEco Studio** or HarmonyOS's command-line tools for the HAP: `hvigorw`, `ohpm` and `hdc` (`DEVECO_STUDIO_HOME`
  when they are elsewhere than `/Applications/DevEco-Studio.app`). Without them, `package` writes the project and you
  build it in DevEco Studio.
- **Or, without DevEco, for OpenHarmony** (`--runtime-os openharmony`): an OpenHarmony SDK
  (`OHOS_BASE_SDK_HOME`, a directory per API as hvigor reads it), hvigor from npm (`npm install
  @ohos/hvigor @ohos/hvigor-ohos-plugin --registry https://repo.harmonyos.com/npm/` into `HVIGOR_HOME`,
  run by `node`) and a Java runtime. Without a signature in the project, the HAP is signed with the
  SDK's own debug keys, which OpenHarmony devices and emulators install.

| Option | |
|---|---|
| `-o, --out DIR` | Where `package` writes the project (default `.nsh/out/harmonyos`) |
| `--runtime-os harmonyos\|openharmony` | The OS the HAP is for: `harmonyos` (default) for Huawei's devices and AppGallery, `openharmony` for OpenHarmony devices |
| `--arch arm64\|armv7a\|x86_64` | Native architecture to build (`package`: `arm64`; `run`/`dev`/`test`: the device's) |
| `--device ID` | The device `hdc` installs on (`hdc list targets`) |
| `--debug` / `--release` | How the native library is built (`package`: release; `run`: debug) |
| `--no-hap` | Write the project only |

In the app, the platform is `platform.name === "harmonyos"` (TSX) or `platform_name() == "harmonyos"` (Rust).
`platform.os` is `"android"`: HarmonyOS is not one of NSH's operating systems, and it runs apps as Android does
(a system back gesture, a mobile idiom), so `android={{…}}` props and `platform.select({ android })` apply here too.

## Device tests

```sh
nsh test examples/counter-tsx --target harmonyos --device <ID>
nsh test examples/counter --target harmonyos --device <ID>
nsh test examples/testing --target harmonyos --device <ID> --junit .nsh/results.xml
```

The same `*.test.tsx` / `*.test.ts` files importing `nsh/test` run on the device.
`--filter NAME` selects test names; `--trace` prints mount batches. A test file or
directory may be selected, including nested test directories. Given an app's
entry, steps are read from standard input, with `app` and `expect` available.

The CLI builds a host with the `dev` feature and the separate bundle ID
`<app-id>.nshtest`, installs it once, and creates a fresh connection, storage,
filesystem and cache for each `open`. Generated projects, reports and failure
screenshots are under the app's `.nsh/harmonyos/test/`. For a Huawei phone,
configure signing in that directory's `project/` for the test bundle ID; an
OpenHarmony host can use the SDK's debug keys as above. Emulators that accept
unsigned HAPs do not need signing configuration.

Tests require a foreground, unlocked device with `uitest`, `uinput`, and a
configured input method with its first-run prompts completed. They use system
input; one test run owns the selected device and one app can be open at a
time. With multiple devices, `--device` is required. NSH's model resolves selectors
and assertions; UITest supplies the surface's screen rectangle and screenshots.
Touches account for pixel density and the surface's screen offset. Input command
errors and timeouts fail the test; interrupts stop the host and remove that
session's reverse forward. Desktop window sizing, mouse and paste operations are
not supported. An unqualified `nsh test` still runs headless; this command runs
TypeScript tests, while Rust unit tests remain `cargo test` on the computer.

## Publishing to HarmonyOS phones

HarmonyOS NEXT (5.0 and later) is built on OpenHarmony: ArkUI's C API, N-API, the NDK and the
`aarch64-unknown-linux-ohos` ABI are the same, so the library this platform builds with the OpenHarmony NDK runs on
Huawei's phones. API 20 is HarmonyOS 6.0. What makes a HAP that installs there is HarmonyOS's own tool chain:

1. `nsh package my-app --target harmonyos` writes the project with `runtimeOS: "HarmonyOS"` and the app's `id` as the
   bundle name.
2. Open it in DevEco Studio once, sign in with a Huawei developer account and let **File → Project Structure →
   Signing Configs** generate a debug signature (or set up a release certificate and profile from AppGallery
   Connect). Packaging again keeps the project's `signingConfigs`.
3. `nsh run my-app --target harmonyos` then builds the signed HAP and installs it; `nsh package` builds it for
   AppGallery Connect.

Apps that deliver modules at run time: check AppGallery's review rules on code delivered after installation before
relying on it. A bundle packaged in the HAP is part of the app.

## How it works

```
your app (Rust or TSX)     components, layout, state
nsh-engine                 one Engine per NshSurface (QuickJS for TSX), on threads of its own
nsh-platform-harmonyos     the ArkUI backend, a Rust library on N-API:
  app/                     NshSurface: engine, sessions, loads, signed packages, the dev server
  render/                  mount batches → ArkUI custom nodes (OH_ArkUI_*), decoration, canvases, shaders, NativeVSync
  text/                    a TextEngine on OH_Drawing_Typography: laid out on the engine thread, drawn on the UI thread
  input/ scroll/           touches, keys, system text inputs, native Scroll and Refresh
  navigation/ media/       stacks, modals, video (AVPlayer), web views (ArkWeb), images
  accessibility/           ArkUI's accessibility providers (api-23, api-26)
  arkts/                   @nsh/harmonyos: NshHost and the ArkTS side of editors, menus, web views
nsh-platform-harmonyos-cli
  package / run            `$NSH build <app>`, a cdylib runner (the backend + NSH's generated `nsh_rust_app`) built
                           with the NDK into libnsh_platform_harmonyos.so, a DevEco project from template/, hvigor, hdc
```

The project the CLI writes is a standard stage-model app: `EntryAbility` loads `pages/Index`, which places an
`NshSurface`'s `content` in a `ContentSlot` that fills the window. An app written in Rust is linked into the library
(`surface.runApp()`); a TSX app's bundle is a raw file of the HAP (`surface.loadResource("bundle.js")`). The same
`@nsh/harmonyos` package embeds NSH apps in an existing HarmonyOS app: see [docs/embedding.md](docs/embedding.md).

[docs/architecture.md](docs/architecture.md) describes the backend's parts.

## Status

- Apps written in **Rust** (`nsh.json` `"rust"` and `"root"`) and in **TSX**. TSX apps whose own Rust crates export
  components (`"rust"` without `"root"`) are not linked yet.
- The backend covers what NSH's mobile backends do: views and their decoration, text, images, canvases, shader views,
  scroll views and lists with pull to refresh, text inputs, gestures and the responder system, the native animation
  graph, navigation stacks with HarmonyOS's headers, modals and sheets, popups, video, web views, accessibility,
  signed packages, `nsh dev` (feature `dev`).
- Checked here: the backend builds for `aarch64-unknown-linux-ohos` against NSH's `main` with every feature set
  (default, `dev`, `api-23`, `api-26`), and NSH's gallery (TSX) and counter (Rust) run on OpenHarmony 7.0 (API 26)
  in an emulator, built, signed and installed by `nsh run --target harmonyos --runtime-os openharmony`. On a
  HarmonyOS phone it ran in longbridge/nsh#475, against an earlier NSH.

## Following NSH

The platform is built against a revision of NSH (`[patch.crates-io]` in `Cargo.toml`, until a release of NSH has the
platform extension points). Mount items, the text engine and events are NSH's protocol and grow with each release; a
new NSH may add mount items this backend skips until it handles them (`render/mount.rs`).

## Development

```sh
cargo test --workspace                                   # the CLI, and the backend's platform-independent parts
OHOS_NDK_HOME=… cargo run -p nsh-platform-harmonyos-cli -- typedefs   # regenerate @nsh/harmonyos's Index.d.ts

# the backend for the device (the CLI sets the same environment for cargo)
OHOS_NDK_HOME=… nsh package examples/counter --target harmonyos -v
```

`NSH_HARMONYOS_SOURCE=<checkout>` makes the CLI build runners on a local checkout of this repository.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the scope, the checks and how to title commits and pull requests.

## License

Apache-2.0.
