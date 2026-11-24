import {
  AbilityConstant,
  AbilityLifecycleCallback,
  Configuration,
  ConfigurationConstant,
  EnvironmentCallback,
  UIAbility,
  common,
} from "@kit.AbilityKit";
import { accessibility } from "@kit.AccessibilityKit";
import { NodeContent, window } from "@kit.ArkUI";
import { fileIo as fs, picker } from "@kit.CoreFileKit";
import { photoAccessHelper } from "@kit.MediaLibraryKit";
import { FrameNode } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import deviceInfo from "@ohos.deviceInfo";
import display from "@ohos.display";
import i18n from "@ohos.i18n";
import pointer from "@ohos.multimodalInput.pointer";
import {
  EditorEventPayload,
  EditorKeyEventPayload,
  EditorReceipt,
  MenuActionPayload,
  NshRuntimeContext,
  WebEventPayload,
} from "./NativeEditors";
import { Batch, NshPlugin } from "./NshPlugin";
import { NativeMobileControl } from "./NativeMobileControl";
import { NativeTabBar, TabBarContext } from "./NativeTabBar";
import { createFrameSource, FrameCallbacks, FrameRequest } from "./NativeFrames";

import { NativeAlertOptions, showNativeAlert } from "./NativeAlerts";
import { isDesktopDevice, NativeDesktop } from "./NativeDesktop";
import { startAbilityForResult } from "./NativeExtensions";
import { NativeSystem, SystemReply, SystemRequest } from "./NativeSystem";

interface AvoidRectPayload {
  left: number;
  top: number;
  width: number;
  height: number;
}

interface AvoidAreaPayload {
  visible: boolean;
  leftRect: AvoidRectPayload;
  topRect: AvoidRectPayload;
  rightRect: AvoidRectPayload;
  bottomRect: AvoidRectPayload;
}

interface HostInitPayload {
  speechAvailable: boolean;
  shareAvailable: boolean;
  sdkApiVersion: number;
  deviceType: string;
  cacheDir: string;
  filesDir: string;
  pixelRatio: number;
  maxRefreshRate: number;
  rtl: boolean;
  lifecycle: string;
  colorMode: number;
  fontSizeScale: number;
  language: string;
  languages?: string[];
  timeZone?: string;
  regionalLocale?: string;
  is24HourClock?: boolean;
  screenReader: boolean;
  reduceMotion?: boolean;
}

interface HostCommandPayload {
  system?: SystemRequest;
  kind: string;
  enabled?: boolean;
  lightContent?: boolean;
  animated?: boolean;
  text?: string;
  announcement?: string;
  elementId?: number;
  windowId?: number;
  openId?: number;
  limit?: number;
  filters?: string[];
  url?: string;
}

interface PickedDescriptor {
  fd: number;
  name: string;
}

interface NshNativeModule {
  nshRegisterSessionBinder(
    sdkApiVersion: number,
    binder: (uiContext: UIContext) => number,
    alerts: (options: NativeAlertOptions) => () => void,
  ): void;
  nshCreateSession(
    init: HostInitPayload,
    platformWindowId: number,
    resourceManager: object,
    applyBatch: (batch: Batch) => void,
    onCommand: (command: HostCommandPayload) => void,
    tabBar: (context: TabBarContext) => object,
    frames: (callbacks: FrameCallbacks) => (request: FrameRequest) => void,
  ): number;
  nshCloseSession(sessionId: number): void;
  nshServiceEvent(sessionId: number, key: number, event: SystemReply): void;
  nshDesktopEvent(sessionId: number, kind: string, payload: string): void;
  nshAttachSubWindow(
    sessionId: number,
    windowId: number,
    content: NodeContent,
    platformWindowId: number,
  ): boolean;
  nshSetLifecycle(sessionId: number, lifecycle: string): void;
  nshUpdateKeyboard(sessionId: number, duration: number): void;
  nshUpdateConfig(
    sessionId: number,
    colorMode: number,
    fontSizeScale: number,
    language: string,
    rtl: boolean,
    deviceType?: string,
    pixelRatio?: number,
    languages?: string[],
    timeZone?: string,
    regionalLocale?: string,
    is24HourClock?: boolean,
    fontChanged?: boolean,
  ): void;
  nshUpdateAccessibility(sessionId: number, screenReader?: boolean, reduceMotion?: boolean): void;
  nshUpdateWindow(
    sessionId: number,
    platformWindowId: number,
    visual: AvoidAreaPayload[],
    keyboard: AvoidAreaPayload,
    maxRefreshRate?: number,
  ): void;
  nshOpenUrlResult(sessionId: number, openId: number, opened: boolean): void;
  nshPickImagesResult(sessionId: number, pickId: number, files: PickedDescriptor[]): void;
  nshRunUiTask(
    sessionId: number,
    taskId: number,
    uiContext: UIContext,
    bundleName: string,
    moduleName?: string,
  ): void;
  nshAbilityResult(
    sessionId: number,
    requestId: number,
    ok: boolean,
    payload?: string,
    code?: number,
    message?: string,
  ): void;
  nshReportHostError(sessionId: number, message: string): void;
  nshAttachNativeNode(targetId: number, content: FrameNode): void;
  nshAttachNativeSubtree(targetId: number, content: FrameNode, slot: NodeContent): void;
  nshRetireNativeNode(targetId: number): void;
  nshEditorEvent(targetId: number, event: EditorEventPayload): EditorReceipt;
  nshEditorKeyEvent(targetId: number, event: EditorKeyEventPayload): boolean;
  nshViewAction(action: MenuActionPayload): boolean;
  nshWebControllerAttached(targetId: number, nativeTag: string): boolean;
  nshWebNavigate(targetId: number, url: string): boolean;
  nshWebDownloadAllowed(targetId: number, url: string): boolean;
  nshWebEvent(event: WebEventPayload): boolean;
}

