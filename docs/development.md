# Development Guide

[中文](development.zh-CN.md)

This guide covers developing [Native Shell](https://github.com/longbridge/nsh) (NSH) apps for **HarmonyOS** and **OpenHarmony** in two ways:

1. **[Starting from an empty repository](#1-starting-from-an-empty-repository)** — create a standalone NSH app (TSX, Rust, or TSX + Rust crates) and let `nsh` generate and manage the HarmonyOS project.
2. **[Starting from an existing HarmonyOS / OpenHarmony repository](#2-starting-from-an-existing-harmonyos--openharmony-repository)** — integrate `@nsh/harmonyos` and `NshSurface` into an existing Stage-model DevEco Studio project.

---

## Prerequisites

1. **Rust & OHOS target**:
   ```sh
   rustup target add aarch64-unknown-linux-ohos
   # Optional for 32-bit ARM or x86_64 emulators:
   # rustup target add armv7-unknown-linux-ohos x86_64-unknown-linux-ohos
   ```

2. **`nsh` CLI and the HarmonyOS target CLI**:
   Make sure `nsh` (`0.5` or later) is on your `PATH`, then install `nsh-platform-harmonyos`:
   ```sh
   cargo install --git https://github.com/huacnlee/nsh-platform-harmonyos nsh-platform-harmonyos-cli
   ```

3. **OpenHarmony NDK (API 20 or later)**:
   - Automatically detected if **DevEco Studio** is installed in its default location (`/Applications/DevEco-Studio.app` on macOS, `C:\Program Files\Huawei\DevEco Studio` on Windows), or via `DEVECO_STUDIO_HOME`.
   - Or point `OHOS_NDK_HOME` to an OpenHarmony SDK's `openharmony` directory (the directory containing `native/`).

4. **HAP build & device tools (`hvigorw`, `ohpm`, `hdc`)**:
   - **HarmonyOS (Huawei phones / tablets / emulator)**: use DevEco Studio (`DEVECO_STUDIO_HOME` if installed in a custom path).
   - **OpenHarmony without DevEco Studio (`--runtime-os openharmony`)**: set `OHOS_BASE_SDK_HOME` (one directory per API version), `HVIGOR_HOME` (`npm install @ohos/hvigor @ohos/hvigor-ohos-plugin --registry https://repo.harmonyos.com/npm/`), and have `node` and `java` on `PATH`.

---

## 1. Starting from an Empty Repository

When starting from scratch, your repository only needs NSH source files and an `nsh.json` manifest. `nsh` generates the DevEco Studio project on demand.

### 1.1 Option A: TSX App

Create a new directory `my-app`:

```
my-app/
├── nsh.json
├── tsconfig.json
└── App.tsx
```

**`nsh.json`**:
```json
{
  "name": "My App",
  "id": "com.example.myapp",
  "version": "1.0.0",
  "build": 1
}
```

**`tsconfig.json`**:
```json
{
  "extends": "./.nsh/tsconfig.json",
  "include": ["**/*.ts", "**/*.tsx"]
}
```

**`App.tsx`**:
```tsx
import { mount, signal, Text, View } from "nsh";

function App() {
  const count = signal(0);

  return (
    <View style={{ flex: 1, alignItems: "center", justifyContent: "center", gap: 16 }}>
      <Text style={{ fontSize: 28, fontWeight: "bold" }}>Count: {count()}</Text>
      <View
        onPress={() => count.set(count() + 1)}
        style={{ backgroundColor: "#0a59f7", paddingHorizontal: 20, paddingVertical: 12, borderRadius: 10 }}
      >
        <Text style={{ color: "#ffffff", fontSize: 16 }}>Increment</Text>
      </View>
    </View>
  );
}

mount(App);
```

### 1.2 Option B: Rust App

Create a new directory `my-rust-app`:

```
my-rust-app/
├── nsh.json
├── Cargo.toml
└── src/
    └── lib.rs
```

**`nsh.json`** (`"rust"` points to the crate directory and `"root"` names the root component):
```json
{
  "name": "My Rust App",
  "id": "com.example.myrustapp",
  "version": "1.0.0",
  "build": 1,
  "rust": ".",
  "root": "App"
}
```

**`Cargo.toml`**:
```toml
[package]
name = "my-rust-app"
version = "0.1.0"
edition = "2024"
publish = false

[workspace]

[lib]
path = "src/lib.rs"

[dependencies]
nsh-core = "0.5"
nsh-base = "0.5"
```

**`src/lib.rs`**:
```rust
use nsh_base::theme;
use nsh_core::prelude::*;

#[native_module("my_rust_app/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

#[component]
pub fn App() -> impl IntoElement {
    let count = signal(0_u32);
    view()
        .flex_1()
        .bg(move || theme().background)
        .items_center()
        .justify_center()
        .gap_4()
        .child(text(move || format!("Count: {}", count.get())).text_size(px(28.0)))
        .child(
            view()
                .px_5()
                .py_3()
                .rounded(px(10.0))
                .bg(move || theme().primary)
                .on_press(move || count.update(|v| *v += 1))
                .child(text("Increment").text_color(move || theme().primary_foreground)),
        )
}
```

### 1.3 Option C: TSX App with Rust Native Crates

If your app is written in TSX but includes custom Rust components, modules, or classes, set `"rust"` in `nsh.json` **without** `"root"`:

```json
{
  "name": "My Hybrid App",
  "id": "com.example.myhybridapp",
  "version": "1.0.0",
  "build": 1,
  "rust": ["./crates/native"]
}
```

The HarmonyOS runner links each listed crate's `#[native_module]` registry into `libnsh_platform_harmonyos.so` so your TSX bundle can import it directly.

### 1.4 Build, Sign, Run, Hot Reload & Test

1. **Generate the DevEco project & configure signing**:
   ```sh
   # For nsh run / nsh dev (project written to .nsh/harmonyos/project):
   nsh run . --target harmonyos

   # Or for release / AppGallery packaging (project written to .nsh/out/harmonyos):
   nsh package . --target harmonyos
   ```
   - **Huawei HarmonyOS phones/tablets**: Physical Huawei devices require a signed HAP. Open `.nsh/harmonyos/project` (for `run`/`dev`) or `.nsh/out/harmonyos` (for `package`) once in DevEco Studio, sign in with your Huawei developer account, and open **File → Project Structure → Signing Configs** to auto-generate a debug signature. Subsequent runs of `nsh run`, `nsh dev`, and `nsh package` preserve `signingConfigs` in `build-profile.json5`.
   - **OpenHarmony devices / emulators**: Pass `--runtime-os openharmony` to sign automatically with the OpenHarmony SDK's debug certificate without DevEco Studio:
     ```sh
     nsh run . --target harmonyos --runtime-os openharmony
     ```

2. **Live development with hot reload (`TSX` apps)**:
   ```sh
   nsh dev . --target harmonyos
   ```
   Builds the app with the `dev` feature, installs and launches it on the connected device, forwards the dev server port via `hdc rport`, and hot-reloads on file edits.

3. **On-device automated tests**:
   ```sh
   nsh test . --target harmonyos --device <DEVICE_ID>
   ```
   Runs `*.test.tsx` / `*.test.ts` suites (`import { ... } from "nsh/test"`) on the device under the isolated bundle ID `<app-id>.nshtest` (project located at `.nsh/harmonyos/test/project`).

---

## 2. Starting from an Existing HarmonyOS / OpenHarmony Repository

If you already have a Stage-model HarmonyOS or OpenHarmony project in DevEco Studio (`my-ohos-app/`), you can embed one or more NSH surfaces anywhere inside your existing ArkUI pages.

### 2.1 Directory Layout

Keep your NSH source code in a subdirectory (for example `nsh-app/`) inside your repository:

```
my-ohos-app/
├── AppScope/
├── build-profile.json5
├── oh-package.json5
├── entry/
│   ├── libs/
│   │   └── arm64-v8a/
│   │       └── libnsh_platform_harmonyos.so   # (generated in step 2.2)
│   ├── oh-package.json5
│   └── src/main/
│       ├── ets/
│       │   ├── entryability/EntryAbility.ets
│       │   └── pages/Index.ets
│       ├── module.json5
│       └── resources/rawfile/                 # TSX bundle & assets (step 2.2)
├── nsh/                                       # @nsh/harmonyos module (step 2.2)
└── nsh-app/                                   # Your NSH app source
    ├── nsh.json
    ├── tsconfig.json
    └── App.tsx
```

### 2.2 Build the Native Library, `@nsh/harmonyos` Module, and Bundle

Run `nsh package` with `--no-hap` on `nsh-app/` to generate the native library (`libnsh_platform_harmonyos.so`), the `@nsh/harmonyos` ArkTS module (`nsh/`), and the compiled bundle:

```sh
nsh package ./nsh-app --target harmonyos --no-hap -o ./.nsh/harmonyos-out
```

> Use `--arch arm64` (default, `arm64-v8a`), `--arch armv7a` (`armeabi-v7a`), or `--arch x86_64` (`x86_64` emulator) to match your target device ABI.

Copy the generated artifacts into your existing HarmonyOS project:

```sh
# 1. Copy the @nsh/harmonyos ArkTS module into your project root
cp -R ./.nsh/harmonyos-out/nsh ./nsh

# 2. Copy the compiled native library into entry/libs/<abi>/
mkdir -p ./entry/libs/arm64-v8a
cp ./.nsh/harmonyos-out/entry/libs/arm64-v8a/libnsh_platform_harmonyos.so ./entry/libs/arm64-v8a/

# 3. For TSX apps, copy the compiled bundle and assets into entry/src/main/resources/rawfile/
mkdir -p ./entry/src/main/resources/rawfile
cp -R ./.nsh/harmonyos-out/entry/src/main/resources/rawfile/* ./entry/src/main/resources/rawfile/
```

**Fast incremental bundle updates (TSX apps)**:
When you only change `.ts` / `.tsx` files or assets in `nsh-app/` (and have not added new Rust crates), you do not need to recompile `libnsh_platform_harmonyos.so`. Rebuild only `rawfile` directly:

```sh
nsh build ./nsh-app/App.tsx --release --out ./entry/src/main/resources/rawfile
```

### 2.3 Configure the DevEco Studio Project

1. **Register the `nsh` module in the root `build-profile.json5`**:
   Add `nsh_harmonyos` to the `"modules"` list:
   ```json5
   {
     "app": {
       // ...existing app config...
     },
     "modules": [
       {
         "name": "nsh_harmonyos",
         "srcPath": "./nsh"
       },
       {
         "name": "entry",
         "srcPath": "./entry",
         "targets": [
           {
             "name": "default",
             "applyToProducts": ["default"]
           }
         ]
       }
     ]
   }
   ```

2. **Add dependencies in `entry/oh-package.json5`**:
   ```json5
   {
     "name": "entry",
     "version": "1.0.0",
     "dependencies": {
       "@nsh/harmonyos": "file:../nsh",
       "libnsh_platform_harmonyos.so": "file:../nsh/src/main/cpp/types/libnsh_platform_harmonyos"
     }
   }
   ```
   Then sync packages in DevEco Studio or from the terminal:
   ```sh
   ohpm install
   ```

3. **Check permissions in `entry/src/main/module.json5`**:
   Ensure `"ohos.permission.INTERNET"` is listed under `"requestPermissions"` if your NSH app uses networking, `WebView`, or `nsh dev`:
   ```json5
   "requestPermissions": [
     { "name": "ohos.permission.INTERNET" }
   ]
   ```

### 2.4 Mount `NshSurface` in `EntryAbility` and ArkUI Pages

1. **Forward notification wants (optional) in `EntryAbility.ets`**:
   `@nsh/harmonyos` works with a standard `UIAbility` without any custom base class. If your NSH app uses local notifications, forward `want` in `onCreate` and `onNewWant`:
   ```ts
   import { UIAbility, Want } from '@kit.AbilityKit';
   import { onNotificationWant } from '@nsh/harmonyos';

   export default class EntryAbility extends UIAbility {
     onCreate(want: Want): void {
       onNotificationWant(want);
     }

     onNewWant(want: Want): void {
       onNotificationWant(want);
     }
   }
   ```

2. **Render `NshSurface` inside an ArkUI page or component (`Index.ets`)**:
   `NshSurface.content` is a standard ArkUI `NodeContent` that mounts inside `ContentSlot`. It can fill the entire page or sit inside any `Column`, `Row`, `Stack`, or `Tabs` layout:
   ```ts
   import { NshSurface } from '@nsh/harmonyos';

   @Entry
   @Component
   struct Index {
     private surface: NshSurface = new NshSurface(this.getUIContext());

     aboutToAppear(): void {
       this.surface.setErrorHandler((message: string) => {
         console.error(`nsh: ${message}`);
       });

       if (this.surface.hasApp) {
         // Rust app linked into libnsh_platform_harmonyos.so
         this.surface.runApp();
       } else {
         // TSX bundle in entry/src/main/resources/rawfile/bundle.js
         this.surface.loadResource('bundle.js');
       }
     }

     aboutToDisappear(): void {
       this.surface.dispose();
     }

     onBackPress(): boolean {
       return this.surface.onBackPress();
     }

     build() {
       Column() {
         ContentSlot(this.surface.content)
       }
       .width('100%')
       .height('100%')
     }
   }
   ```

### 2.5 Hot Reload (`nsh dev`) in an Existing Project

To use `nsh dev` hot reload inside your existing HarmonyOS project:

1. **Build `libnsh_platform_harmonyos.so` with the `dev` feature**:
   Run `nsh dev ./nsh-app --target harmonyos` once (or build the `nsh-platform-harmonyos` crate with `--features dev`) and copy `.nsh/harmonyos/project/entry/libs/arm64-v8a/libnsh_platform_harmonyos.so` into your project's `entry/libs/arm64-v8a/`.
2. **Start the NSH dev server and forward its port**:
   ```sh
   hdc rport tcp:8790 tcp:8790
   nsh dev ./nsh-app/App.tsx --server-only --port 8790
   ```
3. **Connect the surface to the dev server**:
   ```ts
   this.surface.connectDevServer('ws://127.0.0.1:8790/ws');
   ```

---

## Further Reading

- [Embedding NSH in a HarmonyOS app](embedding.md) — full `NshSurface` API reference, host–app communication (`setParams`, `handle`, `send`), theme overrides, capability grants, VM limits, and signed `.nsh` packages (`NshTrust`).
- [Backend Architecture](architecture.md) — how the ArkUI rendering pipeline, off-thread `OH_Drawing_Typography` text engine, shaders, media, and device test runner work.
