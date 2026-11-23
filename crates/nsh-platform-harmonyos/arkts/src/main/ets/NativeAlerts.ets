import { UIContext } from "@ohos.arkui.UIContext";
import { ComponentContent, LengthMetrics } from "@ohos.arkui.node";
import {
  AdvancedDialogV2Button,
  AlertDialogV2,
  CustomContentDialogV2,
} from "@ohos.arkui.advanced.DialogV2";

interface NativeAlertButton {
  value?: string;
  title?: string;
  destructive?: boolean;
  style?: string;
  action: () => void;
}

export interface NativeAlertOptions {
  ui: UIContext;
  title?: string;
  message: string;
  autoCancel: boolean;
  sheet: boolean;
  appearance?: string;
  buttons?: NativeAlertButton[];
  sheets?: NativeAlertButton[];
  confirm?: NativeAlertButton;
  onDidAppear?: () => void;
  onWillDisappear?: () => void;
  onDidDisappear: () => void;
}

function parseColorMode(appearance?: string): ThemeColorMode {
  if (appearance === "dark") {
    return ThemeColorMode.DARK;
  }
  if (appearance === "light") {
    return ThemeColorMode.LIGHT;
  }
  return ThemeColorMode.SYSTEM;
}

function isDestructive(item: NativeAlertButton): boolean {
  return item.style === "destructive" || item.destructive === true;
}

class AlertContent {
  options: NativeAlertOptions;
  colorMode: ThemeColorMode;
  buttons: AdvancedDialogV2Button[];
  actions: NativeAlertButton[];
  cancel?: NativeAlertButton;
  hasHeader: boolean;
  select: (button: NativeAlertButton) => void;
  constructor(options: NativeAlertOptions, close: () => void) {
    this.options = options;
    this.colorMode = parseColorMode(options.appearance);
    const button = (item: NativeAlertButton): AdvancedDialogV2Button =>
      new AdvancedDialogV2Button({
        content: item.value ?? item.title ?? "",
        role: isDestructive(item) ? ButtonRole.ERROR : ButtonRole.NORMAL,
        action: (): void => {
          item.action();
          close();
        },
      });
    this.buttons = (options.buttons ?? []).map(button);
    this.actions = (options.sheets ?? []).slice();
    this.cancel = options.confirm;
    this.hasHeader = (options.title ?? "").length > 0 || (options.message ?? "").length > 0;
    this.select = (item: NativeAlertButton): void => {
      item.action();
      close();
    };
  }
}

@ComponentV2
struct MultiButtonAlert {
  @Param title?: string = undefined;
  @Param message: string = "";
  @Param buttons: AdvancedDialogV2Button[] = [];

  @Builder
  private buildMessage() {
    if (this.message.length > 0) {
      Text(this.message)
        .fontSize(
          this.title && this.title.length > 0 ? $r("sys.float.Body_M") : $r("sys.float.Body_L"),
        )
        .fontColor(
          this.title && this.title.length > 0
            ? $r("sys.color.font_secondary")
            : $r("sys.color.font_primary"),
        )
        .width("100%")
        .textAlign(TextAlign.Center);
    }
  }

  build() {
    CustomContentDialogV2({
      primaryTitle: this.title,
      contentAreaPadding: {
        top: LengthMetrics.vp(0),
        bottom: LengthMetrics.vp(8),
        start: LengthMetrics.vp(24),
        end: LengthMetrics.vp(24),
      },
      contentBuilder: (): void => {
        this.buildMessage();
      },
      buttons: this.buttons,
    });
  }
}

