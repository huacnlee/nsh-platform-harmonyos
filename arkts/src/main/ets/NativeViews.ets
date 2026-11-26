import { NativeMenuItem, NativeMenuCleanup } from "./NativeMenus";
import {
  BuilderNode,
  NodeRenderType,
  NodeController,
  ComponentContent,
  NodeContent,
} from "@ohos.arkui.node";
import { LengthMetrics } from "@kit.ArkUI";
import { UIContext } from "@ohos.arkui.UIContext";
import deviceInfo from "@ohos.deviceInfo";
import { NshRuntimeContext } from "./NativeEditors";

interface ViewAction {
  name: string;
  label: string;
}
interface ViewMenu {
  generation: number;
  visible: boolean;
  x: number;
  y: number;
  items: NativeMenuItem[];
}
export interface Settings {
  textureSurface?: string;
  textureInsets?: TextureInsets | null;
  targetId: number;
  windowId?: number | null;
  testId?: string | null;
  width: number;
  height: number;
  semantic: boolean;
  clickable: boolean;
  keyFocusable?: boolean;
  focusCommandGen?: number;
  focusCommandFocus?: boolean;
  disabled: boolean;
  accessible?: boolean | null;
  hidden: boolean;
  label: string;
  hint: string;
  value: string;
  role: string;
  checked?: boolean | null;
  selected: boolean;
  api23: boolean;
  api26: boolean;
  actions: ViewAction[];
  menu?: ViewMenu | null;
}
interface TextureInsets {
  left: number;
  top: number;
  right: number;
  bottom: number;
}
interface ViewParameters {
  entry: ViewEntry;
  settings: Settings;
}
interface MenuParameters {
  entry: ViewEntry;
  menu: ViewMenu;
}
interface MenuLease {
  ui: UIContext;
  content: ComponentContent<MenuParameters>;
  generation: number;
  opening: Promise<void>;
  retired: boolean;
  disappeared: boolean;
}
function captureWidth(settings: Settings): number {
  return (
    settings.width + (settings.textureInsets?.left ?? 0) + (settings.textureInsets?.right ?? 0)
  );
}
function captureHeight(settings: Settings): number {
  return (
    settings.height + (settings.textureInsets?.top ?? 0) + (settings.textureInsets?.bottom ?? 0)
  );
}
interface Acknowledgement {
  accepted: boolean;
}
interface CustomAction {
  name: string;
  onAction: () => void;
}
// A structural interface keeps API-26 methods behind both feature and runtime guards.
interface ActionsAttribute {
  accessibilityCustomActions(actions: CustomAction[]): CommonAttribute;
}
interface StateAttribute {
  accessibilityStateDescription(description: string): CommonAttribute;
}
const quarantinedViews: Set<ViewEntry> = new Set();
const roles: Map<string, AccessibilityRoleType> = new Map([
  ["button", AccessibilityRoleType.BUTTON],
  ["checkbox", AccessibilityRoleType.CHECKBOX],
  ["switch", AccessibilityRoleType.SWITCH],
  ["radio", AccessibilityRoleType.RADIO],
  ["slider", AccessibilityRoleType.SLIDER],
  ["image", AccessibilityRoleType.IMAGE],
  ["link", AccessibilityRoleType.HYPERLINK],
  ["list", AccessibilityRoleType.LIST],
  ["grid", AccessibilityRoleType.GRID],
  ["progress", AccessibilityRoleType.PROGRESS],
  ["search", AccessibilityRoleType.SEARCH],
  ["textbox", AccessibilityRoleType.TEXT_INPUT],
  ["tabpanel", AccessibilityRoleType.TAB_CONTENT],
  ["menuitem", AccessibilityRoleType.MENU_ITEM],
  ["text", AccessibilityRoleType.TEXT],
]);
function role(value: string): AccessibilityRoleType | undefined {
  if (value === "") {
    return undefined;
  }
  const result = roles.get(value);
  if (result === undefined) {
    throw new Error("Invalid native wrapper role");
  }
  return result;
}

