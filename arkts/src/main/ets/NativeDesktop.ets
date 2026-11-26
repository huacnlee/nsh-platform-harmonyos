import { common } from "@kit.AbilityKit";
import { util } from "@kit.ArkTS";
import { ColorMetrics, window } from "@kit.ArkUI";
import { deviceInfo, emitter } from "@kit.BasicServicesKit";
import type statusBarManager from "@hms.pcService.statusBarManager";
import { image } from "@kit.ImageKit";
import { BuilderNode, ComponentContent, FrameNode, NodeContent } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import display from "@ohos.display";
import pointer from "@ohos.multimodalInput.pointer";
import { NativeMenuCleanup } from "./NativeMenus";
import { syncActiveEditorImeCursor } from "./NativeEditors";
import {
  NSH_SUBWINDOW_BRIDGE_KEY,
  NSH_SUBWINDOW_ROUTE,
  SubWindowPageBridge,
} from "./NshSubWindowPage";

export interface WireMenuItem {
  kind: string;
  id?: number;
  label?: string;
  enabled?: boolean;
  checked?: boolean;
  shortcut?: string;
  role?: string;
  destructive?: boolean;
  items?: WireMenuItem[];
}

interface WireSize {
  width: number;
  height: number;
}

interface WireTrayIcon {
  format: string;
  width?: number;
  height?: number;
  whiteBase64?: string;
  blackBase64?: string;
  dataBase64?: string;
}

interface WireCommand {
  op: string;
  windowId?: number;
  kind?: string;
  modal?: boolean;
  parentWindowId?: number;
  position?: string;
  title?: string;
  titleBarStyle?: string;
  titleBarHeight?: number;
  frame?: string;
  minSize?: WireSize | null;
  maxSize?: WireSize | null;
  resizable?: boolean;
  movable?: boolean;
  minimizable?: boolean;
  maximizable?: boolean;
  closable?: boolean;
  focusable?: boolean;
  activate?: boolean;
  closeOnPress?: boolean;
  closeOnBlur?: boolean;
  level?: string;
  pointerEvents?: string;
  transparent?: boolean;
  backdrop?: string;
  shadow?: boolean;
  action?: string;
  mode?: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  contentSized?: boolean;
  titleBarHidden?: boolean;
  visible?: boolean;
  display?: string;
  critical?: boolean;
  generation?: number;
  targetId?: number;
  openIndex?: number;
  items?: WireMenuItem[];
  tooltip?: string;
  click?: number;
  icon?: WireTrayIcon;
  id?: number;
  hasOpenSubWindows?: boolean;
}

interface TrayMenuTarget {
  id: number;
  role?: string;
}

interface WireBoundsPayload {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface WireWindowStatePayload {
  mode: string;
  minimized: boolean;
  visible: boolean;
  focused: boolean;
  bounds: WireBoundsPayload;
  contentBounds: WireBoundsPayload;
  normalBounds: WireBoundsPayload;
  displayName?: string;
  scaleFactor: number;
}

interface WireTitleBarPayload {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

interface PopupMenuContentParams {
  owner: NativeDesktop;
  generation: number;
  items: WireMenuItem[];
  colorMode: ThemeColorMode;
}

@Observed
class MenuBarState {
  items: WireMenuItem[] = [];
  colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  activeIndex: number = -1;
  revision: number = 0;
}

interface MenuBarContentParams {
  owner: NativeDesktop;
  state: MenuBarState;
}

interface MenuBarDropdownLease {
  triggerIndex: number;
  menuContent: ComponentContent<PopupMenuContentParams>;
  opening: Promise<void>;
  retired: boolean;
  disappeared: boolean;
}

interface PopupMenuAnchorParams {
  anchorId: string;
  x: number;
  y: number;
  onReady: (uniqueId: number) => void;
}

interface PopupMenuLease {
  windowId?: number;
  generation: number;
  ui: UIContext;
  anchorContent: ComponentContent<PopupMenuAnchorParams>;
  menuContent?: ComponentContent<PopupMenuContentParams>;
  opening: Promise<void>;
  retired: boolean;
  disappeared: boolean;
  settled: boolean;
}

// HarmonyOS `statusBarManager` API limits (`@hms.pcService.statusBarManager.d.ts`).
const STATUS_BAR_MAX_MENU_ITEMS: number = 20;
const STATUS_BAR_MAX_HOVER_TIPS_CHARS: number = 128;
// Distinct from menu bar (`0`) and popup menu generations (`1..TRAY_MENU_GENERATION`).
const TRAY_MENU_GENERATION: number = 0xffffffff;
const KEYCODE_DPAD_LEFT: number = 2014;
const KEYCODE_DPAD_RIGHT: number = 2015;

export function isDesktopDevice(deviceType: string | undefined): boolean {
  const normalized = (deviceType ?? "").trim();
  return normalized === "2in1" || normalized === "pc" || normalized === "desktop";
}

function menuItemKey(item: WireMenuItem, index: number): string {
  return `${index}:${item.kind}:${item.id ?? 0}:${item.label ?? ""}`;
}

function splitMenuGroups(items: WireMenuItem[]): WireMenuItem[][] {
  const groups: WireMenuItem[][] = [];
  let current: WireMenuItem[] = [];
  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    if (item.kind === "separator") {
      if (current.length > 0) {
        groups.push(current);
        current = [];
      }
    } else {
      current.push(item);
    }
  }
  if (current.length > 0) {
    groups.push(current);
  }
  return groups;
}

@Component
struct DesktopSubmenuView {
  owner?: NativeDesktop;
  generation: number = 0;
  items: WireMenuItem[] = [];

  @Builder
  subMenuBuilder(children: WireMenuItem[]) {
    DesktopSubmenuView({
      owner: this.owner,
      generation: this.generation,
      items: children,
    });
  }

  build() {
    Menu() {
      ForEach(
        splitMenuGroups(this.items),
        (group: WireMenuItem[], groupIndex: number) => {
          MenuItemGroup() {
            ForEach(
              group,
              (item: WireMenuItem, index: number) => {
                if (item.kind === "submenu") {
                  MenuItem({
                    content: item.label ?? "",
                    builder:
                      item.enabled !== false && (item.items?.length ?? 0) > 0
                        ? (): void => {
                            this.subMenuBuilder(item.items ?? []);
                          }
                        : undefined,
                  }).enabled(item.enabled !== false);
                } else if (item.destructive) {
                  MenuItem({
                    content: item.label ?? "",
                    labelInfo: item.shortcut,
                  })
                    .selected(item.checked === true)
                    .selectIcon(item.checked !== undefined)
                    .enabled(item.enabled !== false)
                    .contentFontColor($r("sys.color.warning"))
                    .onClick((): void => {
                      this.owner?.selectMenuItem(this.generation, item.id ?? 0, item.role);
                    });
                } else {
                  MenuItem({
                    content: item.label ?? "",
                    labelInfo: item.shortcut,
                  })
                    .selected(item.checked === true)
                    .selectIcon(item.checked !== undefined)
                    .enabled(item.enabled !== false)
                    .onClick((): void => {
                      this.owner?.selectMenuItem(this.generation, item.id ?? 0, item.role);
                    });
                }
              },
              (item: WireMenuItem, index: number): string => menuItemKey(item, index),
            );
          };
        },
        (_group: WireMenuItem[], groupIndex: number): string => `g:${groupIndex}`,
      );
    }.onKeyEvent((event: KeyEvent): void => {
      if (this.generation !== 0 || event.type !== KeyType.Down) {
        return;
      }
      if (event.keyCode === KEYCODE_DPAD_LEFT) {
        this.owner?.stepMenuBarDropdown(-1);
      } else if (event.keyCode === KEYCODE_DPAD_RIGHT) {
        this.owner?.stepMenuBarDropdown(1);
      }
    });
  }
}

@Builder
function desktopPopupMenuBuilder(params: PopupMenuContentParams) {
  WithTheme({ colorMode: params.colorMode }) {
    DesktopSubmenuView({
      owner: params.owner,
      generation: params.generation,
      items: params.items,
    });
  };
}

@Component
struct DesktopMenuBarTrigger {
  owner?: NativeDesktop;
  @ObjectLink state!: MenuBarState;
  item: WireMenuItem = { kind: "action" };
  triggerIndex: number = 0;
  @State hovered: boolean = false;

  private anchorId(): string {
    return `nsh-menubar-item-${this.triggerIndex}`;
  }

  build() {
    Row() {
      Text(this.item.label ?? "")
        .fontSize($r("sys.float.Body_S"))
        .fontColor(
          this.item.enabled === false
            ? $r("sys.color.font_tertiary")
            : $r("sys.color.font_primary"),
        )
        .maxLines(1);
    }
    .id(this.anchorId())
    .height(26)
    .padding({ left: 10, right: 10 })
    .borderRadius(6)
    .alignItems(VerticalAlign.Center)
    .enabled(this.item.enabled !== false)
    .backgroundColor(
      (this.hovered || this.state.activeIndex === this.triggerIndex) && this.item.enabled !== false
        ? $r("sys.color.interactive_hover")
        : Color.Transparent,
    )
    .onHover((isHover: boolean): void => {
      this.hovered = isHover;
      if (isHover && this.item.enabled !== false) {
        this.owner?.hoverMenuBarTrigger(this.triggerIndex, this.item);
      }
    })
    .onClick((): void => {
      this.owner?.clickMenuBarTrigger(this.triggerIndex, this.item);
    });
  }
}

@Component
struct DesktopMenuBarView {
  owner?: NativeDesktop;
  @ObjectLink state!: MenuBarState;

  build() {
    WithTheme({ colorMode: this.state.colorMode }) {
      Column() {
        Scroll() {
          Row({ space: 2 }) {
            ForEach(
              this.state.items,
              (item: WireMenuItem, index: number): void => {
                if (item.kind !== "separator") {
                  DesktopMenuBarTrigger({
                    owner: this.owner,
                    state: this.state,
                    item,
                    triggerIndex: index,
                  });
                }
              },
              (item: WireMenuItem, index: number): string =>
                `${this.state.colorMode}:${menuItemKey(item, index)}:${item.enabled !== false ? 1 : 0}:${item.items?.length ?? 0}`,
            );
          }
          .height(31)
          .padding({ left: 8, right: 8 })
          .alignItems(VerticalAlign.Center);
        }
        .scrollable(ScrollDirection.Horizontal)
        .scrollBar(BarState.Off)
        .align(Alignment.Start)
        .width("100%")
        .height(31);

        Divider().color($r("sys.color.comp_divider")).strokeWidth("1px");
      }
      .width("100%")
      .height(32)
      .backgroundColor($r("sys.color.background_secondary"));
    };
  }
}

@Builder
function desktopMenuBarBuilder(params: MenuBarContentParams) {
  DesktopMenuBarView({
    owner: params.owner,
    state: params.state,
  });
}

@Component
struct DesktopPopupAnchor {
  anchorId: string = "";
  x: number = 0;
  y: number = 0;
  onReady?: (uniqueId: number) => void;
  private readyFired: boolean = false;

