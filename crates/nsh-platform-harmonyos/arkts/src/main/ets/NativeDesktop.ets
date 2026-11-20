import { common } from "@kit.AbilityKit";
import { util } from "@kit.ArkTS";
import { LengthMetrics, window } from "@kit.ArkUI";
import { deviceInfo, emitter } from "@kit.BasicServicesKit";
import { statusBarManager } from "@kit.DeskTopExtensionKit";
import { image } from "@kit.ImageKit";
import { inputConsumer } from "@kit.InputKit";
import { ComponentContent, NodeContent } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import display from "@ohos.display";
import pointer from "@ohos.multimodalInput.pointer";
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
  display?: string;
  critical?: boolean;
  generation?: number;
  items?: WireMenuItem[];
  tooltip?: string;
  click?: number;
  icon?: WireTrayIcon;
  id?: number;
  preKeys?: number[];
  finalKey?: number;
}

interface TrayMenuTarget {
  id: number;
  role?: string;
}

interface RegisteredHotkey {
  options: inputConsumer.HotkeyOptions;
  callback: (options: inputConsumer.HotkeyOptions) => void;
}

interface WireBoundsPayload {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface WireDisplayPayload {
  idName: string;
  name: string;
  bounds: WireBoundsPayload;
  visibleBounds?: WireBoundsPayload;
  primary: boolean;
  scaleFactor: number;
  refreshRate?: number;
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

interface PopupMenuParams {
  owner: NativeDesktop;
  generation: number;
  x: number;
  y: number;
  windowWidth: number;
  windowHeight: number;
  items: WireMenuItem[];
  colorMode: ThemeColorMode;
}

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
                } else {
                  MenuItem({
                    content: item.label ?? "",
                    labelInfo: item.shortcut,
                  })
                    .selected(item.checked === true)
                    .selectIcon(item.checked !== undefined)
                    .enabled(item.enabled !== false)
                    .contentFontColor(
                      item.destructive ? "#D92D20" : $r("sys.color.font_primary"),
                    )
                    .onClick((): void => {
                      this.owner?.selectPopupMenuItem(this.generation, item.id ?? 0, item.role);
                    });
                }
              },
              (item: WireMenuItem, index: number): string => menuItemKey(item, index),
            );
          };
        },
        (_group: WireMenuItem[], groupIndex: number): string => `g:${groupIndex}`,
      );
    }
    .menuItemGroupDivider({
      strokeWidth: LengthMetrics.vp(0.5),
      color: $r("sys.color.comp_divider"),
      startMargin: LengthMetrics.vp(8),
      endMargin: LengthMetrics.vp(8),
    })
    .padding(4);
  }
}

@Component
struct DesktopPopupOverlay {
  owner?: NativeDesktop;
  generation: number = 0;
  x: number = 0;
  y: number = 0;
  windowWidth: number = 0;
  windowHeight: number = 0;
  items: WireMenuItem[] = [];
  colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  @State private menuWidth: number = 180;
  @State private menuHeight: number = 0;

  private clampedX(): number {
    const winW = this.windowWidth > 0 ? this.windowWidth : this.owner?.windowWidthVp() ?? 0;
    if (winW <= 0) {
      return Math.max(8, this.x);
    }
    return Math.max(8, Math.min(winW - this.menuWidth - 8, this.x));
  }

  private clampedY(): number {
    const winH = this.windowHeight > 0 ? this.windowHeight : this.owner?.windowHeightVp() ?? 0;
    if (winH <= 0 || this.menuHeight <= 0) {
      return Math.max(8, this.y);
    }
    return Math.max(8, Math.min(winH - this.menuHeight - 8, this.y));
  }

  build() {
    WithTheme({ colorMode: this.colorMode }) {
      Stack({ alignContent: Alignment.TopStart }) {
        Column() {
          DesktopSubmenuView({
            owner: this.owner,
            generation: this.generation,
            items: this.items,
          });
        }
        .backgroundColor($r("sys.color.comp_background_primary"))
        .borderRadius(12)
        .border({ width: 0.5, color: $r("sys.color.comp_divider") })
        .shadow(ShadowStyle.OUTER_DEFAULT_MD)
        .onAreaChange((_old: Area, area: Area): void => {
          const w = Number(area.width);
          const h = Number(area.height);
          if (w > 0) {
            this.menuWidth = w;
          }
          if (h > 0) {
            this.menuHeight = h;
          }
        })
        .position({ x: this.clampedX(), y: this.clampedY() })
        .onClick((): void => {});
      }
      .width("100%")
      .height("100%")
      .backgroundColor(Color.Transparent)
      .onClick((): void => {
        this.owner?.dismissPopupMenu(this.generation);
      })
      .onKeyEvent((event: KeyEvent): void => {
        if (event.type === KeyType.Down && event.keyCode === 2070) {
          this.owner?.dismissPopupMenu(this.generation);
        }
      });
    };
  }
}