let cachedNativeModule: NshNativeModule | undefined = undefined;

function getNativeModule(): NshNativeModule {
  if (cachedNativeModule !== undefined) {
    return cachedNativeModule;
  }
  const libraryName = "libnsh_platform_harmonyos.so";
  let loaded: ESObject;
  try {
    loaded = loadNativeModule(libraryName);
  } catch (error) {
    throw new Error(`${libraryName} failed to load: ${String(error)}`);
  }
  const candidate: ESObject =
    typeof loaded?.nshRegisterSessionBinder === "function" ? loaded : loaded?.default;
  const mod = candidate as NshNativeModule;
  if (!mod || typeof mod.nshRegisterSessionBinder !== "function") {
    throw new Error(`${libraryName} does not export the Native Shell OHOS host ABI`);
  }
  cachedNativeModule = mod;
  return mod;
}

function toAvoidRect(rect: window.Rect): AvoidRectPayload {
  return {
    left: rect.left,
    top: rect.top,
    width: rect.width,
    height: rect.height,
  };
}

function toAvoidArea(area: window.AvoidArea): AvoidAreaPayload {
  return {
    visible: area.visible,
    leftRect: toAvoidRect(area.leftRect),
    topRect: toAvoidRect(area.topRect),
    rightRect: toAvoidRect(area.rightRect),
    bottomRect: toAvoidRect(area.bottomRect),
  };
}

function emptyAvoidArea(): AvoidAreaPayload {
  const zero: AvoidRectPayload = { left: 0, top: 0, width: 0, height: 0 };
  return {
    visible: false,
    leftRect: zero,
    topRect: zero,
    rightRect: zero,
    bottomRect: zero,
  };
}

function safeAvoidArea(win: window.Window, type: window.AvoidAreaType): AvoidAreaPayload {
  try {
    return toAvoidArea(win.getWindowAvoidArea(type));
  } catch {
    return emptyAvoidArea();
  }
}

interface SystemLocaleDetails {
  languages?: string[];
  timeZone?: string;
  regionalLocale?: string;
  is24HourClock?: boolean;
}

function readSystemLocaleDetails(): SystemLocaleDetails {
  let languages: string[] | undefined = undefined;
  try {
    const list = i18n.System.getPreferredLanguageList();
    if (Array.isArray(list) && list.length > 0) {
      languages = list;
    }
  } catch {}
  let timeZone: string | undefined = undefined;
  try {
    timeZone = i18n.getTimeZone().getID();
  } catch {}
  let regionalLocale: string | undefined = undefined;
  try {
    regionalLocale = i18n.System.getSystemLocaleInstance().toString();
  } catch {}
  let is24HourClock: boolean | undefined = undefined;
  try {
    is24HourClock = i18n.System.is24HourClock();
  } catch {}
  return { languages, timeZone, regionalLocale, is24HourClock };
}

class ComponentRuntimeContext implements NshRuntimeContext {
  private readonly ui: UIContext;
  private readonly mod: NshNativeModule;
  private active: boolean = true;
  private themeColorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  private focusHandler?: (componentId: string) => boolean;
  private uiContextResolver?: (componentId: string) => UIContext | undefined;

  constructor(ui: UIContext, mod: NshNativeModule) {
    this.ui = ui;
    this.mod = mod;
  }

  colorMode(): ThemeColorMode {
    return this.themeColorMode;
  }

  setColorMode(colorMode: ThemeColorMode): void {
    this.themeColorMode = colorMode;
  }

  setFocusHandler(handler?: (componentId: string) => boolean): void {
    this.focusHandler = handler;
  }

  setUIContextResolver(resolver?: (componentId: string) => UIContext | undefined): void {
    this.uiContextResolver = resolver;
  }

  getUIContextFor(componentId: string): UIContext {
    return this.uiContextResolver?.(componentId) ?? this.ui;
  }

  requestFocus(componentId: string): boolean {
    if (this.focusHandler?.(componentId)) {
      return true;
    }
    try {
      this.ui.getFocusController().requestFocus(componentId);
      return true;
    } catch {
      return false;
    }
  }

  deactivate(): void {
    this.active = false;
  }

  isActive(): boolean {
    return this.active;
  }

  getUIContext(): UIContext {
    return this.ui;
  }

  attachNativeNode(targetId: number, content: FrameNode | null): boolean {
    if (!this.active || content === null) {
      return false;
    }
    try {
      this.mod.nshAttachNativeNode(targetId, content);
      return true;
    } catch (error) {
      console.error(`[NSH] attachNativeNode(${targetId}) failed: ${String(error)}`);
      return false;
    }
  }

  attachNativeSubtree(targetId: number, content: FrameNode | null, slot: NodeContent): boolean {
    if (!this.active || content === null) {
      return false;
    }
    try {
      this.mod.nshAttachNativeSubtree(targetId, content, slot);
      return true;
    } catch (error) {
      console.error(`[NSH] attachNativeSubtree(${targetId}) failed: ${String(error)}`);
      return false;
    }
  }

  retireNativeNode(targetId: number): boolean {
    try {
      this.mod.nshRetireNativeNode(targetId);
      return true;
    } catch (error) {
      console.error(`[NSH] retireNativeNode(${targetId}) failed: ${String(error)}`);
      return false;
    }
  }

  editorEvent(targetId: number, event: EditorEventPayload): EditorReceipt {
    if (!this.active) {
      return { accepted: false, count: 0 };
    }
    try {
      return this.mod.nshEditorEvent(targetId, event);
    } catch {
      return { accepted: false, count: 0 };
    }
  }

  editorKeyEvent(targetId: number, event: EditorKeyEventPayload): boolean {
    if (!this.active) {
      return false;
    }
    try {
      return this.mod.nshEditorKeyEvent(targetId, event);
    } catch {
      return false;
    }
  }