  build() {
    Stack({ alignContent: Alignment.TopStart }) {
      Column()
        .id(this.anchorId)
        .width(1)
        .height(1)
        .position({ x: this.x, y: this.y })
        .hitTestBehavior(HitTestMode.None)
        .onAreaChange((): void => {
          if (this.readyFired) {
            return;
          }
          const frame = this.getUIContext().getFrameNodeById(this.anchorId);
          if (frame !== null) {
            this.readyFired = true;
            this.onReady?.(frame.getUniqueId());
          }
        });
    }
    .width("100%")
    .height("100%")
    .hitTestBehavior(HitTestMode.None);
  }
}

@Builder
function desktopPopupAnchorBuilder(params: PopupMenuAnchorParams) {
  DesktopPopupAnchor({
    anchorId: params.anchorId,
    x: params.x,
    y: params.y,
    onReady: params.onReady,
  });
}

function resolveSubWindowZLevel(kind?: string, level?: string): number {
  if (level === "aboveDock") {
    return 200;
  }
  if (level === "floating") {
    return 100;
  }
  if (level === "desktopIcon") {
    return -100;
  }
  if (kind === "popup" || kind === "overlay") {
    return 150;
  }
  return 0;
}

// HarmonyOS `Window.setWindowDecorHeight` requires an integer in `[37, 112]` vp.
function clampDecorHeight(height: number): number {
  return Math.max(37, Math.min(112, Math.round(height)));
}

function resolveSystemWindowBackground(
  abilityContext: common.UIAbilityContext,
): string | ColorMetrics | undefined {
  if (deviceInfo.sdkApiVersion >= 18) {
    try {
      return ColorMetrics.resourceColor($r("sys.color.background_primary"));
    } catch {}
  }
  try {
    const argb = abilityContext.resourceManager.getColorSync($r("sys.color.background_primary").id);
    return `#${(argb >>> 0).toString(16).padStart(8, "0").toUpperCase()}`;
  } catch {
    return undefined;
  }
}

function resolveScaleFactor(displayId: number | undefined, fallbackUi: UIContext): number {
  try {
    const screen =
      displayId !== undefined
        ? display.getDisplayByIdSync(displayId)
        : display.getDefaultDisplaySync();
    if (screen.densityPixels > 0) {
      return Math.round(screen.densityPixels * 10000) / 10000;
    }
  } catch {}
  try {
    const vp = fallbackUi.vp2px(1);
    if (vp > 0) {
      return Math.round(vp * 10000) / 10000;
    }
  } catch {}
  return 1.0;
}

interface PixelArea {
  left: number;
  top: number;
  width: number;
  height: number;
}

const FNV_OFFSET_64: bigint = BigInt("0xcbf29ce484222325");
const FNV_PRIME_64: bigint = BigInt("0x00000100000001b3");
const MASK_64: bigint = BigInt("0xffffffffffffffff");

// 64-bit FNV-1a hex digest; matches `nsh_core::desktop::DisplayId::from_name` for ASCII names.
function fnv1a64Hex(input: string): string {
  let hash: bigint = FNV_OFFSET_64;
  for (let i = 0; i < input.length; i++) {
    hash = hash ^ BigInt(input.charCodeAt(i) & 0xff);
    hash = (hash * FNV_PRIME_64) & MASK_64;
  }
  return hash.toString(16).padStart(16, "0");
}

function displayIdHex(idName: string): string {
  return fnv1a64Hex(idName);
}

function trayIconDigest(icon?: WireTrayIcon): string {
  if (!icon) {
    return "default";
  }
  return fnv1a64Hex(
    `${icon.format}|${icon.width ?? 0}x${icon.height ?? 0}|${icon.whiteBase64 ?? ""}|${icon.blackBase64 ?? ""}|${icon.dataBase64 ?? ""}`,
  );
}

async function resolveTargetDisplay(
  displayHex?: string,
  fallbackDisplayId?: number,
): Promise<display.Display> {
  if (displayHex && displayHex.length > 0) {
    const wanted = displayHex.toLowerCase();
    try {
      const screens = await display.getAllDisplays();
      for (let i = 0; i < screens.length; i++) {
        if (displayIdHex(`display-${screens[i].id}`) === wanted) {
          return screens[i];
        }
      }
    } catch {}
  }
  if (fallbackDisplayId !== undefined) {
    try {
      return display.getDisplayByIdSync(fallbackDisplayId);
    } catch {}
  }
  return display.getDefaultDisplaySync();
}

async function displayAvailableRectPx(screen: display.Display): Promise<PixelArea> {
  const originX = screen.x ?? 0;
  const originY = screen.y ?? 0;
  try {
    const area = await screen.getAvailableArea();
    if (area.width > 0 && area.height > 0) {
      return {
        left: originX + area.left,
        top: originY + area.top,
        width: area.width,
        height: area.height,
      };
    }
  } catch {}
  return {
    left: originX,
    top: originY,
    width: screen.width,
    height: screen.height,
  };
}

interface WindowChromeState {
  frameNone: boolean;
  titleBarHidden: boolean;
  requestedTitleBarHeight?: number;
  maximizable: boolean;
  minimizable: boolean;
  closable: boolean;
  transparent: boolean;
  backdrop: string;
  backgroundOverridden: boolean;
}

interface WindowStatusResult {
  mode: string;
  minimized: boolean;
}

function windowStatusToState(status: window.WindowStatusType, currentMode: string): WindowStatusResult {
  if (status === window.WindowStatusType.FULL_SCREEN) {
    return { mode: "fullScreen", minimized: false };
  }
  if (status === window.WindowStatusType.MAXIMIZE) {
    return { mode: "maximized", minimized: false };
  }
  if (status === window.WindowStatusType.MINIMIZE) {
    return { mode: currentMode, minimized: true };
  }
  return { mode: "normal", minimized: false };
}

function buildWindowLimits(
  cmd: WireCommand,
  scale: number,
  defaults?: window.WindowLimits,
): window.WindowLimits | undefined {
  const limits: window.WindowLimits = {};
  let hasLimits = false;
  if (cmd.minSize !== undefined) {
    if (cmd.minSize !== null) {
      limits.minWidth = Math.max(1, Math.round(cmd.minSize.width * scale));
      limits.minHeight = Math.max(1, Math.round(cmd.minSize.height * scale));
      hasLimits = true;
    } else if (defaults !== undefined) {
      limits.minWidth = defaults.minWidth;
      limits.minHeight = defaults.minHeight;
      hasLimits = true;
    }
  }
  if (cmd.maxSize !== undefined) {
    if (cmd.maxSize !== null) {
      limits.maxWidth = Math.max(1, Math.round(cmd.maxSize.width * scale));
      limits.maxHeight = Math.max(1, Math.round(cmd.maxSize.height * scale));
      hasLimits = true;
    } else if (defaults !== undefined) {
      limits.maxWidth = defaults.maxWidth;
      limits.maxHeight = defaults.maxHeight;
      hasLimits = true;
    }
  }
  return hasLimits ? limits : undefined;
}

async function setWindowModeHelper(win: window.Window, mode: string): Promise<void> {
  if (mode === "maximized") {
    await win.maximize(window.MaximizePresentation.EXIT_IMMERSIVE);
  } else if (mode === "fullScreen") {
    await win.maximize(window.MaximizePresentation.ENTER_IMMERSIVE);
  } else {
    await win.recover();
  }
}

async function applyWindowBoundsHelper(
  win: window.Window,
  fallbackUi: UIContext,
  mode: string,
  x?: number,
  y?: number,
  width?: number,
  height?: number,
): Promise<boolean> {
  const props = win.getWindowProperties();
  const rect = props.windowRect;
  const scale = resolveScaleFactor(props.displayId, fallbackUi);
  const moveX = x !== undefined ? Math.round(x * scale) : rect.left;
  const moveY = y !== undefined ? Math.round(y * scale) : rect.top;
  const sizeW = width !== undefined ? Math.max(1, Math.round(width * scale)) : rect.width;
  const sizeH = height !== undefined ? Math.max(1, Math.round(height * scale)) : rect.height;
  const move =
    (x !== undefined || y !== undefined) &&
    (Math.abs(moveX - rect.left) > 1 || Math.abs(moveY - rect.top) > 1);
  const resize =
    (width !== undefined || height !== undefined) &&
    (Math.abs(sizeW - rect.width) > 1 || Math.abs(sizeH - rect.height) > 1);
  if (mode !== "normal") {
    try {
      await win.recover();
    } catch {}
  }
  if (move) {
    try {
      await win.moveWindowTo(moveX, moveY);
    } catch {}
  }
  if (resize) {
    try {
      await win.resize(sizeW, sizeH);
    } catch {}
  }
  return move;
}

function applyTitleBarAndFrameHelper(
  win: window.Window,
  cmd: WireCommand,
  state: WindowChromeState,
  applyOpaqueBackground: () => void,
  emitTitleBar: (payload: WireTitleBarPayload) => void,
): boolean {
  // Track `frameNone` and `titleBarHidden` independently (matching Rust `MainWindowState`)
  // so toggling `frame: none -> default` preserves an explicit `titleBarStyle: hidden`.
  if (cmd.titleBarStyle !== undefined) {
    state.titleBarHidden = cmd.titleBarStyle === "hidden";
  }
  if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0) {
    state.requestedTitleBarHeight = cmd.titleBarHeight;
  }
  if (cmd.frame === "none") {
    state.frameNone = true;
    try {
      win.setWindowDecorVisible(false);
    } catch {}
    try {
      win.setWindowTitleButtonVisible(false, false, false);
    } catch {}
    emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
    return false;
  }
  const restoredFrame = cmd.frame === "default" && state.frameNone;
  if (restoredFrame) {
    state.frameNone = false;
    if (state.backgroundOverridden && !state.transparent && state.backdrop === "none") {
      applyOpaqueBackground();
    }
  }
  if ((cmd.titleBarStyle !== undefined || restoredFrame) && !state.frameNone) {
    try {
      win.setWindowDecorVisible(!state.titleBarHidden);
    } catch {}
    if (restoredFrame) {
      try {
        win.setWindowTitleButtonVisible(state.maximizable, state.minimizable, state.closable);
      } catch {}
    }
    if (state.titleBarHidden) {
      try {
        if (state.requestedTitleBarHeight !== undefined) {
          win.setWindowDecorHeight(clampDecorHeight(state.requestedTitleBarHeight));
        }
      } catch {}
      return true;
    } else {
      emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
    }
  } else if (
    cmd.titleBarHeight !== undefined &&
    cmd.titleBarHeight > 0 &&
    state.titleBarHidden &&
    !state.frameNone
  ) {
    try {
      win.setWindowDecorHeight(clampDecorHeight(cmd.titleBarHeight));
    } catch {}
    return true;
  }
  return false;
}

class SubWindowHost {
  readonly windowId: number;
  private readonly owner: NativeDesktop;
  private win?: window.Window;
  private ui?: UIContext;
  private pageBridge?: SubWindowPageBridge;
  private queue: Promise<void> = Promise.resolve();
  private destroyed: boolean = false;
  private closingByHost: boolean = false;
  private isModal: boolean = false;
  private frameNone: boolean = false;
  private transparent: boolean = false;
  private backdrop: string = "none";
  private backgroundOverridden: boolean = false;
  private shadowDisabled: boolean = false;
  private titleBarHidden: boolean = false;
  private requestedTitleBarHeight?: number = undefined;
  private maximizable: boolean = true;
  private minimizable: boolean = true;
  private closable: boolean = true;
  private closeOnBlur: boolean = false;
  private hadFocus: boolean = false;
  private focused: boolean = false;
  private visible: boolean = false;
  private mode: string = "normal";
  private minimized: boolean = false;
  private initialPosition: string = "auto";
  private parentWindowId?: number;
  private userPositioned: boolean = false;
  private hiddenByApp: boolean = false;
  private normalBounds?: WireBoundsPayload;
  private lastStateJson: string = "";
  private lastTitleBarJson: string = "";
  private titleButtonSubscribed: boolean = false;
  private pendingBounds?: WireCommand;
  private flushingBounds: boolean = false;
  private pendingFocusId?: string;
  private defaultLimits?: window.WindowLimits;

  private readonly onRectChange = (data: window.RectChangeOptions): void => {
    if (this.destroyed) {
      return;
    }
    if (
      data.reason === window.RectChangeReason.DRAG_START ||
      data.reason === window.RectChangeReason.DRAG ||
      data.reason === window.RectChangeReason.DRAG_END ||
      data.reason === window.RectChangeReason.MOVE
    ) {
      this.userPositioned = true;
    }
    this.syncState();
    syncActiveEditorImeCursor();
  };

  private readonly onStatusChange = (status: window.WindowStatusType): void => {
    if (this.destroyed) {
      return;
    }
    this.applyWindowStatus(status);
    this.syncState();
    if (this.titleBarHidden && !this.frameNone) {
      this.syncTitleBar();
    }
    syncActiveEditorImeCursor();
  };

  private readonly onWindowEvent = (event: window.WindowEventType): void => {
    if (this.destroyed) {
      return;
    }
    let resyncTitleBar = false;
    switch (event) {
      case window.WindowEventType.WINDOW_SHOWN:
        this.visible = true;
        this.hiddenByApp = false;
        resyncTitleBar = true;
        break;
      case window.WindowEventType.WINDOW_ACTIVE:
        this.visible = true;
        this.focused = true;
        this.minimized = false;
        this.hiddenByApp = false;
        this.hadFocus = true;
        this.flushPendingFocus();
        resyncTitleBar = true;
        break;
      case window.WindowEventType.WINDOW_INACTIVE:
        this.focused = false;
        if (this.closeOnBlur && this.hadFocus && this.visible && !this.closingByHost) {
          this.owner.emitEvent("sub-window-blur", JSON.stringify({ windowId: this.windowId }));
        }
        break;
      case window.WindowEventType.WINDOW_HIDDEN:
        this.visible = false;
        this.focused = false;
        break;
      case window.WindowEventType.WINDOW_DESTROYED:
        this.destroyed = true;
        this.owner.onSubWindowRemoved(this.windowId);
        if (!this.closingByHost) {
          this.owner.emitEvent(
            "sub-window-destroyed",
            JSON.stringify({ windowId: this.windowId }),
          );
        }
        return;
    }
    this.syncState();
    if (resyncTitleBar && this.titleBarHidden && !this.frameNone) {
      this.syncTitleBar();
    }
    syncActiveEditorImeCursor();
  };

  private readonly onTitleButtonRectChange = (rect: window.TitleButtonRect): void => {
    if (!this.destroyed && this.titleBarHidden && !this.frameNone) {
      this.reportTitleBar(rect);
    }
  };

  private readonly onWindowWillClose = (): Promise<boolean> => {
    if (!this.destroyed && !this.closingByHost) {
      this.owner.emitEvent(
        "sub-window-close-press",
        JSON.stringify({ windowId: this.windowId, reason: "closeButton" }),
      );
      // Always defer teardown to Rust so ContentRoot unmounts cleanly before destroyWindow().
      return Promise.resolve(true);
    }
    return Promise.resolve(false);
  };

  constructor(owner: NativeDesktop, windowId: number) {
    this.owner = owner;
    this.windowId = windowId;
  }

  windowRef(): window.Window | undefined {
    return this.destroyed ? undefined : this.win;
  }

  uiContext(): UIContext | undefined {
    return this.destroyed ? undefined : this.ui;
  }