@Builder
function desktopPopupBuilder(params: PopupMenuParams) {
  DesktopPopupOverlay({
    owner: params.owner,
    generation: params.generation,
    x: params.x,
    y: params.y,
    windowWidth: params.windowWidth,
    windowHeight: params.windowHeight,
    items: params.items,
    colorMode: params.colorMode,
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

function resolveScaleFactor(displayId: number | undefined, fallbackUi: UIContext): number {
  try {
    const screen =
      displayId !== undefined
        ? display.getDisplayByIdSync(displayId)
        : display.getDefaultDisplaySync();
    if (screen.densityPixels > 0) {
      return Math.round(screen.densityPixels * 100) / 100;
    }
  } catch {}
  try {
    const vp = fallbackUi.vp2px(1);
    if (vp > 0) {
      return Math.round(vp * 100) / 100;
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

// Matches `nsh_core::desktop::DisplayId::from_name` (64-bit FNV-1a, 16 lowercase hex digits).
function displayIdHex(idName: string): string {
  let hash: bigint = FNV_OFFSET_64;
  for (let i = 0; i < idName.length; i++) {
    hash = hash ^ BigInt(idName.charCodeAt(i) & 0xff);
    hash = (hash * FNV_PRIME_64) & MASK_64;
  }
  return hash.toString(16).padStart(16, "0");
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
  try {
    const area = await screen.getAvailableArea();
    if (area.width > 0 && area.height > 0) {
      return {
        left: area.left,
        top: area.top,
        width: area.width,
        height: area.height,
      };
    }
  } catch {}
  return {
    left: 0,
    top: 0,
    width: screen.width,
    height: screen.height,
  };
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
  private titleBarHidden: boolean = false;
  private requestedTitleBarHeight: number = 40;
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
  private normalBounds?: WireBoundsPayload;
  private lastStateJson: string = "";
  private lastTitleBarJson: string = "";
  private pendingBounds?: WireCommand;
  private flushingBounds: boolean = false;
  private pendingFocusId?: string;

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
  };

  private readonly onStatusChange = (status: window.WindowStatusType): void => {
    if (this.destroyed) {
      return;
    }
    this.applyWindowStatus(status);
    this.syncState();
  };

  private readonly onWindowEvent = (event: window.WindowEventType): void => {
    if (this.destroyed) {
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
        this.hadFocus = true;
        this.flushPendingFocus();
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
    if (!this.destroyed) {
      this.pageBridge?.setColorMode(mode);
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

  private computeInitialRect(cmd: WireCommand): window.Rect {
    const parentWin =
      (cmd.parentWindowId !== undefined ? this.owner.getSubWindowRef(cmd.parentWindowId) : undefined) ??
      this.owner.mainWindowRef();
    let parentProps: window.WindowProperties | undefined = undefined;
    try {
      parentProps = parentWin.getWindowProperties();
    } catch {}
    const displayId = parentProps?.displayId;
    const scale = resolveScaleFactor(displayId, this.owner.mainUiContext());
    let screenWidth = Math.round(1600 * scale);
    let screenHeight = Math.round(1000 * scale);
    try {
      const screen =
        displayId !== undefined
          ? display.getDisplayByIdSync(displayId)
          : display.getDefaultDisplaySync();
      if (screen.width > 0 && screen.height > 0) {
        screenWidth = screen.width;
        screenHeight = screen.height;
      }
    } catch {}
    const widthPx = Math.max(120, Math.round((cmd.width ?? 640) * scale));
    const heightPx = Math.max(80, Math.round((cmd.height ?? 520) * scale));
    const parentRect = parentProps?.windowRect ?? {
      left: Math.round((screenWidth - widthPx) / 2),
      top: Math.round((screenHeight - heightPx) / 2),
      width: Math.min(screenWidth, Math.round(1100 * scale)),
      height: Math.min(screenHeight, Math.round(760 * scale)),
    };

    let leftPx = Math.round((screenWidth - widthPx) / 2);
    let topPx = Math.round((screenHeight - heightPx) / 2);
    const pos = cmd.position ?? "auto";
    if (pos === "at" && cmd.x !== undefined && cmd.y !== undefined) {
      leftPx = Math.round(cmd.x * scale);
      topPx = Math.round(cmd.y * scale);
      this.userPositioned = true;
    } else if (pos === "parentCenter") {
      leftPx = Math.round(parentRect.left + (parentRect.width - widthPx) / 2);
      topPx = Math.round(parentRect.top + (parentRect.height - heightPx) / 2);
    } else if (pos === "center") {
      leftPx = Math.round((screenWidth - widthPx) / 2);
      topPx = Math.round((screenHeight - heightPx) / 2);
    } else if (cmd.kind === "panel") {
      const gap = Math.round(16 * scale);
      if (parentRect.left + parentRect.width + gap + widthPx <= screenWidth) {
        leftPx = parentRect.left + parentRect.width + gap;
      } else if (parentRect.left - gap - widthPx >= 0) {
        leftPx = parentRect.left - gap - widthPx;
      } else {
        leftPx = Math.max(0, screenWidth - widthPx - Math.round(24 * scale));
      }
      topPx = Math.max(0, Math.min(screenHeight - heightPx, parentRect.top));
    } else if (cmd.kind === "popup" || cmd.kind === "overlay") {
      leftPx = Math.round(parentRect.left + (parentRect.width - widthPx) / 2);
      topPx = Math.round(parentRect.top + Math.min(parentRect.height * 0.16, 120 * scale));
    } else {
      const step = Math.round(32 * scale);
      const offset = ((this.owner.nextCascade() % 6) + 1) * step;
      leftPx = parentRect.left + offset;
      topPx = parentRect.top + offset;
    }

    const maxLeft = Math.max(0, screenWidth - widthPx);
    const maxTop = Math.max(0, screenHeight - heightPx);
    leftPx = Math.max(0, Math.min(maxLeft, leftPx));
    topPx = Math.max(0, Math.min(maxTop, topPx));
    return { left: leftPx, top: topPx, width: widthPx, height: heightPx };
  }

  private async openInternal(cmd: WireCommand): Promise<void> {
    this.isModal = cmd.modal === true;
    this.frameNone = cmd.frame === "none";
    this.transparent = cmd.transparent === true;
    this.backdrop = cmd.backdrop ?? "none";
    this.titleBarHidden = this.frameNone || cmd.titleBarStyle === "hidden";
    if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0) {
      this.requestedTitleBarHeight = cmd.titleBarHeight;
    }
    this.closeOnBlur = cmd.closeOnBlur === true;
    this.initialPosition = cmd.position ?? "auto";
    this.parentWindowId = cmd.parentWindowId;
    this.maximizable = cmd.maximizable !== false;
    this.minimizable = cmd.minimizable !== false;
    this.closable = cmd.closable !== false;

    const initialRect = this.computeInitialRect(cmd);
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
        deviceInfo.sdkApiVersion >= 26 &&
        cmd.kind === "normal" &&
        (cmd.level === undefined || cmd.level === "normal")
      ) {
        options.zLevelAboveParentLoosened = true;
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
    const sub = await this.owner.mainWindowRef().createSubWindowWithOptions(subName, options);
    if (this.destroyed) {
      try {
        await sub.destroyWindow();
      } catch {}
      return;
    }
    this.win = sub;
    this.subscribe(sub);

    if (deviceInfo.sdkApiVersion < 18) {
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

    try {
      this.ui = sub.getUIContext();
    } catch {}

    let platformWindowId = 0;
    try {
      platformWindowId = sub.getWindowProperties().id;
    } catch {}

    if (this.transparent || this.frameNone || this.backdrop !== "none") {
      try {
        sub.setWindowBackgroundColor("#00000000");
      } catch {}
    }
    if (this.transparent && cmd.shadow === undefined) {
      try {
        await sub.setWindowShadowEnabled(false);
      } catch {}
    }
    await this.applyAttributesInternal(cmd);

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
    this.updateCursor(this.owner.activeCursorStyle());
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
      sub.on("windowTitleButtonRectChange", this.onTitleButtonRectChange);
    } catch {}
    try {
      sub.on("windowWillClose", this.onWindowWillClose);
    } catch {}
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
    try {
      sub.off("windowTitleButtonRectChange", this.onTitleButtonRectChange);
    } catch {}
    try {
      sub.off("windowWillClose", this.onWindowWillClose);
    } catch {}
  }

  private applyWindowStatus(status: window.WindowStatusType): void {
    if (status === window.WindowStatusType.FULL_SCREEN) {
      this.mode = "fullScreen";
      this.minimized = false;
    } else if (status === window.WindowStatusType.MAXIMIZE) {
      this.mode = "maximized";
      this.minimized = false;
    } else if (status === window.WindowStatusType.MINIMIZE) {
      this.minimized = true;
    } else {
      this.mode = "normal";
      this.minimized = false;
    }
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
        try {
          sub.setWindowBackgroundColor("#00000000");
        } catch {}
      }
    }
    if (cmd.backdrop !== undefined) {
      this.backdrop = cmd.backdrop;
      this.pageBridge?.setBackdrop(this.backdrop);
      if (this.backdrop !== "none") {
        try {
          sub.setWindowBackgroundColor("#00000000");
        } catch {}
      }
    }
    if (cmd.title !== undefined) {
      try {
        await sub.setWindowTitle(cmd.title);
      } catch {}
    }
    if (cmd.frame === "none") {
      this.frameNone = true;
      this.titleBarHidden = true;
      try {
        sub.setWindowDecorVisible(false);
      } catch {}
      try {
        sub.setWindowTitleButtonVisible(false, false, false);
      } catch {}
      this.emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
    } else if (cmd.titleBarStyle !== undefined && !this.frameNone) {
      const hidden = cmd.titleBarStyle === "hidden";
      this.titleBarHidden = hidden;
      if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0) {
        this.requestedTitleBarHeight = cmd.titleBarHeight;
      }
      try {
        sub.setWindowDecorVisible(!hidden);
      } catch {}
      if (hidden) {
        try {
          if (this.requestedTitleBarHeight > 0) {
            sub.setWindowDecorHeight(this.requestedTitleBarHeight);
          }
        } catch {}
        this.syncTitleBar();
      } else {
        this.emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
      }
    } else if (
      cmd.titleBarHeight !== undefined &&
      cmd.titleBarHeight > 0 &&
      this.titleBarHidden &&
      !this.frameNone
    ) {
      this.requestedTitleBarHeight = cmd.titleBarHeight;
      try {
        sub.setWindowDecorHeight(cmd.titleBarHeight);
      } catch {}
      this.syncTitleBar();
    }

    if (
      cmd.maximizable !== undefined ||
      cmd.minimizable !== undefined ||
      cmd.closable !== undefined
    ) {
      if (cmd.maximizable !== undefined) {
        this.maximizable = cmd.maximizable;
      }
      if (cmd.minimizable !== undefined) {
        this.minimizable = cmd.minimizable;
      }
      if (cmd.closable !== undefined) {
        this.closable = cmd.closable;
      }
      if (!this.frameNone) {
        try {
          sub.setWindowTitleButtonVisible(this.maximizable, this.minimizable, this.closable);
        } catch {}
      }
    }

    if (cmd.minSize !== undefined || cmd.maxSize !== undefined) {
      const scale = resolveScaleFactor(
        sub.getWindowProperties().displayId,
        this.ui ?? this.owner.mainUiContext(),
      );
      const limits: window.WindowLimits = {};
      if (cmd.minSize !== undefined && cmd.minSize !== null) {
        limits.minWidth = Math.max(1, Math.round(cmd.minSize.width * scale));
        limits.minHeight = Math.max(1, Math.round(cmd.minSize.height * scale));
      }
      if (cmd.maxSize !== undefined && cmd.maxSize !== null) {
        limits.maxWidth = Math.max(1, Math.round(cmd.maxSize.width * scale));
        limits.maxHeight = Math.max(1, Math.round(cmd.maxSize.height * scale));
      }
      try {
        await sub.setWindowLimits(limits);
      } catch {}
    }

    if (cmd.level !== undefined && !this.isModal) {
      if (deviceInfo.sdkApiVersion >= 20) {
        try {
          await sub.setSubWindowZLevel(resolveSubWindowZLevel(cmd.kind, cmd.level));
        } catch {}
      }
      const topmost = cmd.level === "floating" || cmd.level === "aboveDock";
      try {
        await sub.setWindowTopmost(topmost);
      } catch {}
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

    if (cmd.shadow !== undefined) {
      try {
        await sub.setWindowShadowEnabled(cmd.shadow);
      } catch {}
    }
  }

  private async applyRequestInternal(cmd: WireCommand): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    switch (cmd.action) {
      case "show":
      case "focus":
        try {
          await sub.showWindow();
        } catch {}
        this.syncState();
        break;
      case "hide":
      case "minimize":
        try {
          await sub.minimize();
        } catch {}
        this.syncState();
        break;
      case "restore":
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
        if (cmd.mode && cmd.mode !== "normal") {
          await this.setModeInternal(cmd.mode);
        }
        break;
      case "center":
        await this.centerInternal(false, undefined, undefined, cmd.display);
        break;
    }
  }

  private async setModeInternal(mode: string): Promise<void> {
    const sub = this.win;
    if (!sub || this.destroyed) {
      return;
    }
    try {
      if (mode === "maximized") {
        await sub.maximize(window.MaximizePresentation.EXIT_IMMERSIVE);
      } else if (mode === "fullScreen") {
        await sub.maximize(window.MaximizePresentation.ENTER_IMMERSIVE);
      } else {
        await sub.recover();
      }
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
      const props = sub.getWindowProperties();
      const rect = props.windowRect;
      const scale = resolveScaleFactor(props.displayId, this.ui ?? this.owner.mainUiContext());
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
      if (move) {
        this.userPositioned = true;
      }
      if (this.mode !== "normal") {
        try {
          await sub.recover();
        } catch {}
      }
      if (move) {
        try {
          await sub.moveWindowTo(moveX, moveY);
        } catch {}
      }
      if (resize) {
        try {
          await sub.resize(sizeW, sizeH);
        } catch {}
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
      let height = this.requestedTitleBarHeight;
      try {
        const decor = sub.getWindowDecorHeight();
        if (decor > 0) {
          height = decor;
        }
      } catch {}
      this.emitTitleBar({ top: height, right: 136, bottom: 0, left: 0 });
    }
  }

  private reportTitleBar(rect: window.TitleButtonRect): void {
    const sub = this.win;
    let height = this.requestedTitleBarHeight;
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
        minimized: this.minimized,
        visible: this.visible && !this.minimized,
        focused: this.focused && this.visible && !this.minimized,
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
  private colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  private currentCursorStyle?: pointer.PointerStyle;
  private nextCascadeIndex: number = 0;
  private titleBarHidden: boolean = false;
  private requestedTitleBarHeight: number = 44;
  private closeOnPress: boolean = true;
  private maximizable: boolean = true;
  private minimizable: boolean = true;
  private closable: boolean = true;
  private focused: boolean = true;
  private visible: boolean = true;
  private mode: string = "normal";
  private minimized: boolean = false;
  private normalBounds?: WireBoundsPayload;
  private lastWindowStateJson: string = "";
  private lastTitleBarJson: string = "";
  private popupContent?: ComponentContent<PopupMenuParams>;
  private popupUi?: UIContext;
  private popupGeneration: number = 0;
  private trayAdded: boolean = false;
  private traySubscribed: boolean = false;
  private trayGeneration: number = 0;
  private trayClickId: number = 0;
  private trayLastIconKey: string = "";
  private trayLastHoverTips: string = "";
  private trayMenuTargets: Map<string, TrayMenuTarget> = new Map();
  private trayQueue: Promise<void> = Promise.resolve();
  private hotkeys: Map<number, RegisteredHotkey> = new Map();

  private readonly onTrayIconClick = (_event: emitter.EventData): void => {
    if (this.closed) {
      return;
    }
    if (this.trayClickId !== 0) {
      this.emit("tray-click", JSON.stringify({ id: this.trayClickId }));
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
      JSON.stringify({ generation: 0, id: target.id, role: target.role }),
    );
  };

  private readonly onWindowRectChange = (_data: window.RectChangeOptions): void => {
    this.syncWindowState();
  };

  private readonly onWindowStatusChange = (status: window.WindowStatusType): void => {
    this.applyWindowStatus(status);
    this.syncWindowState();
  };

  private readonly onWindowEvent = (event: window.WindowEventType): void => {
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
    if (this.titleBarHidden) {
      this.reportTitleBar(rect);
    }
  };

  private readonly onWindowWillClose = (): Promise<boolean> => {
    if (!this.closed) {
      this.emit("close-press", JSON.stringify({ reason: "closeButton" }));
    }
    // `resolve(true)` prevents closing; `resolve(false)` lets the window close.
    return Promise.resolve(!this.closeOnPress);
  };

  private readonly onDisplayChange = (_id: number): void => {
    this.syncDisplays();
    this.syncWindowState();
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
  ) {
    this.ui = ui;
    this.abilityContext = abilityContext;
    this.mainWindow = mainWindow;
    this.emit = emit;
    this.attachSubWindowCb = attachSubWindow;
    try {
      this.visible = mainWindow.isWindowShowing();
      this.focused = mainWindow.isFocused();
    } catch {}
    try {
      this.applyWindowStatus(mainWindow.getWindowStatus());
    } catch {}
    this.subscribe();
    this.syncDisplays();
    this.syncWindowState();
  }

  mainWindowRef(): window.Window {
    return this.mainWindow;
  }

  mainUiContext(): UIContext {
    return this.ui;
  }

  getSubWindowRef(windowId: number): window.Window | undefined {
    return this.subWindows.get(windowId)?.windowRef();
  }

  nextCascade(): number {
    return this.nextCascadeIndex++;
  }

  activeCursorStyle(): pointer.PointerStyle | undefined {
    return this.currentCursorStyle;
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
  }

  updateCursor(style?: pointer.PointerStyle): void {
    this.currentCursorStyle = style;
    this.subWindows.forEach((sub: SubWindowHost): void => {
      sub.updateCursor(style);
    });
  }

  setColorMode(mode: ThemeColorMode): void {
    this.colorMode = mode;
    this.subWindows.forEach((sub: SubWindowHost): void => {
      sub.setColorMode(mode);
    });
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
      case "setTray":
        this.enqueueSetTray(cmd);
        break;
      case "removeTray":
        this.enqueueRemoveTray();
        break;
      case "registerGlobalShortcut":
        this.registerGlobalShortcut(cmd.id ?? 0, cmd.preKeys ?? [], cmd.finalKey ?? 0);
        break;
      case "unregisterGlobalShortcut":
        this.unregisterGlobalShortcut(cmd.id ?? 0);
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
      case "quit":
        this.closePopupMenuOnly();
        this.closeAllSubWindows();
        this.removeTraySync();
        this.unregisterAllGlobalShortcuts();
        this.abilityContext.terminateSelf().catch((): void => {});
        break;
      case "restart":
        this.closePopupMenuOnly();
        this.closeAllSubWindows();
        this.removeTraySync();
        this.unregisterAllGlobalShortcuts();
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

  selectPopupMenuItem(generation: number, id: number, role?: string): void {
    if (this.closed || generation !== this.popupGeneration) {
      return;
    }
    this.closePopupMenuOnly();
    this.emit("menu-select", JSON.stringify({ generation, id, role }));
  }

  dismissPopupMenu(generation: number): void {
    if (this.closed || generation !== this.popupGeneration) {
      return;
    }
    this.closePopupMenuOnly();
    this.emit("menu-close", JSON.stringify({ generation }));
  }

  close(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.closePopupMenuOnly();
    this.closeAllSubWindows();
    this.removeTraySync();
    this.unregisterAllGlobalShortcuts();
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
    try {
      display.off("add", this.onDisplayChange);
      display.off("remove", this.onDisplayChange);
      display.off("change", this.onDisplayChange);
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
    try {
      display.on("add", this.onDisplayChange);
      display.on("remove", this.onDisplayChange);
      display.on("change", this.onDisplayChange);
    } catch {}
  }

  private applyWindowStatus(status: window.WindowStatusType): void {
    if (status === window.WindowStatusType.FULL_SCREEN) {
      this.mode = "fullScreen";
      this.minimized = false;
    } else if (status === window.WindowStatusType.MAXIMIZE) {
      this.mode = "maximized";
      this.minimized = false;
    } else if (status === window.WindowStatusType.MINIMIZE) {
      this.minimized = true;
    } else {
      this.mode = "normal";
      this.minimized = false;
    }
  }

  private applyAttributes(cmd: WireCommand): void {
    if (cmd.title !== undefined) {
      try {
        this.mainWindow.setWindowTitle(cmd.title).catch((): void => {});
      } catch {}
    }
    if (
      cmd.transparent === true ||
      (cmd.backdrop !== undefined && cmd.backdrop !== "none")
    ) {
      try {
        this.mainWindow.setWindowBackgroundColor("#00000000");
      } catch {}
    }
    if (cmd.closeOnPress !== undefined) {
      this.closeOnPress = cmd.closeOnPress;
    }
    if (cmd.frame === "none") {
      this.titleBarHidden = true;
      try {
        this.mainWindow.setWindowDecorVisible(false);
      } catch {}
      try {
        this.mainWindow.setWindowTitleButtonVisible(false, false, false);
      } catch {}
      this.emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
    } else if (cmd.titleBarStyle !== undefined) {
      const hidden = cmd.titleBarStyle === "hidden";
      this.titleBarHidden = hidden;
      if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0) {
        this.requestedTitleBarHeight = cmd.titleBarHeight;
      }
      try {
        this.mainWindow.setWindowDecorVisible(!hidden);
      } catch {}
      if (hidden) {
        try {
          if (this.requestedTitleBarHeight > 0) {
            this.mainWindow.setWindowDecorHeight(this.requestedTitleBarHeight);
          }
        } catch {}
        this.syncTitleBar();
      } else {
        this.emitTitleBar({ top: 0, right: 0, bottom: 0, left: 0 });
      }
    } else if (cmd.titleBarHeight !== undefined && cmd.titleBarHeight > 0 && this.titleBarHidden) {
      this.requestedTitleBarHeight = cmd.titleBarHeight;
      try {
        this.mainWindow.setWindowDecorHeight(cmd.titleBarHeight);
      } catch {}
      this.syncTitleBar();
    }

    if (
      cmd.maximizable !== undefined ||
      cmd.minimizable !== undefined ||
      cmd.closable !== undefined
    ) {
      if (cmd.maximizable !== undefined) {
        this.maximizable = cmd.maximizable;
      }
      if (cmd.minimizable !== undefined) {
        this.minimizable = cmd.minimizable;
      }
      if (cmd.closable !== undefined) {
        this.closable = cmd.closable;
      }
      if (cmd.frame !== "none") {
        try {
          this.mainWindow.setWindowTitleButtonVisible(
            this.maximizable,
            this.minimizable,
            this.closable,
          );
        } catch {}
      }
    }

    if (cmd.minSize !== undefined || cmd.maxSize !== undefined) {
      const limits: window.WindowLimits = {};
      if (cmd.minSize !== undefined && cmd.minSize !== null) {
        limits.minWidth = Math.max(1, Math.round(this.ui.vp2px(cmd.minSize.width)));
        limits.minHeight = Math.max(1, Math.round(this.ui.vp2px(cmd.minSize.height)));
      }
      if (cmd.maxSize !== undefined && cmd.maxSize !== null) {
        limits.maxWidth = Math.max(1, Math.round(this.ui.vp2px(cmd.maxSize.width)));
        limits.maxHeight = Math.max(1, Math.round(this.ui.vp2px(cmd.maxSize.height)));
      }
      try {
        this.mainWindow.setWindowLimits(limits).catch((): void => {});
      } catch {}
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

  private applyRequest(cmd: WireCommand): void {
    switch (cmd.action) {
      case "show":
      case "focus":
        try {
          this.mainWindow.showWindow().catch((): void => {});
        } catch {}
        break;
      case "hide":
      case "minimize":
        try {
          this.mainWindow.minimize().catch((): void => {});
        } catch {}
        break;
      case "restore":
        try {
          this.mainWindow
            .restore()
            .catch((): void => {
              this.mainWindow.recover().catch((): void => {});
            });
        } catch {
          try {
            this.mainWindow.recover().catch((): void => {});
          } catch {}
        }
        break;
      case "close":
        this.closePopupMenuOnly();
        this.abilityContext.terminateSelf().catch((): void => {});
        break;
      case "setMode":
        this.setMode(cmd.mode ?? "normal");
        break;
      case "setBounds":
        this.applyBounds(cmd.x, cmd.y, cmd.width, cmd.height);
        break;
      case "setContentSize":
        this.applyBounds(undefined, undefined, cmd.width, cmd.height);
        break;
      case "setPlacement":
        this.applyBounds(cmd.x, cmd.y, cmd.width, cmd.height);
        if (cmd.mode && cmd.mode !== "normal") {
          this.setMode(cmd.mode);
        }
        break;
      case "center":
        void this.centerWindow(cmd.display);
        break;
    }
  }

  private setMode(mode: string): void {
    try {
      if (mode === "maximized") {
        this.mainWindow
          .maximize(window.MaximizePresentation.EXIT_IMMERSIVE)
          .then((): void => this.syncWindowState())
          .catch((): void => {});
      } else if (mode === "fullScreen") {
        this.mainWindow
          .maximize(window.MaximizePresentation.ENTER_IMMERSIVE)
          .then((): void => this.syncWindowState())
          .catch((): void => {});
      } else {
        this.mainWindow
          .recover()
          .then((): void => this.syncWindowState())
          .catch((): void => {});
      }
    } catch {}
  }

  private toggleMaximize(): void {
    if (this.mode === "maximized" || this.mode === "fullScreen") {
      this.setMode("normal");
    } else {
      this.setMode("maximized");
    }
  }

  private applyBounds(x?: number, y?: number, width?: number, height?: number): void {
    try {
      const rect = this.mainWindow.getWindowProperties().windowRect;
      const moveX = x !== undefined ? Math.round(this.ui.vp2px(x)) : rect.left;
      const moveY = y !== undefined ? Math.round(this.ui.vp2px(y)) : rect.top;
      const sizeW = width !== undefined ? Math.max(1, Math.round(this.ui.vp2px(width))) : rect.width;
      const sizeH =
        height !== undefined ? Math.max(1, Math.round(this.ui.vp2px(height))) : rect.height;
      const move =
        (x !== undefined || y !== undefined) &&
        (Math.abs(moveX - rect.left) > 1 || Math.abs(moveY - rect.top) > 1);
      const resize =
        (width !== undefined || height !== undefined) &&
        (Math.abs(sizeW - rect.width) > 1 || Math.abs(sizeH - rect.height) > 1);
      if (this.mode !== "normal") {
        this.mainWindow
          .recover()
          .then((): void => {
            if (move) {
              this.mainWindow.moveWindowTo(moveX, moveY).catch((): void => {});
            }
            if (resize) {
              this.mainWindow.resize(sizeW, sizeH).catch((): void => {});
            }
          })
          .catch((): void => {});
        return;
      }
      if (move) {
        this.mainWindow.moveWindowTo(moveX, moveY).catch((): void => {});
      }
      if (resize) {
        this.mainWindow.resize(sizeW, sizeH).catch((): void => {});
      }
    } catch {}
  }

  private async centerWindow(displayHex?: string): Promise<void> {
    if (this.closed) {
      return;
    }
    try {
      const props = this.mainWindow.getWindowProperties();
      const rect = props.windowRect;
      const screen = await resolveTargetDisplay(displayHex, props.displayId);
      const area = await displayAvailableRectPx(screen);
      const minX = area.left;
      const minY = area.top;
      const maxX = area.left + Math.max(0, area.width - rect.width);
      const maxY = area.top + Math.max(0, area.height - rect.height);
      const targetX = Math.max(
        minX,
        Math.min(maxX, area.left + Math.round((area.width - rect.width) / 2)),
      );
      const targetY = Math.max(
        minY,
        Math.min(maxY, area.top + Math.round((area.height - rect.height) / 2)),
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
    if (!this.titleBarHidden) {
      return;
    }
    try {
      const rect = this.mainWindow.getTitleButtonRect();
      this.reportTitleBar(rect);
    } catch {
      let height = this.requestedTitleBarHeight;
      try {
        const decor = this.mainWindow.getWindowDecorHeight();
        if (decor > 0) {
          height = decor;
        }
      } catch {}
      this.emitTitleBar({
        top: height,
        right: 136,
        bottom: 0,
        left: 0,
      });
    }
  }

  private reportTitleBar(rect: window.TitleButtonRect): void {
    let height = this.requestedTitleBarHeight;
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
    if (this.closed) {
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
          scale = Math.round(screen.densityPixels * 100) / 100;
        }
        displayName = `display-${screen.id}`;
      } catch {
        const vp = this.ui.vp2px(1);
        if (vp > 0) {
          scale = Math.round(vp * 100) / 100;
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

  private syncDisplays(): void {
    if (this.closed) {
      return;
    }
    const buildPayload = async (screens: display.Display[]): Promise<void> => {
      const out: WireDisplayPayload[] = [];
      let defaultId = 0;
      try {
        defaultId = display.getDefaultDisplaySync().id;
      } catch {}
      for (let i = 0; i < screens.length; i++) {
        const screen = screens[i];
        const scale =
          screen.densityPixels > 0 ? Math.round(screen.densityPixels * 100) / 100 : 1.0;
        const x = (screen.x ?? 0) / scale;
        const y = (screen.y ?? 0) / scale;
        const bounds: WireBoundsPayload = {
          x,
          y,
          width: screen.width / scale,
          height: screen.height / scale,
        };
        let visibleBounds: WireBoundsPayload = bounds;
        try {
          const area = await screen.getAvailableArea();
          if (area.width > 0 && area.height > 0) {
            visibleBounds = {
              x: x + area.left / scale,
              y: y + area.top / scale,
              width: area.width / scale,
              height: area.height / scale,
            };
          }
        } catch {}
        const rawName = (screen.name ?? "").trim();
        const displayLabel =
          rawName.length > 0 && rawName.toUpperCase() !== "UNKNOWN"
            ? rawName
            : screen.id === defaultId
              ? "Built-in Display"
              : `Display ${i + 1}`;
        out.push({
          idName: `display-${screen.id}`,
          name: displayLabel,
          bounds,
          visibleBounds,
          primary: screen.id === defaultId || i === 0,
          scaleFactor: scale,
          refreshRate: screen.refreshRate > 0 ? screen.refreshRate : undefined,
        });
      }
      if (!this.closed && out.length > 0) {
        this.emit("displays", JSON.stringify(out));
      }
    };
    display
      .getAllDisplays()
      .then((screens: display.Display[]): Promise<void> => buildPayload(screens))
      .catch((): void => {
        try {
          const screen = display.getDefaultDisplaySync();
          void buildPayload([screen]);
        } catch {}
      });
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
    this.popupGeneration = generation;
    const posX = x ?? Math.max(16, winW * 0.5 - 90);
    const posY = y ?? Math.max(48, winH * 0.35);
    const content = new ComponentContent<PopupMenuParams>(
      targetUi,
      wrapBuilder(desktopPopupBuilder),
      {
        owner: this,
        generation,
        x: posX,
        y: posY,
        windowWidth: winW,
        windowHeight: winH,
        items,
        colorMode: this.colorMode,
      },
      { nestingBuilderSupported: true },
    );
    this.popupContent = content;
    this.popupUi = targetUi;
    try {
      targetUi.getOverlayManager().addComponentContent(content);
    } catch (error) {
      console.error(`[NSH] desktop popup menu failed: ${String(error)}`);
      if (this.popupContent === content) {
        this.popupContent = undefined;
        this.popupUi = undefined;
      }
      try {
        content.dispose();
      } catch {}
      this.emit("menu-close", JSON.stringify({ generation }));
    }
  }

  private closePopupMenuOnly(): void {
    const content = this.popupContent;
    const targetUi = this.popupUi ?? this.ui;
    if (content === undefined) {
      return;
    }
    this.popupContent = undefined;
    this.popupUi = undefined;
    try {
      targetUi.getOverlayManager().removeComponentContent(content);
    } catch {}
    try {
      content.dispose();
    } catch {}
  }

  private registerGlobalShortcut(id: number, preKeys: number[], finalKey: number): void {
    if (id === 0) {
      return;
    }
    this.unregisterGlobalShortcut(id);
    if (
      preKeys.length === 0 ||
      finalKey === 0 ||
      !canIUse("SystemCapability.MultimodalInput.Input.InputConsumer")
    ) {
      this.emit("global-shortcut-registered", JSON.stringify({ id, registered: false }));
      return;
    }
    const options: inputConsumer.HotkeyOptions = {
      preKeys,
      finalKey,
      isRepeat: false,
    };
    const callback = (_event: inputConsumer.HotkeyOptions): void => {
      if (!this.closed) {
        this.emit("global-shortcut-trigger", JSON.stringify({ id }));
      }
    };
    try {
      inputConsumer.on("hotkeyChange", options, callback);
      this.hotkeys.set(id, { options, callback });
      this.emit("global-shortcut-registered", JSON.stringify({ id, registered: true }));
    } catch (error) {
      console.warn(`[NSH] global shortcut registration failed (id=${id}): ${String(error)}`);
      this.emit("global-shortcut-registered", JSON.stringify({ id, registered: false }));
    }
  }

  private unregisterGlobalShortcut(id: number): void {
    const entry = this.hotkeys.get(id);
    if (entry === undefined) {
      return;
    }
    this.hotkeys.delete(id);
    try {
      inputConsumer.off("hotkeyChange", entry.options, entry.callback);
    } catch {}
  }

  private unregisterAllGlobalShortcuts(): void {
    const entries = Array.from(this.hotkeys.values());
    this.hotkeys.clear();
    for (let i = 0; i < entries.length; i++) {
      try {
        inputConsumer.off("hotkeyChange", entries[i].options, entries[i].callback);
      } catch {}
    }
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
    if (deviceInfo.sdkApiVersion >= 26) {
      try {
        const supported = await statusBarManager.isStatusBarCapabilitySupported(
          this.abilityContext,
        );
        if (!supported) {
          return;
        }
      } catch {}
    }
    this.ensureTraySubscribed();
    this.trayClickId = cmd.click ?? 0;

    const menuGroups = this.buildStatusBarMenu(generation, cmd.items ?? []);
    const rawTips = (cmd.tooltip ?? "").trim() || (cmd.title ?? "").trim();
    const hoverTips = rawTips.length > 128 ? rawTips.substring(0, 128) : rawTips;
    const iconKey = cmd.icon ? JSON.stringify(cmd.icon) : "default";

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
        await this.addToStatusBarAsync(item);
        this.trayAdded = true;
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
      await this.updateStatusBarMenuAsync(menuGroups);
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
        await this.updateStatusBarIconAsync(icons);
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
        await statusBarManager.updateStatusBarHoverTips(this.abilityContext, hoverTips);
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
      await this.removeFromStatusBarAsync();
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
      try {
        statusBarManager.removeFromStatusBar(this.abilityContext);
      } catch {}
    }
    this.unsubscribeTray();
  }

  private ensureTraySubscribed(): void {
    if (this.traySubscribed || !canIUse("SystemCapability.PCService.StatusBarManager")) {
      return;
    }
    try {
      statusBarManager.on("statusBarIconClick", this.onTrayIconClick);
      statusBarManager.on("rightMenuClick", this.onTrayRightMenuClick);
      this.traySubscribed = true;
    } catch (error) {
      console.warn(`[NSH] statusBarManager event subscribe failed: ${String(error)}`);
    }
  }

  private unsubscribeTray(): void {
    if (!this.traySubscribed || !canIUse("SystemCapability.PCService.StatusBarManager")) {
      return;
    }
    this.traySubscribed = false;
    try {
      statusBarManager.off("statusBarIconClick", this.onTrayIconClick);
    } catch {}
    try {
      statusBarManager.off("rightMenuClick", this.onTrayRightMenuClick);
    } catch {}
  }

  private addToStatusBarAsync(item: statusBarManager.StatusBarItem): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      try {
        if (deviceInfo.sdkApiVersion >= 14) {
          statusBarManager.addToStatusBar(this.abilityContext, item, (err): void => {
            if (err) {
              reject(err);
            } else {
              resolve();
            }
          });
        } else {
          statusBarManager.addToStatusBar(this.abilityContext, item);
          resolve();
        }
      } catch (error) {
        reject(error);
      }
    });
  }

  private removeFromStatusBarAsync(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      try {
        if (deviceInfo.sdkApiVersion >= 14) {
          statusBarManager.removeFromStatusBar(this.abilityContext, (err): void => {
            if (err) {
              reject(err);
            } else {
              resolve();
            }
          });
        } else {
          statusBarManager.removeFromStatusBar(this.abilityContext);
          resolve();
        }
      } catch (error) {
        reject(error);
      }
    });
  }

  private updateStatusBarMenuAsync(groups: statusBarManager.StatusBarGroupMenu[]): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      try {
        if (deviceInfo.sdkApiVersion >= 14) {
          statusBarManager.updateStatusBarMenu(this.abilityContext, groups, (err): void => {
            if (err) {
              reject(err);
            } else {
              resolve();
            }
          });
        } else {
          statusBarManager.updateStatusBarMenu(this.abilityContext, groups);
          resolve();
        }
      } catch (error) {
        reject(error);
      }
    });
  }

  private updateStatusBarIconAsync(icons: statusBarManager.StatusBarIcon): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      try {
        if (deviceInfo.sdkApiVersion >= 14) {
          statusBarManager.updateStatusBarIcon(this.abilityContext, icons, (err): void => {
            if (err) {
              reject(err);
            } else {
              resolve();
            }
          });
        } else {
          statusBarManager.updateStatusBarIcon(this.abilityContext, icons);
          resolve();
        }
      } catch (error) {
        reject(error);
      }
    });
  }

  private buildStatusBarMenu(
    generation: number,
    items: WireMenuItem[],
  ): statusBarManager.StatusBarGroupMenu[] {
    this.trayMenuTargets.clear();
    const abilityName = this.abilityContext.abilityInfo?.name ?? "EntryAbility";
    const moduleName = this.abilityContext.abilityInfo?.moduleName ?? "entry";
    const supportsItemOptions = deviceInfo.sdkApiVersion >= 24;
    let seq = 0;
    let topLevelCount = 0;

    const makeCode = (item: WireMenuItem): string => {
      const code = `nsh-${generation}-${++seq}`;
      this.trayMenuTargets.set(code, { id: item.id ?? 0, role: item.role });
      return code;
    };

    const collectSubItems = (
      children: WireMenuItem[],
      out: statusBarManager.StatusBarSubMenuItem[],
    ): void => {
      for (let i = 0; i < children.length && out.length < 20; i++) {
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
    for (let g = 0; g < wireGroups.length && topLevelCount < 20; g++) {
      const group = wireGroups[g];
      const outGroup: statusBarManager.StatusBarMenuItem[] = [];
      for (let i = 0; i < group.length && topLevelCount < 20; i++) {
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
        const black = await image.createPixelMap(blackBuf, opts);
        return { white, black };
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
          const black = await source.createPixelMap(opts);
          return { white, black };
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
    const black = await image.createPixelMap(blackPixels.buffer, opts);
    return { white, black };
  }
}