  viewAction(action: MenuActionPayload): boolean {
    if (!this.active) {
      return false;
    }
    try {
      return this.mod.nshViewAction(action);
    } catch {
      return false;
    }
  }

  webControllerAttached(targetId: number, nativeTag: string): boolean {
    if (!this.active) {
      return false;
    }
    try {
      return this.mod.nshWebControllerAttached(targetId, nativeTag);
    } catch {
      return false;
    }
  }

  webDownloadAllowed(targetId: number, url: string): boolean {
    return this.isActive() && this.mod.nshWebDownloadAllowed(targetId, url);
  }
  webNavigate(targetId: number, url: string): boolean {
    if (!this.active) {
      return true;
    }
    try {
      return this.mod.nshWebNavigate(targetId, url);
    } catch {
      return true;
    }
  }

  webEvent(event: WebEventPayload): boolean {
    if (!this.active) {
      return false;
    }
    try {
      return this.mod.nshWebEvent(event);
    } catch {
      return false;
    }
  }
}

class HostEnvironmentListener extends EnvironmentCallback {
  private readonly owner: HostSession;
  constructor(owner: HostSession) {
    super();
    this.owner = owner;
  }
  onConfigurationUpdated(config: Configuration): void {
    this.owner.onConfigurationUpdated(config);
  }
  onMemoryLevel(_level: AbilityConstant.MemoryLevel): void {}
}

class HostLifecycleListener extends AbilityLifecycleCallback {
  private readonly owner: HostSession;
  constructor(owner: HostSession) {
    super();
    this.owner = owner;
  }
  onAbilityCreate(_ability: UIAbility): void {}
  onWindowStageCreate(_ability: UIAbility, _windowStage: window.WindowStage): void {}
  onWindowStageActive(_ability: UIAbility, _windowStage: window.WindowStage): void {}
  onWindowStageInactive(_ability: UIAbility, _windowStage: window.WindowStage): void {}
  onWindowStageDestroy(ability: UIAbility, windowStage: window.WindowStage): void {
    if (this.owner.matchesWindowStage(ability.context, windowStage)) {
      this.owner.close();
    }
  }
  onAbilityDestroy(ability: UIAbility): void {
    if (this.owner.matchesAbility(ability.context)) {
      this.owner.close();
    }
  }
  onAbilityForeground(_ability: UIAbility): void {}
  onAbilityBackground(_ability: UIAbility): void {}
  onAbilityContinue(_ability: UIAbility): void {}
}

class HostSession {
  private static readonly windows: Map<string, HostSession> = new Map();
  private readonly windowName: string;
  readonly sessionId: number;
  private readonly mod: NshNativeModule;
  private readonly abilityContext: common.UIAbilityContext;
  private readonly appContext?: common.ApplicationContext;
  private readonly windowStage?: window.WindowStage;
  private readonly mainWindow: window.Window;
  private readonly mobileControls: Set<NativeMobileControl> = new Set();
  private readonly tabBars: Set<NativeTabBar> = new Set();
  private colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  private systemColorMode: number = -1;
  private lastFontSizeScale: number = 1.0;
  private lastLanguage: string = "";
  private fontId: string = "";
  private readonly windowId: number;
  private readonly runtimeContext: ComponentRuntimeContext;
  private readonly system: NativeSystem;
  private readonly desktop?: NativeDesktop;
  private readonly isDesktop: boolean;
  private readonly plugin: NshPlugin = new NshPlugin();
  private closed: boolean = false;
  private savedPointerVisible?: boolean;
  private savedBars?: window.SystemBarProperties;
  private savedStatusVisible: boolean = true;
  private savedNavigationVisible: boolean = true;
  private barsWork: Promise<void> = Promise.resolve();
  private envCallbackId?: number;
  private lifecycleCallbackId?: number;
  private screenReaderHandler?: (enabled: boolean) => void;
  private reduceMotionHandler?: (enabled: boolean) => void;

  private readonly onWindowStageEvent = (event: window.WindowStageEventType): void => {
    if (this.closed) {
      return;
    }
    switch (event) {
      case window.WindowStageEventType.ACTIVE:
      case window.WindowStageEventType.RESUMED:
        this.onLifecycleChanged("active");
        break;
      case window.WindowStageEventType.SHOWN:
        this.onLifecycleChanged(this.mainWindow.isFocused() ? "active" : "inactive");
        break;
      case window.WindowStageEventType.INACTIVE:
      case window.WindowStageEventType.PAUSED:
        this.onLifecycleChanged("inactive");
        break;
      case window.WindowStageEventType.HIDDEN:
        this.onLifecycleChanged("background");
        break;
    }
  };

  private readonly onAvoidAreaChange = (_data: window.AvoidAreaOptions): void => {
    this.syncWindowMetrics();
  };

  private readonly onWindowSizeChange = (_size: window.Size): void => {
    this.syncWindowMetrics();
    this.refreshMobileControls();
  };

  private readonly onKeyboardWillChange = (info: window.KeyboardInfo): void => {
    const duration = info.animated === false ? 0 : info.config?.duration;
    if (!this.closed && duration !== undefined) {
      this.mod.nshUpdateKeyboard(this.sessionId, duration);
    }
  };

  private readonly onKeyboardHeightChange = (_height: number): void => {
    this.syncWindowMetrics();
  };

