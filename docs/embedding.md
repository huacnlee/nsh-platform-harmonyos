# Embedding NSH in a HarmonyOS app

An existing HarmonyOS or OpenHarmony app can show NSH apps inside its own UI, with ArkUI custom nodes and native typography, from API 20, on `arm64-v8a`. It embeds modules in an `NshSurface`, loads bundles or signed packages, and runs them with limits and capabilities of its own choosing. In the app, `platform.name` is `"harmonyos"` and `platform.os` is `"android"` (README, "Usage").

## Get the library

`nsh package <app> --target harmonyos --no-hap` writes a project whose `nsh/` directory is the `@nsh/harmonyos`
package and whose `entry/libs/arm64-v8a/libnsh_platform_harmonyos.so` is the native library, linked with the app (an
app written in Rust) or ready for a TSX app's bundles. Copy both into your app:

- `nsh/` as a module of the project (`build-profile.json5` `"modules"`: `{ "name": "nsh_harmonyos", "srcPath": "./nsh" }`);
- the library into `entry/libs/arm64-v8a/`;
- the dependencies into `entry/oh-package.json5`:

```json5
{
  "dependencies": {
    "@nsh/harmonyos": "file:../nsh",
    "libnsh_platform_harmonyos.so": "file:../nsh/src/main/cpp/types/libnsh_platform_harmonyos"
  }
}
```

The backend's Cargo features `dev` (connects to `nsh dev`), `api-23` and `api-26` (accessibility providers,
reduced-motion observation and named accessibility actions on newer systems) are features of the crate
`nsh-platform-harmonyos`. Standalone CLI builds enable `api-23` or `api-26` — and raise the generated
project's `compatibleSdkVersion` — via `--api 23` or `--api 26`.

## Ship a bundle with the app

Build the TSX app into the module's `rawfile` resources:

```sh
nsh build App.tsx --release --out entry/src/main/resources/rawfile
```

This writes `bundle.nshc` (bytecode), `bundle.js` (source), `bundle.js.map`, `capabilities.json` (when `nsh.json` declares [capabilities](https://github.com/longbridge/nsh/blob/main/website/docs/sandbox-and-capabilities.md)) and `assets/` (images, fonts and media imported by the app).

## Show the app

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
    this.surface.loadResource('bundle.js');
  }

  aboutToDisappear(): void {
    this.surface.dispose();
  }

  onBackPress(): boolean {
    return this.surface.onBackPress();
  }

  build() {
    Stack() {
      ContentSlot(this.surface.content)
    }
    .width('100%')
    .height('100%')
  }
}
```

`NshSurface` can also sit inside any ArkUI layout, for example below your own toolbar or side by side in a `Row`:

```ts
Column() {
  Toolbar()
  Stack() {
    ContentSlot(this.surface.content)
  }
  .width('100%')
  .layoutWeight(1)
}
.width('100%')
.height('100%')
```

The slot's size is the app's window: NSH lays the app out at the slot's size and relayouts when it resizes. Loading again replaces the running app. The project `nsh package` writes (`template/entry/src/main/ets/pages/Index.ets`) is a complete example.

## `NshSurface`

| Member | Description |
|---|---|
| `new NshSurface(uiContext: UIContext, options?: NshOptions)` | Create an empty surface bound to `uiContext` with optional VM limits, granted capabilities, asset directory and initial environment. |
| `readonly content: NodeContent` | The platform `NodeContent` to mount inside a standard `ContentSlot`. |
| `setErrorHandler(handler: (message: string) => void)` | Receive fatal app errors on the UI thread (failed loads, uncaught JS exceptions, VM stopped by a limit). Errors are always logged. |
| `setThemeOverrides(json?: string \| null)` | Put the host's brand over the app's [theme](https://github.com/longbridge/nsh/blob/main/website/docs/tailwind.md#theme-tokens): a theme file in the format of `nsh.json`'s `"theme"`. Applies at once and to apps loaded later; `null` restores the app's own theme. Throws if the JSON does not parse. |
| `setEnvironment(changes: NshEnvironment)` | Override locale, font scale, color scheme (`"light"` / `"dark"`), safe-area insets or keyboard height for this surface. Omitted fields keep their current values. |
| `runApp()` | Run the app written in Rust that is linked into the library; `hasApp` says whether one is. |
| `loadResource(name: string, sourceMap?: string): Promise<void>` | Run a bundle (`bundle.js` or `bundle.nshc`) from `resources/rawfile/`; app assets are read from `rawfile/assets/`. |
| `loadSource(source: string, sourceMap?: string)` | Run trusted JS source directly, replacing the running app. |
| `runPackage(pkg: NshPackage)` | Run a verified signed package, replacing the running app. A package can be run once per `openPackage` call. |
| `registerComponent(name: string, factory: (id: number) => object)` | Register a [native component](https://github.com/longbridge/nsh/blob/main/website/docs/native-components.md) for this surface. |
| `emitComponentEvent(handle: number, name: string, data: object \| string \| number \| boolean \| null)` | Emit an event from a mounted native component to JS. |
| `setComponentSize(handle: number, width: number, height: number)` | Report a mounted native component's natural size in `vp`. |
| `setParams(params?: object \| null)` | Launch parameters (`Record<string, Object>`) for the next app loaded: the app reads them as `host.params` at startup ([`nsh/host`](https://github.com/longbridge/nsh/blob/main/website/modules/nsh-host.md)). |
| `send(name: string, data?: object \| string \| number \| boolean \| null)` | Send an event to the app (`host.on(name, …)`); kept until the app listens, or for the next app when none is running. |
| `handle(name: string, handler?: ((data: Object \| null, reply: NshHostReply) => void) \| null)` | Handle the app's calls (`host.call`) and posts (`host.post`) for `name` on the UI thread; `null` (or `removeHandler(name)`) removes the handler. Answer once via `reply.resolve(value)` or `reply.reject(message)`. |
| `setOpenUrlHandler(handler?: ((url: string) => boolean) \| null)` | Intercept URLs the app opens; return `true` if handled by the host, or `false` to let the system open it. |
| `connectDevServer(url: string)` | Development: load from a `nsh dev` server and reload on every change (requires the library built with the feature `dev`). |
| `disconnectDevServer()` | Disconnect from the dev server while keeping the current app running. |
| `activate()` | Mark this surface as the active target for the system back gesture when switching between multiple surfaces. |
| `onBackPress(): boolean` | Route a page `onBackPress()` event to this surface's window session; returns `true` when consumed by the app. |
| `stop()` | Stop the running app and cancel pending loads while keeping the surface, its options and registered components for later loads. |
| `dispose()` | Permanently stop the app and release the surface's native resources. |

## Talking to the app

A surface and the app it runs talk over a channel of their own ([`nsh/host`](https://github.com/longbridge/nsh/blob/main/website/modules/nsh-host.md)): launch parameters the app reads at startup, events from the host app, and calls and posts from the app, all as JSON values. Neither a view in the app nor a native component is needed.

```ts
// Before loading: what the app should open to (`host.params` in TSX).
surface.setParams({ conversation: 'conv-42' });
surface.loadResource('bundle.js');

