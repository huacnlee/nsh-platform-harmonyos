import { ComponentContent } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import webview from "@ohos.web.webview";
import { SystemReply } from "./NativeSystem";

function matchesCallbackScheme(url: string, callbackScheme: string): boolean {
  const scheme = callbackScheme.trim().toLowerCase();
  if (scheme.length === 0) {
    return false;
  }
  return url.trim().toLowerCase().startsWith(`${scheme}:`);
}

function isDisallowedAuthScheme(url: string): boolean {
  const lower = url.trim().toLowerCase();
  return (
    lower.startsWith("file:") ||
    lower.startsWith("vbscript:") ||
    lower.startsWith("content:") ||
    lower.startsWith("resource:")
  );
}

function isHttpUrl(url: string): boolean {
  const lower = url.trim().toLowerCase();
  return lower.startsWith("https://") || lower.startsWith("http://");
}

function extractHostLabel(url: string): string {
  const match = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/([^/?#]+)/.exec(url.trim());
  return match && match[1] ? match[1] : url;
}

@Observed
class AuthSessionUiState {
  hostLabel: string;
  progress: number = 0;
  loading: boolean = true;

  constructor(url: string) {
    this.hostLabel = extractHostLabel(url);
  }
}

interface AuthSessionViewParams {
  url: string;
  callbackScheme: string;
  ephemeral: boolean;
  colorMode: ThemeColorMode;
  controller: webview.WebviewController;
  state: AuthSessionUiState;
  onComplete: (callbackUrl: string) => void;
  onCancel: () => void;
}

@Component
struct AuthSessionSheet {
  params!: AuthSessionViewParams;
  @ObjectLink state!: AuthSessionUiState;

  build() {
    WithTheme({ colorMode: this.params.colorMode }) {
      Column() {
        Row({ space: 8 }) {
          Text(this.state.hostLabel)
            .fontSize($r("sys.float.Body_M"))
            .fontWeight(FontWeight.Medium)
            .fontColor($r("sys.color.font_primary"))
            .maxLines(1)
            .textOverflow({ overflow: TextOverflow.Ellipsis })
            .layoutWeight(1);
          Button({ type: ButtonType.Circle, stateEffect: true }) {
            SymbolGlyph($r("sys.symbol.xmark"))
              .fontSize(16)
              .fontColor([$r("sys.color.icon_primary")]);
          }
          .width(32)
          .height(32)
          .backgroundColor($r("sys.color.comp_background_tertiary"))
          .onClick((): void => {
            this.params.onCancel();
          });
        }
        .width("100%")
        .height(48)
        .padding({ left: 16, right: 12 })
        .alignItems(VerticalAlign.Center);

        if (this.state.loading) {
          Progress({ value: this.state.progress, total: 100, type: ProgressType.Linear })
            .width("100%")
            .height(2)
            .color($r("sys.color.brand"))
            .backgroundColor(Color.Transparent);
        } else {
          Divider().color($r("sys.color.comp_divider")).strokeWidth("1px");
        }

        Web({
          src: this.params.url,
          controller: this.params.controller,
          incognitoMode: this.params.ephemeral,
        })
          .width("100%")
          .layoutWeight(1)
          .javaScriptAccess(true)
          .domStorageAccess(true)
          .fileAccess(false)
          .onlineImageAccess(true)
          .imageAccess(true)
          .onLoadIntercept((event): boolean => {
            const url = event.data.getRequestUrl();
            const isMainFrame = event.data.isMainFrame();
            if (isMainFrame && matchesCallbackScheme(url, this.params.callbackScheme)) {
              this.params.onComplete(url);
              return true;
            }
            if (isDisallowedAuthScheme(url)) {
              return true;
            }
            if (isMainFrame && isHttpUrl(url)) {
              this.state.hostLabel = extractHostLabel(url);
            }
            return false;
          })
          .onPageBegin((event): void => {
            this.state.loading = true;
            if (event && event.url && isHttpUrl(event.url)) {
              this.state.hostLabel = extractHostLabel(event.url);
            }
          })
          .onProgressChange((event): void => {
            if (event) {
              this.state.progress = event.newProgress;
              if (event.newProgress >= 100) {
                this.state.loading = false;
              }
            }
          })
          .onPageEnd((): void => {
            this.state.loading = false;
            this.state.progress = 100;
          });
      }
      .width("92%")
      .height("84%")
      .constraintSize({ maxWidth: 520, maxHeight: 720, minHeight: 360 })
      .backgroundColor($r("sys.color.background_primary"))
      .borderRadius($r("sys.float.ohos_id_corner_radius_dialog"))
      .shadow(ShadowStyle.OUTER_DEFAULT_MD)
      .clip(true);
    };
  }
}

@Builder
function authSessionBuilder(params: AuthSessionViewParams) {
  AuthSessionSheet({ params: params, state: params.state });
}

interface ActiveAuthSession {
  key: number;
  ephemeral: boolean;
  controller: webview.WebviewController;
  view?: ComponentContent<AuthSessionViewParams>;
  opening: boolean;
  mounted: boolean;
  closing: boolean;
  settled: boolean;
  reply: (event: SystemReply) => void;
}

export class NativeAuthSession {
  private readonly ui: UIContext;
  private colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  private current?: ActiveAuthSession;
  private closed: boolean = false;

  constructor(ui: UIContext) {
    this.ui = ui;
  }

  setColorMode(mode: ThemeColorMode): void {
    this.colorMode = mode;
  }

  open(
    key: number,
    url: string,
    callbackScheme: string,
    ephemeral: boolean,
    reply: (event: SystemReply) => void,
  ): void {
    if (this.closed) {
      reply({ phase: "cancelled" } as SystemReply);
      return;
    }
    if (this.current !== undefined) {
      this.finishSession(this.current, { phase: "cancelled" } as SystemReply);
    }
    try {
      webview.WebviewController.initializeWebEngine();
      if (ephemeral) {
        webview.WebCookieManager.clearAllCookiesSync(true);
      }
    } catch {}
    const controller = new webview.WebviewController();
    const state = new AuthSessionUiState(url);
    const session: ActiveAuthSession = {
      key,
      ephemeral,
      controller,
      opening: true,
      mounted: false,
      closing: false,
      settled: false,
      reply,
    };
    this.current = session;

    const params: AuthSessionViewParams = {
      url,
      callbackScheme,
      ephemeral,
      colorMode: this.colorMode,
      controller,
      state,
      onComplete: (callbackUrl: string): void => {
        this.finishSession(session, { phase: "completed", uri: callbackUrl } as SystemReply);
      },
      onCancel: (): void => {
        this.finishSession(session, { phase: "cancelled" } as SystemReply);
      },
    };
    const view = new ComponentContent<AuthSessionViewParams>(
      this.ui,
      wrapBuilder(authSessionBuilder),
      params,
      { nestingBuilderSupported: true },
    );
    session.view = view;

    try {
      this.ui
        .getPromptAction()
        .openCustomDialog(view, {
          autoCancel: true,
          onWillDismiss: (action: DismissDialogAction): void => {
            action.dismiss();
          },
          onDidDisappear: (): void => {
            session.opening = false;
            session.mounted = false;
            this.disposeView(session);
            if (!session.settled) {
              this.settle(session, { phase: "cancelled" } as SystemReply);
            }
          },
        })
        .then((): void => {
          session.opening = false;
          session.mounted = true;
          if (session.closing) {
            this.dismissDialog(session);
          }
        })
        .catch((error: Error): void => {
          session.opening = false;
          session.mounted = false;
          this.disposeView(session);
          this.settle(session, {
            error: `openAuthSession: failed to present sign-in page: ${String(error)}`,
          } as SystemReply);
        });
    } catch (error) {
      session.opening = false;
      session.mounted = false;
      this.disposeView(session);
      this.settle(session, {
        error: `openAuthSession: failed to present sign-in page: ${String(error)}`,
      } as SystemReply);
    }
  }

  close(key: number): void {
    if (this.current !== undefined && this.current.key === key) {
      this.finishSession(this.current, { phase: "cancelled" } as SystemReply);
    }
  }

  cancel(key: number): void {
    this.close(key);
  }

  closeAll(): void {
    this.closed = true;
    if (this.current !== undefined) {
      const active = this.current;
      active.settled = true;
      this.current = undefined;
      if (active.ephemeral) {
        try {
          webview.WebCookieManager.clearAllCookiesSync(true);
        } catch {}
      }
      this.dismissDialog(active);
    }
  }

  private finishSession(session: ActiveAuthSession, event: SystemReply): void {
    this.settle(session, event);
    this.dismissDialog(session);
  }

  private settle(session: ActiveAuthSession, event: SystemReply): void {
    if (session.settled) {
      return;
    }
    session.settled = true;
    if (this.current === session) {
      this.current = undefined;
    }
    try {
      session.controller.stop();
      if (session.ephemeral) {
        webview.WebCookieManager.clearAllCookiesSync(true);
      }
    } catch {}
    session.reply(event);
  }

  private dismissDialog(session: ActiveAuthSession): void {
    session.closing = true;
    const view = session.view;
    if (!view) {
      return;
    }
    if (session.opening) {
      // Wait for openCustomDialog to resolve before closing so the dialog is
      // never stranded open with a disposed content node.
      return;
    }
    if (!session.mounted) {
      this.disposeView(session);
      return;
    }
    session.mounted = false;
    try {
      this.ui
        .getPromptAction()
        .closeCustomDialog(view)
        .catch((): void => {
          this.disposeView(session);
        });
    } catch {
      this.disposeView(session);
    }
  }

  private disposeView(session: ActiveAuthSession): void {
    const view = session.view;
    if (view !== undefined) {
      session.view = undefined;
      try {
        view.dispose();
      } catch {}
    }
  }
}
