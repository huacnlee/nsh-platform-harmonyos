import { FrameCallback } from "@ohos.arkui.UIContext";
import { BuilderNode } from "@ohos.arkui.node";
import picker from "@ohos.file.picker";
import webview from "@ohos.web.webview";
import { NshRuntimeContext } from "./NativeEditors";

export interface WebSettings {
  sharedCookies: boolean;
  clearCookies: boolean;
  targetId: number;
  url?: string | null;
  html?: string | null;
  htmlBaseUrl?: string | null;
  loadGeneration: number;
  javaScriptEnabled: boolean;
  scrollEnabled: boolean;
  userAgent?: string | null;
  injectedJavascript?: string | null;
  documentStartScript: string;
  downloadDirectory: string;
}

export interface WebHeader {
  headerKey: string;
  headerValue: string;
}

export interface WebCommand {
  targetId: number;
  kind: string;
  url?: string | null;
  headers?: WebHeader[] | null;
  script?: string | null;
  x?: number | null;
  y?: number | null;
  requestId?: string | null;
  downloadId?: string | null;
  path?: string | null;
}

let webEngineInitialized = false;
let webComponentMounted = false;
let nextWebNativeTagId = 1;

function ensureWebEngineInitialized(): void {
  if (!webEngineInitialized) {
    webview.WebviewController.initializeWebEngine();
    webEngineInitialized = true;
  }
}

/** Prepare ArkWeb on the UI thread during standalone launch. A failed attempt is retried at first mount. */
export function prepareWebEngine(): void {
  try {
    ensureWebEngineInitialized();
  } catch (error) {
    console.warn(`[NSH] ArkWeb startup initialization: ${String(error)}`);
  }
}

// Clear the selected store before the controller's first navigation, including
// settings queued while its native node was still attaching.
function clearCookiesIfNeeded(entry: WebEntry): void {
  if (!entry.settings.clearCookies) {
    return;
  }
  try {
    webview.WebCookieManager.clearAllCookiesSync(!entry.settings.sharedCookies);
    entry.settings.clearCookies = false;
  } catch (error) {
    entry.emit("error", `Cookie reset failed: ${String(error)}`);
    throw new Error(String(error));
  }
}

@Observed
class WebViewState {
  revealed: boolean = false;
}

@Component
struct WebViewHost {
  entry!: WebEntry;
  @ObjectLink state!: WebViewState;

  build() {
    Web({
      src: "",
      controller: this.entry.controller,
      renderMode: RenderMode.SYNC_RENDER,
      incognitoMode: !this.entry.settings.sharedCookies,
    })
      .id(`nsh-web-${this.entry.settings.targetId}`)
      .width("100%")
      .height("100%")
      .opacity(this.state.revealed ? 1 : 0)
      .backgroundColor(Color.Transparent)
      .hitTestBehavior(HitTestMode.Block)
      .javaScriptAccess(this.entry.settings.javaScriptEnabled)
      .domStorageAccess(true)
      .fileAccess(false)
      .onlineImageAccess(true)
      .imageAccess(true)
      .runJavaScriptOnDocumentStart([
        {
          script: this.entry.settings.documentStartScript,
          scriptRules: ["*", "nsh-asset://"],
        },
      ])
      .onShowFileSelector((event): boolean => {
        if (!event) {
          return false;
        }
        this.entry.selectFiles(event.fileSelector, event.result);
        return true;
      })
      .onControllerAttached(() => {
        this.entry.onControllerAttached();
      })
      .onLoadIntercept((event): boolean => {
        return this.entry.onLoadIntercept(event.data.getRequestUrl());
      })
      .onPageBegin(() => {
        this.entry.onPageBegin();
      })
      .onPageVisible((event) => {
        this.entry.onPageVisible(event.url);
      })
      .onFirstContentfulPaint(() => {
        this.entry.onFirstPaint();
      })
      .onFirstMeaningfulPaint(() => {
        this.entry.onFirstPaint();
      })
      .onPageEnd(() => {
        this.entry.onPageEnd();
      })
      .onTitleReceive(() => {
        this.entry.emit("title");
      })
      .onFocus(() => {
        this.entry.emit("focus");
      })
      .onBlur(() => {
        this.entry.emit("blur");
      })
      .onErrorReceive((event) => {
        if (event.request.isMainFrame()) {
          this.entry.onFirstBufferReady();
          this.entry.emit("error", event.error.getErrorInfo(), event.error.getErrorCode());
        }
      })
      .onRenderExited(() => {
        this.entry.emit("render-exited");
      });
  }
}

