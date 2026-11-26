import { KeyboardAvoidMode } from "@kit.ArkUI";
import { NodeContent } from "@ohos.arkui.node";

export const NSH_SUBWINDOW_ROUTE = "nshSubWindow";
export const NSH_SUBWINDOW_BRIDGE_KEY = "nsh.subWindowBridge";

export class SubWindowPageBridge {
  readonly content: NodeContent;
  backdrop: string;
  colorMode: ThemeColorMode;
  private listener?: (backdrop: string, colorMode: ThemeColorMode) => void;

  constructor(
    content: NodeContent,
    backdrop: string = "none",
    colorMode: ThemeColorMode = ThemeColorMode.SYSTEM,
  ) {
    this.content = content;
    this.backdrop = backdrop;
    this.colorMode = colorMode;
  }

  bind(listener: (backdrop: string, colorMode: ThemeColorMode) => void): void {
    this.listener = listener;
    listener(this.backdrop, this.colorMode);
  }

  unbind(): void {
    this.listener = undefined;
  }

  setBackdrop(backdrop: string): void {
    this.backdrop = backdrop;
    this.listener?.(this.backdrop, this.colorMode);
  }

  setColorMode(colorMode: ThemeColorMode): void {
    this.colorMode = colorMode;
    this.listener?.(this.backdrop, this.colorMode);
  }
}

@Entry({ routeName: "nshSubWindow" })
@Component
struct NshSubWindowPage {
  private bridge?: SubWindowPageBridge;
  private content: NodeContent = new NodeContent();
  @State private backdrop: string = "none";
  @State private colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;

  aboutToAppear(): void {
    try {
      this.getUIContext().setKeyboardAvoidMode(KeyboardAvoidMode.NONE);
    } catch {}
    const bridge = this.getUIContext()
      .getSharedLocalStorage()
      ?.get<SubWindowPageBridge>(NSH_SUBWINDOW_BRIDGE_KEY);
    if (bridge !== undefined) {
      this.bridge = bridge;
      this.content = bridge.content;
      this.backdrop = bridge.backdrop;
      this.colorMode = bridge.colorMode;
      bridge.bind((nextBackdrop: string, nextColorMode: ThemeColorMode): void => {
        this.backdrop = nextBackdrop;
        this.colorMode = nextColorMode;
      });
    }
  }

  aboutToDisappear(): void {
    this.bridge?.unbind();
  }

  build() {
    // Subwindow backdrop blur is applied natively on the Rust root node; keep
    // this ArkTS container transparent so blur is never stacked twice.
    WithTheme({ colorMode: this.colorMode }) {
      Stack() {
        ContentSlot(this.content);
      }
      .width("100%")
      .height("100%")
      .backgroundColor(Color.Transparent);
    };
  }
}