class ViewModifier implements AttributeModifier<StackAttribute> {
  private entry: ViewEntry;
  private settings: Settings;
  constructor(entry: ViewEntry, settings: Settings) {
    this.entry = entry;
    this.settings = settings;
  }
  applyNormalAttribute(node: StackAttribute): void {
    const s = this.settings;
    const useApi23 =
      s.api23 &&
      deviceInfo.sdkApiVersion >= 23 &&
      typeof (node as StateAttribute).accessibilityStateDescription === "function";
    node
      .id(s.testId ?? `nsh-view-${s.targetId}`)
      .width(s.width)
      .height(s.height)
      .clip(false)
      .tabStop(s.menu != null)
      .hitTestBehavior(HitTestMode.Transparent)
      .accessibilityText(s.label)
      .accessibilityDescription(
        useApi23 ? s.hint : [s.hint, s.value].filter((value) => value.length > 0).join(", "),
      )
      .accessibilityLevel(
        s.hidden ? "no-hide-descendants" : !s.semantic || s.accessible === false ? "no" : "yes",
      )
      .accessibilityGroup(s.accessible === true)
      .accessibilitySelected(s.selected);
    node.accessibilityRole(role(s.role));
    node.accessibilityChecked(s.checked === null ? undefined : s.checked);
    const keyFocusable = s.keyFocusable === true && !s.disabled;
    node
      .enabled(!s.disabled)
      .onClick(s.clickable || s.menu != null ? (): void => {} : undefined)
      .focusOnTouch(keyFocusable);
    // Do not call `.focusable(false)` on the wrapper `Stack`: in ArkUI that
    // disables focus for the entire descendant subtree (including child editors).
    if (keyFocusable) {
      node
        .focusable(true)
        .focusBox({ strokeWidth: LengthMetrics.vp(0) })
        .onFocus((): void => {
          this.entry.focusEvent("focus");
        })
        .onBlur((): void => {
          this.entry.focusEvent("blur");
        });
    } else {
      node.onFocus(undefined).onBlur(undefined);
    }
    if (useApi23) {
      (node as StateAttribute).accessibilityStateDescription(s.value);
    }
    if (
      deviceInfo.sdkApiVersion >= 26 &&
      typeof (node as ActionsAttribute).accessibilityCustomActions === "function"
    ) {
      const actions: CustomAction[] = s.actions.map((action: ViewAction): CustomAction => {
        return {
          name: action.label,
          onAction: (): void => {
            this.entry.action(action.name);
          },
        };
      });
      (node as ActionsAttribute).accessibilityCustomActions(actions);
    } else if (s.api26 && s.actions.length > 0) {
      console.warn(
        "[NSH] runtime lacks ArkUI accessibilityCustomActions; degrading custom actions",
      );
    }
  }
}
@Component
struct ViewMenuContent {
  entry!: ViewEntry;
  @Prop menu!: ViewMenu;
  build() {
    Menu() {
      ForEach(
        this.menu.items,
        (item: NativeMenuItem) => {
          MenuItem({ content: item.label }).onClick((): void => {
            this.entry.action(item.name, "menu", this.menu.generation);
          });
        },
        (item: NativeMenuItem): string => item.name,
      )
    }.padding(4);
  }
}
@Builder
function menuContent(params: MenuParameters) {
  ViewMenuContent({ entry: params.entry, menu: params.menu });
}
@Builder
function nativeView(params: ViewParameters) {
  Stack({ alignContent: Alignment.TopStart }) {
    ContentSlot(params.entry.slot)
  }
  .width(params.settings.width)
  .height(params.settings.height)
  .attributeModifier(new ViewModifier(params.entry, params.settings))
  .onAppear((): void => {
    params.entry.appeared();
  })
  .onAccessibilityActionIntercept(
    (action: AccessibilityAction): AccessibilityActionInterceptResult => {
      if (action === AccessibilityAction.ACCESSIBILITY_CLICK && params.entry.action("activate")) {
        return AccessibilityActionInterceptResult.ACTION_INTERCEPT;
      }
      return AccessibilityActionInterceptResult.ACTION_CONTINUE;
    },
  );
}
class ViewController extends NodeController {
  private node?: BuilderNode<[ViewParameters]>;
  constructor(node: BuilderNode<[ViewParameters]>) {
    super();
    this.node = node;
  }
  makeNode(_context: UIContext): FrameNode | null {
    return this.node?.getFrameNode() ?? null;
  }
  clear(): void {
    this.node = undefined;
    this.rebuild();
  }
}
@Observed
class ViewState {
  settings: Settings;
  constructor(settings: Settings) {
    this.settings = settings;
  }
}
// The source owns one native hierarchy and one texture destination for its lifetime.
@Component
struct SourceRoot {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    Stack({ alignContent: Alignment.TopStart }) {
      nativeView({ entry: this.entry, settings: this.state.settings })
    }
    .width(captureWidth(this.state.settings))
    .height(captureHeight(this.state.settings))
    .padding(this.state.settings.textureInsets ?? { left: 0, top: 0, right: 0, bottom: 0 })
    .clip(false)
    .hitTestBehavior(HitTestMode.Transparent);
  }
}
@Builder
function sourceContent(params: ViewParameters) {
  SourceRoot({ entry: params.entry, state: params.entry.state });
}
@Component
struct TextureContainer {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    NodeContainer(this.entry.textureController!)
      .width(captureWidth(this.state.settings))
      .height(captureHeight(this.state.settings))
      .clip(false)
      .hitTestBehavior(HitTestMode.Transparent);
  }
}
@Builder
function viewContent(params: ViewParameters) {
  if (params.entry.textureController) {
    TextureContainer({ entry: params.entry, state: params.entry.state });
  } else {
    SourceRoot({ entry: params.entry, state: params.entry.state });
  }
}
class ViewEntry {
  readonly textureNode?: BuilderNode<[ViewParameters]>;
  readonly textureController?: ViewController;
  private textureDisposed: boolean = false;
  readonly state: ViewState;
  readonly slot: NodeContent = new NodeContent();
  private node: BuilderNode<[ViewParameters]>;
  private context: NshRuntimeContext;
  private ui: UIContext;
  private menus: NativeMenuCleanup;
  private closed: boolean = false;
  private attached: boolean = false;
  private retiredNative: boolean = false;
  private nodeDisposed: boolean = false;
  private sdkBusy: boolean = false;
  private disposePending: boolean = false;
  private retirementFailure?: Error;
  private menuLease?: MenuLease;
  private appearedReady: boolean = false;
  private focusPending: boolean = false;
  private lastFocusCommandGen: number = 0;
  constructor(
    settings: Settings,
    context: NshRuntimeContext,
    menus: NativeMenuCleanup,
  ) {
    this.state = new ViewState(settings);
    this.menus = menus;
    this.context = context;
    const windowUi =
      settings.windowId !== undefined && settings.windowId !== null
        ? context.getWindowUIContext?.(settings.windowId)
        : undefined;
    this.ui = windowUi ?? context.getUIContext();
    if (settings.textureSurface !== undefined) {
      this.textureNode = new BuilderNode<[ViewParameters]>(this.ui, {
        type: NodeRenderType.RENDER_TYPE_TEXTURE,
        surfaceId: settings.textureSurface,
        selfIdealSize: { width: captureWidth(settings), height: captureHeight(settings) },
      });
      this.textureController = new ViewController(this.textureNode);
    }
    this.node = new BuilderNode<[ViewParameters]>(this.ui);
  }
  get settings(): Settings {
    return this.state.settings;
  }
  isClosed(): boolean {
    return this.closed;
  }
  belongsTo(context: NshRuntimeContext): boolean {
    return this.context === context;
  }
  private focusTargetNode(): FrameNode | null {
    const root = this.node.getFrameNode();
    const expectedId = this.viewId();
    // Walk down to the inner `nativeView` Stack (which carries `viewId()`),
    // skipping the outer `SourceRoot` container Stack.
    let cursor: FrameNode | null = root;
    let lastStack: FrameNode | null = null;
    let depth = 0;
    while (cursor && depth < 8) {
      if (cursor.getId() === expectedId) {
        return cursor;
      }
      if (cursor.getNodeType() === "Stack") {
        lastStack = cursor;
      }
      cursor = cursor.getFirstChild();
      depth += 1;
    }
    return lastStack ?? root;
  }
  private abortCreate(): void {
    this.closed = true;
    try {
      if (this.menuLease) {
        this.retireMenu(this.menuLease);
        this.menuLease = undefined;
      }
    } catch (_) {}
    try {
      this.disposeBuilders();
    } catch (error) {
      console.error(
        `[NSH] failed to clean up aborted wrapper ${this.settings.targetId}: ${String(error)}`,
      );
    }
  }
  create(): void {
    this.sdkBusy = true;
    let stage: string = "build";
    try {
      const params: ViewParameters = { entry: this, settings: this.settings };
      this.textureNode?.build(wrapBuilder(sourceContent), params, {
        nestingBuilderSupported: true,
      });
      this.node.build(wrapBuilder(viewContent), params, { nestingBuilderSupported: true });
      if (this.closed || !this.context.isActive()) {
        throw new Error("Native wrapper creation was cancelled");
      }
      stage = "attach";
      const accepted = this.context.attachNativeSubtree(
        this.settings.targetId,
        this.node.getFrameNode(),
        this.slot,
      );
      if (!accepted) {
        throw new Error("Native wrapper attachment was rejected");
      }
      this.attached = true;
      this.syncMenu();
      this.syncFocus();
    } catch (error) {
      if (this.attached) {
        try {
          this.dispose();
        } catch (_) {}
      } else {
        this.abortCreate();
      }
      throw new Error(`Native wrapper ${this.settings.targetId} ${stage} failed: ${String(error)}`);
    } finally {
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.disposeBuilders();
      }
    }
  }
  private static sameCaptureSize(a: Settings, b: Settings): boolean {
    const aw = a.width + (a.textureInsets?.left ?? 0) + (a.textureInsets?.right ?? 0);
    const ah = a.height + (a.textureInsets?.top ?? 0) + (a.textureInsets?.bottom ?? 0);
    const bw = b.width + (b.textureInsets?.left ?? 0) + (b.textureInsets?.right ?? 0);
    const bh = b.height + (b.textureInsets?.top ?? 0) + (b.textureInsets?.bottom ?? 0);
    return aw === bw && ah === bh;
  }
  update(settings: Settings): void {
    if (this.closed) {
      throw new Error("Native wrapper is already closed");
    }
    if (settings.targetId !== this.settings.targetId) {
      throw new Error("Native wrapper target cannot change");
    }
    const previous = this.state.settings;
    this.state.settings = settings;
    const params: ViewParameters = { entry: this, settings: settings };
    this.sdkBusy = true;
    try {
      if (this.textureNode) {
        this.textureNode.update(params);
        if (!ViewEntry.sameCaptureSize(previous, settings)) {
          this.node.update(params);
        }
      } else {
        this.node.update(params);
      }
    } finally {
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.disposeBuilders();
      }
    }
    if (!this.closed) {
      this.syncMenu();
      this.syncFocus();
    }
  }
  appeared(): void {
    this.appearedReady = true;
    if (this.focusPending && !this.closed && this.context.isActive()) {
      this.focusPending = false;
      this.requestViewFocus();
    } else if (!this.closed && this.context.isActive() && this.settings.windowId != null) {
      this.context.viewAction({
        targetId: this.settings.targetId,
        action: "appear",
        kind: "focus",
      });
    }
  }
  focusEvent(name: string): void {
    if (this.closed || !this.context.isActive() || this.settings.keyFocusable !== true) {
      return;
    }
    this.context.viewAction({
      targetId: this.settings.targetId,
      action: name,
      kind: "focus",
    });
  }
  private viewId(): string {
    return this.settings.testId ?? `nsh-view-${this.settings.targetId}`;
  }
  private viewUIContext(): UIContext {
    const id = this.viewId();
    return this.context.getUIContextFor ? this.context.getUIContextFor(id) : this.ui;
  }
  private requestViewFocus(): boolean {
    const id = this.viewId();
    try {
      if (this.context.requestFocus) {
        return this.context.requestFocus(id);
      }
      this.ui.getFocusController().requestFocus(id);
      return true;
    } catch (_) {
      return false;
    }
  }
  private syncFocus(): void {
    const gen = this.settings.focusCommandGen ?? 0;
    if (gen === 0 || gen === this.lastFocusCommandGen) {
      return;
    }
    this.lastFocusCommandGen = gen;
    if (this.settings.focusCommandFocus === true) {
      if (!this.appearedReady || !this.requestViewFocus()) {
        this.focusPending = true;
      }
    } else {
      this.focusPending = false;
      try {
        this.viewUIContext().getFocusController().clearFocus();
      } catch (_) {}
    }
  }
  private syncMenu(): void {
    try {
      this.applyMenu();
    } catch (error) {
      console.error(`[NSH] native menu construction failed: ${String(error)}`);
      const lease = this.menuLease;
      this.menuLease = undefined;
      if (lease) {
        this.retireMenu(lease);
      }
      this.action("", "menu-dismiss", this.settings.menu?.generation);
    }
  }
  private applyMenu(): void {
    const menu = this.settings.menu;
    const previous = this.menuLease;
    if (this.closed || menu?.visible !== true) {
      this.menuLease = undefined;
      if (previous) {
        this.retireMenu(previous);
      }
      return;
    }
    if (previous?.generation === menu.generation) {
      previous.content.update({ entry: this, menu: menu });
      return;
    }
    if (previous) {
      this.retireMenu(previous);
    }
    const ui = this.viewUIContext();
    const frame = this.focusTargetNode() ?? this.node.getFrameNode();
    if (!frame) {
      return;
    }
    // C custom nodes have a disabled focus type. Their native Stack wrapper
    // owns selection focus, so keys bubble only inside this surface and the
    // menu restores that focus without taking it back from a host control.
    ui.getFocusController().requestFocus(this.viewId());
    const content = new ComponentContent<MenuParameters>(
      ui,
      wrapBuilder(menuContent),
      { entry: this, menu: menu },
      { nestingBuilderSupported: true },
    );
    const lease: MenuLease = {
      ui: ui,
      content: content,
      generation: menu.generation,
      opening: Promise.resolve(),
      retired: false,
      disappeared: false,
    };
    this.menuLease = lease;
    lease.opening = ui
      .getPromptAction()
      .openMenu(content, { id: frame.getUniqueId() }, this.menuOptions(lease));
    lease.opening.catch((error: Error): void => {
      lease.disappeared = true;
      console.error(`[NSH] native menu open failed: ${String(error)}`);
      if (this.menuLease === lease) {
        this.menuLease = undefined;
        this.action("", "menu-dismiss", lease.generation);
      }
      this.retireMenu(lease);
    });
  }
  private retireMenu(lease: MenuLease): void {
    if (lease.retired) {
      return;
    }
    lease.retired = true;
    this.menus.retire(
      lease,
      lease.opening,
      (): Promise<void> =>
        lease.disappeared
          ? Promise.resolve()
          : lease.ui.getPromptAction().closeMenu(lease.content),
      (): void => lease.content.dispose(),
    );
  }

  menuOptions(lease: MenuLease): MenuOptions {
    const menu = this.settings.menu;
    return {
      placement: Placement.TopLeft,
      offset: { x: menu?.x ?? 0, y: menu?.y ?? 0 },
      showInSubWindow: false,
      onDisappear: (): void => {
        lease.disappeared = true;
        // Every popup retains its own generation, including during replacement.
        const generation = menu?.generation;
        if (generation !== undefined) {
          this.action("", "menu-dismiss", generation);
        }
      },
    };
  }
  action(name: string, kind?: string, generation?: number): boolean {
    if (
      this.closed ||
      !this.context.isActive() ||
      (kind !== undefined && generation !== this.settings.menu?.generation)
    ) {
      return false;
    }
    return this.context.viewAction({
      targetId: this.settings.targetId,
      action: name,
      kind: kind,
      generation: generation,
    });
  }
  dispose(cleanup: NshRuntimeContext = this.context): void {
    if (this.closed && !this.retirementFailure) {
      return;
    }
    this.closed = true;
    this.retirementFailure = undefined;
    quarantinedViews.add(this);
    try {
      if (this.menuLease) {
        this.retireMenu(this.menuLease);
        this.menuLease = undefined;
      }
      if (this.attached && !this.retiredNative) {
        if (!cleanup.retireNativeNode(this.settings.targetId)) {
          throw new Error("Native wrapper retirement was rejected");
        }
        this.retiredNative = true;
      }
      // Detachment and builder disposal have separate ownership. A failed
      // builder remains retryable even after its native target is retired.
      this.disposeBuilders();
    } catch (error) {
      quarantinedViews.add(this);
      this.retirementFailure = new Error(`Native wrapper retirement failed: ${String(error)}`);
      throw this.retirementFailure;
    }
  }
  private disposeBuilders(): void {
    quarantinedViews.add(this);
    if (this.sdkBusy) {
      this.disposePending = true;
      return;
    }
    let failure: Error | undefined;
    const step = (fn: () => void): void => {
      try {
        fn();
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    };
    step((): void => this.textureController?.clear());
    if (!this.nodeDisposed) {
      step((): void => {
        this.node.dispose();
        this.nodeDisposed = true;
      });
    }
    if (this.textureNode && !this.textureDisposed) {
      step((): void => {
        this.textureNode!.dispose();
        this.textureDisposed = true;
      });
    }
    if (failure) {
      quarantinedViews.add(this);
      this.retirementFailure = failure;
      console.error(`[NSH] wrapper disposal failed: ${String(failure)}`);
      throw failure;
    }
    this.retirementFailure = undefined;
    quarantinedViews.delete(this);
  }
}
export class NativeViews {
  private entries: Map<number, ViewEntry> = new Map();
  private menus: NativeMenuCleanup = new NativeMenuCleanup();
  private retryQuarantined(context: NshRuntimeContext): void {
    for (const view of Array.from(quarantinedViews)) {
      if (!view.belongsTo(context)) {
        continue;
      }
      try {
        view.dispose(context);
      } catch (_) {}
    }
  }
  remove(ids: number[], context: NshRuntimeContext): void {
    this.retryQuarantined(context);
    let failure: Error | undefined;
    for (const id of ids) {
      const entry = this.entries.get(id);
      if (!entry) {
        continue;
      }
      try {
        entry.dispose(context);
        this.entries.delete(id);
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
  update(states: Settings[], context: NshRuntimeContext): void {
    this.retryQuarantined(context);
    let failure: Error | undefined;
    for (const state of states) {
      try {
        if (!context.isActive()) {
          throw new Error("NSH SDK session is inactive");
        }
        let entry = this.entries.get(state.targetId);
        if (entry && entry.isClosed()) {
          this.entries.delete(state.targetId);
          entry = undefined;
        }
        if (entry) {
          entry.update(state);
        } else {
          const created = new ViewEntry(state, context, this.menus);
          created.create();
          this.entries.set(state.targetId, created);
        }
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
  async settled(): Promise<void> {
    await this.menus.settled();
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
    for (const view of Array.from(quarantinedViews)) {
      if (!view.belongsTo(context)) {
        continue;
      }
      try {
        view.dispose(context);
      } catch (error) {
        failure ??= new Error(String(error));
      }
    }
    this.menus.dispose();
    if (failure) {
      throw failure;
    }
  }
}