// The app's `await host.call("user.profile", { id })`, handled on the UI thread.
surface.handle('user.profile', (data, reply) => {
  const req = data as Record<string, Object>;
  api.fetchProfile(req['id'] as string)
    .then((profile) => reply.resolve({ id: req['id'], name: profile.name }))
    .catch((err: Error) => reply.reject(err.message));
});

// The app's `host.post("analytics", …)`: nothing to answer.
surface.handle('analytics', (data, _reply) => {
  analytics.log(data);
});

// Later: received by `host.on("conversation.open", …)` in the app.
surface.send('conversation.open', { id: tappedConversation });
```

Handlers run on the UI thread; calls wait in the app for the reply without blocking either side. Events of a name the app has not subscribed to yet are kept until it does, and events sent while no app is running are kept for the next app.

## Theme overrides

An app embedded in several branded host apps takes on each host's brand: the host gives the tokens its brand changes as a theme file in the format of `nsh.json`'s `"theme"` (shadcn/ui's `cssVars`, values as CSS writes them). They override the app's tokens of the same names while the app's other tokens stay as they are. Give a token's dark-mode value under `dark` (a token set only under `light` or `theme` has that color in both appearances):

```ts
surface.setThemeOverrides(JSON.stringify({
  cssVars: {
    theme: { radius: '0.75rem' },
    light: { primary: '#e11d48', 'primary-foreground': '#ffffff' },
    dark: { primary: '#fb7185', 'primary-foreground': '#1c1917' },
  },
}));
surface.loadResource('bundle.js');
```

Call it before or after loading: classes like `bg-primary` and `rounded-(--radius)` restyle at once, and every app loaded later starts with the overrides in its first frame. `setThemeOverrides(null)` restores the app's own theme.

## Links the app opens

URLs the app opens ([`openUrl`](https://github.com/longbridge/nsh/blob/main/website/modules/nsh-device.md#openurl) and links in a `TextView` with `links="open"`) go to the surface's `setOpenUrlHandler` callback first, on the UI thread: return `true` when the host app handles the URL itself (for example with its own router), or `false` to let the system open it. Without a handler, every URL is opened by the system. Only URLs allowed by the app's `openUrl` capability reach the handler.

```ts
surface.setOpenUrlHandler((url: string): boolean => {
  if (url.startsWith('lb://') || url.startsWith('https://longbridge.com/')) {
    router.open(url);
    return true;
  }
  return false;
});
```

## Limits

Pass VM and node limits in `NshOptions` when creating the surface:

```ts
const surface = new NshSurface(this.getUIContext(), {
  memoryBytes: 128 * 1024 * 1024, // 128 MiB heap
  stackBytes: 4 * 1024 * 1024,    // 4 MiB stack
  loadBudgetMs: 10_000,           // 10 s to load
  callBudgetMs: 1_000,            // 1 s per call
  maxNodes: 50_000,               // 50,000 nodes
});
```

The defaults are: 256 MiB heap, 4 MiB stack, 10 s to load, 2 s per call, 200,000 nodes. See [Sandbox & Capabilities](https://github.com/longbridge/nsh/blob/main/website/docs/sandbox-and-capabilities.md) for what happens when each limit is reached.

## Capabilities

Pass the host's capability grant in `NshOptions.capabilities` (a JSON string in the same format as `capabilities` in `nsh.json`):

```ts
const surface = new NshSurface(this.getUIContext(), {
  capabilities: JSON.stringify({
    network: { hosts: ['api.example.com'] },
    webview: { hosts: ['help.example.com'] },
    host: true,
  }),
});
surface.loadResource('bundle.js');
```

A request must be allowed by both the app's `nsh.json` (shipped next to the bundle as `capabilities.json` or inside a signed package) and this grant. See [Sandbox & Capabilities](https://github.com/longbridge/nsh/blob/main/website/docs/sandbox-and-capabilities.md).

## Signed packages

Verify a downloaded package off the UI thread with `NshTrust`, then run it on the surface:

```ts
import { NshTrust } from 'libnsh_platform_harmonyos.so';