  widthVp(): number {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return 0;
    }
    try {
      const props = sub.getWindowProperties();
      const rect = props.drawableRect.width > 0 ? props.drawableRect : props.windowRect;
      const scale = resolveScaleFactor(props.displayId, this.ui ?? this.owner.mainUiContext());
      return rect.width / scale;
    } catch {
      return 0;
    }
  }

  heightVp(): number {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return 0;
    }
    try {
      const props = sub.getWindowProperties();
      const rect = props.drawableRect.height > 0 ? props.drawableRect : props.windowRect;
      const scale = resolveScaleFactor(props.displayId, this.ui ?? this.owner.mainUiContext());
      return rect.height / scale;
    } catch {
      return 0;
    }
  }

  updateCursor(style?: pointer.PointerStyle): void {
    const ui = this.ui;
    if (!ui || this.destroyed) {
      return;
    }
    try {
      const controller = ui.getCursorController();
      if (style === undefined) {
        controller.restoreDefault();
      } else {
        controller.setCursor(style);
      }
    } catch {}
  }

  setColorMode(mode: ThemeColorMode): void {
    if (this.destroyed) {
      return;
    }
    this.pageBridge?.setColorMode(mode);
    if (this.backgroundOverridden && !this.transparent && this.backdrop === "none" && !this.frameNone) {
      this.applyOpaqueBackground();
    }
  }

  private applyOpaqueBackground(): void {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    const bg = resolveSystemWindowBackground(this.owner.abilityContextRef());
    if (bg !== undefined) {
      try {
        sub.setWindowBackgroundColor(bg);
      } catch {}
    }
  }

  requestComponentFocus(componentId: string): boolean {
    const ui = this.ui;
    if (!ui || this.destroyed) {
      return false;
    }
    try {
      if (ui.getFrameNodeById(componentId) === null) {
        return false;
      }
    } catch {
      return false;
    }
    if (!this.visible || !this.focused) {
      this.pendingFocusId = componentId;
    }
    try {
      ui.getFocusController().requestFocus(componentId);
    } catch {
      this.pendingFocusId = componentId;
    }
    return true;
  }

  private flushPendingFocus(): void {
    const id = this.pendingFocusId;
    const ui = this.ui;
    if (!id || !ui || this.destroyed) {
      return;
    }
    try {
      ui.getFocusController().requestFocus(id);
      this.pendingFocusId = undefined;
    } catch {}
  }

  open(cmd: WireCommand): void {
    this.enqueue(async (): Promise<void> => {
      if (this.destroyed) {
        return;
      }
      await this.openInternal(cmd);
    });
  }

  setAttributes(cmd: WireCommand): void {
    this.enqueue(async (): Promise<void> => {
      if (this.destroyed || !this.win) {
        return;
      }
      await this.applyAttributesInternal(cmd);
    });
  }

  request(cmd: WireCommand): void {
    if (cmd.action === "setBounds") {
      this.pendingBounds = cmd;
      if (this.flushingBounds) {
        return;
      }
      this.flushingBounds = true;
      this.enqueue(async (): Promise<void> => {
        try {
          while (!this.destroyed && this.win && this.pendingBounds !== undefined) {
            const next = this.pendingBounds;
            this.pendingBounds = undefined;
            await this.applyBoundsInternal(next.x, next.y, next.width, next.height);
          }
        } finally {
          this.flushingBounds = false;
        }
      });
      return;
    }
    this.enqueue(async (): Promise<void> => {
      if (this.destroyed || !this.win) {
        return;
      }
      await this.applyRequestInternal(cmd);
    });
  }

  startMoving(): void {
    this.enqueue(async (): Promise<void> => {
      if (this.destroyed || !this.win) {
        return;
      }
      this.userPositioned = true;
      try {
        await this.win.startMoving();
      } catch {}
    });
  }

  toggleMaximize(): void {
    this.enqueue(async (): Promise<void> => {
      if (this.destroyed || !this.win) {
        return;
      }
      if (this.mode === "maximized" || this.mode === "fullScreen") {
        await this.setModeInternal("normal");
      } else {
        await this.setModeInternal("maximized");
      }
    });
  }

  close(): void {
    this.closingByHost = true;
    this.destroyed = true;
    this.pageBridge = undefined;
    this.owner.onSubWindowRemoved(this.windowId);
    const sub = this.win;
    if (sub !== undefined) {
      this.win = undefined;
      this.ui = undefined;
      this.unsubscribe(sub);
      try {
        sub.destroyWindow().catch((): void => {});
      } catch {}
    }
  }

  private enqueue(task: () => Promise<void>): void {
    this.queue = this.queue
      .then(task)
      .catch((error: Object): void => {
        console.warn(`[NSH] subwindow(${this.windowId}) command failed: ${String(error)}`);
      });
  }

  private async computeInitialRect(cmd: WireCommand): Promise<window.Rect> {
    const parentWin =
      (cmd.parentWindowId !== undefined ? this.owner.getSubWindowRef(cmd.parentWindowId) : undefined) ??
      this.owner.mainWindowRef();
    let parentProps: window.WindowProperties | undefined = undefined;
    try {
      parentProps = parentWin.getWindowProperties();
    } catch {}
    const screen = await resolveTargetDisplay(cmd.display, parentProps?.displayId);
    const scale = resolveScaleFactor(screen.id, this.owner.mainUiContext());
    const area = await displayAvailableRectPx(screen);
    const screenLeft = area.left;
    const screenTop = area.top;
    const screenWidth = area.width;
    const screenHeight = area.height;
    const widthPx = Math.max(1, Math.round((cmd.width ?? 1) * scale));
    const heightPx = Math.max(1, Math.round((cmd.height ?? 1) * scale));
    const parentRect = parentProps?.windowRect;

    let leftPx = screenLeft + Math.round((screenWidth - widthPx) / 2);
    let topPx = screenTop + Math.round((screenHeight - heightPx) / 2);
    const pos = cmd.position ?? "auto";
    if (pos === "at" && cmd.x !== undefined && cmd.y !== undefined) {
      this.userPositioned = true;
      return {
        left: Math.round(cmd.x * scale),
        top: Math.round(cmd.y * scale),
        width: widthPx,
        height: heightPx,
      };
    } else if (pos === "parentCenter" && !cmd.display && parentRect !== undefined) {
      leftPx = Math.round(parentRect.left + (parentRect.width - widthPx) / 2);
      topPx = Math.round(parentRect.top + (parentRect.height - heightPx) / 2);
    } else if (pos === "center" || pos === "parentCenter") {
      leftPx = screenLeft + Math.round((screenWidth - widthPx) / 2);
      topPx = screenTop + Math.round((screenHeight - heightPx) / 2);
    } else if (cmd.kind === "panel" && parentRect !== undefined) {
      const gap = Math.round(16 * scale);
      if (parentRect.left + parentRect.width + gap + widthPx <= screenLeft + screenWidth) {
        leftPx = parentRect.left + parentRect.width + gap;
      } else if (parentRect.left - gap - widthPx >= screenLeft) {
        leftPx = parentRect.left - gap - widthPx;
      } else {
        leftPx = screenLeft + Math.max(0, screenWidth - widthPx);
      }
      topPx = parentRect.top;
    } else if ((cmd.kind === "popup" || cmd.kind === "overlay") && parentRect !== undefined) {
      leftPx = Math.round(parentRect.left + (parentRect.width - widthPx) / 2);
      topPx = Math.round(parentRect.top + (parentRect.height - heightPx) / 2);
    } else if (!cmd.display && parentRect !== undefined) {
      const step = Math.round(32 * scale);
      const offset = ((this.owner.nextCascade() % 6) + 1) * step;
      leftPx = parentRect.left + offset;
      topPx = parentRect.top + offset;
    }

    if (screenWidth > 0 && screenHeight > 0) {
      const maxLeft = screenLeft + Math.max(0, screenWidth - widthPx);
      const maxTop = screenTop + Math.max(0, screenHeight - heightPx);
      leftPx = Math.max(screenLeft, Math.min(maxLeft, leftPx));
      topPx = Math.max(screenTop, Math.min(maxTop, topPx));
    }
    return { left: leftPx, top: topPx, width: widthPx, height: heightPx };
  }

  private async openInternal(cmd: WireCommand): Promise<void> {
    this.isModal = cmd.modal === true;
    this.frameNone = cmd.frame === "none";
    this.transparent = cmd.transparent === true;
    this.backdrop = cmd.backdrop ?? "none";
    this.titleBarHidden = cmd.titleBarStyle === "hidden";
    if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0) {
      this.requestedTitleBarHeight = cmd.titleBarHeight;
    }
    this.closeOnBlur = cmd.closeOnBlur === true;
    this.initialPosition = cmd.position ?? "auto";
    this.parentWindowId = cmd.parentWindowId;
    this.maximizable = cmd.maximizable !== false;
    this.minimizable = cmd.minimizable !== false;
    this.closable = cmd.closable !== false;

    const initialRect = await this.computeInitialRect(cmd);
    if (this.destroyed) {
      return;
    }
    const parentSub =
      this.parentWindowId !== undefined
        ? this.owner.getSubWindowRef(this.parentWindowId)
        : undefined;
    const createdOnMain = parentSub === undefined;
    const decorEnabled = !this.frameNone;
    const options: window.SubWindowOptions = {
      title: cmd.title ?? "",
      decorEnabled,
    };
    if (this.isModal) {
      options.isModal = true;
      if (deviceInfo.sdkApiVersion >= 14) {
        options.modalityType = window.ModalityType.WINDOW_MODALITY;
      }
    } else if (deviceInfo.sdkApiVersion >= 18) {
      options.zLevel = resolveSubWindowZLevel(cmd.kind, cmd.level);
      if (
        createdOnMain &&
        deviceInfo.sdkApiVersion >= 26 &&
        this.parentWindowId === undefined
      ) {
        // Only supported when the parent is the main window. Loosening the
        // z-level restriction lets independent subwindows stack and survive
        // independently of the parked/minimized main window.
        const rawOptions: ESObject = options;
        rawOptions["zLevelAboveParentLoosened"] = true;
      }
    }
    if (deviceInfo.sdkApiVersion >= 18) {
      options.windowRect = initialRect;
    }
    if (deviceInfo.sdkApiVersion >= 19) {
      options.maximizeSupported =
        this.maximizable &&
        cmd.kind !== "dialog" &&
        cmd.kind !== "popup" &&
        cmd.kind !== "overlay";
    }
    if (deviceInfo.sdkApiVersion >= 20) {
      options.outlineEnabled = !this.transparent && cmd.shadow !== false;
    }

    const subName = `nsh_sub_${this.windowId}_${Date.now()}`;
    let sub: window.Window;
    if (parentSub !== undefined) {
      try {
        sub = await parentSub.createSubWindowWithOptions(subName, options);
      } catch {
        sub = await this.owner.mainWindowRef().createSubWindowWithOptions(subName, options);
      }
    } else {
      sub = await this.owner.mainWindowRef().createSubWindowWithOptions(subName, options);
    }
    if (this.destroyed) {
      try {
        await sub.destroyWindow();
      } catch {}
      return;
    }
    this.win = sub;
    try {
      this.defaultLimits = sub.getWindowLimits();
    } catch {}
    this.subscribe(sub);

    if (cmd.display && deviceInfo.sdkApiVersion >= 15) {
      try {
        const targetScreen = await resolveTargetDisplay(cmd.display);
        await sub.moveWindowToGlobal(initialRect.left, initialRect.top, {
          displayId: targetScreen.id,
        });
      } catch {}
    } else if (deviceInfo.sdkApiVersion < 18) {
      try {
        await sub.moveWindowTo(initialRect.left, initialRect.top);
      } catch {}
      try {
        await sub.resize(initialRect.width, initialRect.height);
      } catch {}
    }

    const content = new NodeContent();
    const bridge = new SubWindowPageBridge(content, this.backdrop, this.owner.activeColorMode());
    this.pageBridge = bridge;
    const storage = new LocalStorage();
    storage.setOrCreate(NSH_SUBWINDOW_BRIDGE_KEY, bridge);
    await sub.loadContentByName(NSH_SUBWINDOW_ROUTE, storage);
    if (this.destroyed) {
      this.unsubscribe(sub);
      this.win = undefined;
      this.pageBridge = undefined;
      try {
        await sub.destroyWindow();
      } catch {}
      return;
    }

    this.subscribeContentEvents(sub);

    try {
      this.ui = sub.getUIContext();
    } catch {}

    let platformWindowId = 0;
    try {
      platformWindowId = sub.getWindowProperties().id;
    } catch {}

    if (this.transparent || this.frameNone || this.backdrop !== "none") {
      this.backgroundOverridden = true;
      try {
        sub.setWindowBackgroundColor("#00000000");
      } catch {}
    }
    if (this.transparent && cmd.shadow === undefined && deviceInfo.sdkApiVersion >= 17) {
      this.shadowDisabled = true;
      try {
        sub.setWindowShadowRadius(0);
      } catch {}
    }
    await this.applyAttributesInternal(cmd);
    if (this.destroyed) {
      return;
    }

    const attached = this.owner.attachSubWindow(this.windowId, content, platformWindowId);
    if (!attached || this.destroyed) {
      this.destroyed = true;
      this.closingByHost = true;
      this.pageBridge = undefined;
      this.owner.onSubWindowRemoved(this.windowId);
      this.unsubscribe(sub);
      this.win = undefined;
      this.ui = undefined;
      try {
        await sub.destroyWindow();
      } catch {}
      return;
    }

    if (deviceInfo.sdkApiVersion >= 20 && cmd.activate === false && !this.isModal) {
      try {
        await sub.showWindow({ focusOnShow: false });
      } catch {
        await sub.showWindow();
      }
    } else {
      await sub.showWindow();
    }
    if (this.destroyed) {
      return;
    }

    if (this.transparent || this.frameNone || this.backdrop !== "none") {
      try {
        sub.setWindowBackgroundColor("#00000000");
      } catch {}
    }
    try {
      this.visible = sub.isWindowShowing();
      this.focused = sub.isFocused();
      if (this.focused) {
        this.hadFocus = true;
      }
      this.applyWindowStatus(sub.getWindowStatus());
    } catch {}
    this.flushPendingFocus();
    if (cmd.mode && cmd.mode !== "normal") {
      await this.setModeInternal(cmd.mode);
    }
    this.syncTitleBar();
    this.syncState();
  }

  private subscribe(sub: window.Window): void {
    try {
      sub.on("windowRectChange", this.onRectChange);
    } catch {}
    try {
      sub.on("windowStatusChange", this.onStatusChange);
    } catch {}
    try {
      sub.on("windowEvent", this.onWindowEvent);
    } catch {}
    try {
      sub.on("windowWillClose", this.onWindowWillClose);
    } catch {}
  }

  private subscribeContentEvents(sub: window.Window): void {
    if (this.titleButtonSubscribed) {
      return;
    }
    try {
      sub.on("windowTitleButtonRectChange", this.onTitleButtonRectChange);
      this.titleButtonSubscribed = true;
    } catch (error) {
      console.warn(
        `[NSH] subwindow(${this.windowId}) windowTitleButtonRectChange subscribe failed: ${String(error)}`,
      );
    }
  }

  private unsubscribe(sub: window.Window): void {
    try {
      sub.off("windowRectChange", this.onRectChange);
    } catch {}
    try {
      sub.off("windowStatusChange", this.onStatusChange);
    } catch {}
    try {
      sub.off("windowEvent", this.onWindowEvent);
    } catch {}
    if (this.titleButtonSubscribed) {
      this.titleButtonSubscribed = false;
      try {
        sub.off("windowTitleButtonRectChange", this.onTitleButtonRectChange);
      } catch {}
    }
    try {
      sub.off("windowWillClose", this.onWindowWillClose);
    } catch {}
  }

  private applyWindowStatus(status: window.WindowStatusType): void {
    const next = windowStatusToState(status, this.mode);
    this.mode = next.mode;
    this.minimized = next.minimized;
  }

  private async applyAttributesInternal(cmd: WireCommand): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    if (cmd.closeOnBlur !== undefined) {
      this.closeOnBlur = cmd.closeOnBlur;
    }
    if (cmd.transparent !== undefined) {
      this.transparent = cmd.transparent;
      if (this.transparent) {
        this.backgroundOverridden = true;
        try {
          sub.setWindowBackgroundColor("#00000000");
        } catch {}
      } else if (this.backgroundOverridden && this.backdrop === "none" && !this.frameNone) {
        this.applyOpaqueBackground();
      }
    }
    if (cmd.backdrop !== undefined) {
      this.backdrop = cmd.backdrop;
      this.pageBridge?.setBackdrop(this.backdrop);
      if (this.backdrop !== "none") {
        this.backgroundOverridden = true;
        try {
          sub.setWindowBackgroundColor("#00000000");
        } catch {}
      } else if (this.backgroundOverridden && !this.transparent && !this.frameNone) {
        this.applyOpaqueBackground();
      }
    }
    if (cmd.title !== undefined) {
      try {
        await sub.setWindowTitle(cmd.title);
      } catch {}
    }
    const buttonsChanged =
      cmd.maximizable !== undefined ||
      cmd.minimizable !== undefined ||
      cmd.closable !== undefined;
    if (cmd.maximizable !== undefined) {
      this.maximizable = cmd.maximizable;
    }
    if (cmd.minimizable !== undefined) {
      this.minimizable = cmd.minimizable;
    }
    if (cmd.closable !== undefined) {
      this.closable = cmd.closable;
    }
    const chromeState: WindowChromeState = {
      frameNone: this.frameNone,
      titleBarHidden: this.titleBarHidden,
      requestedTitleBarHeight: this.requestedTitleBarHeight,
      maximizable: this.maximizable,
      minimizable: this.minimizable,
      closable: this.closable,
      transparent: this.transparent,
      backdrop: this.backdrop,
      backgroundOverridden: this.backgroundOverridden,
    };
    const needsTitleBarSync = applyTitleBarAndFrameHelper(
      sub,
      cmd,
      chromeState,
      (): void => this.applyOpaqueBackground(),
      (payload: WireTitleBarPayload): void => this.emitTitleBar(payload),
    );
    this.frameNone = chromeState.frameNone;
    this.titleBarHidden = chromeState.titleBarHidden;
    this.requestedTitleBarHeight = chromeState.requestedTitleBarHeight;
    if (buttonsChanged && !this.frameNone) {
      try {
        sub.setWindowTitleButtonVisible(this.maximizable, this.minimizable, this.closable);
      } catch {}
    }
    if (needsTitleBarSync || (buttonsChanged && !this.frameNone && this.titleBarHidden)) {
      this.syncTitleBar();
    }

    if (cmd.minSize !== undefined || cmd.maxSize !== undefined) {
      const scale = resolveScaleFactor(
        sub.getWindowProperties().displayId,
        this.ui ?? this.owner.mainUiContext(),
      );
      const limits = buildWindowLimits(cmd, scale, this.defaultLimits);
      if (limits !== undefined) {
        try {
          await sub.setWindowLimits(limits);
        } catch {}
      }
    }

    if (cmd.level !== undefined && !this.isModal) {
      if (deviceInfo.sdkApiVersion >= 20) {
        try {
          await sub.setSubWindowZLevel(resolveSubWindowZLevel(cmd.kind, cmd.level));
        } catch {}
      }
    }

    if (cmd.focusable !== undefined) {
      try {
        await sub.setWindowFocusable(cmd.focusable);
      } catch {}
    }

    if (cmd.pointerEvents !== undefined) {
      try {
        await sub.setWindowTouchable(cmd.pointerEvents !== "none");
      } catch {}
    }

    if (cmd.resizable !== undefined) {
      try {
        await sub.setResizeByDragEnabled(cmd.resizable);
      } catch {}
    }

    if (cmd.movable !== undefined) {
      try {
        sub.setWindowTitleMoveEnabled(cmd.movable);
      } catch {}
    }

    if (cmd.shadow !== undefined && deviceInfo.sdkApiVersion >= 17) {
      if (!cmd.shadow) {
        this.shadowDisabled = true;
        try {
          sub.setWindowShadowRadius(0);
        } catch {}
      } else if (this.shadowDisabled) {
        // HarmonyOS has no `getWindowShadowRadius` API; leave native shadow untouched unless
        // re-enabling after an explicit disable.
        this.shadowDisabled = false;
        const scale = resolveScaleFactor(
          sub.getWindowProperties().displayId,
          this.ui ?? this.owner.mainUiContext(),
        );
        try {
          sub.setWindowShadowRadius(Math.round(16 * scale));
        } catch {}
      }
    }
  }

  private async applyRequestInternal(cmd: WireCommand): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    switch (cmd.action) {
      case "show":
        this.hiddenByApp = false;
        if (deviceInfo.sdkApiVersion >= 20) {
          try {
            await sub.showWindow({ focusOnShow: false });
          } catch {
            try {
              await sub.showWindow();
            } catch {}
          }
        } else {
          try {
            await sub.showWindow();
          } catch {}
        }
        this.syncState();
        break;
      case "focus":
        this.hiddenByApp = false;
        try {
          await sub.showWindow();
        } catch {}
        this.syncState();
        break;
      case "hide":
        this.hiddenByApp = true;
        try {
          // Public HarmonyOS window.Window exposes minimize() without a separate hide() API;
          // hiddenByApp keeps hidden distinct from minimized in state reports to Rust.
          await sub.minimize();
        } catch {}
        this.syncState();
        break;
      case "minimize":
        this.hiddenByApp = false;
        try {
          await sub.minimize();
        } catch {}
        this.syncState();
        break;
      case "restore":
        this.hiddenByApp = false;
        try {
          await sub.restore();
        } catch {
          try {
            await sub.recover();
          } catch {}
        }
        try {
          await sub.showWindow();
        } catch {}
        this.syncState();
        break;
      case "setMode":
        await this.setModeInternal(cmd.mode ?? "normal");
        break;
      case "setBounds":
        await this.applyBoundsInternal(cmd.x, cmd.y, cmd.width, cmd.height);
        break;
      case "setContentSize":
        await this.applyContentSizeInternal(cmd.width, cmd.height);
        break;
      case "setPlacement":
        await this.applyBoundsInternal(cmd.x, cmd.y, cmd.width, cmd.height);
        if (cmd.mode !== undefined && (cmd.mode !== "normal" || this.mode !== "normal")) {
          await this.setModeInternal(cmd.mode);
        }
        break;
      case "center":
        await this.centerInternal(false, undefined, undefined, cmd.display);
        break;
      case "requestAttention":
        // HarmonyOS public API 26 exposes no non-activating taskbar-flash or
        // dock-bounce API; RequestAttention must never restore, raise, or steal focus.
        break;
    }
  }

  private async setModeInternal(mode: string): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      await setWindowModeHelper(sub, mode);
      this.syncState();
    } catch {}
  }

  private async applyBoundsInternal(
    x?: number,
    y?: number,
    width?: number,
    height?: number,
  ): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      const moved = await applyWindowBoundsHelper(
        sub,
        this.ui ?? this.owner.mainUiContext(),
        this.mode,
        x,
        y,
        width,
        height,
      );
      if (moved) {
        this.userPositioned = true;
      }
      this.syncState();
    } catch {}
  }

  private async applyContentSizeInternal(width?: number, height?: number): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      const props = sub.getWindowProperties();
      const rect = props.windowRect;
      const drawable = props.drawableRect;
      const scale = resolveScaleFactor(props.displayId, this.ui ?? this.owner.mainUiContext());
      const chromeW = drawable.width > 0 ? Math.max(0, rect.width - drawable.width) : 0;
      const chromeH = drawable.height > 0 ? Math.max(0, rect.height - drawable.height) : 0;
      const targetW =
        width !== undefined ? Math.max(1, Math.round(width * scale) + chromeW) : rect.width;
      const targetH =
        height !== undefined ? Math.max(1, Math.round(height * scale) + chromeH) : rect.height;
      if (Math.abs(targetW - rect.width) <= 1 && Math.abs(targetH - rect.height) <= 1) {
        return;
      }
      await sub.resize(targetW, targetH);
      if (
        !this.userPositioned &&
        (this.initialPosition === "center" || this.initialPosition === "parentCenter")
      ) {
        await this.centerInternal(this.initialPosition === "parentCenter", targetW, targetH);
      }
      this.syncState();
    } catch {}
  }

  private async centerInternal(
    onParent: boolean,
    overrideW?: number,
    overrideH?: number,
    displayHex?: string,
  ): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      const props = sub.getWindowProperties();
      const rect = props.windowRect;
      const w = overrideW ?? rect.width;
      const h = overrideH ?? rect.height;
      const screen = await resolveTargetDisplay(displayHex, props.displayId);
      const area = await displayAvailableRectPx(screen);
      let targetX = area.left + Math.round((area.width - w) / 2);
      let targetY = area.top + Math.round((area.height - h) / 2);
      if (onParent && !displayHex) {
        const parentWin =
          (this.parentWindowId !== undefined
            ? this.owner.getSubWindowRef(this.parentWindowId)
            : undefined) ?? this.owner.mainWindowRef();
        try {
          const parentRect = parentWin.getWindowProperties().windowRect;
          targetX = Math.round(parentRect.left + (parentRect.width - w) / 2);
          targetY = Math.round(parentRect.top + (parentRect.height - h) / 2);
        } catch {}
      }
      const minX = area.left;
      const minY = area.top;
      const maxX = area.left + Math.max(0, area.width - w);
      const maxY = area.top + Math.max(0, area.height - h);
      targetX = Math.max(minX, Math.min(maxX, targetX));
      targetY = Math.max(minY, Math.min(maxY, targetY));
      if (displayHex && deviceInfo.sdkApiVersion >= 15) {
        try {
          await sub.moveWindowToGlobal(targetX, targetY, { displayId: screen.id });
          this.syncState();
          return;
        } catch {}
      }
      await sub.moveWindowTo(targetX, targetY);
      this.syncState();
    } catch {}
  }

  private syncTitleBar(): void {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    if (this.frameNone || !this.titleBarHidden) {
      this.emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
      return;
    }
    try {
      const rect = sub.getTitleButtonRect();
      this.reportTitleBar(rect);
    } catch {
      let height = this.requestedTitleBarHeight ?? 0;
      try {
        const decor = sub.getWindowDecorHeight();
        if (decor > 0) {
          height = Math.max(height, decor);
        }
      } catch {}
      this.emitTitleBar({ top: height, right: 0, bottom: 0, left: 0 });
    }
  }

  private reportTitleBar(rect: window.TitleButtonRect): void {
    const sub = this.win;
    let height = this.requestedTitleBarHeight ?? 0;
    if (sub) {
      try {
        const decor = sub.getWindowDecorHeight();
        if (decor > 0) {
          height = Math.max(height, decor);
        }
      } catch {}
    }
    const top = Math.max(height, rect.top + rect.height);
    const right = Math.max(0, rect.right + rect.width);
    this.emitTitleBar({ top, right, bottom: 0, left: 0 });
  }

  private emitTitleBar(payload: WireTitleBarPayload): void {
    if (this.destroyed) {
      return;
    }
    const json = JSON.stringify({
      windowId: this.windowId,
      top: payload.top,
      right: payload.right,
      bottom: payload.bottom,
      left: payload.left,
    });
    if (json !== this.lastTitleBarJson) {
      this.lastTitleBarJson = json;
      this.owner.emitEvent("sub-window-title-bar", json);
    }
  }

  private syncState(): void {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      const props = sub.getWindowProperties();
      const rect = props.windowRect;
      const drawable = props.drawableRect;
      const scale = resolveScaleFactor(props.displayId, this.ui ?? this.owner.mainUiContext());
      let displayName: string | undefined = undefined;
      try {
        const screen =
          props.displayId !== undefined
            ? display.getDisplayByIdSync(props.displayId)
            : display.getDefaultDisplaySync();
        displayName = `display-${screen.id}`;
      } catch {}
      const bounds: WireBoundsPayload = {
        x: rect.left / scale,
        y: rect.top / scale,
        width: rect.width / scale,
        height: rect.height / scale,
      };
      const contentBounds: WireBoundsPayload = {
        x: (rect.left + drawable.left) / scale,
        y: (rect.top + drawable.top) / scale,
        width: (drawable.width > 0 ? drawable.width : rect.width) / scale,
        height: (drawable.height > 0 ? drawable.height : rect.height) / scale,
      };
      if (this.mode === "normal" && !this.minimized && bounds.width > 0 && bounds.height > 0) {
        this.normalBounds = bounds;
      }
      const state: WireWindowStatePayload = {
        mode: this.mode,
        minimized: this.minimized && !this.hiddenByApp,
        visible: this.visible && !this.minimized && !this.hiddenByApp,
        focused: this.focused && this.visible && !this.minimized && !this.hiddenByApp,
        bounds,
        contentBounds,
        normalBounds: this.normalBounds ?? bounds,
        displayName,
        scaleFactor: scale,
      };
      const json = JSON.stringify({ windowId: this.windowId, state });
      if (json !== this.lastStateJson) {
        this.lastStateJson = json;
        this.owner.emitEvent("sub-window-state", json);
      }
    } catch {}
  }
}