  private readonly onSecondInstance = (rawArgs?: string[]): void => {
    if (this.closed || !this.isDesktop) {
      return;
    }
    try {
      this.mainWindow.showWindow().catch((): void => {});
    } catch {}
    const args: string[] = [];
    if (Array.isArray(rawArgs)) {
      for (let i = 0; i < rawArgs.length; i++) {
        if (typeof rawArgs[i] === "string") {
          args.push(rawArgs[i]);
        }
      }
    }
    try {
      this.mod.nshDesktopEvent(
        this.sessionId,
        "second-instance",
        JSON.stringify({ args, cwd: "" }),
      );
      if (args.length === 0) {
        this.mod.nshDesktopEvent(this.sessionId, "reopen", "{}");
      }
    } catch {}
  };

  private constructor(
    mod: NshNativeModule,
    uiContext: UIContext,
    abilityContext: common.UIAbilityContext,
    mainWindow: window.Window,
    windowName: string,
  ) {
    this.windowName = windowName;
    this.mod = mod;
    this.abilityContext = abilityContext;
    this.windowStage = abilityContext.windowStage;
    this.mainWindow = mainWindow;
    try {
      this.appContext = abilityContext.getApplicationContext();
    } catch {}

    let windowId = 0;
    try {
      windowId = mainWindow.getWindowProperties().id;
    } catch {}
    this.windowId = windowId;

    this.runtimeContext = new ComponentRuntimeContext(uiContext, mod);

    let screenReader = false;
    try {
      screenReader = accessibility.isScreenReaderOpenSync();
    } catch {}

    let reduceMotion: boolean | undefined = undefined;
    if (deviceInfo.sdkApiVersion >= 23) {
      try {
        reduceMotion = accessibility.isAnimationReduceEnabledSync();
      } catch {}
    }

    const config = abilityContext.config;
    const language = config?.language || i18n.System.getSystemLanguage();
    const localeDetails = readSystemLocaleDetails();
    let pixelRatio = uiContext.vp2px(1);
    const displayId = mainWindow.getWindowProperties().displayId;
    if (displayId !== undefined) {
      pixelRatio = display.getDisplayByIdSync(displayId).densityPixels;
    }
    if (pixelRatio > 0) {
      pixelRatio = Math.round(pixelRatio * 10000) / 10000;
    }
    const lifecycle = mainWindow.isWindowShowing()
      ? mainWindow.isFocused()
        ? "active"
        : "inactive"
      : "background";
    this.systemColorMode = config?.colorMode ?? -1;
    this.lastFontSizeScale = config?.fontSizeScale ?? 1.0;
    this.lastLanguage = language;
    this.fontId = config?.fontId ?? "";
    this.sessionId = mod.nshCreateSession(
      {
        shareAvailable:
          deviceInfo.sdkApiVersion >= 18 && canIUse("SystemCapability.Collaboration.SystemShare"),
        speechAvailable: canIUse("SystemCapability.AI.SpeechRecognizer"),
        sdkApiVersion: deviceInfo.sdkApiVersion,
        deviceType: deviceInfo.deviceType ?? "phone",
        cacheDir: abilityContext.cacheDir,
        filesDir: abilityContext.filesDir,
        colorMode: this.systemColorMode,
        fontSizeScale: this.lastFontSizeScale,
        language,
        languages: localeDetails.languages,
        timeZone: localeDetails.timeZone,
        regionalLocale: localeDetails.regionalLocale,
        is24HourClock: localeDetails.is24HourClock,
        rtl: i18n.isRTL(language),
        pixelRatio,
        maxRefreshRate: maximumRefreshRate(mainWindow),
        lifecycle,
        screenReader,
        reduceMotion,
      },
      windowId,
      abilityContext.resourceManager,
      (batch: Batch): void => {
        if (!this.closed) {
          this.plugin.apply(batch, this.runtimeContext);
        }
      },
      (command: HostCommandPayload): void => {
        this.handleHostCommand(command);
      },
      (context: TabBarContext): object =>
        context.name === "nsh/date-time-picker"
          ? new NativeMobileControl(
              uiContext,
              context,
              mainWindow,
              this.mobileControls,
              this.colorMode,
            )
          : new NativeTabBar(uiContext, context, this.tabBars, this.colorMode),
      (callbacks: FrameCallbacks): ((request: FrameRequest) => void) =>
        createFrameSource(uiContext, callbacks),
    );

    let system: NativeSystem | null = null;
    try {
      system = new NativeSystem(uiContext, abilityContext, (key: number, event: object): void =>
        this.mod.nshServiceEvent(this.sessionId, key, event),
      );
      this.system = system;
      this.isDesktop = isDesktopDevice(deviceInfo.deviceType);
      if (this.isDesktop) {
        this.desktop = new NativeDesktop(
          uiContext,
          abilityContext,
          mainWindow,
          (kind: string, payload: string): void => {
            if (!this.closed) {
              this.mod.nshDesktopEvent(this.sessionId, kind, payload);
            }
          },
          (windowId: number, content: NodeContent, platformWindowId: number): boolean => {
            if (this.closed) {
              return false;
            }
            try {
              return this.mod.nshAttachSubWindow(this.sessionId, windowId, content, platformWindowId);
            } catch (error) {
              console.error(`[NSH] attachSubWindow(${windowId}) failed: ${String(error)}`);
              return false;
            }
          },
          (): void => {
            this.syncWindowMetrics();
          },
          (targetId: number, content: FrameNode | null): boolean =>
            this.runtimeContext.attachNativeNode(targetId, content),
          (targetId: number): boolean => this.runtimeContext.retireNativeNode(targetId),
        );
        this.runtimeContext.setFocusHandler((componentId: string): boolean =>
          this.desktop?.requestComponentFocus(componentId) ?? false,
        );
        this.runtimeContext.setUIContextResolver((componentId: string): UIContext | undefined =>
          this.desktop?.resolveComponentUIContext(componentId),
        );
        try {
          abilityContext.eventHub.on("nsh.secondInstance", this.onSecondInstance);
        } catch {}
      }
      this.bindWindowListeners();
      this.subscribeLifecycleListeners();
      this.subscribeAccessibility();
      this.syncWindowMetrics();
    } catch (error) {
      this.closed = true;
      try {
        this.desktop?.close();
      } catch {}
      try {
        system?.close();
      } catch {}
      try {
        this.mod.nshCloseSession(this.sessionId);
      } catch {}
      throw error instanceof Error ? error : new Error(String(error));
    }
  }