@Builder
function nativeAlert(content: AlertContent) {
  WithTheme({ colorMode: content.colorMode }) {
    if (content.options.sheet) {
      Column() {
        if (content.hasHeader) {
          Column({ space: 4 }) {
            if ((content.options.title ?? "").length > 0) {
              Text(content.options.title)
                .fontSize($r("sys.float.Body_M"))
                .fontWeight(FontWeight.Medium)
                .fontColor($r("sys.color.font_secondary"))
                .textAlign(TextAlign.Center)
                .width("100%");
            }
            if ((content.options.message ?? "").length > 0) {
              Text(content.options.message)
                .fontSize($r("sys.float.Body_S"))
                .fontWeight(FontWeight.Regular)
                .fontColor($r("sys.color.font_tertiary"))
                .textAlign(TextAlign.Center)
                .width("100%");
            }
          }
          .width("100%")
          .padding({ left: 16, right: 16, top: 4, bottom: 12 });
        }
        if (content.actions.length > 0) {
          Column() {
            ForEach(content.actions, (item: NativeAlertButton, index: number) => {
              if (index > 0) {
                Divider().color($r("sys.color.comp_divider")).strokeWidth("1px");
              }
              Button(item.title ?? item.value ?? "", { type: ButtonType.Normal })
                .width("100%")
                .constraintSize({ minHeight: 46 })
                .fontSize(15)
                .fontWeight(FontWeight.Regular)
                .fontColor(
                  isDestructive(item) ? $r("sys.color.warning") : $r("sys.color.font_primary"),
                )
                .backgroundColor(Color.Transparent)
                .onClick((): void => content.select(item));
            })
          }
          .width("100%")
          .backgroundColor($r("sys.color.comp_background_list_card"))
          .borderRadius($r("sys.float.corner_radius_level8"))
          .clip(true);
        }
        if (content.cancel) {
          Column() {
            Button(content.cancel.value ?? content.cancel.title ?? "", { type: ButtonType.Normal })
              .width("100%")
              .constraintSize({ minHeight: 46 })
              .fontSize(15)
              .fontWeight(FontWeight.Medium)
              .fontColor($r("sys.color.font_emphasize"))
              .backgroundColor(Color.Transparent)
              .onClick((): void => {
                if (content.cancel) {
                  content.select(content.cancel);
                }
              })
          }
          .width("100%")
          .margin({ top: content.actions.length > 0 ? 8 : 0 })
          .backgroundColor($r("sys.color.comp_background_list_card"))
          .borderRadius($r("sys.float.corner_radius_level8"))
          .clip(true);
        }
      }
      .id("nsh-native-sheet")
      .width("100%")
      .padding({
        left: $r("sys.float.padding_level8"),
        right: $r("sys.float.padding_level8"),
        top: $r("sys.float.padding_level6"),
        bottom: $r("sys.float.padding_level8"),
      });
    } else {
      Column() {
        if (content.buttons.length <= 2) {
          AlertDialogV2({
            primaryTitle: content.options.title,
            content: content.options.message,
            primaryButton: content.buttons[0],
            secondaryButton: content.buttons[1],
          });
        } else {
          MultiButtonAlert({
            title: content.options.title,
            message: content.options.message,
            buttons: content.buttons,
          });
        }
      }
      .width("82%")
      .constraintSize({ maxWidth: 360 })
      .borderRadius($r("sys.float.ohos_id_corner_radius_dialog"))
      .backgroundBlurStyle(BlurStyle.COMPONENT_ULTRA_THICK, { colorMode: content.colorMode })
      .shadow(ShadowStyle.OUTER_DEFAULT_MD)
      .clip(true);
    }
  };
}

// Return a closer immediately. Close requests during native presentation take
// effect as soon as the same dialog is attached to this window.
export function showNativeAlert(options: NativeAlertOptions): () => void {
  const prompt = options.ui.getPromptAction();
  let mounted = false;
  let closing = false;
  let finished = false;
  let view: ComponentContent<AlertContent> | undefined;
  const finish = (): void => {
    if (finished) {
      return;
    }
    finished = true;
    view?.dispose();
    options.onDidDisappear();
  };
  const close = (): void => {
    closing = true;
    if (mounted && view && !finished) {
      mounted = false;
      try {
        const dismissal = options.sheet
          ? options.ui.closeBindSheet(view)
          : prompt.closeCustomDialog(view);
        dismissal.catch((error: Error) => {
          console.error("NSH dialog close", error.message);
          mounted = !finished;
        });
      } catch (error) {
        console.error("NSH dialog close", (error as Error).message);
        mounted = !finished;
      }
    }
  };
  view = new ComponentContent(
    options.ui,
    wrapBuilder(nativeAlert),
    new AlertContent(options, close),
  );
  const allowDismiss = (): boolean => closing || options.autoCancel;
  const sheetOptions: SheetOptions = {
    height: SheetSize.FIT_CONTENT,
    dragBar: false,
    backgroundColor: $r("sys.color.background_secondary"),
    showClose: options.autoCancel && !options.confirm,
    onWillDismiss: (event: DismissSheetAction): void => {
      if (allowDismiss()) {
        event.dismiss();
      }
    },
    onAppear: options.onDidAppear,
    onWillDisappear: options.onWillDisappear,
    onDisappear: finish,
  };
  try {
    const presentation = options.sheet
      ? options.ui.openBindSheet(view, sheetOptions)
      : prompt.openCustomDialog(view, {
          autoCancel: options.autoCancel,
          onWillDismiss: (event: DismissDialogAction): void => {
            if (allowDismiss()) {
              event.dismiss();
            }
          },
          onDidAppear: options.onDidAppear,
          onWillDisappear: options.onWillDisappear,
          onDidDisappear: finish,
        });
    presentation
      .then((): void => {
        mounted = !finished;
        if (closing) {
          close();
        }
      })
      .catch((error: Error) => {
        console.error("NSH dialog open", error.message);
        finish();
      });
  } catch (error) {
    console.error("NSH dialog open", (error as Error).message);
    finish();
  }
  return close;
}
