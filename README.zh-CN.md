# nsh-platform-harmonyos

[English](README.md)

在 **HarmonyOS**（鸿蒙）和 **OpenHarmony** 上运行 [Native Shell](https://github.com/longbridge/nsh)（NSH）应用，界面用 ArkUI 的原生视图绘制。

<p>
  <img src="docs/screenshots/home.jpeg" width="19%" alt="Gallery">
  <img src="docs/screenshots/stocks.jpeg" width="19%" alt="Stocks">
  <img src="docs/screenshots/stock-detail.jpeg" width="19%" alt="A stock">
  <img src="docs/screenshots/ai-chat-reply.jpeg" width="19%" alt="AI Chat">
  <img src="docs/screenshots/photos.jpeg" width="19%" alt="Photos">
</p>

NSH 的 Gallery 运行在 OpenHarmony 7.0（API 26）模拟器上。

> **NSH 之外的社区平台。** NSH 自带的后端有 iOS、Android、macOS、Windows、Linux、GPUI、终端和 Web。这个平台和
> [`nsh-platform-miniprogram`](https://github.com/huacnlee/nsh-platform-miniprogram) 一样，建在 NSH 公开的平台扩展点上，放在单独的仓库里。NSH
> 不发布、不测试、也不支持它，Issue 和 Pull Request 请提到这里，不要提到 `longbridge/nsh`。ArkUI 后端最初由
> [@richerfu](https://github.com/richerfu) 为 NSH 编写（longbridge/nsh#475），之后迁移到这里，改为基于扩展点实现。

## 使用

```sh
cargo install --git https://github.com/huacnlee/nsh-platform-harmonyos nsh-platform-harmonyos-cli
rustup target add aarch64-unknown-linux-ohos

nsh package my-app --target harmonyos              # 生成 DevEco Studio 工程；装有 DevEco 工具时同时构建 HAP
nsh run my-app --target harmonyos                  # 构建 HAP，安装到 hdc 连接的设备上并启动
```

`--target harmonyos` 时，`nsh` 会从 `PATH` 里找 `nsh-platform-harmonyos` 来执行。需要准备：

- **OpenHarmony NDK，API 20 或以上**：可以用 DevEco Studio 自带的，也可以把 `OHOS_NDK_HOME` 设成某个 SDK 的
  `openharmony` 目录（里面有 `native/`）。它用来编译 native 库。
- **DevEco Studio** 或 HarmonyOS 命令行工具：提供 `hvigorw`、`ohpm` 和 `hdc`，用来构建 HAP。如果不在
  `/Applications/DevEco-Studio.app`，用 `DEVECO_STUDIO_HOME` 指定位置。没有这些工具时，`package` 只写出工程，需要在 DevEco Studio 里构建。
- **或者，不装 DevEco，面向 OpenHarmony**（`--runtime-os openharmony`）：准备一个 OpenHarmony SDK（`OHOS_BASE_SDK_HOME`，
  按 hvigor 的要求每个 API 一个目录）、从 npm 安装的 hvigor（`npm install @ohos/hvigor @ohos/hvigor-ohos-plugin --registry
  https://repo.harmonyos.com/npm/` 装到 `HVIGOR_HOME`，用 `node` 运行），以及 Java 运行时。工程里没有签名时，HAP 用 SDK
  自带的调试密钥签名，OpenHarmony 设备和模拟器都能安装。

| 选项 | |
|---|---|
| `-o, --out DIR` | `package` 写出工程的位置（默认 `.nsh/out/harmonyos`） |
| `--runtime-os harmonyos\|openharmony` | HAP 面向的系统：`harmonyos`（默认）用于华为设备和应用市场（AppGallery），`openharmony` 用于 OpenHarmony 设备 |
| `--device ID` | `hdc` 安装到哪台设备（见 `hdc list targets`） |
| `--debug` / `--release` | native 库的构建方式（`package` 默认 release，`run` 默认 debug） |
| `--no-hap` | 只写出工程，不构建 HAP |

在应用里，平台名是 `platform.name === "harmonyos"`（TSX）或 `platform_name() == "harmonyos"`（Rust）。`platform.os` 是
`"android"`：NSH 的操作系统列表里没有 HarmonyOS，而它运行应用的方式和 Android 一样（系统返回手势、移动端形态），所以
`android={{…}}` 属性和 `platform.select({ android })` 在这里同样生效。

## 设备测试

```sh
nsh test examples/counter-tsx --target harmonyos --device <ID>
nsh test examples/counter --target harmonyos --device <ID>
nsh test examples/testing --target harmonyos --device <ID> --junit .nsh/results.xml
```

直接复用导入 `nsh/test` 的 `*.test.tsx` / `*.test.ts` 用例。可以指定文件或目录，
`--filter NAME` 按名称筛选，`--trace` 输出挂载批次。指定应用入口时，从标准输入读取步骤，
可直接使用 `app` 和 `expect`。

CLI 构建启用 `dev` 特性的独立测试宿主，包名为 `<app-id>.nshtest`，每次运行安装一次。
每个 `open` 都有独立连接、存储、文件目录和缓存。生成工程、测试产物和失败截图保存在应用的
`.nsh/harmonyos/test/` 下。华为真机需要在其中的 `project/` 为测试包名单独配置签名；
OpenHarmony 可使用前述 SDK 调试签名。允许安装未签名 HAP 的模拟器无需配置签名。

测试需要解锁并保持前台的设备，设备应提供 `uitest` 和 `uinput`，并先完成输入法的首次启动提示。
同一设备由一个测试进程独占，同一时间只允许打开一个应用。连接多台设备时必须指定 `--device`。选择器和断言使用 NSH 的模型，
UITest 提供根视图屏幕位置和截图，输入坐标会换算密度及窗口偏移。输入命令失败或超时会使测试失败；
中断时停止宿主并移除本次端口转发。桌面窗口尺寸、鼠标及粘贴操作暂不支持。
不带 `--target` 的 `nsh test` 仍运行无界面测试；设备命令执行 TypeScript 用例，Rust 单测继续在电脑上运行 `cargo test`。

## 发布到鸿蒙手机

HarmonyOS NEXT（5.0 起）建立在 OpenHarmony 之上，两者的 ArkUI C API、N-API、NDK 和 `aarch64-unknown-linux-ohos` ABI 都一样。所以用
OpenHarmony NDK 编出来的库，可以直接在华为手机上运行。API 20 对应 HarmonyOS 6.0。要做出能装到手机上的 HAP，需要走 HarmonyOS 自己的工具链：

1. 运行 `nsh package my-app --target harmonyos`，写出的工程是 `runtimeOS: "HarmonyOS"`，bundle name 用应用的 `id`。
2. 用 DevEco Studio 打开一次，登录华为开发者账号，在 **File → Project Structure → Signing Configs** 里自动生成调试签名；发布时改用从
   AppGallery Connect 申请的证书和 Profile。之后再次打包时，工程里的 `signingConfigs` 会保留。
3. 之后 `nsh run my-app --target harmonyos` 会构建已签名的 HAP 并安装；`nsh package` 则构建用来提交 AppGallery Connect 的 HAP。

如果应用要在运行时下发模块，请先确认 AppGallery 对安装后下发代码的审核规则。打包进 HAP 的 bundle 本身就是应用的一部分，不受影响。

## 工作原理

```
应用（Rust 或 TSX）        组件、布局、状态
nsh-engine                 每个 NshSurface 一个 Engine（TSX 用 QuickJS），运行在独立线程上
nsh-platform-harmonyos     ArkUI 后端，基于 N-API 的 Rust 库：
  app/                     NshSurface：引擎、会话、加载、签名包、dev server
  render/                  mount batch → ArkUI 自定义节点（OH_ArkUI_*）、装饰、canvas、shader、NativeVSync
  text/                    基于 OH_Drawing_Typography 的 TextEngine：在引擎线程排版，在 UI 线程绘制
  input/ scroll/           触摸、按键、系统文本输入框、原生 Scroll 和 Refresh
  navigation/ media/       导航栈、模态、视频（AVPlayer）、网页（ArkWeb）、图片
  accessibility/           ArkUI 无障碍 provider（api-23、api-26）
  arkts/                   @nsh/harmonyos：NshHost，以及编辑器、菜单、网页的 ArkTS 部分
nsh-platform-harmonyos-cli
  package / run            `$NSH build <app>`；用 NDK 把 cdylib runner（后端 + NSH 生成的 `nsh_rust_app`）编译成
                           libnsh_platform_harmonyos.so；从 template/ 生成 DevEco 工程；调用 hvigor、hdc
```

CLI 写出的是标准的 Stage 模型应用：`EntryAbility` 加载 `pages/Index`，页面里一个铺满窗口的 `ContentSlot` 放着 `NshSurface` 的
`content`。Rust 写的应用链接进库里，启动时调用 `surface.runApp()`；TSX 应用的 bundle 作为 HAP 的 raw file，用
`surface.loadResource("bundle.js")` 加载。同一个 `@nsh/harmonyos` 包也能把 NSH 应用嵌进已有的鸿蒙应用，见
[docs/embedding.md](docs/embedding.md)。后端各部分的说明见 [docs/architecture.md](docs/architecture.md)。

## 状态

- 支持 **Rust** 写的应用（`nsh.json` 里有 `"rust"` 和 `"root"`）和 **TSX** 应用。TSX 应用如果还带着导出组件的 Rust crate（有
  `"rust"` 没有 `"root"`），暂时还不会被链接进来。
- 后端覆盖了 NSH 移动端后端的全部能力：视图和装饰、文字、图片、canvas、shader 视图、滚动视图和带下拉刷新的列表、文本输入、手势和 responder
  系统、原生动画图、带鸿蒙标题栏的导航栈、模态和 sheet、弹出层、视频、网页、无障碍、签名包，以及 `nsh dev`（特性 `dev`）。
- 这里已验证：后端针对 NSH `main` 能为 `aarch64-unknown-linux-ohos` 编译，所有特性组合（默认、`dev`、`api-23`、`api-26`）都通过；
  NSH 的 Gallery（TSX）和 counter（Rust）在 OpenHarmony 7.0（API 26）模拟器上运行，由
  `nsh run --target harmonyos --runtime-os openharmony` 构建、签名并安装。在鸿蒙手机上，这套后端在 longbridge/nsh#475
  里跑过，当时用的是更早版本的 NSH。

## 跟进 NSH

这个平台针对 NSH 的某个 revision 构建（`Cargo.toml` 里的 `[patch.crates-io]`，等 NSH 发布带平台扩展点的版本后再改）。mount item、文本引擎和事件都是
NSH 的协议，会随版本增长；新版 NSH 可能加入这个后端还没处理的 mount item，在处理之前会被跳过（`render/mount.rs`）。

## 开发

```sh
cargo test --workspace                                   # CLI，以及后端中与平台无关的部分
OHOS_NDK_HOME=… cargo run -p nsh-platform-harmonyos-cli -- typedefs   # 重新生成 @nsh/harmonyos 的 Index.d.ts

# 为设备构建后端（CLI 会给 cargo 设置同样的环境）
OHOS_NDK_HOME=… nsh package examples/counter --target harmonyos -v
```

设置 `NSH_HARMONYOS_SOURCE=<checkout>` 后，CLI 会用这个本地仓库来构建 runner。

## 参与贡献

范围、检查项以及提交和 PR 标题的写法见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可

Apache-2.0。
