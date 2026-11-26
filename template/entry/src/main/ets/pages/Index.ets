import { common } from "@kit.AbilityKit";
import { KeyboardAvoidMode } from "@kit.ArkUI";
import { bindLaunchSurface, NshSurface, prepareWebEngine } from "@nsh/harmonyos";

// The app fills the window: written in Rust, it is linked into the native
// library (`runApp`); written in TSX, its bundle is a raw file of the HAP.
@Entry
@Component
struct Index {
  private surface: NshSurface = new NshSurface(this.getUIContext());
  private unbindLaunch?: () => void;
  private readonly onOpenUrl = (url: string): void => {
    this.surface.openUrl(url);
  };
  private readonly onDevUrl = (url: string): void => {
    this.surface.connectDevServer(url);
  };

  aboutToAppear(): void {
    try {
      this.getUIContext().setKeyboardAvoidMode(KeyboardAvoidMode.NONE);
    } catch {}
    const host = this.getUIContext().getHostContext() as common.UIAbilityContext | undefined;
    host?.eventHub.on("nsh.devUrl", this.onDevUrl);
    this.unbindLaunch = host ? bindLaunchSurface(host, this.onOpenUrl) : undefined;
    const devUrl = AppStorage.get<string>("nsh.devUrl");
    if (typeof devUrl === "string" && devUrl.length > 0) {
      this.surface.connectDevServer(devUrl);
    } else if (this.surface.hasApp) {
      this.surface.runApp();
    } else {
      this.surface.loadResource("{{bundleResource}}");
    }
    if ({{usesWebView}}) {
      prepareWebEngine();
    }
  }

  aboutToDisappear(): void {
    const host = this.getUIContext().getHostContext() as common.UIAbilityContext | undefined;
    host?.eventHub.off("nsh.devUrl", this.onDevUrl);
    this.unbindLaunch?.();
    this.unbindLaunch = undefined;
    this.surface.dispose();
  }

  onBackPress(): boolean {
    return this.surface.onBackPress();
  }

  build() {
    Column() {
      ContentSlot(this.surface.content)
    }
    .width("100%")
    .height("100%")
  }
}