export class NativeDesktop {
  private readonly ui: UIContext;
  private readonly abilityContext: common.UIAbilityContext;
  private readonly mainWindow: window.Window;
  private readonly emit: (kind: string, payload: string) => void;
  private readonly attachSubWindowCb: (
    windowId: number,
    content: NodeContent,
    platformWindowId: number,
  ) => boolean;
  private readonly subWindows: Map<number, SubWindowHost> = new Map();
  private closed: boolean = false;
  private mainParked: boolean = false;
  private reopenEmittedWhileParked: boolean = false;
  private colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  private lastDisplayId?: number;
  private readonly onDisplayMetricsChanged?: () => void;
  private nextCascadeIndex: number = 0;
  private frameNone: boolean = false;
  private transparent: boolean = false;
  private backdrop: string = "none";
  private backgroundOverridden: boolean = false;
  private titleBarHidden: boolean = false;
  private requestedTitleBarHeight?: number = undefined;
  private maximizable: boolean = true;
  private minimizable: boolean = true;
  private closable: boolean = true;
  private focused: boolean = true;
  private visible: boolean = true;
  private mode: string = "normal";
  private minimized: boolean = false;
  private normalBounds?: WireBoundsPayload;
  private defaultLimits?: window.WindowLimits;
  private mainQueue: Promise<void> = Promise.resolve();
  private pendingMainBounds?: WireCommand;
  private flushingMainBounds: boolean = false;
  private lastWindowStateJson: string = "";
  private lastTitleBarJson: string = "";
  private readonly popupMenus: NativeMenuCleanup = new NativeMenuCleanup();
  private popupLease?: PopupMenuLease;
  private popupGeneration: number = 0;
  private readonly menuBarState: MenuBarState = new MenuBarState();
  private menuBarNode?: BuilderNode<[MenuBarContentParams]>;
  private menuBarTargetId?: number;
  private menuBarDropdown?: MenuBarDropdownLease;
  private activeMenuBarIndex: number = -1;
  private switchingMenuBar: boolean = false;
  private readonly attachNativeNodeCb?: (targetId: number, content: FrameNode | null) => boolean;
  private readonly retireNativeNodeCb?: (targetId: number) => boolean;
  private trayAdded: boolean = false;
  private traySubscribed: boolean = false;
  private trayGeneration: number = 0;
  private trayClickId: number = 0;
  private trayLastIconKey: string = "";
  private trayLastHoverTips: string = "";
  private trayMenuTargets: Map<string, TrayMenuTarget> = new Map();
  private trayQueue: Promise<void> = Promise.resolve();
  private trayRemoveSyncFn?: () => void;
  private trayUnsubscribeFn?: () => void;

  private readonly onTrayIconClick = (_event: emitter.EventData): void => {
    if (this.closed) {
      return;
    }
    if (this.trayClickId !== 0) {
      this.emit("tray-click", JSON.stringify({ id: this.trayClickId }));
    } else if (this.mainParked) {
      this.emit("reopen", "{}");
    } else {
      try {
        this.mainWindow.showWindow().catch((): void => {});
      } catch {}
    }
  };

  private readonly onTrayRightMenuClick = (event: emitter.EventData): void => {
    if (this.closed) {
      return;
    }
    const data = event.data as Record<string, Object> | undefined;
    const rawCode = data?.["menuCode"];
    const code = typeof rawCode === "string" ? rawCode : "";
    const target = this.trayMenuTargets.get(code);
    if (target === undefined) {
      return;
    }
    this.emit(
      "menu-select",
      JSON.stringify({ generation: TRAY_MENU_GENERATION, id: target.id, role: target.role }),
    );
  };

  private readonly onWindowRectChange = (_data: window.RectChangeOptions): void => {
    this.syncWindowState();
    syncActiveEditorImeCursor();
    try {
      const displayId = this.mainWindow.getWindowProperties().displayId;
      if (displayId !== undefined && displayId !== this.lastDisplayId) {
        this.lastDisplayId = displayId;
        this.onDisplayMetricsChanged?.();
      }
    } catch {}
  };