  static bind(uiContext: UIContext): number {
    const mod = getNativeModule();
    if (!uiContext || typeof uiContext.isAvailable !== "function" || !uiContext.isAvailable()) {
      throw new Error("NshSurface requires an available UIContext");
    }
    const windowName = uiContext.getWindowName() ?? "";
    if (!windowName) {
      throw new Error("OHOS UIContext does not have an associated window name");
    }
    const existing = HostSession.windows.get(windowName);
    if (existing !== undefined) {
      return existing.sessionId;
    }
    const abilityContext = uiContext.getHostContext() as common.UIAbilityContext | undefined;
    if (
      !abilityContext ||
      typeof abilityContext.cacheDir !== "string" ||
      !abilityContext.resourceManager
    ) {
      throw new Error("OHOS UIContext is not bound to a UIAbilityContext");
    }
    let mainWindow: window.Window | undefined = undefined;
    try {
      mainWindow = window.findWindow(windowName);
    } catch {}
    if (!mainWindow && abilityContext.windowStage !== undefined) {
      try {
        const candidate = abilityContext.windowStage.getMainWindowSync();
        if (candidate.getUIContext().getWindowName() === windowName) {
          mainWindow = candidate;
        }
      } catch {}
    }
    if (!mainWindow) {
      throw new Error(`OHOS window '${windowName}' could not be resolved from UIContext`);
    }
    const session = new HostSession(mod, uiContext, abilityContext, mainWindow, windowName);
    HostSession.windows.set(windowName, session);
    return session.sessionId;
  }

  matchesAbility(context: common.UIAbilityContext): boolean {
    return this.abilityContext === context;
  }

  matchesWindowStage(context: common.UIAbilityContext, stage: window.WindowStage): boolean {
    return (
      this.abilityContext === context &&
      (this.windowStage === undefined || this.windowStage === stage)
    );
  }

  onLifecycleChanged(lifecycle: string): void {
    if (this.closed) {
      return;
    }
    try {
      this.mod.nshSetLifecycle(this.sessionId, lifecycle);
    } catch (error) {
      this.mod.nshReportHostError(this.sessionId, `OHOS lifecycle update: ${String(error)}`);
    }
  }

  // Remeasure mounted system controls after changes to fonts, language or window size.
  private refreshMobileControls(): void {
    this.mobileControls.forEach((control: NativeMobileControl): void => control.refresh());
  }

  onConfigurationUpdated(newConfig: Configuration): void {
    if (this.closed) {
      return;
    }
    try {
      if (newConfig.colorMode !== undefined) {
        this.systemColorMode = newConfig.colorMode;
      }
      if (newConfig.fontSizeScale !== undefined && newConfig.fontSizeScale > 0) {
        this.lastFontSizeScale = newConfig.fontSizeScale;
      }
      if (newConfig.language !== undefined && newConfig.language.length > 0) {
        this.lastLanguage = newConfig.language;
      }
      let fontChanged = false;
      if (newConfig.fontId !== undefined && newConfig.fontId !== this.fontId) {
        this.fontId = newConfig.fontId;
        fontChanged = true;
      }
      const localeDetails = readSystemLocaleDetails();
      const language = this.lastLanguage || i18n.System.getSystemLanguage();
      this.mod.nshUpdateConfig(
        this.sessionId,
        this.systemColorMode,
        this.lastFontSizeScale,
        language,
        i18n.isRTL(language),
        deviceInfo.deviceType,
        Math.round(this.runtimeContext.getUIContext().vp2px(1) * 10000) / 10000,
        localeDetails.languages,
        localeDetails.timeZone,
        localeDetails.regionalLocale,
        localeDetails.is24HourClock,
        fontChanged,
      );
      this.refreshMobileControls();
      this.tabBars.forEach((bar: NativeTabBar): void => bar.refresh());
    } catch {}
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    HostSession.windows.delete(this.windowName);
    if (this.colorMode !== ThemeColorMode.SYSTEM) {
      try {
        this.appContext?.setColorMode(ConfigurationConstant.ColorMode.COLOR_MODE_NOT_SET);
      } catch {}
    }
    this.mobileControls.clear();
    this.tabBars.clear();
    if (this.isDesktop) {
      try {
        this.abilityContext.eventHub.off("nsh.secondInstance", this.onSecondInstance);
      } catch {}
    }
    this.desktop?.close();
    this.system.close();
    try {
      this.updateCursor("auto");
    } catch {}
    try {
      this.updateSystemBars();
    } catch {}

    if (this.windowStage !== undefined) {
      try {
        this.windowStage.off("windowStageEvent", this.onWindowStageEvent);
      } catch {}
    }

    try {
      this.mainWindow.off("avoidAreaChange", this.onAvoidAreaChange);
    } catch {}
    try {
      this.mainWindow.off("windowSizeChange", this.onWindowSizeChange);
    } catch {}
    try {
      this.mainWindow.off("keyboardHeightChange", this.onKeyboardHeightChange);
      this.mainWindow.off("keyboardWillShow", this.onKeyboardWillChange);
      this.mainWindow.off("keyboardWillHide", this.onKeyboardWillChange);
    } catch {}

    if (this.appContext !== undefined) {
      if (this.envCallbackId !== undefined) {
        try {
          this.appContext.off("environment", this.envCallbackId);
        } catch {}
        this.envCallbackId = undefined;
      }
      if (this.lifecycleCallbackId !== undefined) {
        try {
          this.appContext.off("abilityLifecycle", this.lifecycleCallbackId);
        } catch {}
        this.lifecycleCallbackId = undefined;
      }
    }

    if (this.screenReaderHandler !== undefined) {
      try {
        accessibility.off("screenReaderStateChange", this.screenReaderHandler);
      } catch {}
      this.screenReaderHandler = undefined;
    }
    if (deviceInfo.sdkApiVersion >= 23 && this.reduceMotionHandler !== undefined) {
      try {
        accessibility.offAnimationReduceStateChange(this.reduceMotionHandler);
      } catch {}
      this.reduceMotionHandler = undefined;
    }

    try {
      this.plugin.dispose(this.runtimeContext);
    } catch {}
    this.runtimeContext.deactivate();
    void this.plugin.settled();

    try {
      this.mod.nshCloseSession(this.sessionId);
    } catch {}
  }

