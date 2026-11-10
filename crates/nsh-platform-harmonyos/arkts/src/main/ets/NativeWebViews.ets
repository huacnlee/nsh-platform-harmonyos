import { BuilderNode } from "@ohos.arkui.node";
import { fileIo as fs } from "@kit.CoreFileKit";
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
}

let webEngineInitialized = false;
let nextWebNativeTagId = 1;

function ensureWebEngineInitialized(): void {
  if (!webEngineInitialized) {
    webview.WebviewController.initializeWebEngine();
    webEngineInitialized = true;
  }
}

// Clear the selected store before the controller's first navigation, including
// settings queued while its native node was still attaching.
function clearCookiesIfNeeded(entry: WebEntry): void {
  if (!entry.settings.clearCookies) { return; }
  try {
    webview.WebCookieManager.clearAllCookiesSync(!entry.settings.sharedCookies);
    entry.settings.clearCookies = false;
  } catch (error) {
    entry.emit("error", `Cookie reset failed: ${String(error)}`);
    throw new Error(String(error));
  }
}

@Builder
function webContent(entry: WebEntry) {
  Web({
    src: "",
    controller: entry.controller,
    renderMode: RenderMode.SYNC_RENDER,
    incognitoMode: !entry.settings.sharedCookies,
  })
    .id(`nsh-web-${entry.settings.targetId}`)
    .width("100%")
    .height("100%")
    .backgroundColor(Color.Transparent)
    .hitTestBehavior(HitTestMode.Block)
    .javaScriptAccess(entry.settings.javaScriptEnabled)
    .domStorageAccess(true)
    .fileAccess(false)
    .onlineImageAccess(true)
    .imageAccess(true)
    .runJavaScriptOnDocumentStart([
      {
        script: entry.settings.documentStartScript,
        scriptRules: ["*", "nsh-asset://"],
      },
    ])
    .onShowFileSelector((event): boolean => {
      if (!event) { return false; }
      entry.selectFiles(event.fileSelector, event.result);
      return true;
    })
    .onControllerAttached(() => {
      entry.onControllerAttached();
    })
    .onLoadIntercept((event): boolean => {
      return entry.onLoadIntercept(event.data.getRequestUrl());
    })
    .onPageBegin(() => {
      entry.onPageBegin();
    })
    .onPageEnd(() => {
      entry.onPageEnd();
    })
    .onTitleReceive(() => {
      entry.emit("title");
    })
    .onFocus(() => {
      entry.emit("focus");
    })
    .onBlur(() => {
      entry.emit("blur");
    })
    .onErrorReceive((event) => {
      if (event.request.isMainFrame()) {
        entry.emit("error", event.error.getErrorInfo(), event.error.getErrorCode());
      }
    });
}

const quarantinedWebs: Set<WebEntry> = new Set();