  private readonly onWindowStatusChange = (status: window.WindowStatusType): void => {
    if (this.mainParked) {
      return;
    }
    this.applyWindowStatus(status);
    this.syncWindowState();
    if (!this.frameNone && this.titleBarHidden) {
      this.syncTitleBar();
    }
  };

  private readonly onWindowEvent = (event: window.WindowEventType): void => {
    if (this.mainParked) {
      if (
        event === window.WindowEventType.WINDOW_SHOWN ||
        event === window.WindowEventType.WINDOW_ACTIVE
      ) {
        if (!this.reopenEmittedWhileParked) {
          this.reopenEmittedWhileParked = true;
          this.emit("reopen", "{}");
        }
      } else if (
        event === window.WindowEventType.WINDOW_INACTIVE ||
        event === window.WindowEventType.WINDOW_HIDDEN
      ) {
        this.reopenEmittedWhileParked = false;
      }
      return;
    }
    switch (event) {
      case window.WindowEventType.WINDOW_SHOWN:
        this.visible = true;
        break;
      case window.WindowEventType.WINDOW_ACTIVE:
        this.visible = true;
        this.focused = true;
        this.minimized = false;
        break;
      case window.WindowEventType.WINDOW_INACTIVE:
        this.focused = false;
        break;
      case window.WindowEventType.WINDOW_HIDDEN:
        this.visible = false;
        this.focused = false;
        break;
    }
    this.syncWindowState();
  };

  private readonly onTitleButtonRectChange = (rect: window.TitleButtonRect): void => {
    if (!this.mainParked && !this.frameNone && this.titleBarHidden) {
      this.reportTitleBar(rect);
    }
  };

  private readonly onWindowWillClose = (): Promise<boolean> => {
    if (!this.closed) {
      if (this.mainParked) {
        // The main window is parked without a hosted logical window; keep the
        // Ability alive for surviving subwindows or tray-resident mode.
        this.enqueueMain(async (): Promise<void> => {
          if (this.mainParked) {
            try {
              await this.mainWindow.minimize();
            } catch {}
          }
        });
        return Promise.resolve(true);
      }
      this.emit("close-press", JSON.stringify({ reason: "closeButton" }));
      // Always defer teardown to Rust so ClosePress, child windows, Closed, and Quit run cleanly.
      return Promise.resolve(true);
    }
    return Promise.resolve(false);
  };

  constructor(
    ui: UIContext,
    abilityContext: common.UIAbilityContext,
    mainWindow: window.Window,
    emit: (kind: string, payload: string) => void,
    attachSubWindow: (
      windowId: number,
      content: NodeContent,
      platformWindowId: number,
    ) => boolean,
    onDisplayMetricsChanged?: () => void,
    attachNativeNode?: (targetId: number, content: FrameNode | null) => boolean,
    retireNativeNode?: (targetId: number) => boolean,
  ) {
    this.ui = ui;
    this.abilityContext = abilityContext;
    this.mainWindow = mainWindow;
    this.emit = emit;
    this.attachSubWindowCb = attachSubWindow;
    this.onDisplayMetricsChanged = onDisplayMetricsChanged;
    this.attachNativeNodeCb = attachNativeNode;
    this.retireNativeNodeCb = retireNativeNode;
    try {
      this.lastDisplayId = mainWindow.getWindowProperties().displayId;
    } catch {}
    try {
      this.visible = mainWindow.isWindowShowing();
      this.focused = mainWindow.isFocused();
    } catch {}
    try {
      this.applyWindowStatus(mainWindow.getWindowStatus());
    } catch {}
    try {
      this.defaultLimits = mainWindow.getWindowLimits();
    } catch {}
    this.subscribe();
    this.syncWindowState();
  }

  mainWindowRef(): window.Window {
    return this.mainWindow;
  }

  mainUiContext(): UIContext {
    return this.ui;
  }

  abilityContextRef(): common.UIAbilityContext {
    return this.abilityContext;
  }

  getSubWindowRef(windowId: number): window.Window | undefined {
    return this.subWindows.get(windowId)?.windowRef();
  }

  getSubWindowUIContext(windowId: number): UIContext | undefined {
    return this.subWindows.get(windowId)?.uiContext();
  }

  nextCascade(): number {
    return this.nextCascadeIndex++;
  }

  activeColorMode(): ThemeColorMode {
    return this.colorMode;
  }

  emitEvent(kind: string, payload: string): void {
    if (!this.closed) {
      this.emit(kind, payload);
    }
  }

  attachSubWindow(windowId: number, content: NodeContent, platformWindowId: number): boolean {
    if (this.closed) {
      return false;
    }
    return this.attachSubWindowCb(windowId, content, platformWindowId);
  }

  onSubWindowRemoved(windowId: number): void {
    this.subWindows.delete(windowId);
    if (this.popupLease !== undefined && this.popupLease.windowId === windowId) {
      this.popupLease.disappeared = true;
      this.closePopupMenuOnly();
    }
  }

  updateWindowCursor(windowId: number, style?: pointer.PointerStyle): void {
    this.subWindows.get(windowId)?.updateCursor(style);
  }

  resolveComponentUIContext(componentId: string): UIContext | undefined {
    const hosts = Array.from(this.subWindows.values());
    for (let i = 0; i < hosts.length; i++) {
      const ui = hosts[i].uiContext();
      if (ui === undefined) {
        continue;
      }
      try {
        if (ui.getFrameNodeById(componentId) !== null) {
          return ui;
        }
      } catch {}
    }
    return undefined;
  }

  setColorMode(mode: ThemeColorMode): void {
    this.colorMode = mode;
    this.menuBarState.colorMode = mode;
    if (this.backgroundOverridden && !this.transparent && this.backdrop === "none" && !this.frameNone) {
      this.applyOpaqueBackground();
    }
    this.subWindows.forEach((sub: SubWindowHost): void => {
      sub.setColorMode(mode);
    });
  }

  private applyOpaqueBackground(): void {
    const bg = resolveSystemWindowBackground(this.abilityContext);
    if (bg !== undefined) {
      try {
        this.mainWindow.setWindowBackgroundColor(bg);
      } catch {}
    }
  }

  requestComponentFocus(componentId: string): boolean {
    const hosts = Array.from(this.subWindows.values());
    for (let i = 0; i < hosts.length; i++) {
      if (hosts[i].requestComponentFocus(componentId)) {
        return true;
      }
    }
    return false;
  }

  windowWidthVp(): number {
    try {
      const rect = this.mainWindow.getWindowProperties().windowRect;
      return this.ui.px2vp(rect.width);
    } catch {
      return 0;
    }
  }

  windowHeightVp(): number {
    try {
      const rect = this.mainWindow.getWindowProperties().windowRect;
      return this.ui.px2vp(rect.height);
    } catch {
      return 0;
    }
  }

  execute(json: string): void {
    if (this.closed) {
      return;
    }
    let cmd: WireCommand;
    try {
      cmd = JSON.parse(json) as WireCommand;
    } catch (error) {
      console.warn(`[NSH] invalid desktop command: ${String(error)}`);
      return;
    }
    switch (cmd.op) {
      case "openSubWindow":
        if (cmd.windowId !== undefined) {
          this.openSubWindow(cmd);
        }
        break;
      case "closeSubWindow":
        if (cmd.windowId !== undefined) {
          this.closeSubWindow(cmd.windowId);
        }
        break;
      case "setAttributes":
        if (cmd.windowId !== undefined) {
          this.subWindows.get(cmd.windowId)?.setAttributes(cmd);
        } else {
          this.applyAttributes(cmd);
        }
        break;
      case "request":
        if (cmd.windowId !== undefined) {
          this.subWindows.get(cmd.windowId)?.request(cmd);
        } else {
          this.applyRequest(cmd);
        }
        break;
      case "popupMenu":
        this.openPopupMenu(
          cmd.windowId,
          cmd.generation ?? 0,
          cmd.x,
          cmd.y,
          cmd.items ?? [],
        );
        break;
      case "closePopupMenu":
        this.closePopupMenuOnly();
        break;
      case "setMenuBar":
        this.updateMenuBar(cmd.visible === true, cmd.targetId, cmd.items ?? [], cmd.openIndex);
        break;
      case "setTray":
        this.enqueueSetTray(cmd);
        break;
      case "removeTray":
        this.enqueueRemoveTray();
        break;
      case "startMoving":
        if (cmd.windowId !== undefined) {
          this.subWindows.get(cmd.windowId)?.startMoving();
        } else {
          try {
            this.mainWindow.startMoving().catch((): void => {});
          } catch {}
        }
        break;
      case "toggleMaximize":
        if (cmd.windowId !== undefined) {
          this.subWindows.get(cmd.windowId)?.toggleMaximize();
        } else {
          this.toggleMaximize();
        }
        break;
      case "applyMainWindowLaunch":
        this.enqueueMain(async (): Promise<void> => {
          await this.applyMainWindowLaunchAsync(cmd);
        });
        break;
      case "parkMainWindow":
        this.parkMainWindow(cmd.hasOpenSubWindows === true || this.subWindows.size > 0);
        break;
      case "unparkMainWindow":
        this.unparkMainWindow();
        break;
      case "quit":
        this.removeMenuBar();
        this.closePopupMenuOnly();
        this.closeAllSubWindows();
        this.removeTraySync();
        this.abilityContext.terminateSelf().catch((): void => {});
        break;
      case "restart":
        this.removeMenuBar();
        this.closePopupMenuOnly();
        this.closeAllSubWindows();
        this.removeTraySync();
        try {
          const bundleName = this.abilityContext.abilityInfo?.bundleName ?? "";
          const abilityName = this.abilityContext.abilityInfo?.name ?? "EntryAbility";
          this.abilityContext.getApplicationContext().restartApp({ bundleName, abilityName });
        } catch {
          this.abilityContext.terminateSelf().catch((): void => {});
        }
        break;
    }
  }

  private openSubWindow(cmd: WireCommand): void {
    const windowId = cmd.windowId;
    if (windowId === undefined) {
      return;
    }
    this.closeSubWindow(windowId);
    const host = new SubWindowHost(this, windowId);
    this.subWindows.set(windowId, host);
    host.open(cmd);
  }

  private closeSubWindow(windowId: number): void {
    const existing = this.subWindows.get(windowId);
    if (existing !== undefined) {
      existing.close();
    }
  }

  private closeAllSubWindows(): void {
    const entries = Array.from(this.subWindows.values());
    this.subWindows.clear();
    for (let i = 0; i < entries.length; i++) {
      entries[i].close();
    }
  }

  selectMenuItem(generation: number, id: number, role?: string): void {
    if (this.closed) {
      return;
    }
    if (generation === 0) {
      this.closeMenuBarDropdown(false);
      this.emit("menu-select", JSON.stringify({ generation: 0, id, role }));
      return;
    }
    this.selectPopupMenuItem(generation, id, role);
  }

  clickMenuBarTrigger(triggerIndex: number, item: WireMenuItem): void {
    if (this.closed || item.enabled === false) {
      return;
    }
    if (item.kind === "submenu" && (item.items?.length ?? 0) > 0) {
      if (this.activeMenuBarIndex === triggerIndex && this.menuBarDropdown !== undefined) {
        this.closeMenuBarDropdown(true);
        return;
      }
      this.openMenuBarDropdown(triggerIndex, `nsh-menubar-item-${triggerIndex}`, item.items ?? []);
    } else if (item.kind !== "submenu") {
      this.selectMenuItem(0, item.id ?? 0, item.role);
    }
  }

  hoverMenuBarTrigger(triggerIndex: number, item: WireMenuItem): void {
    if (
      this.closed ||
      this.activeMenuBarIndex < 0 ||
      this.activeMenuBarIndex === triggerIndex ||
      item.enabled === false ||
      item.kind !== "submenu" ||
      (item.items?.length ?? 0) === 0
    ) {
      return;
    }
    this.openMenuBarDropdown(triggerIndex, `nsh-menubar-item-${triggerIndex}`, item.items ?? []);
  }

  stepMenuBarDropdown(delta: number): void {
    if (this.closed || this.activeMenuBarIndex < 0) {
      return;
    }
    const items = this.menuBarState.items;
    const count = items.length;
    if (count === 0) {
      return;
    }
    for (let step = 1; step <= count; step++) {
      const idx = (this.activeMenuBarIndex + delta * step + count * step) % count;
      const candidate = items[idx];
      if (
        candidate.kind === "submenu" &&
        candidate.enabled !== false &&
        (candidate.items?.length ?? 0) > 0
      ) {
        if (idx !== this.activeMenuBarIndex) {
          this.openMenuBarDropdown(idx, `nsh-menubar-item-${idx}`, candidate.items ?? []);
        }
        return;
      }
    }
  }

  openMenuBarDropdown(triggerIndex: number, anchorId: string, items: WireMenuItem[]): void {
    if (this.closed || items.length === 0) {
      this.closeMenuBarDropdown(false);
      return;
    }
    let anchorUniqueId = 0;
    try {
      const frame = this.ui.getFrameNodeById(anchorId);
      if (frame === null) {
        return;
      }
      anchorUniqueId = frame.getUniqueId();
    } catch {
      return;
    }
    this.switchingMenuBar = this.menuBarDropdown !== undefined;
    this.closeMenuBarDropdown(false);
    this.switchingMenuBar = false;
    const menuContent = new ComponentContent<PopupMenuContentParams>(
      this.ui,
      wrapBuilder(desktopPopupMenuBuilder),
      {
        owner: this,
        generation: 0,
        items,
        colorMode: this.colorMode,
      },
      { nestingBuilderSupported: true },
    );
    const lease: MenuBarDropdownLease = {
      triggerIndex,
      menuContent,
      opening: Promise.resolve(),
      retired: false,
      disappeared: false,
    };
    this.menuBarDropdown = lease;
    this.activeMenuBarIndex = triggerIndex;
    this.menuBarState.activeIndex = triggerIndex;
    const options: MenuOptions = {
      placement: Placement.BottomLeft,
      showInSubWindow: true,
      backgroundBlurStyle: BlurStyle.COMPONENT_ULTRA_THICK,
      backgroundBlurStyleOptions: { colorMode: this.colorMode },
      onDisappear: (): void => {
        lease.disappeared = true;
        const wasActive = this.menuBarDropdown === lease;
        this.retireMenuBarDropdown(lease);
        if (wasActive && !this.switchingMenuBar && !this.closed) {
          this.emit("menu-bar-dismiss", "{}");
        }
      },
    };
    lease.opening = this.ui
      .getPromptAction()
      .openMenu(menuContent, { id: anchorUniqueId }, options);
    lease.opening.catch((error: Error): void => {
      lease.disappeared = true;
      console.error(`[NSH] desktop menu bar dropdown failed: ${String(error)}`);
      const wasActive = this.menuBarDropdown === lease;
      this.retireMenuBarDropdown(lease);
      if (wasActive && !this.switchingMenuBar && !this.closed) {
        this.emit("menu-bar-dismiss", "{}");
      }
    });
  }