  private bindWindowListeners(): void {
    if (this.windowStage !== undefined) {
      try {
        this.windowStage.on("windowStageEvent", this.onWindowStageEvent);
      } catch {}
    }
    try {
      this.mainWindow.on("avoidAreaChange", this.onAvoidAreaChange);
    } catch {}
    try {
      this.mainWindow.on("windowSizeChange", this.onWindowSizeChange);
    } catch {}
    try {
      this.mainWindow.on("keyboardHeightChange", this.onKeyboardHeightChange);
      this.mainWindow.on("keyboardWillShow", this.onKeyboardWillChange);
      this.mainWindow.on("keyboardWillHide", this.onKeyboardWillChange);
    } catch {}
  }

  private subscribeLifecycleListeners(): void {
    const appCtx = this.appContext;
    if (!appCtx) {
      return;
    }
    try {
      this.envCallbackId = appCtx.on("environment", new HostEnvironmentListener(this));
    } catch {}
    try {
      this.lifecycleCallbackId = appCtx.on("abilityLifecycle", new HostLifecycleListener(this));
    } catch {}
  }

  private subscribeAccessibility(): void {
    const srHandler = (enabled: boolean): void => {
      if (!this.closed) {
        try {
          this.mod.nshUpdateAccessibility(this.sessionId, enabled, undefined);
        } catch {}
      }
    };
    try {
      accessibility.on("screenReaderStateChange", srHandler);
      this.screenReaderHandler = srHandler;
    } catch {}

    if (deviceInfo.sdkApiVersion >= 23) {
      const rmHandler = (enabled: boolean): void => {
        if (!this.closed) {
          try {
            this.mod.nshUpdateAccessibility(this.sessionId, undefined, enabled);
          } catch {}
        }
      };
      try {
        accessibility.onAnimationReduceStateChange(rmHandler);
        this.reduceMotionHandler = rmHandler;
      } catch {}
    }
  }

  private syncWindowMetrics(): void {
    if (this.closed) {
      return;
    }
    const visual: AvoidAreaPayload[] = [
      safeAvoidArea(this.mainWindow, window.AvoidAreaType.TYPE_SYSTEM),
      safeAvoidArea(this.mainWindow, window.AvoidAreaType.TYPE_CUTOUT),
      safeAvoidArea(this.mainWindow, window.AvoidAreaType.TYPE_NAVIGATION_INDICATOR),
    ];
    const keyboard = safeAvoidArea(this.mainWindow, window.AvoidAreaType.TYPE_KEYBOARD);
    try {
      this.mod.nshUpdateWindow(
        this.sessionId,
        this.windowId,
        visual,
        keyboard,
        maximumRefreshRate(this.mainWindow),
      );
    } catch {}
  }

  private bundleName(): string {
    return (
      this.abilityContext.abilityInfo?.bundleName ?? this.appContext?.applicationInfo?.name ?? ""
    );
  }

  private moduleName(): string {
    const name = this.abilityContext.abilityInfo?.moduleName;
    return typeof name === "string" && name.length > 0 ? name : "entry";
  }

  private updateSystemBars(command?: HostCommandPayload): void {
    if (this.isDesktop) {
      return;
    }
    // Keep restoration ordered after outstanding writes, including when a
    // surface stops while a window API promise is still pending.
    this.barsWork = this.barsWork
      .then(async (): Promise<void> => {
        if (command === undefined) {
          const saved = this.savedBars;
          if (saved === undefined) {
            return;
          }
          this.savedBars = undefined;
          await this.mainWindow.setWindowSystemBarProperties(saved);
          await this.mainWindow.setSpecificSystemBarEnabled("status", this.savedStatusVisible);
          await this.mainWindow.setSpecificSystemBarEnabled(
            "navigationIndicator",
            this.savedNavigationVisible,
          );
          return;
        }
        if (this.savedBars === undefined) {
          this.savedBars = this.mainWindow.getWindowSystemBarProperties();
          this.savedStatusVisible = safeAvoidArea(
            this.mainWindow,
            window.AvoidAreaType.TYPE_SYSTEM,
          ).visible;
          this.savedNavigationVisible = safeAvoidArea(
            this.mainWindow,
            window.AvoidAreaType.TYPE_NAVIGATION_INDICATOR,
          ).visible;
        }
        if (command.kind === "video-fullscreen") {
          await this.mainWindow.setSpecificSystemBarEnabled(
            "status",
            command.enabled ? false : this.savedStatusVisible,
          );
          await this.mainWindow.setSpecificSystemBarEnabled(
            "navigationIndicator",
            command.enabled ? false : this.savedNavigationVisible,
          );
        } else {
          await this.mainWindow.setSpecificSystemBarEnabled(
            "status",
            command.enabled ?? true,
            command.animated ?? false,
          );
          await this.mainWindow.setWindowSystemBarProperties({
            statusBarContentColor: command.lightContent ? "#FFFFFFFF" : "#FF000000",
          });
        }
      })
      .catch((error: Error): void => {
        if (!this.closed) {
          this.mod.nshReportHostError(this.sessionId, `OHOS system bar update: ${String(error)}`);
        }
      });
  }

