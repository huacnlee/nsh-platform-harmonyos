# 开发指南

[English](development.md)

本文档介绍在 **HarmonyOS**（鸿蒙）和 **OpenHarmony** 上开发 [Native Shell](https://github.com/longbridge/nsh)（NSH）应用的两种方式：

1. **[从空仓库开始](#1-从空仓库开始)** — 从零创建独立的 NSH 应用（支持 TSX、纯 Rust、或 TSX + Rust 原生模块），由 `nsh` 自动生成并管理鸿蒙工程。
2. **[从已有 OHOS 仓库开始](#2-从已有-ohos-仓库开始)** — 将 `@nsh/harmonyos` 与 `NshSurface` 集成到现有的 Stage 模型 DevEco Studio 工程中。

---

## 环境准备

1. **安装 Rust 与 OHOS 编译目标**：
   ```sh
   rustup target add aarch64-unknown-linux-ohos
   # 如需 32 位 ARM 或 x86_64 模拟器支持，可额外添加：
   # rustup target add armv7-unknown-linux-ohos x86_64-unknown-linux-ohos
   ```

2. **安装 `nsh` CLI 与鸿蒙平台插件**：
   确保 `PATH` 中已安装 `nsh`（`0.5` 或以上版本），然后安装 `nsh-platform-harmonyos`：
   ```sh
   cargo install --git https://github.com/huacnlee/nsh-platform-harmonyos nsh-platform-harmonyos-cli
   ```

3. **准备 OpenHarmony NDK（API 20 或以上）**：
   - 若已安装 **DevEco Studio**（默认路径如 macOS `/Applications/DevEco-Studio.app` 或通过 `DEVECO_STUDIO_HOME` 指定），CLI 会自动探测其内置 NDK。
   - 也可以手动将环境变量 `OHOS_NDK_HOME` 指向任意 OpenHarmony SDK 的 `openharmony` 目录（即包含 `native/` 的那一层）。

4. **HAP 构建与设备工具（`hvigorw`、`ohpm`、`hdc`）**：
   - **HarmonyOS（华为真机 / 平板 / 模拟器）**：使用 DevEco Studio（非默认安装位置可通过 `DEVECO_STUDIO_HOME` 指定）。
   - **不装 DevEco Studio 的纯命令行环境（面向 OpenHarmony，`--runtime-os openharmony`）**：设置 `OHOS_BASE_SDK_HOME`（按 API 版本分目录）、`HVIGOR_HOME`（执行 `npm install @ohos/hvigor @ohos/hvigor-ohos-plugin --registry https://repo.harmonyos.com/npm/`），并确保 `PATH` 中有 `node` 和 `java`。

---

## 1. 从空仓库开始

从空仓库开始时，仓库中只需维护 NSH 应用源码和 `nsh.json` 清单，DevEco 工程由 `nsh` 命令按需生成。

### 1.1 方案 A：TSX 应用

新建目录 `my-app`：

```
my-app/
├── nsh.json
├── tsconfig.json
└── App.tsx
```

**`nsh.json`**：
```json
{
  "name": "My App",
  "id": "com.example.myapp",
  "version": "1.0.0",
  "build": 1
}
```

**`tsconfig.json`**：
```json
{
  "extends": "./.nsh/tsconfig.json",
  "include": ["**/*.ts", "**/*.tsx"]
}
```

**`App.tsx`**：
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

### 1.2 方案 B：纯 Rust 应用

新建目录 `my-rust-app`：

```
my-rust-app/
├── nsh.json
├── Cargo.toml
└── src/
    └── lib.rs
```

**`nsh.json`**（`"rust"` 指向 crate 目录，`"root"` 指定根组件名）：
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

**`Cargo.toml`**：
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

**`src/lib.rs`**：
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

### 1.3 方案 C：带自定义 Rust Crate 的 TSX 应用

如果应用主体使用 TSX 编写，同时需要自定义 Rust 原生组件、模块或类，只需在 `nsh.json` 中配置 `"rust"` 且**不设** `"root"`：

```json
{
  "name": "My Hybrid App",
  "id": "com.example.myhybridapp",
  "version": "1.0.0",
  "build": 1,
  "rust": ["./crates/native"]
}
```

CLI 生成 runner 时会自动把所列 crate 的 `#[native_module]` 注册表链接进 `libnsh_platform_harmonyos.so`，TSX 代码可直接 `import` 使用。

### 1.4 打包、签名、真机运行、热重载与测试

1. **生成工程与配置签名**：
   ```sh
   # 用于本地运行与调试（工程生成在 .nsh/harmonyos/project）：
   nsh run . --target harmonyos

   # 或用于构建发布包（工程默认生成在 .nsh/out/harmonyos）：
   nsh package . --target harmonyos
   ```
   - **华为 HarmonyOS 真机**：华为手机要求安装签名后的 HAP。首次生成后，用 DevEco Studio 打开 `.nsh/harmonyos/project`（对应 `run`/`dev`）或 `.nsh/out/harmonyos`（对应 `package`），登录华为开发者账号，在 **File → Project Structure → Signing Configs** 中自动生成调试签名。后续再次执行 `nsh run`、`nsh dev` 或 `nsh package` 时，会自动保留 `build-profile.json5` 中的 `signingConfigs`。
   - **OpenHarmony 设备 / 模拟器**：传入 `--runtime-os openharmony` 可直接使用 OpenHarmony SDK 自带的调试证书自动签名，无需打开 DevEco Studio：
     ```sh
     nsh run . --target harmonyos --runtime-os openharmony
     ```

2. **开发热重载（TSX 应用）**：
   ```sh
   nsh dev . --target harmonyos
   ```
   自动启用 `dev` 特性编译宿主、安装并启动应用，通过 `hdc rport` 转发端口并启动开发服务器，保存代码即实时热更新。

3. **设备端自动化测试**：
   ```sh
   nsh test . --target harmonyos --device <DEVICE_ID>
   ```
   在真机或模拟器上运行导入 `nsh/test` 的 `*.test.tsx` / `*.test.ts` 用例（测试宿主使用独立包名 `<app-id>.nshtest`，生成工程位于 `.nsh/harmonyos/test/project`）。

---

## 2. 从已有 OHOS 仓库开始

如果你已经有一个现成的 Stage 模型 HarmonyOS / OpenHarmony 工程（例如 `my-ohos-app/`），可以将 `@nsh/harmonyos` 和 `NshSurface` 作为模块接入现有工程，在任意页面或组件树中嵌入 NSH 视图。

### 2.1 目录结构规划

建议在现有 OHOS 仓库下新建一个子目录（例如 `nsh-app/`）存放 NSH 侧源码：

```
my-ohos-app/
├── AppScope/
├── build-profile.json5
├── oh-package.json5
├── entry/
│   ├── libs/
│   │   └── arm64-v8a/
│   │       └── libnsh_platform_harmonyos.so   # （由步骤 2.2 生成）
│   ├── oh-package.json5
│   └── src/main/
│       ├── ets/
│       │   ├── entryability/EntryAbility.ets
│       │   └── pages/Index.ets
│       ├── module.json5
│       └── resources/rawfile/                 # TSX bundle 与静态资源（步骤 2.2）
├── nsh/                                       # @nsh/harmonyos 模块（步骤 2.2）
└── nsh-app/                                   # NSH 应用源码目录
    ├── nsh.json
    ├── tsconfig.json
    └── App.tsx
```

### 2.2 生成 Native 库、`@nsh/harmonyos` 模块与 Bundle

对 `nsh-app/` 执行带 `--no-hap` 的 `nsh package` 命令，生成原生库 `libnsh_platform_harmonyos.so`、`@nsh/harmonyos` ArkTS 模块（`nsh/`）以及打包后的 bundle 资源：

```sh
nsh package ./nsh-app --target harmonyos --no-hap -o ./.nsh/harmonyos-out
```

> 可通过 `--arch arm64`（默认，对应 `arm64-v8a`）、`--arch armv7a`（对应 `armeabi-v7a`）或 `--arch x86_64`（对应 `x86_64` 模拟器）指定目标架构。

将生成物复制到现有 OHOS 工程中：

```sh
# 1. 复制 @nsh/harmonyos ArkTS 模块到工程根目录
cp -R ./.nsh/harmonyos-out/nsh ./nsh

# 2. 复制编译好的 native 库到 entry/libs/<abi>/
mkdir -p ./entry/libs/arm64-v8a
cp ./.nsh/harmonyos-out/entry/libs/arm64-v8a/libnsh_platform_harmonyos.so ./entry/libs/arm64-v8a/

# 3. 若为 TSX 应用，将 bundle 及资源复制到 entry/src/main/resources/rawfile/
mkdir -p ./entry/src/main/resources/rawfile
cp -R ./.nsh/harmonyos-out/entry/src/main/resources/rawfile/* ./entry/src/main/resources/rawfile/
```

**TSX 代码增量构建（无需重编 `.so`）**：
后续若只修改了 `nsh-app/` 下的 `.ts` / `.tsx` 或静态资源（未改动 Rust crate），无需重新编译 `libnsh_platform_harmonyos.so`，直接将最新 bundle 输出到 `rawfile` 即可：

```sh
nsh build ./nsh-app/App.tsx --release --out ./entry/src/main/resources/rawfile
```

### 2.3 配置现有 DevEco 工程

1. **在工程根目录 `build-profile.json5` 中注册 `nsh` 模块**：
   在 `"modules"` 数组中加入 `nsh_harmonyos`：
   ```json5
   {
     "app": {
       // ...保留原有配置...
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

2. **在 `entry/oh-package.json5` 中添加依赖**：
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
   随后在 DevEco Studio 中点击 **Sync Now**，或在终端执行：
   ```sh
   ohpm install
   ```

3. **在 `entry/src/main/module.json5` 中检查权限**：
   如果 NSH 应用需要使用网络、`WebView` 或连接 `nsh dev` 开发服务器，请确保 `"requestPermissions"` 中包含网络权限：
   ```json5
   "requestPermissions": [
     { "name": "ohos.permission.INTERNET" }
   ]
   ```

### 2.4 在 `EntryAbility` 与页面中挂载 `NshSurface`

1. **在 `EntryAbility.ets` 中转发通知点击事件（可选）**：
   `@nsh/harmonyos` 直接适配标准的 `UIAbility`，无需继承特定基类。若应用使用了本地通知，可在 `onCreate` 与 `onNewWant` 中调用 `onNotificationWant(want)`：
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

2. **在 ArkUI 页面或组件中挂载 `NshSurface`（如 `Index.ets`）**：
   `NshSurface.content` 是标准的 ArkUI `NodeContent`，放入 `ContentSlot` 即可渲染，既可以铺满整页，也可以嵌入 `Column`、`Row`、`Stack` 或 `Tabs` 的局部区域：
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
         // 运行链接进 libnsh_platform_harmonyos.so 的 Rust 应用
         this.surface.runApp();
       } else {
         // 加载 entry/src/main/resources/rawfile/bundle.js 中的 TSX bundle
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

### 2.5 在已有 OHOS 工程中使用 `nsh dev` 热重载

如果希望在已有鸿蒙工程中享受 TSX 热重载：

1. **构建带 `dev` 特性的 `libnsh_platform_harmonyos.so`**：
   对 `nsh-app/` 执行一次 `nsh dev ./nsh-app --target harmonyos`（或编译启用 `dev` 特性的 `nsh-platform-harmonyos`），将生成的 `.nsh/harmonyos/project/entry/libs/arm64-v8a/libnsh_platform_harmonyos.so` 拷贝到现有工程的 `entry/libs/arm64-v8a/`。
2. **启动开发服务器并配置端口转发**：
   ```sh
   hdc rport tcp:8790 tcp:8790
   nsh dev ./nsh-app/App.tsx --server-only --port 8790
   ```
3. **在宿主页面中连接开发服务器**：
   ```ts
   this.surface.connectDevServer('ws://127.0.0.1:8790/ws');
   ```

---

## 延伸阅读

- [在鸿蒙应用中嵌入 NSH（Embedding Guide）](embedding.md) — `NshSurface` 完整 API 表、宿主与 NSH 双向通信（`setParams`、`handle`、`send`）、主题覆盖（`setThemeOverrides`）、权限与沙箱限额配置，以及签名包（`NshTrust`）校验加载。
- [后端架构说明（Architecture）](architecture.md) — ArkUI 自定义节点渲染管线、`OH_Drawing_Typography` 异步排版、Shader 线程、音视频/WebView 以及真机测试引擎的内部实现。