  private closeMenuBarDropdown(notifyDismiss: boolean): void {
    const lease = this.menuBarDropdown;
    if (lease === undefined) {
      return;
    }
    this.menuBarDropdown = undefined;
    this.activeMenuBarIndex = -1;
    this.menuBarState.activeIndex = -1;
    this.retireMenuBarDropdown(lease);
    if (notifyDismiss && !this.closed) {
      this.emit("menu-bar-dismiss", "{}");
    }
  }

  private retireMenuBarDropdown(lease: MenuBarDropdownLease): void {
    if (this.menuBarDropdown === lease) {
      this.menuBarDropdown = undefined;
      this.activeMenuBarIndex = -1;
      this.menuBarState.activeIndex = -1;
    }
    if (lease.retired) {
      return;
    }
    lease.retired = true;
    this.popupMenus.retire(
      lease,
      lease.opening,
      (): Promise<void> =>
        lease.disappeared
          ? Promise.resolve()
          : this.ui
              .getPromptAction()
              .closeMenu(lease.menuContent)
              .catch((): void => {}),
      (): void => {
        try {
          lease.menuContent.dispose();
        } catch {}
      },
    );
  }

  selectPopupMenuItem(generation: number, id: number, role?: string): void {
    if (this.closed || generation !== this.popupGeneration) {
      return;
    }
    if (this.popupLease !== undefined && this.popupLease.generation === generation) {
      this.popupLease.settled = true;
    }
    this.closePopupMenuOnly();
    this.emit("menu-select", JSON.stringify({ generation, id, role }));
  }

  dismissPopupMenu(generation: number): void {
    if (this.closed || generation !== this.popupGeneration) {
      return;
    }
    if (this.popupLease !== undefined && this.popupLease.generation === generation) {
      this.popupLease.settled = true;
    }
    this.closePopupMenuOnly();
    this.emit("menu-close", JSON.stringify({ generation }));
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.removeMenuBar();
    this.closePopupMenuOnly();
    void this.popupMenus.settled();
    this.closeAllSubWindows();
    this.removeTraySync();
    try {
      this.mainWindow.off("windowRectChange", this.onWindowRectChange);
    } catch {}
    try {
      this.mainWindow.off("windowStatusChange", this.onWindowStatusChange);
    } catch {}
    try {
      this.mainWindow.off("windowEvent", this.onWindowEvent);
    } catch {}
    try {
      this.mainWindow.off("windowTitleButtonRectChange", this.onTitleButtonRectChange);
    } catch {}
    try {
      this.mainWindow.off("windowWillClose", this.onWindowWillClose);
    } catch {}
  }

  private subscribe(): void {
    try {
      this.mainWindow.on("windowRectChange", this.onWindowRectChange);
    } catch {}
    try {
      this.mainWindow.on("windowStatusChange", this.onWindowStatusChange);
    } catch {}
    try {
      this.mainWindow.on("windowEvent", this.onWindowEvent);
    } catch {}
    try {
      this.mainWindow.on("windowTitleButtonRectChange", this.onTitleButtonRectChange);
    } catch {}
    try {
      this.mainWindow.on("windowWillClose", this.onWindowWillClose);
    } catch {}
  }

  private applyWindowStatus(status: window.WindowStatusType): void {
    const next = windowStatusToState(status, this.mode);
    this.mode = next.mode;
    this.minimized = next.minimized;
  }

  private applyAttributes(cmd: WireCommand): void {
    if (cmd.title !== undefined) {
      try {
        this.mainWindow.setWindowTitle(cmd.title).catch((): void => {});
      } catch {}
    }
    if (cmd.transparent !== undefined) {
      this.transparent = cmd.transparent;
      if (this.transparent) {
        this.backgroundOverridden = true;
        try {
          this.mainWindow.setWindowBackgroundColor("#00000000");
        } catch {}
      } else if (this.backgroundOverridden && this.backdrop === "none" && !this.frameNone) {
        this.applyOpaqueBackground();
      }
    }
    if (cmd.backdrop !== undefined) {
      this.backdrop = cmd.backdrop;
      if (this.backdrop !== "none") {
        this.backgroundOverridden = true;
        try {
          this.mainWindow.setWindowBackgroundColor("#00000000");
        } catch {}
      } else if (this.backgroundOverridden && !this.transparent && !this.frameNone) {
        this.applyOpaqueBackground();
      }
    }
    const buttonsChanged =
      cmd.maximizable !== undefined ||
      cmd.minimizable !== undefined ||
      cmd.closable !== undefined;
    if (cmd.maximizable !== undefined) {
      this.maximizable = cmd.maximizable;
    }
    if (cmd.minimizable !== undefined) {
      this.minimizable = cmd.minimizable;
    }
    if (cmd.closable !== undefined) {
      this.closable = cmd.closable;
    }
    const chromeState: WindowChromeState = {
      frameNone: this.frameNone,
      titleBarHidden: this.titleBarHidden,
      requestedTitleBarHeight: this.requestedTitleBarHeight,
      maximizable: this.maximizable,
      minimizable: this.minimizable,
      closable: this.closable,
      transparent: this.transparent,
      backdrop: this.backdrop,
      backgroundOverridden: this.backgroundOverridden,
    };
    const needsTitleBarSync = applyTitleBarAndFrameHelper(
      this.mainWindow,
      cmd,
      chromeState,
      (): void => this.applyOpaqueBackground(),
      (payload: WireTitleBarPayload): void => this.emitTitleBar(payload),
    );
    this.frameNone = chromeState.frameNone;
    this.titleBarHidden = chromeState.titleBarHidden;
    this.requestedTitleBarHeight = chromeState.requestedTitleBarHeight;
    if (buttonsChanged && !this.frameNone) {
      try {
        this.mainWindow.setWindowTitleButtonVisible(
          this.maximizable,
          this.minimizable,
          this.closable,
        );
      } catch {}
    }
    if (needsTitleBarSync || (buttonsChanged && !this.frameNone && this.titleBarHidden)) {
      this.syncTitleBar();
    }

    if (cmd.minSize !== undefined || cmd.maxSize !== undefined) {
      const scale = resolveScaleFactor(
        this.mainWindow.getWindowProperties().displayId,
        this.ui,
      );
      const limits = buildWindowLimits(cmd, scale, this.defaultLimits);
      if (limits !== undefined) {
        try {
          this.mainWindow.setWindowLimits(limits).catch((): void => {});
        } catch {}
      }
    }

    if (cmd.level !== undefined) {
      const topmost = cmd.level === "floating" || cmd.level === "aboveDock";
      try {
        this.mainWindow.setWindowTopmost(topmost).catch((): void => {});
      } catch {}
    }

    if (cmd.focusable !== undefined) {
      try {
        this.mainWindow.setWindowFocusable(cmd.focusable).catch((): void => {});
      } catch {}
    }

    if (cmd.pointerEvents !== undefined) {
      try {
        this.mainWindow.setWindowTouchable(cmd.pointerEvents !== "none").catch((): void => {});
      } catch {}
    }

    if (cmd.resizable !== undefined) {
      try {
        this.mainWindow.setResizeByDragEnabled(cmd.resizable).catch((): void => {});
      } catch {}
    }

    if (cmd.movable !== undefined) {
      try {
        this.mainWindow.setWindowTitleMoveEnabled(cmd.movable);
      } catch {}
    }

    if (cmd.shadow !== undefined) {
      try {
        this.mainWindow.setWindowShadowEnabled(cmd.shadow).catch((): void => {});
      } catch {}
    }
  }

  private enqueueMain(task: () => Promise<void>): void {
    this.mainQueue = this.mainQueue
      .then(async (): Promise<void> => {
        if (this.closed) {
          return;
        }
        await task();
      })
      .catch((error: Object): void => {
        console.warn(`[NSH] main window command failed: ${String(error)}`);
      });
  }

  private applyRequest(cmd: WireCommand): void {
    if (cmd.action === "setBounds") {
      this.pendingMainBounds = cmd;
      if (this.flushingMainBounds) {
        return;
      }
      this.flushingMainBounds = true;
      this.enqueueMain(async (): Promise<void> => {
        try {
          while (!this.closed && this.pendingMainBounds !== undefined) {
            const next = this.pendingMainBounds;
            this.pendingMainBounds = undefined;
            await this.applyBoundsAsync(next.x, next.y, next.width, next.height);
          }
        } finally {
          this.flushingMainBounds = false;
        }
      });
      return;
    }
    switch (cmd.action) {
      case "show":
      case "focus":
        this.enqueueMain(async (): Promise<void> => {
          try {
            await this.mainWindow.showWindow();
          } catch {}
          this.syncWindowState();
        });
        break;
      case "hide":
      case "minimize":
        this.enqueueMain(async (): Promise<void> => {
          try {
            await this.mainWindow.minimize();
          } catch {}
          this.syncWindowState();
        });
        break;
      case "restore":
        this.enqueueMain(async (): Promise<void> => {
          try {
            await this.mainWindow.restore();
          } catch {
            try {
              await this.mainWindow.recover();
            } catch {}
          }
          this.syncWindowState();
        });
        break;
      case "close":
        this.parkMainWindow(this.subWindows.size > 0);
        break;
      case "setMode":
        this.enqueueMain(async (): Promise<void> => {
          await this.setModeAsync(cmd.mode ?? "normal");
        });
        break;
      case "setContentSize":
        this.enqueueMain(async (): Promise<void> => {
          await this.applyContentSizeAsync(cmd.width, cmd.height);
        });
        break;
      case "setPlacement":
        this.enqueueMain(async (): Promise<void> => {
          await this.applyBoundsAsync(cmd.x, cmd.y, cmd.width, cmd.height);
          if (cmd.mode !== undefined && (cmd.mode !== "normal" || this.mode !== "normal")) {
            await this.setModeAsync(cmd.mode);
          }
        });
        break;
      case "center":
        this.enqueueMain(async (): Promise<void> => {
          await this.centerWindow(cmd.display);
        });
        break;
      case "requestAttention":
        // HarmonyOS public API 26 exposes no non-activating taskbar-flash or
        // dock-bounce API; RequestAttention must never restore, raise, or steal focus.
        break;
    }
  }

  private parkMainWindow(hasOpenSubWindows: boolean): void {
    this.mainParked = true;
    this.reopenEmittedWhileParked = false;
    this.closePopupMenuOnly();
    this.closeMenuBarDropdown(false);
    this.frameNone = false;
    this.titleBarHidden = false;
    this.requestedTitleBarHeight = undefined;
    this.maximizable = true;
    this.minimizable = true;
    this.closable = true;
    this.transparent = false;
    this.backdrop = "none";
    this.lastWindowStateJson = "";
    this.lastTitleBarJson = "";
    const useHideAbility = !hasOpenSubWindows && this.trayAdded;
    this.enqueueMain(async (): Promise<void> => {
      if (!this.mainParked) {
        return;
      }
      try {
        await this.mainWindow.setWindowTitle("");
      } catch {}
      try {
        this.mainWindow.setWindowDecorVisible(true);
      } catch {}
      try {
        this.mainWindow.setWindowTitleButtonVisible(true, true, true);
      } catch {}
      if (this.backgroundOverridden) {
        this.backgroundOverridden = false;
        this.applyOpaqueBackground();
      }
      if (useHideAbility) {
        try {
          await this.abilityContext.hideAbility();
          return;
        } catch {}
      }
      try {
        await this.mainWindow.minimize();
      } catch {}
    });
  }

  private unparkMainWindow(): void {
    const wasParked = this.mainParked;
    this.mainParked = false;
    this.reopenEmittedWhileParked = false;
    this.lastWindowStateJson = "";
    this.lastTitleBarJson = "";
    if (!wasParked) {
      return;
    }
    this.enqueueMain(async (): Promise<void> => {
      if (this.mainParked) {
        return;
      }
      try {
        await this.abilityContext.showAbility();
      } catch {}
      try {
        await this.mainWindow.restore();
      } catch {
        try {
          await this.mainWindow.recover();
        } catch {}
      }
      try {
        await this.mainWindow.showWindow();
      } catch {}
      this.syncWindowState();
    });
  }

  private async setModeAsync(mode: string): Promise<void> {
    try {
      await setWindowModeHelper(this.mainWindow, mode);
      this.syncWindowState();
    } catch {}
  }

  private toggleMaximize(): void {
    const next =
      this.mode === "maximized" || this.mode === "fullScreen" ? "normal" : "maximized";
    this.enqueueMain(async (): Promise<void> => {
      await this.setModeAsync(next);
    });
  }

  private async applyBoundsAsync(
    x?: number,
    y?: number,
    width?: number,
    height?: number,
  ): Promise<void> {
    try {
      await applyWindowBoundsHelper(
        this.mainWindow,
        this.ui,
        this.mode,
        x,
        y,
        width,
        height,
      );
      this.syncWindowState();
    } catch {}
  }

  private async applyContentSizeAsync(width?: number, height?: number): Promise<void> {
    try {
      const props = this.mainWindow.getWindowProperties();
      const rect = props.windowRect;
      const drawable = props.drawableRect;
      const scale = resolveScaleFactor(props.displayId, this.ui);
      const chromeW = drawable.width > 0 ? Math.max(0, rect.width - drawable.width) : 0;
      const chromeH =
        this.titleBarHidden || this.frameNone
          ? 0
          : drawable.height > 0
            ? Math.max(0, rect.height - drawable.height)
            : 0;
      const targetW =
        width !== undefined ? Math.max(1, Math.round(width * scale) + chromeW) : rect.width;
      const targetH =
        height !== undefined ? Math.max(1, Math.round(height * scale) + chromeH) : rect.height;
      if (Math.abs(targetW - rect.width) <= 1 && Math.abs(targetH - rect.height) <= 1) {
        return;
      }
      if (this.mode !== "normal") {
        try {
          await this.mainWindow.recover();
        } catch {}
      }
      await this.mainWindow.resize(targetW, targetH);
      this.syncWindowState();
    } catch {}
  }