  private updateCursor(name: string, windowId?: number): void {
    const controller = this.runtimeContext.getUIContext().getCursorController();
    if (name === "none") {
      if (this.savedPointerVisible === undefined) {
        this.savedPointerVisible = pointer.isPointerVisibleSync();
      }
      pointer.setPointerVisibleSync(false);
      return;
    }
    if (this.savedPointerVisible !== undefined) {
      pointer.setPointerVisibleSync(this.savedPointerVisible);
      this.savedPointerVisible = undefined;
    }
    let style: pointer.PointerStyle;
    switch (name) {
      case "default":
        style = pointer.PointerStyle.DEFAULT;
        break;
      case "pointer":
        style = pointer.PointerStyle.HAND_POINTING;
        break;
      case "text":
        style = pointer.PointerStyle.TEXT_CURSOR;
        break;
      case "wait":
        style = pointer.PointerStyle.LOADING;
        break;
      case "progress":
        style = pointer.PointerStyle.RUNNING;
        break;
      case "help":
        style = pointer.PointerStyle.HELP;
        break;
      case "crosshair":
        style = pointer.PointerStyle.CROSS;
        break;
      case "move":
        style = pointer.PointerStyle.MOVE;
        break;
      case "grab":
        style = pointer.PointerStyle.HAND_OPEN;
        break;
      case "grabbing":
        style = pointer.PointerStyle.HAND_GRABBING;
        break;
      case "not-allowed":
        style = pointer.PointerStyle.CURSOR_FORBID;
        break;
      case "zoom-in":
        style = pointer.PointerStyle.ZOOM_IN;
        break;
      case "zoom-out":
        style = pointer.PointerStyle.ZOOM_OUT;
        break;
      case "col-resize":
        style = pointer.PointerStyle.WEST_EAST;
        break;
      case "row-resize":
        style = pointer.PointerStyle.NORTH_SOUTH;
        break;
      case "nwse-resize":
        style = pointer.PointerStyle.NORTH_WEST_SOUTH_EAST;
        break;
      case "nesw-resize":
        style = pointer.PointerStyle.NORTH_EAST_SOUTH_WEST;
        break;
      default:
        if (windowId !== undefined) {
          this.desktop?.updateWindowCursor(windowId, undefined);
        } else {
          controller.restoreDefault();
        }
        return;
    }
    if (windowId !== undefined) {
      this.desktop?.updateWindowCursor(windowId, style);
    } else {
      controller.setCursor(style);
    }
  }