interface WebContentParams {
  entry: WebEntry;
}

@Builder
function webContent(params: WebContentParams) {
  WebViewHost({ entry: params.entry, state: params.entry.viewState });
}

const quarantinedWebs: Set<WebEntry> = new Set();

class WebMountFrame extends FrameCallback {
  private entry: WebEntry;
  constructor(entry: WebEntry) {
    super();
    this.entry = entry;
  }
  onFrame(_frameTime: number): void {
    // The navigation frame must return to ArkUI before ArkWeb starts.
    setTimeout((): void => this.entry.mountAfterFrame(), 0);
  }
}

class WebEntry {
  readonly nativeTag: string = `nsh.web.${nextWebNativeTagId++}`;
  readonly controller: webview.WebviewController = new webview.WebviewController(this.nativeTag);
  readonly node: BuilderNode<[WebContentParams]>;
  readonly viewState: WebViewState = new WebViewState();
  settings: WebSettings;
  private context: NshRuntimeContext;
  private attached: boolean = false;
  private wasWarmMount: boolean = false;
  private controllerObserved: boolean = false;
  private controllerReady: boolean = false;
  private attachFinalized: boolean = false;
  private sourceApplied: boolean = false;
  private sourceEpoch: number = 0;
  private appliedLoadGeneration: number = 0;
  private pendingCommands: WebCommand[] = [];
  private inputCommands: Promise<void> = Promise.resolve();
  private fileSelectors: Set<FileSelectorResult> = new Set();
  private downloads: Map<string, webview.WebDownloadItem> = new Map();
  private downloadGuids: Map<string, string> = new Map();
  private startedDownloads: Set<string> = new Set();
  private pendingCancels: Map<string, string[]> = new Map();
  private cancelledDownloads: Set<string> = new Set();
  private tearingDownDownloads: Set<string> = new Set();
  private teardownReleasePending: boolean = false;
  private nextDownloadSeq: number = 1;
  private readonly downloadDelegate: webview.WebDownloadDelegate =
    new webview.WebDownloadDelegate();
  private inputTimestamp: number = 0;
  private loading: boolean = false;
  private epoch: number = 0;
  private sdkBusy: boolean = false;
  private disposePending: boolean = false;
  private closed: boolean = false;
  private retired: boolean = false;
  private disposed: boolean = false;

  constructor(settings: WebSettings, context: NshRuntimeContext) {
    this.settings = settings;
    this.context = context;
    this.node = new BuilderNode<[WebContentParams]>(context.getUIContext());
  }

  create(): void {
    // Let the settled navigation or commit frame present before creating NWeb.
    this.context.getUIContext().postFrameCallback(new WebMountFrame(this));
  }