  private async applyMainWindowLaunchAsync(cmd: WireCommand): Promise<void> {
    try {
      const props = this.mainWindow.getWindowProperties();
      const rect = props.windowRect;
      const drawable = props.drawableRect;
      const screen = await resolveTargetDisplay(cmd.display, props.displayId);
      const scale = resolveScaleFactor(screen.id, this.ui);
      const chromeW =
        cmd.contentSized && drawable.width > 0 ? Math.max(0, rect.width - drawable.width) : 0;
      const chromeH =
        cmd.contentSized &&
        !this.titleBarHidden &&
        !this.frameNone &&
        cmd.titleBarHidden !== true &&
        drawable.height > 0
          ? Math.max(0, rect.height - drawable.height)
          : 0;
      const targetW =
        cmd.width !== undefined ? Math.max(1, Math.round(cmd.width * scale) + chromeW) : rect.width;
      const targetH =
        cmd.height !== undefined
          ? Math.max(1, Math.round(cmd.height * scale) + chromeH)
          : rect.height;
      if (
        (cmd.width !== undefined || cmd.height !== undefined) &&
        (Math.abs(targetW - rect.width) > 1 || Math.abs(targetH - rect.height) > 1)
      ) {
        try {
          await this.mainWindow.resize(targetW, targetH);
        } catch {}
      }
      if (cmd.position === "at" && cmd.x !== undefined && cmd.y !== undefined) {
        const moveX = Math.round(cmd.x * scale);
        const moveY = Math.round(cmd.y * scale);
        if (cmd.display && deviceInfo.sdkApiVersion >= 15) {
          try {
            await this.mainWindow.moveWindowToGlobal(moveX, moveY, { displayId: screen.id });
          } catch {
            try {
              await this.mainWindow.moveWindowTo(moveX, moveY);
            } catch {}
          }
        } else {
          try {
            await this.mainWindow.moveWindowTo(moveX, moveY);
          } catch {}
        }
      } else if (cmd.position === "center" || cmd.position === "parentCenter" || cmd.display) {
        await this.centerWindow(cmd.display, targetW, targetH);
      }
      if (cmd.mode && cmd.mode !== "normal") {
        await this.setModeAsync(cmd.mode);
      }
      if (cmd.visible === false) {
        try {
          await this.mainWindow.minimize();
        } catch {}
      }
      this.syncWindowState();
    } catch {}
  }

  private async centerWindow(
    displayHex?: string,
    overrideW?: number,
    overrideH?: number,
  ): Promise<void> {
    if (this.closed) {
      return;
    }
    try {
      const props = this.mainWindow.getWindowProperties();
      const rect = props.windowRect;
      const w = overrideW ?? rect.width;
      const h = overrideH ?? rect.height;
      const screen = await resolveTargetDisplay(displayHex, props.displayId);
      const area = await displayAvailableRectPx(screen);
      const minX = area.left;
      const minY = area.top;
      const maxX = area.left + Math.max(0, area.width - w);
      const maxY = area.top + Math.max(0, area.height - h);
      const targetX = Math.max(
        minX,
        Math.min(maxX, area.left + Math.round((area.width - w) / 2)),
      );
      const targetY = Math.max(
        minY,
        Math.min(maxY, area.top + Math.round((area.height - h) / 2)),
      );
      if (displayHex && deviceInfo.sdkApiVersion >= 15) {
        try {
          await this.mainWindow.moveWindowToGlobal(targetX, targetY, { displayId: screen.id });
          this.syncWindowState();
          return;
        } catch {}
      }
      await this.mainWindow.moveWindowTo(targetX, targetY);
      this.syncWindowState();
    } catch {}
  }

  private syncTitleBar(): void {
    if (this.frameNone || !this.titleBarHidden) {
      return;
    }
    try {
      const rect = this.mainWindow.getTitleButtonRect();
      this.reportTitleBar(rect);
    } catch {
      let height = this.requestedTitleBarHeight ?? 0;
      try {
        const decor = this.mainWindow.getWindowDecorHeight();
        if (decor > 0) {
          height = Math.max(height, decor);
        }
      } catch {}
      this.emitTitleBar({
        top: height,
        right: 0,
        bottom: 0,
        left: 0,
      });
    }
  }

  private reportTitleBar(rect: window.TitleButtonRect): void {
    let height = this.requestedTitleBarHeight ?? 0;
    try {
      const decor = this.mainWindow.getWindowDecorHeight();
      if (decor > 0) {
        height = Math.max(height, decor);
      }
    } catch {}
    const top = Math.max(height, rect.top + rect.height);
    const right = Math.max(0, rect.right + rect.width);
    this.emitTitleBar({ top, right, bottom: 0, left: 0 });
  }

  private emitTitleBar(payload: WireTitleBarPayload): void {
    if (this.closed) {
      return;
    }
    const json = JSON.stringify(payload);
    if (json !== this.lastTitleBarJson) {
      this.lastTitleBarJson = json;
      this.emit("title-bar", json);
    }
  }

  private syncWindowState(): void {
    if (this.closed || this.mainParked) {
      return;
    }
    try {
      const props = this.mainWindow.getWindowProperties();
      const rect = props.windowRect;
      const drawable = props.drawableRect;
      let scale = 1.0;
      let displayName: string | undefined = undefined;
      try {
        const screen =
          props.displayId !== undefined
            ? display.getDisplayByIdSync(props.displayId)
            : display.getDefaultDisplaySync();
        if (screen.densityPixels > 0) {
          scale = Math.round(screen.densityPixels * 10000) / 10000;
        }
        displayName = `display-${screen.id}`;
      } catch {
        const vp = this.ui.vp2px(1);
        if (vp > 0) {
          scale = Math.round(vp * 10000) / 10000;
        }
      }
      const bounds: WireBoundsPayload = {
        x: rect.left / scale,
        y: rect.top / scale,
        width: rect.width / scale,
        height: rect.height / scale,
      };
      const contentBounds: WireBoundsPayload = {
        x: (rect.left + drawable.left) / scale,
        y: (rect.top + drawable.top) / scale,
        width: (drawable.width > 0 ? drawable.width : rect.width) / scale,
        height: (drawable.height > 0 ? drawable.height : rect.height) / scale,
      };
      if (this.mode === "normal" && !this.minimized && bounds.width > 0 && bounds.height > 0) {
        this.normalBounds = bounds;
      }
      const payload: WireWindowStatePayload = {
        mode: this.mode,
        minimized: this.minimized,
        visible: this.visible && !this.minimized,
        focused: this.focused && this.visible && !this.minimized,
        bounds,
        contentBounds,
        normalBounds: this.normalBounds ?? bounds,
        displayName,
        scaleFactor: scale,
      };
      const json = JSON.stringify(payload);
      if (json !== this.lastWindowStateJson) {
        this.lastWindowStateJson = json;
        this.emit("window-state", json);
      }
    } catch {}
  }

  private openMenuBarByIndex(index: number): boolean {
    if (this.closed || index < 0 || index >= this.menuBarState.items.length) {
      return true;
    }
    const item = this.menuBarState.items[index];
    if (item.kind === "separator" || item.enabled === false) {
      return true;
    }
    if (item.kind === "submenu") {
      if ((item.items?.length ?? 0) === 0) {
        return true;
      }
      const anchorId = `nsh-menubar-item-${index}`;
      try {
        if (this.ui.getFrameNodeById(anchorId) === null) {
          return false;
        }
      } catch {
        return false;
      }
      this.openMenuBarDropdown(index, anchorId, item.items ?? []);
      return true;
    }
    this.selectMenuItem(0, item.id ?? 0, item.role);
    return true;
  }

  private openMenuBarWhenReady(index: number, revision: number, retries: number): void {
    if (this.closed || this.menuBarState.revision !== revision) {
      return;
    }
    if (this.openMenuBarByIndex(index) || retries <= 0) {
      return;
    }
    setTimeout((): void => {
      this.openMenuBarWhenReady(index, revision, retries - 1);
    }, 16);
  }

  private updateMenuBar(
    visible: boolean,
    targetId: number | undefined,
    items: WireMenuItem[],
    openIndex?: number,
  ): void {
    if (!visible || items.length === 0 || targetId === undefined) {
      if (targetId === undefined) {
        this.removeMenuBar();
      } else {
        this.closeMenuBarDropdown(false);
        this.menuBarState.items = [];
        this.menuBarState.revision++;
      }
      return;
    }
    this.menuBarState.colorMode = this.colorMode;
    this.menuBarState.items = items;
    this.menuBarState.revision++;
    const scheduleOpen = (): void => {
      if (openIndex === undefined) {
        return;
      }
      this.openMenuBarWhenReady(openIndex, this.menuBarState.revision, 3);
    };
    if (this.menuBarNode !== undefined && this.menuBarTargetId === targetId) {
      scheduleOpen();
      return;
    }
    this.removeMenuBar();
    if (this.attachNativeNodeCb === undefined) {
      return;
    }
    const node = new BuilderNode<[MenuBarContentParams]>(this.ui);
    try {
      node.build(
        wrapBuilder(desktopMenuBarBuilder),
        {
          owner: this,
          state: this.menuBarState,
        },
        { nestingBuilderSupported: true },
      );
      const accepted = this.attachNativeNodeCb(targetId, node.getFrameNode());
      if (!accepted || this.closed) {
        this.retireNativeNodeCb?.(targetId);
        try {
          node.dispose();
        } catch {}
        return;
      }
      this.menuBarNode = node;
      this.menuBarTargetId = targetId;
      scheduleOpen();
    } catch (error) {
      console.warn(`[NSH] desktop menu bar mount failed: ${String(error)}`);
      try {
        node.dispose();
      } catch {}
    }
  }

  private removeMenuBar(): void {
    this.closeMenuBarDropdown(false);
    const node = this.menuBarNode;
    const targetId = this.menuBarTargetId;
    this.menuBarNode = undefined;
    this.menuBarTargetId = undefined;
    this.menuBarState.items = [];
    if (node === undefined) {
      return;
    }
    if (targetId !== undefined) {
      this.retireNativeNodeCb?.(targetId);
    }
    try {
      node.dispose();
    } catch {}
  }

  private openPopupMenu(
    windowId: number | undefined,
    generation: number,
    x: number | undefined,
    y: number | undefined,
    items: WireMenuItem[],
  ): void {
    this.closePopupMenuOnly();
    if (items.length === 0) {
      this.emit("menu-close", JSON.stringify({ generation }));
      return;
    }
    const sub = windowId !== undefined ? this.subWindows.get(windowId) : undefined;
    const targetUi = sub?.uiContext() ?? this.ui;
    const winW = sub ? sub.widthVp() : this.windowWidthVp();
    const winH = sub ? sub.heightVp() : this.windowHeightVp();
    const centered = x === undefined || y === undefined;
    const posX = Math.max(0, x ?? winW / 2);
    const posY = Math.max(0, y ?? winH / 2);
    const anchorId = `nsh-desktop-popup-${generation}`;
    this.popupGeneration = generation;

    let lease: PopupMenuLease | undefined = undefined;
    const anchorContent = new ComponentContent<PopupMenuAnchorParams>(
      targetUi,
      wrapBuilder(desktopPopupAnchorBuilder),
      {
        anchorId,
        x: posX,
        y: posY,
        onReady: (uniqueId: number): void => {
          if (lease !== undefined) {
            this.presentPopupMenu(lease, uniqueId, items, sub === undefined, centered);
          }
        },
      },
    );
    lease = {
      windowId,
      generation,
      ui: targetUi,
      anchorContent,
      opening: Promise.resolve(),
      retired: false,
      disappeared: false,
      settled: false,
    };
    this.popupLease = lease;
    try {
      targetUi.getOverlayManager().addComponentContent(anchorContent);
    } catch (error) {
      console.error(`[NSH] desktop popup menu anchor failed: ${String(error)}`);
      if (this.popupLease === lease) {
        this.popupLease = undefined;
      }
      lease.retired = true;
      lease.disappeared = true;
      lease.settled = true;
      try {
        anchorContent.dispose();
      } catch {}
      this.emit("menu-close", JSON.stringify({ generation }));
    }
  }

  private presentPopupMenu(
    lease: PopupMenuLease,
    anchorUniqueId: number,
    items: WireMenuItem[],
    allowSubWindow: boolean,
    centered: boolean,
  ): void {
    if (this.closed || lease.retired || this.popupLease !== lease) {
      return;
    }
    const menuContent = new ComponentContent<PopupMenuContentParams>(
      lease.ui,
      wrapBuilder(desktopPopupMenuBuilder),
      {
        owner: this,
        generation: lease.generation,
        items,
        colorMode: this.colorMode,
      },
      { nestingBuilderSupported: true },
    );
    lease.menuContent = menuContent;
    const options: MenuOptions = {
      placement: centered ? Placement.Bottom : Placement.BottomLeft,
      showInSubWindow: allowSubWindow,
      backgroundBlurStyle: BlurStyle.COMPONENT_ULTRA_THICK,
      backgroundBlurStyleOptions: { colorMode: this.colorMode },
      onDisappear: (): void => {
        lease.disappeared = true;
        this.onPopupMenuDisappeared(lease);
      },
    };
    lease.opening = lease.ui
      .getPromptAction()
      .openMenu(menuContent, { id: anchorUniqueId }, options);
    lease.opening.catch((error: Error): void => {
      lease.disappeared = true;
      console.error(`[NSH] desktop popup menu failed: ${String(error)}`);
      this.onPopupMenuDisappeared(lease);
    });
  }

  private onPopupMenuDisappeared(lease: PopupMenuLease): void {
    if (this.popupLease === lease) {
      this.popupLease = undefined;
    }
    const shouldEmitClose = !this.closed && !lease.settled;
    lease.settled = true;
    this.retirePopupLease(lease);
    if (shouldEmitClose) {
      this.emit("menu-close", JSON.stringify({ generation: lease.generation }));
    }
  }

  private closePopupMenuOnly(): void {
    const lease = this.popupLease;
    if (lease === undefined) {
      return;
    }
    this.popupLease = undefined;
    lease.settled = true;
    this.retirePopupLease(lease);
  }

  private retirePopupLease(lease: PopupMenuLease): void {
    if (lease.retired) {
      return;
    }
    lease.retired = true;
    const removeAnchor = (): void => {
      try {
        lease.ui.getOverlayManager().removeComponentContent(lease.anchorContent);
      } catch {}
      try {
        lease.anchorContent.dispose();
      } catch {}
    };
    const menuContent = lease.menuContent;
    if (menuContent === undefined) {
      removeAnchor();
      return;
    }
    this.popupMenus.retire(
      lease,
      lease.opening,
      (): Promise<void> =>
        lease.disappeared
          ? Promise.resolve()
          : lease.ui
              .getPromptAction()
              .closeMenu(menuContent)
              .catch((): void => {}),
      (): void => {
        try {
          menuContent.dispose();
        } catch {}
        removeAnchor();
      },
    );
  }

  private enqueueSetTray(cmd: WireCommand): void {
    const generation = ++this.trayGeneration;
    this.trayQueue = this.trayQueue
      .then(async (): Promise<void> => {
        if (this.closed || generation !== this.trayGeneration) {
          return;
        }
        await this.applySetTray(generation, cmd);
      })
      .catch((error: Object): void => {
        console.warn(`[NSH] setTray failed: ${String(error)}`);
      });
  }

  private enqueueRemoveTray(): void {
    const generation = ++this.trayGeneration;
    this.trayQueue = this.trayQueue
      .then(async (): Promise<void> => {
        if (generation !== this.trayGeneration) {
          return;
        }
        await this.applyRemoveTray();
      })
      .catch((): void => {});
  }