  private handleHostCommand(command: HostCommandPayload): void {
    if (this.closed) {
      return;
    }

    if (command.kind === "back") {
      // Programmatic router back resumes the host's default return without
      // re-entering the page's onBackPress and its NSH BackHandler listeners.
      this.runtimeContext.getUIContext().getRouter().back();
      return;
    }
    if (command.kind === "ui-task") {
      this.mod.nshRunUiTask(
        this.sessionId,
        command.openId ?? 0,
        this.runtimeContext.getUIContext(),
        this.bundleName(),
        this.moduleName(),
      );
      return;
    }
    if (command.kind === "ability-start-for-result") {
      const requestId = command.openId ?? 0;
      startAbilityForResult(
        this.abilityContext,
        command.text ?? "{}",
        (ok: boolean, payload?: string, code?: number, message?: string): void => {
          if (!this.closed) {
            this.mod.nshAbilityResult(this.sessionId, requestId, ok, payload, code, message);
          }
        },
      );
      return;
    }
    if (command.kind === "system") {
      this.system
        .execute(command.openId ?? 0, command.system ?? { op: "" })
        .catch((error: Error) => console.error(`NSH system: ${error}`));
      return;
    }
    if (command.kind === "system-cancel") {
      this.system.cancel(command.openId ?? 0);
      return;
    }
    if (command.kind === "desktop") {
      this.desktop?.execute(command.text ?? "{}");
      return;
    }
    if (command.kind === "cursor") {
      this.updateCursor(command.text ?? "auto", command.windowId);
      return;
    }
    if (command.kind === "restore-bars") {
      this.updateSystemBars();
      return;
    }
    if (command.kind === "video-fullscreen" || command.kind === "status-bar") {
      this.updateSystemBars(command);
      return;
    }
    if (command.kind === "appearance") {
      const mode =
        command.text === "dark"
          ? ThemeColorMode.DARK
          : command.text === "light"
            ? ThemeColorMode.LIGHT
            : ThemeColorMode.SYSTEM;
      if (mode !== this.colorMode) {
        const wasSystem = this.colorMode === ThemeColorMode.SYSTEM;
        this.colorMode = mode;
        const appMode =
          mode === ThemeColorMode.DARK
            ? ConfigurationConstant.ColorMode.COLOR_MODE_DARK
            : mode === ThemeColorMode.LIGHT
              ? ConfigurationConstant.ColorMode.COLOR_MODE_LIGHT
              : ConfigurationConstant.ColorMode.COLOR_MODE_NOT_SET;
        try {
          if (wasSystem && mode !== ThemeColorMode.SYSTEM) {
            const currentSystem =
              this.systemColorMode === ConfigurationConstant.ColorMode.COLOR_MODE_DARK
                ? ConfigurationConstant.ColorMode.COLOR_MODE_DARK
                : ConfigurationConstant.ColorMode.COLOR_MODE_LIGHT;
            if (currentSystem !== appMode) {
              this.appContext?.setColorMode(currentSystem);
            }
          }
          this.appContext?.setColorMode(appMode);
        } catch {}
        this.mobileControls.forEach((control: NativeMobileControl): void =>
          control.setColorMode(mode),
        );
        this.tabBars.forEach((bar: NativeTabBar): void => bar.setColorMode(mode));
        this.desktop?.setColorMode(mode);
        this.system.setColorMode(mode);
        this.runtimeContext.setColorMode(mode);
      }
      return;
    }

    if (command.kind === "accessibility-live") {
      const text = command.text ?? "";
      if (!text) {
        return;
      }
      accessibility
        .sendAccessibilityEvent({
          type: "announceForAccessibility",
          bundleName: this.bundleName(),
          triggerAction: "common",
          textAnnouncedForAccessibility: text,
        })
        .catch((): void => {});
      return;
    }

    if (command.kind === "accessibility-event") {
      const announcement = command.announcement ?? undefined;
      const elementId = command.elementId;
      accessibility
        .sendAccessibilityEvent({
          type: announcement ? "announceForAccessibility" : "requestFocusForAccessibility",
          bundleName: this.bundleName(),
          triggerAction: "common",
          textAnnouncedForAccessibility: announcement,
          customId: elementId !== undefined && elementId !== null ? String(elementId) : undefined,
        })
        .catch((): void => {});
      return;
    }

    if (command.kind === "open-url") {
      const openId = command.openId ?? 0;
      const url = command.url ?? "";
      const finishOpen = (opened: boolean): void => {
        if (!this.closed) {
          try {
            this.mod.nshOpenUrlResult(this.sessionId, openId, opened);
          } catch {}
        }
      };
      if (!url || !/^[A-Za-z][A-Za-z0-9+.-]*:\S+$/.test(url)) {
        finishOpen(false);
        return;
      }
      this.abilityContext
        .openLink(url)
        .then((): void => finishOpen(true))
        .catch((): void => {
          if (this.closed) {
            return;
          }
          this.abilityContext
            .startAbility({
              action: "ohos.want.action.viewData",
              uri: url,
            })
            .then((): void => finishOpen(true))
            .catch((): void => finishOpen(false));
        });
      return;
    }

    if (command.kind === "pick-images") {
      const pickId = command.openId ?? 0;
      const limit = Math.max(1, Math.min(100, command.limit ?? 1));
      try {
        const photoPicker = new photoAccessHelper.PhotoViewPicker();
        const options = new photoAccessHelper.PhotoSelectOptions();
        options.MIMEType = photoAccessHelper.PhotoViewMIMETypes.IMAGE_TYPE;
        options.maxSelectNumber = limit;
        photoPicker
          .select(options)
          .then((result: photoAccessHelper.PhotoSelectResult): void => {
            if (!this.closed) {
              this.deliverPickedUris(pickId, result?.photoUris ?? [], limit, "image");
            }
          })
          .catch((): void => {
            if (!this.closed) {
              try {
                this.mod.nshPickImagesResult(this.sessionId, pickId, []);
              } catch {}
            }
          });
      } catch {
        if (!this.closed) {
          try {
            this.mod.nshPickImagesResult(this.sessionId, pickId, []);
          } catch {}
        }
      }
      return;
    }

    if (command.kind === "pick-files") {
      const pickId = command.openId ?? 0;
      const limit = Math.max(1, Math.min(100, command.limit ?? 1));
      try {
        const documentPicker = new picker.DocumentViewPicker(this.abilityContext);
        const options = new picker.DocumentSelectOptions();
        options.maxSelectNumber = limit;
        if (command.filters && command.filters.length > 0) {
          options.fileSuffixFilters = command.filters;
        }
        documentPicker
          .select(options)
          .then((uris: Array<string>): void => {
            if (!this.closed) {
              this.deliverPickedUris(pickId, uris ?? [], limit, "file");
            }
          })
          .catch((): void => {
            if (!this.closed) {
              try {
                this.mod.nshPickImagesResult(this.sessionId, pickId, []);
              } catch {}
            }
          });
      } catch {
        if (!this.closed) {
          try {
            this.mod.nshPickImagesResult(this.sessionId, pickId, []);
          } catch {}
        }
      }
    }
  }

  private deliverPickedUris(
    pickId: number,
    uris: Array<string>,
    limit: number,
    prefix: string,
  ): void {
    if (this.closed) {
      return;
    }
    const handles: fs.File[] = [];
    const files: PickedDescriptor[] = [];
    try {
      for (let i = 0; i < uris.length && i < limit; i++) {
        try {
          const file = fs.openSync(uris[i], fs.OpenMode.READ_ONLY);
          handles.push(file);
          const rawName = uris[i].split("/").pop() ?? `${prefix}_${i}`;
          let name = rawName;
          try {
            name = decodeURIComponent(rawName);
          } catch {}
          files.push({ fd: file.fd, name });
        } catch (error) {
          console.warn(`NSH picked ${prefix}: ${String(error)}`);
        }
      }
      // Rust retains independent descriptors synchronously; bytes are read on its worker.
      if (!this.closed) {
        this.mod.nshPickImagesResult(this.sessionId, pickId, files);
      }
    } finally {
      for (const file of handles) {
        try {
          fs.closeSync(file);
        } catch {}
      }
    }
  }
}

const nativeHostModule = getNativeModule();
nativeHostModule.nshRegisterSessionBinder(
  deviceInfo.sdkApiVersion,
  (uiContext: UIContext): number => HostSession.bind(uiContext),
  showNativeAlert,
);

function maximumRefreshRate(mainWindow: window.Window): number {
  const id = mainWindow.getWindowProperties().displayId;
  const screen =
    id === undefined ? display.getDefaultDisplaySync() : display.getDisplayByIdSync(id);
  return (screen.supportedRefreshRates ?? []).reduce(
    (maximum: number, hz: number): number => Math.max(maximum, hz),
    screen.refreshRate,
  );
}
