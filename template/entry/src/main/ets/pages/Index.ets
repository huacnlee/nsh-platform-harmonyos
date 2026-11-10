import { common } from "@kit.AbilityKit";
import { NshSurface } from "@nsh/harmonyos";

// The app fills the window: written in Rust, it is linked into the native
// library (`runApp`); written in TSX, its bundle is a raw file of the HAP.
@Entry
@Component
struct Index {
  private surface: NshSurface = new NshSurface(this.getUIContext());
  private readonly onOpenUrl = (url: string): void => {
    this.surface.openUrl(url);
  };
  private readonly onDevUrl = (url: string): void => {
    this.surface.connectDevServer(url);
  };

  aboutToAppear(): void {
    const host = this.getUIContext().getHostContext() as common.UIAbilityContext | undefined;
    host?.eventHub.on("nsh.openUrl", this.onOpenUrl);
    host?.eventHub.on("nsh.devUrl", this.onDevUrl);
    const launchUrl = AppStorage.get<string>("nsh.launchUrl");
    if (typeof launchUrl === "string" && launchUrl.length > 0) {
      AppStorage.delete("nsh.launchUrl");
      this.surface.openUrl(launchUrl);
    }
    const devUrl = AppStorage.get<string>("nsh.devUrl");
    if (typeof devUrl === "string" && devUrl.length > 0) {
      this.surface.connectDevServer(devUrl);
    } else if (this.surface.hasApp) {
      this.surface.runApp();
    } else {
      this.surface.loadResource("bundle.js");
    }
  }

  aboutToDisappear(): void {
    const host = this.getUIContext().getHostContext() as common.UIAbilityContext | undefined;
    host?.eventHub.off("nsh.openUrl", this.onOpenUrl);
    host?.eventHub.off("nsh.devUrl", this.onDevUrl);
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