  private async applySetTray(generation: number, cmd: WireCommand): Promise<void> {
    if (!canIUse("SystemCapability.PCService.StatusBarManager")) {
      return;
    }
    const kit = await import("@hms.pcService.statusBarManager");
    if (this.closed || generation !== this.trayGeneration) {
      return;
    }
    this.trayRemoveSyncFn = (): void => {
      try {
        kit.default.removeFromStatusBar(this.abilityContext);
      } catch {}
    };
    this.trayUnsubscribeFn = (): void => {
      try {
        kit.default.off("statusBarIconClick", this.onTrayIconClick);
      } catch {}
      try {
        kit.default.off("rightMenuClick", this.onTrayRightMenuClick);
      } catch {}
    };
    if (deviceInfo.sdkApiVersion >= 26) {
      try {
        const supported = await kit.default.isStatusBarCapabilitySupported(this.abilityContext);
        if (!supported) {
          return;
        }
      } catch {}
    }
    if (!this.traySubscribed) {
      try {
        kit.default.on("statusBarIconClick", this.onTrayIconClick);
        kit.default.on("rightMenuClick", this.onTrayRightMenuClick);
        this.traySubscribed = true;
      } catch (error) {
        console.warn(`[NSH] statusBarManager event subscribe failed: ${String(error)}`);
      }
    }
    this.trayClickId = cmd.click ?? 0;

    const nextMenuTargets: Map<string, TrayMenuTarget> = new Map();
    const menuGroups = this.buildStatusBarMenu(generation, cmd.items ?? [], nextMenuTargets);
    const rawTips = (cmd.tooltip ?? "").trim() || (cmd.title ?? "").trim();
    const hoverTips =
      rawTips.length > STATUS_BAR_MAX_HOVER_TIPS_CHARS
        ? rawTips.substring(0, STATUS_BAR_MAX_HOVER_TIPS_CHARS)
        : rawTips;
    const iconKey = trayIconDigest(cmd.icon);

    if (!this.trayAdded) {
      const icons = await this.decodeTrayIcons(cmd.icon);
      if (this.closed || generation !== this.trayGeneration) {
        icons.white.release().catch((): void => {});
        icons.black.release().catch((): void => {});
        return;
      }
      const label =
        hoverTips.length > 0
          ? hoverTips
          : this.abilityContext.abilityInfo?.name ?? "App";
      const item: statusBarManager.StatusBarItem = {
        icons,
        quickOperation: {
          title: label,
          height: 1,
          abilityName: "",
          moduleName: this.abilityContext.abilityInfo?.moduleName ?? "",
        },
        statusBarGroupMenu: menuGroups.length > 0 ? menuGroups : undefined,
      };
      if (deviceInfo.sdkApiVersion >= 22 && hoverTips.length > 0) {
        item.hoverTips = hoverTips;
      }
      try {
        await new Promise<void>((resolve, reject) => {
          try {
            if (deviceInfo.sdkApiVersion >= 14) {
              kit.default.addToStatusBar(this.abilityContext, item, (err): void => {
                if (err) {
                  reject(err);
                } else {
                  resolve();
                }
              });
            } else {
              kit.default.addToStatusBar(this.abilityContext, item);
              resolve();
            }
          } catch (error) {
            reject(error);
          }
        });
        if (this.closed || generation !== this.trayGeneration) {
          try {
            if (deviceInfo.sdkApiVersion >= 14) {
              kit.default.removeFromStatusBar(this.abilityContext, (): void => {});
            } else {
              kit.default.removeFromStatusBar(this.abilityContext);
            }
          } catch {}
          return;
        }
        this.trayAdded = true;
        this.trayMenuTargets = nextMenuTargets;
        this.trayLastIconKey = iconKey;
        this.trayLastHoverTips = hoverTips;
      } catch (error) {
        console.warn(`[NSH] addToStatusBar failed: ${String(error)}`);
      } finally {
        icons.white.release().catch((): void => {});
        icons.black.release().catch((): void => {});
      }
      return;
    }

    try {
      await new Promise<void>((resolve, reject) => {
        try {
          if (deviceInfo.sdkApiVersion >= 14) {
            kit.default.updateStatusBarMenu(this.abilityContext, menuGroups, (err): void => {
              if (err) {
                reject(err);
              } else {
                resolve();
              }
            });
          } else {
            kit.default.updateStatusBarMenu(this.abilityContext, menuGroups);
            resolve();
          }
        } catch (error) {
          reject(error);
        }
      });
      if (!this.closed && generation === this.trayGeneration) {
        this.trayMenuTargets = nextMenuTargets;
      }
    } catch (error) {
      console.warn(`[NSH] updateStatusBarMenu failed: ${String(error)}`);
    }

    if (iconKey !== this.trayLastIconKey) {
      const icons = await this.decodeTrayIcons(cmd.icon);
      if (this.closed || generation !== this.trayGeneration) {
        icons.white.release().catch((): void => {});
        icons.black.release().catch((): void => {});
        return;
      }
      try {
        await new Promise<void>((resolve, reject) => {
          try {
            if (deviceInfo.sdkApiVersion >= 14) {
              kit.default.updateStatusBarIcon(this.abilityContext, icons, (err): void => {
                if (err) {
                  reject(err);
                } else {
                  resolve();
                }
              });
            } else {
              kit.default.updateStatusBarIcon(this.abilityContext, icons);
              resolve();
            }
          } catch (error) {
            reject(error);
          }
        });
        this.trayLastIconKey = iconKey;
      } catch (error) {
        console.warn(`[NSH] updateStatusBarIcon failed: ${String(error)}`);
      } finally {
        icons.white.release().catch((): void => {});
        icons.black.release().catch((): void => {});
      }
    }

    if (
      deviceInfo.sdkApiVersion >= 22 &&
      hoverTips.length > 0 &&
      hoverTips !== this.trayLastHoverTips
    ) {
      try {
        await kit.default.updateStatusBarHoverTips(this.abilityContext, hoverTips);
        this.trayLastHoverTips = hoverTips;
      } catch {}
    }
  }

  private async applyRemoveTray(): Promise<void> {
    this.trayClickId = 0;
    this.trayMenuTargets.clear();
    if (!this.trayAdded || !canIUse("SystemCapability.PCService.StatusBarManager")) {
      this.unsubscribeTray();
      return;
    }
    this.trayAdded = false;
    this.trayLastIconKey = "";
    this.trayLastHoverTips = "";
    try {
      const kit = await import("@hms.pcService.statusBarManager");
      await new Promise<void>((resolve, reject) => {
        try {
          if (deviceInfo.sdkApiVersion >= 14) {
            kit.default.removeFromStatusBar(this.abilityContext, (err): void => {
              if (err) {
                reject(err);
              } else {
                resolve();
              }
            });
          } else {
            kit.default.removeFromStatusBar(this.abilityContext);
            resolve();
          }
        } catch (error) {
          reject(error);
        }
      });
    } catch (error) {
      console.warn(`[NSH] removeFromStatusBar failed: ${String(error)}`);
    }
    this.unsubscribeTray();
  }

  private removeTraySync(): void {
    this.trayGeneration++;
    this.trayClickId = 0;
    this.trayMenuTargets.clear();
    if (this.trayAdded && canIUse("SystemCapability.PCService.StatusBarManager")) {
      this.trayAdded = false;
      this.trayLastIconKey = "";
      this.trayLastHoverTips = "";
      this.trayRemoveSyncFn?.();
    }
    this.unsubscribeTray();
  }

  private unsubscribeTray(): void {
    if (!this.traySubscribed) {
      return;
    }
    this.traySubscribed = false;
    this.trayUnsubscribeFn?.();
  }

  private buildStatusBarMenu(
    generation: number,
    items: WireMenuItem[],
    targets: Map<string, TrayMenuTarget>,
  ): statusBarManager.StatusBarGroupMenu[] {
    const abilityName = this.abilityContext.abilityInfo?.name ?? "EntryAbility";
    const moduleName = this.abilityContext.abilityInfo?.moduleName ?? "entry";
    const supportsItemOptions = deviceInfo.sdkApiVersion >= 24;
    let seq = 0;
    let topLevelCount = 0;

    const makeCode = (item: WireMenuItem): string => {
      const code = `nsh-${generation}-${++seq}`;
      targets.set(code, { id: item.id ?? 0, role: item.role });
      return code;
    };

    const collectSubItems = (
      children: WireMenuItem[],
      out: statusBarManager.StatusBarSubMenuItem[],
    ): void => {
      for (let i = 0; i < children.length && out.length < STATUS_BAR_MAX_MENU_ITEMS; i++) {
        const child = children[i];
        if (child.kind === "separator" || child.enabled === false) {
          continue;
        }
        if (child.kind === "submenu") {
          collectSubItems(child.items ?? [], out);
          continue;
        }
        const code = makeCode(child);
        const subTitle =
          child.checked === true && !supportsItemOptions
            ? `✓ ${child.label ?? ""}`
            : child.label ?? "";
        const subItem: statusBarManager.StatusBarSubMenuItem = {
          subTitle,
          menuAction: {
            abilityName,
            moduleName,
            notifyOnly: true,
            menuCode: code,
          },
        };
        if (supportsItemOptions) {
          subItem.menuCode = code;
          if (child.checked !== undefined) {
            subItem.options = { selected: child.checked === true };
          }
        }
        out.push(subItem);
      }
    };

    const wireGroups = splitMenuGroups(items);
    const result: statusBarManager.StatusBarGroupMenu[] = [];
    for (let g = 0; g < wireGroups.length && topLevelCount < STATUS_BAR_MAX_MENU_ITEMS; g++) {
      const group = wireGroups[g];
      const outGroup: statusBarManager.StatusBarMenuItem[] = [];
      for (let i = 0; i < group.length && topLevelCount < STATUS_BAR_MAX_MENU_ITEMS; i++) {
        const item = group[i];
        if (item.enabled === false) {
          continue;
        }
        if (item.kind === "submenu") {
          const subMenu: statusBarManager.StatusBarSubMenuItem[] = [];
          collectSubItems(item.items ?? [], subMenu);
          if (subMenu.length === 0) {
            continue;
          }
          const menuItem: statusBarManager.StatusBarMenuItem = {
            title: item.label ?? "",
            subMenu,
          };
          if (supportsItemOptions) {
            menuItem.menuCode = `nsh-${generation}-${++seq}`;
          }
          outGroup.push(menuItem);
          topLevelCount++;
        } else {
          const code = makeCode(item);
          const title =
            item.checked === true && !supportsItemOptions
              ? `✓ ${item.label ?? ""}`
              : item.label ?? "";
          const menuItem: statusBarManager.StatusBarMenuItem = {
            title,
            menuAction: {
              abilityName,
              moduleName,
              notifyOnly: true,
              menuCode: code,
            },
          };
          if (supportsItemOptions) {
            menuItem.menuCode = code;
            if (item.checked !== undefined) {
              menuItem.options = { selected: item.checked === true };
            }
          }
          outGroup.push(menuItem);
          topLevelCount++;
        }
      }
      if (outGroup.length > 0) {
        result.push(outGroup);
      }
    }
    return result;
  }

  private async decodeTrayIcons(icon?: WireTrayIcon): Promise<statusBarManager.StatusBarIcon> {
    const base64 = new util.Base64Helper();
    if (
      icon &&
      icon.format === "rgba" &&
      icon.width &&
      icon.height &&
      icon.whiteBase64 &&
      icon.blackBase64
    ) {
      try {
        const whiteBytes = base64.decodeSync(icon.whiteBase64);
        const blackBytes = base64.decodeSync(icon.blackBase64);
        const whiteBuf = whiteBytes.buffer.slice(
          whiteBytes.byteOffset,
          whiteBytes.byteOffset + whiteBytes.byteLength,
        );
        const blackBuf = blackBytes.buffer.slice(
          blackBytes.byteOffset,
          blackBytes.byteOffset + blackBytes.byteLength,
        );
        const opts: image.InitializationOptions = {
          size: { width: icon.width, height: icon.height },
          srcPixelFormat: image.PixelMapFormat.RGBA_8888,
          pixelFormat: image.PixelMapFormat.RGBA_8888,
          alphaType: image.AlphaType.PREMUL,
        };
        const white = await image.createPixelMap(whiteBuf, opts);
        try {
          const black = await image.createPixelMap(blackBuf, opts);
          return { white, black };
        } catch (error) {
          white.release().catch((): void => {});
          throw error instanceof Error ? error : new Error(String(error));
        }
      } catch (error) {
        console.warn(`[NSH] tray RGBA icon decode failed: ${String(error)}`);
      }
    } else if (icon && icon.format === "encoded" && icon.dataBase64) {
      try {
        const raw = base64.decodeSync(icon.dataBase64);
        const rawBuf = raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.byteLength);
        const source = image.createImageSource(rawBuf);
        try {
          const opts: image.DecodingOptions = {
            desiredSize: { width: 48, height: 48 },
            desiredPixelFormat: image.PixelMapFormat.RGBA_8888,
          };
          const white = await source.createPixelMap(opts);
          try {
            const black = await source.createPixelMap(opts);
            return { white, black };
          } catch (error) {
            white.release().catch((): void => {});
            throw error instanceof Error ? error : new Error(String(error));
          }
        } finally {
          source.release().catch((): void => {});
        }
      } catch (error) {
        console.warn(`[NSH] tray encoded icon decode failed: ${String(error)}`);
      }
    }
    return this.createFallbackTrayIcons();
  }

  private async createFallbackTrayIcons(): Promise<statusBarManager.StatusBarIcon> {
    const side = 48;
    const whitePixels = new Uint8Array(side * side * 4);
    const blackPixels = new Uint8Array(side * side * 4);
    const cx = (side - 1) / 2;
    const cy = (side - 1) / 2;
    const r = side * 0.34;
    for (let y = 0; y < side; y++) {
      for (let x = 0; x < side; x++) {
        const dist = Math.hypot(x - cx, y - cy);
        const cov = Math.max(0, Math.min(1, r + 0.75 - dist));
        const alpha = Math.round(cov * 255);
        const idx = (y * side + x) * 4;
        whitePixels[idx] = alpha;
        whitePixels[idx + 1] = alpha;
        whitePixels[idx + 2] = alpha;
        whitePixels[idx + 3] = alpha;
        blackPixels[idx + 3] = alpha;
      }
    }
    const opts: image.InitializationOptions = {
      size: { width: side, height: side },
      srcPixelFormat: image.PixelMapFormat.RGBA_8888,
      pixelFormat: image.PixelMapFormat.RGBA_8888,
      alphaType: image.AlphaType.PREMUL,
    };
    const white = await image.createPixelMap(whitePixels.buffer, opts);
    try {
      const black = await image.createPixelMap(blackPixels.buffer, opts);
      return { white, black };
    } catch (error) {
      white.release().catch((): void => {});
      throw error instanceof Error ? error : new Error(String(error));
    }
  }
}