class WebEntry {
  readonly nativeTag: string = `nsh.web.${nextWebNativeTagId++}`;
  readonly controller: webview.WebviewController = new webview.WebviewController(this.nativeTag);
  readonly node: BuilderNode<[WebEntry]>;
  settings: WebSettings;
  private context: NshRuntimeContext;
  private attached: boolean = false;
  private controllerObserved: boolean = false;
  private controllerReady: boolean = false;
  private appliedLoadGeneration: number = 0;
  private pendingCommands: WebCommand[] = [];
  private inputCommands: Promise<void> = Promise.resolve();
  private fileSelectors: Set<FileSelectorResult> = new Set();
  private downloads: Map<string, webview.WebDownloadItem> = new Map();
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
    ensureWebEngineInitialized();
    this.node = new BuilderNode<[WebEntry]>(context.getUIContext());
  }

  create(): void {
    this.sdkBusy = true;
    try {
      this.node.build(wrapBuilder(webContent), this);
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
      if (this.controllerObserved && !this.controllerReady) {
        this.finishControllerAttach();
      }
    } catch (error) {
      this.dispose();
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
    if (!this.attached || this.controllerReady || this.closed || !this.context.isActive()) {
      return;
    }
    this.finishControllerAttach();
  }

  private finishControllerAttach(): void {
    if (this.controllerReady || this.closed || !this.context.isActive()) {
      return;
    }
    try {
      this.controller.setScrollable(this.settings.scrollEnabled);
      this.controller.setCustomUserAgent(this.settings.userAgent ?? "");
      if (!this.context.webControllerAttached(this.settings.targetId, this.nativeTag)) {
        throw new Error("Native WebView controller attachment was rejected");
      }
      this.downloadDelegate.onBeforeDownload((item: webview.WebDownloadItem) => {
        if (
          this.closed ||
          !this.context.webDownloadAllowed(this.settings.targetId, item.getUrl())
        ) {
          item.cancel();
          return;
        }
        try {
          const id = item.getGuid();
          const filename =
            item.getSuggestedFileName().replace(/[\\/\x00-\x1f]/g, "_") || "download";
          if (!id || id.includes("/") || id.includes("\\") || id === "." || id === "..") {
            throw new Error("Invalid ArkWeb download ID");
          }
          const directory = `${this.settings.downloadDirectory}/${id}`;
          fs.mkdirSync(directory, true);
          this.downloads.set(id, item);
          item.start(
            `${directory}/${filename === "." || filename === ".." ? "download" : filename}`,
          );
          this.download(item, "started");
        } catch (error) {
          this.download(item, "failed", String(error));
          item.cancel();
        }
      });
      this.downloadDelegate.onDownloadFinish((item: webview.WebDownloadItem) => {
        this.download(item, "finished");
        this.downloads.delete(item.getGuid());
      });
      this.downloadDelegate.onDownloadFailed((item: webview.WebDownloadItem) => {
        this.download(item, "failed", String(item.getLastErrorCode()));
        this.downloads.delete(item.getGuid());
        try {
          fs.unlinkSync(item.getFullPath());
        } catch {}
      });
      this.controller.setDownloadDelegate(this.downloadDelegate);
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
        result.handleFileList(this.closed || !this.context.isActive() || epoch !== this.epoch ? [] : uris);
      }
    };
    this.pickFiles(selector).then(finish).catch((error: Error) => {
      console.warn(`NSH WebView file picker: ${String(error)}`);
      finish([]);
    });
  }

  private async pickFiles(selector: FileSelectorParam): Promise<string[]> {
    if (this.closed || !this.context.isActive()) { return []; }
    const mode = selector.getMode();
    if (mode === FileSelectorMode.FileSaveMode) { return []; }
    const limit = mode === FileSelectorMode.FileOpenMultipleMode ? 100 : 1;
    const accept = selector.getAcceptType().map((type: string) => type.toLowerCase());
    const host = this.context.getUIContext().getHostContext();
    if (!host) { return []; }
    const media = accept.length > 0 && accept.every((type: string) => type.startsWith("image/") || type.startsWith("video/"));
    if (media && mode !== FileSelectorMode.FileOpenFolderMode) {
      const photos = new picker.PhotoViewPicker(host);
      const options = new picker.PhotoSelectOptions();
      options.maxSelectNumber = limit;
      options.MIMEType = accept.every((type: string) => type.startsWith("image/"))
        ? picker.PhotoViewMIMETypes.IMAGE_TYPE
        : accept.every((type: string) => type.startsWith("video/"))
          ? picker.PhotoViewMIMETypes.VIDEO_TYPE : picker.PhotoViewMIMETypes.IMAGE_VIDEO_TYPE;
      return (await photos.select(options)).photoUris;
    }
    const documents = new picker.DocumentViewPicker(host);
    const options = new picker.DocumentSelectOptions();
    options.maxSelectNumber = limit;
    if (mode === FileSelectorMode.FileOpenFolderMode) { options.selectMode = picker.DocumentSelectMode.FOLDER; }
    const suffixes = accept.filter((type: string) => /^\.[a-z0-9]+$/.test(type));
    if (suffixes.length > 0) { options.fileSuffixFilters = suffixes; }
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

  onPageEnd(): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
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

  private download(item: webview.WebDownloadItem, state: string, message?: string): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    this.context.webEvent({
      targetId: this.settings.targetId,
      phase: "download",
      url: item.getUrl(),
      title: "",
      loading: false,
      canGoBack: false,
      canGoForward: false,
      message,
      download: {
        state,
        id: item.getGuid(),
        filename: item.getSuggestedFileName(),
        path: item.getFullPath(),
        bytes: item.getReceivedBytes(),
      },
    });
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
      this.context.getUIContext().dispatchKeyEvent(`nsh-web-${this.settings.targetId}`, event);
    }
  }

  command(command: WebCommand): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    if (!this.controllerReady) {
      this.pendingCommands.push(command);
      return;
    }
    try {
      if (command.kind.startsWith("input-")) {
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
      try { result.handleFileList([]); } catch {}
    }
    this.fileSelectors.clear();
    if (this.disposed) {
      return;
    }
    this.closed = true;
    for (const item of this.downloads.values()) {
      try {
        item.cancel();
        fs.unlinkSync(item.getFullPath());
      } catch {}
    }
    this.downloads.clear();
    this.epoch++;
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