  mountAfterFrame(): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    try {
      this.mount();
    } catch (error) {
      console.error(`[NSH] deferred WebView mount: ${String(error)}`);
    }
  }

  private mount(): void {
    this.sdkBusy = true;
    try {
      this.wasWarmMount = webComponentMounted;
      ensureWebEngineInitialized();
      this.node.build(wrapBuilder(webContent), { entry: this }, { nestingBuilderSupported: true });
      if (this.closed || !this.context.isActive()) {
        throw new Error("WebView creation was cancelled");
      }
      const accepted = this.context.attachNativeNode(
        this.settings.targetId,
        this.node.getFrameNode(),
      );
      if (!accepted || this.closed) {
        throw new Error("Native WebView target is no longer live");
      }
      this.attached = true;
      webComponentMounted = true;
      if (this.controllerObserved && !this.attachFinalized) {
        this.finishControllerAttach();
      }
    } catch (error) {
      try {
        if (!this.closed && this.context.isActive()) {
          this.context.webEvent({
            targetId: this.settings.targetId,
            phase: "error",
            url: this.settings.url ?? "",
            title: "",
            loading: false,
            canGoBack: false,
            canGoForward: false,
            message: String(error),
          });
        }
      } finally {
        this.dispose();
      }
      throw new Error(String(error));
    } finally {
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.release();
      }
    }
  }

  onControllerAttached(): void {
    try {
      clearCookiesIfNeeded(this);
    } catch (_) {
      return;
    }
    this.controllerObserved = true;
    if (!this.attached || this.attachFinalized || this.closed || !this.context.isActive()) {
      return;
    }
    this.finishControllerAttach();
  }

  private finishControllerAttach(): void {
    if (this.attachFinalized || this.closed || !this.context.isActive()) {
      return;
    }
    this.attachFinalized = true;
    try {
      this.controller.setScrollable(this.settings.scrollEnabled);
      this.controller.setCustomUserAgent(this.settings.userAgent ?? "");
      if (!this.context.webControllerAttached(this.settings.targetId, this.nativeTag)) {
        throw new Error("Native WebView controller attachment was rejected");
      }
      this.downloadDelegate.onBeforeDownload((item: webview.WebDownloadItem) => {
        if (
          this.closed ||
          !this.context.isActive() ||
          !this.context.webDownloadAllowed(this.settings.targetId, item.getUrl())
        ) {
          item.cancel();
          return;
        }
        const guid = item.getGuid();
        const id =
          guid.length > 0 && !this.downloads.has(guid)
            ? guid
            : `ohos-dl-${this.settings.targetId}-${this.nextDownloadSeq++}`;
        this.downloads.set(id, item);
        if (guid.length > 0) {
          this.downloadGuids.set(guid, id);
        }
        this.emitDownloadOffer(id, item);
      });
      this.downloadDelegate.onDownloadUpdated((item: webview.WebDownloadItem) => {
        const id = this.resolveDownloadId(item);
        if (!id || !this.startedDownloads.has(id) || this.cancelledDownloads.has(id)) {
          return;
        }
        this.emitDownloadProgress(id, item.getReceivedBytes());
      });
      this.downloadDelegate.onDownloadFinish((item: webview.WebDownloadItem) => {
        const id = this.resolveDownloadId(item);
        if (!id) {
          return;
        }
        this.forgetDownload(id, item.getGuid());
        const wasCancelled = this.cancelledDownloads.delete(id);
        const wasTeardown = this.tearingDownDownloads.delete(id);
        const hadPendingCancel = this.resolvePendingCancels(id, true);
        if (!wasCancelled && !hadPendingCancel) {
          if (wasTeardown) {
            this.emitDownloadEnd(
              id,
              "failed",
              0,
              "",
              "the web view went away",
              undefined,
              "ERR_NSH_DOWNLOAD_TEARDOWN",
            );
          } else {
            this.emitDownloadEnd(id, "finished", item.getReceivedBytes(), item.getFullPath());
          }
        }
        this.maybeFinishTeardown();
      });
      this.downloadDelegate.onDownloadFailed((item: webview.WebDownloadItem) => {
        const id = this.resolveDownloadId(item);
        if (!id) {
          return;
        }
        this.forgetDownload(id, item.getGuid());
        const wasCancelled = this.cancelledDownloads.delete(id);
        const wasTeardown = this.tearingDownDownloads.delete(id);
        const hadPendingCancel = this.resolvePendingCancels(id, true);
        if (!wasCancelled && !hadPendingCancel) {
          if (wasTeardown) {
            this.emitDownloadEnd(
              id,
              "failed",
              0,
              "",
              "the web view went away",
              undefined,
              "ERR_NSH_DOWNLOAD_TEARDOWN",
            );
          } else {
            const errorCode = item.getLastErrorCode();
            this.emitDownloadEnd(
              id,
              "failed",
              item.getReceivedBytes(),
              "",
              `download failed (${errorCode})`,
              errorCode,
            );
          }
        }
        this.maybeFinishTeardown();
      });
      this.controller.setDownloadDelegate(this.downloadDelegate);
      const startInitialLoad = (): void => {
        if (this.closed || !this.context.isActive()) {
          return;
        }
        try {
          this.controllerReady = true;
          const queued = this.pendingCommands.splice(0);
          if (this.settings.url === "about:blank" && queued.some((cmd) => cmd.kind === "load")) {
            clearCookiesIfNeeded(this);
            this.appliedLoadGeneration = this.settings.loadGeneration;
          } else {
            this.applySource();
          }
          for (const cmd of queued) {
            this.command(cmd);
          }
          this.emit("ready");
        } catch (error) {
          this.emit("error", String(error));
        }
      };
      if (!this.wasWarmMount) {
        // Yield after cold CreateNWeb so initial navigation doesn't stack in the same turn.
        setTimeout(startInitialLoad, 0);
      } else {
        startInitialLoad();
      }
    } catch (error) {
      this.emit("error", String(error));
    }
  }

  update(settings: WebSettings): void {
    if (this.closed) {
      throw new Error("Native WebView is retired");
    }
    if (settings.targetId !== this.settings.targetId) {
      throw new Error("WebView replacement requires a new target");
    }
    settings.clearCookies = settings.clearCookies || this.settings.clearCookies;
    this.settings = settings;
    if (!this.controllerReady) {
      return;
    }
    try {
      this.controller.setScrollable(settings.scrollEnabled);
      this.controller.setCustomUserAgent(settings.userAgent ?? "");
      if (settings.loadGeneration !== this.appliedLoadGeneration) {
        this.applySource();
      }
    } catch (error) {
      this.emit("error", String(error));
    }
  }

  private applySource(): void {
    clearCookiesIfNeeded(this);
    this.appliedLoadGeneration = this.settings.loadGeneration;
    this.sourceApplied = true;
    this.sourceEpoch = this.epoch;
    if (this.settings.url != null) {
      this.controller.loadUrl(this.settings.url);
    } else {
      this.controller.loadData(
        this.settings.html ?? "",
        "text/html",
        "UTF-8",
        this.settings.htmlBaseUrl ?? "about:blank",
        " ",
      );
    }
  }

  selectFiles(selector: FileSelectorParam, result: FileSelectorResult): void {
    this.fileSelectors.add(result);
    const epoch = this.epoch;
    const finish = (uris: string[]): void => {
      if (this.fileSelectors.delete(result)) {
        result.handleFileList(
          this.closed || !this.context.isActive() || epoch !== this.epoch ? [] : uris,
        );
      }
    };
    this.pickFiles(selector)
      .then(finish)
      .catch((error: Error) => {
        console.warn(`NSH WebView file picker: ${String(error)}`);
        finish([]);
      });
  }

  private async pickFiles(selector: FileSelectorParam): Promise<string[]> {
    if (this.closed || !this.context.isActive()) {
      return [];
    }
    const mode = selector.getMode();
    if (mode === FileSelectorMode.FileSaveMode) {
      return [];
    }
    const limit = mode === FileSelectorMode.FileOpenMultipleMode ? 100 : 1;
    const accept = selector.getAcceptType().map((type: string) => type.toLowerCase());
    const host = this.context.getUIContext().getHostContext();
    if (!host) {
      return [];
    }
    const media =
      accept.length > 0 &&
      accept.every((type: string) => type.startsWith("image/") || type.startsWith("video/"));
    if (media && mode !== FileSelectorMode.FileOpenFolderMode) {
      const photos = new picker.PhotoViewPicker(host);
      const options = new picker.PhotoSelectOptions();
      options.maxSelectNumber = limit;
      options.MIMEType = accept.every((type: string) => type.startsWith("image/"))
        ? picker.PhotoViewMIMETypes.IMAGE_TYPE
        : accept.every((type: string) => type.startsWith("video/"))
          ? picker.PhotoViewMIMETypes.VIDEO_TYPE
          : picker.PhotoViewMIMETypes.IMAGE_VIDEO_TYPE;
      return (await photos.select(options)).photoUris;
    }
    const documents = new picker.DocumentViewPicker(host);
    const options = new picker.DocumentSelectOptions();
    options.maxSelectNumber = limit;
    if (mode === FileSelectorMode.FileOpenFolderMode) {
      options.selectMode = picker.DocumentSelectMode.FOLDER;
    }
    const suffixes = accept.filter((type: string) => /^\.[a-z0-9]+$/.test(type));
    if (suffixes.length > 0) {
      options.fileSuffixFilters = suffixes;
    }
    return documents.select(options);
  }

  onLoadIntercept(url: string): boolean {
    if (this.closed || !this.context.isActive()) {
      return true;
    }
    return this.context.webNavigate(this.settings.targetId, url);
  }

  onPageBegin(): void {
    this.loading = true;
    this.epoch++;
    this.emit("start");
  }

  onPageVisible(url: string): void {
    if (!this.sourceApplied) {
      return;
    }
    if (url.length === 0 || (url === "about:blank" && this.settings.url !== "about:blank")) {
      return;
    }
    this.onFirstBufferReady();
  }

  onFirstPaint(): void {
    if (this.sourceApplied && this.epoch > this.sourceEpoch) {
      this.onFirstBufferReady();
    }
  }

  onFirstBufferReady(): void {
    if (!this.closed && this.sourceApplied && !this.viewState.revealed) {
      this.viewState.revealed = true;
    }
  }

  onPageEnd(): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    this.onFirstBufferReady();
    const epoch = this.epoch;
    const script = this.settings.injectedJavascript;
    if (script && this.settings.javaScriptEnabled) {
      this.controller.runJavaScript(script).then(
        () => {
          if (!this.closed && epoch === this.epoch) {
            this.loading = false;
            this.emit("finish");
          }
        },
        (error: Error) => {
          if (!this.closed && epoch === this.epoch) {
            this.loading = false;
            this.emit("error", String(error));
          }
        },
      );
    } else {
      this.loading = false;
      this.emit("finish");
    }
  }

  emit(phase: string, message?: string, code?: number): void {
    if (!this.attached || this.closed || !this.context.isActive()) {
      return;
    }
    if (phase === "error") {
      this.loading = false;
    }
    let url = "";
    let title = "";
    let canGoBack = false;
    let canGoForward = false;
    if (this.controllerReady) {
      try {
        url = this.controller.getUrl();
        title = this.controller.getTitle();
        canGoBack = this.controller.accessBackward();
        canGoForward = this.controller.accessForward();
      } catch {}
    }
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase,
      url,
      title,
      loading: this.loading,
      canGoBack,
      canGoForward,
      message,
      code,
    });
  }

  private resolveDownloadId(item: webview.WebDownloadItem): string | undefined {
    const guid = item.getGuid();
    if (guid.length > 0) {
      const mapped = this.downloadGuids.get(guid);
      if (mapped !== undefined) {
        return mapped;
      }
      if (this.downloads.has(guid)) {
        return guid;
      }
    }
    for (const entry of this.downloads.entries()) {
      if (entry[1] === item) {
        return entry[0];
      }
    }
    return undefined;
  }

  private forgetDownload(id: string, guid: string): void {
    this.downloads.delete(id);
    this.startedDownloads.delete(id);
    if (guid.length > 0 && this.downloadGuids.get(guid) === id) {
      this.downloadGuids.delete(guid);
    }
  }

  private resolvePendingCancels(id: string, ok: boolean, message?: string): boolean {
    const pending = this.pendingCancels.get(id);
    if (!pending) {
      return false;
    }
    this.pendingCancels.delete(id);
    for (const requestId of pending) {
      this.emitWebReply(requestId, ok, message, id, ok ? "cancel-ok" : "error");
    }
    return true;
  }

  private emitWebReply(
    requestId: string,
    ok: boolean,
    message?: string,
    downloadId: string = "",
    state?: string,
  ): void {
    if (!requestId || !this.context.isActive()) {
      return;
    }
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase: "web-reply",
      url: "",
      title: "",
      loading: false,
      canGoBack: false,
      canGoForward: false,
      message,
      download: {
        state: state ?? (ok ? "ok" : "error"),
        id: downloadId,
        filename: "",
        path: "",
        bytes: 0,
        requestId,
        ok,
      },
    });
  }

  private emitDownloadOffer(id: string, item: webview.WebDownloadItem): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    const mimeType = item.getMimeType();
    const method = item.getMethod();
    const totalBytes = item.getTotalBytes();
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase: "download-offer",
      url: item.getUrl(),
      title: "",
      loading: false,
      canGoBack: false,
      canGoForward: false,
      download: {
        state: "offer",
        id,
        filename: item.getSuggestedFileName() || "download",
        path: "",
        bytes: totalBytes > 0 ? totalBytes : -1,
        mimeType: mimeType.length > 0 ? mimeType : undefined,
        method: method.length > 0 ? method : undefined,
      },
    });
  }

  private emitDownloadProgress(id: string, receivedBytes: number): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase: "download-progress",
      url: "",
      title: "",
      loading: false,
      canGoBack: false,
      canGoForward: false,
      download: {
        state: "progress",
        id,
        filename: "",
        path: "",
        bytes: receivedBytes,
      },
    });
  }

  private emitDownloadEnd(
    id: string,
    state: string,
    bytes: number,
    path: string,
    message?: string,
    code?: number,
    errorCode?: string,
  ): void {
    if (!this.context.isActive()) {
      return;
    }
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase: "download-end",
      url: "",
      title: "",
      loading: false,
      canGoBack: false,
      canGoForward: false,
      message,
      code,
      download: {
        state,
        id,
        filename: "",
        path,
        bytes,
        errorCode,
      },
    });
  }

  private isDownloadTerminal(item: webview.WebDownloadItem): boolean {
    try {
      const state = item.getState();
      return (
        state === webview.WebDownloadState.CANCELED ||
        state === webview.WebDownloadState.INTERRUPTED ||
        state === webview.WebDownloadState.COMPLETED
      );
    } catch {
      return true;
    }
  }

  private maybeFinishTeardown(): void {
    if (this.teardownReleasePending && this.downloads.size === 0 && !this.disposed && !this.sdkBusy) {
      this.teardownReleasePending = false;
      try {
        this.release();
      } catch (error) {
        console.warn(`[NSH] deferred WebView teardown release failed: ${String(error)}`);
      }
    }
  }
  private click(x: number, y: number): void {
    this.context
      .getUIContext()
      .getFocusController()
      .requestFocus(`nsh-web-${this.settings.targetId}`);
    const time = Math.max(Date.now() * 1000000, this.inputTimestamp + 2000000);
    this.inputTimestamp = time;
    x = this.context.getUIContext().vp2px(x);
    y = this.context.getUIContext().vp2px(y);
    for (const type of [TouchType.Down, TouchType.Up]) {
      const touch: TouchObject = {
        type,
        id: 0,
        displayX: x,
        displayY: y,
        windowX: x,
        windowY: y,
        screenX: x,
        screenY: y,
        x,
        y,
      };
      const event: TouchEvent = {
        type,
        touches: type === TouchType.Up ? [] : [touch],
        changedTouches: [touch],
        timestamp: time + (type === TouchType.Up ? 1000000 : 0),
        source: SourceType.TouchScreen,
        sourceTool: SourceTool.Finger,
        pressure: 1,
        tiltX: 0,
        tiltY: 0,
        target: {
          area: { position: { x: 0, y: 0 }, globalPosition: { x: 0, y: 0 }, width: 0, height: 0 },
        },
        stopPropagation: (): void => {},
        preventDefault: (): void => {},
        getHistoricalPoints: (): HistoricalPoint[] => [],
      };
      if (!this.node.postTouchEvent(event)) {
        throw new Error("ArkWeb rejected the native touch event");
      }
    }
  }
  private key(name: string): void {
    const code =
      name === "Enter" ? 2054 : name === "Tab" ? 2049 : name === "Backspace" ? 2055 : 2070;
    for (const type of [KeyType.Down, KeyType.Up]) {
      const event: KeyEvent = {
        type,
        keyCode: code,
        keyText: name,
        keySource: KeySource.Keyboard,
        deviceId: -1,
        metaKey: 0,
        timestamp: Date.now(),
        intentionCode: -1,
        stopPropagation: (): void => {},
      };
      const id = `nsh-web-${this.settings.targetId}`;
      const ui = this.context.getUIContextFor
        ? this.context.getUIContextFor(id)
        : this.context.getUIContext();
      ui.dispatchKeyEvent(id, event);
    }
  }

  command(command: WebCommand): void {
    if (!this.context.isActive()) {
      return;
    }
    if (this.closed) {
      if (command.requestId) {
        this.emitWebReply(command.requestId, false, "WebView is closed");
      }
      return;
    }
    if (!this.controllerReady) {
      this.pendingCommands.push(command);
      return;
    }
    try {
      if (command.kind === "download-start") {
        const requestId = command.requestId ?? "";
        const downloadId = command.downloadId ?? "";
        const targetPath = command.path ?? "";
        const item = this.downloads.get(downloadId);
        if (!item || !targetPath) {
          this.emitWebReply(requestId, false, `no download with id ${downloadId}`);
          return;
        }
        if (this.startedDownloads.has(downloadId)) {
          this.emitWebReply(requestId, false, `download ${downloadId} is already started`);
          return;
        }
        try {
          item.start(targetPath);
          this.startedDownloads.add(downloadId);
          this.emitWebReply(requestId, true, undefined, downloadId);
        } catch (error) {
          // Leave the held offer for nsh-core's follow-up CancelDownload so
          // staging cleanup waits on an explicit stop confirmation.
          this.emitWebReply(requestId, false, String(error), downloadId);
        }
      } else if (command.kind === "download-cancel") {
        const requestId = command.requestId ?? "";
        const downloadId = command.downloadId ?? "";
        const item = this.downloads.get(downloadId);
        if (!item) {
          this.emitWebReply(requestId, false, `no download with id ${downloadId}`);
          return;
        }
        this.cancelledDownloads.add(downloadId);
        const wasStarted = this.startedDownloads.has(downloadId);
        try {
          item.cancel();
        } catch {}
        const terminal = !wasStarted || this.isDownloadTerminal(item);
        if (terminal) {
          this.forgetDownload(downloadId, item.getGuid());
          this.cancelledDownloads.delete(downloadId);
          this.resolvePendingCancels(downloadId, true);
          this.emitWebReply(requestId, true, undefined, downloadId, "cancel-ok");
        } else {
          const list = this.pendingCancels.get(downloadId) ?? [];
          list.push(requestId);
          this.pendingCancels.set(downloadId, list);
        }
      } else if (command.kind.startsWith("input-")) {
        // Serialize the asynchronous ArkWeb editor operation with native keys.
        // execCommand enters the browser editor (selection, undo and trusted
        // input); assigning a DOM value or dispatching a JS event would not.
        this.inputCommands = this.inputCommands
          .then(async (): Promise<void> => {
            if (this.closed || !this.context.isActive()) {
              return;
            }
            if (command.kind === "input-click") {
              this.click(command.x ?? 0, command.y ?? 0);
            } else if (command.kind === "input-text") {
              const text = JSON.stringify(command.script ?? "");
              const inserted = await this.controller.runJavaScript(
                `document.execCommand("insertText", false, ${text})`,
              );
              if (inserted !== "true") {
                throw new Error("ArkWeb has no focused editable element for text input");
              }
            } else if (command.kind === "input-key") {
              this.key(command.script ?? "Escape");
            }
          })
          .catch((error: Error) => this.emit("error", String(error)));
      } else if (command.kind === "reload") {
        this.controller.refresh();
      } else if (command.kind === "go-back") {
        if (this.controller.accessBackward()) {
          this.controller.backward();
        }
      } else if (command.kind === "go-forward") {
        if (this.controller.accessForward()) {
          this.controller.forward();
        }
      } else if (command.kind === "stop-loading") {
        this.controller.stop();
      } else if (command.kind === "focus") {
        this.context
          .getUIContext()
          .getFocusController()
          .requestFocus(`nsh-web-${this.settings.targetId}`);
      } else if (command.kind === "load") {
        if (command.url != null) {
          clearCookiesIfNeeded(this);
          this.sourceApplied = true;
          this.sourceEpoch = this.epoch;
          this.controller.loadUrl(command.url, command.headers ?? []);
        }
      } else if (command.kind === "evaluate") {
        if (command.script != null) {
          this.controller.runJavaScript(command.script).catch((error: Error) => {
            this.emit("error", String(error));
          });
        }
      }
    } catch (error) {
      this.emit("error", String(error));
    }
  }

  private release(): void {
    try {
      try {
        this.controller.stop();
        this.controller.loadUrl("about:blank");
        if (!this.settings.sharedCookies) {
          webview.WebCookieManager.clearAllCookiesSync(true);
        }
      } catch (_error) {}
      this.node.dispose();
      this.disposed = true;
      quarantinedWebs.delete(this);
    } catch (error) {
      quarantinedWebs.add(this);
      throw new Error(String(error));
    }
  }

  dispose(cleanup: NshRuntimeContext = this.context): void {
    for (const result of this.fileSelectors) {
      try {
        result.handleFileList([]);
      } catch {}
    }
    this.fileSelectors.clear();
    if (this.disposed) {
      return;
    }
    this.closed = true;
    for (const pair of Array.from(this.downloads.entries())) {
      const id = pair[0];
      const item = pair[1];
      const wasStarted = this.startedDownloads.has(id);
      try {
        item.cancel();
      } catch {}
      if (!wasStarted || this.isDownloadTerminal(item)) {
        this.forgetDownload(id, item.getGuid());
        const wasCancelled = this.cancelledDownloads.delete(id);
        const hadPendingCancel = this.resolvePendingCancels(id, true);
        if (!wasCancelled && !hadPendingCancel) {
          this.emitDownloadEnd(
            id,
            "failed",
            0,
            "",
            "the web view went away",
            undefined,
            "ERR_NSH_DOWNLOAD_TEARDOWN",
          );
        }
      } else {
        this.tearingDownDownloads.add(id);
      }
    }
    this.epoch++;
    for (const pending of this.pendingCommands) {
      if (pending.requestId) {
        this.emitWebReply(pending.requestId, false, "WebView disposed");
      }
    }
    this.pendingCommands = [];
    try {
      if (!this.retired) {
        if (!cleanup.retireNativeNode(this.settings.targetId)) {
          throw new Error("Native WebView retirement was rejected");
        }
        this.retired = true;
      }
      if (this.sdkBusy) {
        this.disposePending = true;
      } else if (this.downloads.size > 0) {
        // Keep the delegate alive until ArkWeb confirms active writers stopped,
        // so nsh-core receives teardown stop confirmation before cleaning staging.
        this.teardownReleasePending = true;
        quarantinedWebs.add(this);
      } else {
        this.release();
      }
    } catch (error) {
      quarantinedWebs.add(this);
      throw new Error(`Native WebView retirement failed: ${String(error)}`);
    }
  }
}

export class NativeWebViews {
  private entries: Map<number, WebEntry> = new Map();

  remove(ids: number[], context: NshRuntimeContext): void {
    for (const id of ids) {
      this.entries.get(id)?.dispose(context);
      this.entries.delete(id);
    }
  }

  update(states: WebSettings[], commands: WebCommand[], context: NshRuntimeContext): void {
    for (const state of states) {
      if (!context.isActive()) {
        throw new Error("NSH SDK session is inactive");
      }
      let entry = this.entries.get(state.targetId);
      if (entry) {
        entry.update(state);
      } else {
        entry = new WebEntry(state, context);
        this.entries.set(state.targetId, entry);
        entry.create();
      }
    }
    for (const command of commands) {
      this.entries.get(command.targetId)?.command(command);
    }
  }

  dispose(context: NshRuntimeContext): void {
    let failure: Error | undefined;
    for (const id of Array.from(this.entries.keys())) {
      const entry = this.entries.get(id)!;
      try {
        entry.dispose(context);
        this.entries.delete(id);
      } catch (error) {
        failure ??= new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
}