const trust = await NshTrust.open(`${context.filesDir}/nsh-trust`, [rootPublicKey]);
const pkg = await trust.openPackage(downloadedFilePath);
surface.runPackage(pkg);
```

| API | Description |
|---|---|
| `NshTrust.open(stateDirectory: string, rootPublicKeys?: string[]): Promise<NshTrust>` | Open or create a trust store. Its state (latest revocation list, highest accepted build per app) persists in that directory, and package assets are extracted under it. |
| `addRoot(publicKeyBase64: string): void` | Trust a root public key (base64, the contents of a `.pub` file from `nsh keygen`). |
| `updateRevocations(revocationsJson: string): Promise<string>` | Apply a signed revocation list from `nsh revoke` (must be signed by a trusted root and newer than the store's current list). |
| `openPackage(packagePath: string): Promise<NshPackage>` | Verify the package on a worker thread, extract its assets and accept it (older builds of that app are refused from then on). |
| `openResource(name: string): Promise<NshPackage>` | Verify and open a signed `.nsh` package bundled in `resources/rawfile/`. |
| `NshPackage` | `info` (`id`, `name`, `version`, `build`), `isConsumed`. |

See [Signed Packages](https://github.com/longbridge/nsh/blob/main/website/docs/signed-packages.md) for how keys, certificates and revocations work.

## Development

With the library built with the feature `dev`:

```ts
surface.connectDevServer('ws://127.0.0.1:8790/ws');
```

Run `nsh dev App.tsx` on your computer. For an emulator or a device over USB, forward the port with `hdc` so `127.0.0.1` on the device reaches the server:

```sh
hdc rport tcp:8790 tcp:8790
```

Otherwise use the LAN address `nsh dev` prints.

## Ability setup

- **Standard `UIAbility`.** `@nsh/harmonyos` attaches automatically to the active `UIContext`, `UIAbilityContext` and window lifecycle when a surface is created—your `EntryAbility` stays a standard `UIAbility` (`extends UIAbility`) with no custom base class required.
- **Back button.** System back targets the active `NshSurface` (dialogs and modals first, then `BackHandler` listeners, then navigation stacks) before falling back to the Ability. When showing multiple surfaces (for example in tabs), call `surface.activate()` on the active surface.
- **Keyboard and safe area.** `@nsh/harmonyos` observes system bar, cutout and keyboard avoid areas automatically and reports only the portion overlapping each `NshSurface`. `Input` uses the system `TextInput` / `TextArea` and scrolls the focused field clear of the keyboard.
- **Lifecycle.** Detaching a `ContentSlot` keeps the surface and its running VM alive; re-mounting it reattaches the native tree. Call `surface.stop()` to clear the running app while keeping the surface, or `surface.dispose()` when the page or component permanently disappears.

## See also

- [Rust Reference](https://github.com/longbridge/nsh/blob/main/website/docs/rust.md)
- [Embedding in iOS apps](https://github.com/longbridge/nsh/blob/main/website/docs/ios.md)
- [Embedding in Android apps](https://github.com/longbridge/nsh/blob/main/website/docs/android.md)
- [Native Components](https://github.com/longbridge/nsh/blob/main/website/docs/native-components.md)
- [Signed Packages](https://github.com/longbridge/nsh/blob/main/website/docs/signed-packages.md)
- [Sandbox & Capabilities](https://github.com/longbridge/nsh/blob/main/website/docs/sandbox-and-capabilities.md)
