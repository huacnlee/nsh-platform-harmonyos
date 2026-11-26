import { BuilderNode, ComponentContent, FrameNode, NodeContent } from "@ohos.arkui.node";
import { FrameCallback, UIContext } from "@ohos.arkui.UIContext";
import deviceInfo from "@ohos.deviceInfo";
import inputMethod from "@ohos.inputMethod";
import { KeyCode } from "@ohos.multimodalInput.keyCode";
import pasteboard from "@ohos.pasteboard";
import { ExtractedPasteData, extractPasteData } from "./NativeClipboard";

export interface EditorSettings {
  belowViewport?: boolean;
  targetId: number;
  multiline: boolean;
  secure: boolean;
  text: string;
  count: number;
  width: number;
  height: number;
  placeholder: string;
  fontSize: number;
  fontWeight: number;
  fontFamily?: string | null;
  italic: boolean;
  color: number;
  placeholderColor: number;
  caretColor: number;
  selectionColor: number;
  editable: boolean;
  autoCorrect: boolean;
  reportPaste: boolean;
  listensPaste: boolean;
  pasteMaxSize?: number | null;
  editActions?: boolean;
  selectTextOnFocus: boolean;
  scrollbarVisible: boolean;
  hitTest: number;
  align: number;
  rtl: boolean;
  capitalize: number;
  keyboard: string;
  enterKey: string;
  blurOnSubmit: boolean;
  maxLength?: number | null;
  autoFill: boolean;
  contentType?: string | null;
  autoFocus: boolean;
  accessibility?: EditorAccessibility | null;
}
interface EditorAccessibility {
  label: string;
  hint: string;
  stateDescription?: string;
  api23?: boolean;
  hidden: boolean;
  accessible?: boolean | null;
  disabled: boolean;
}
interface EditorStateAttribute {
  accessibilityStateDescription(description: string): CommonAttribute;
}
export interface EditorCommand {
  targetId: number;
  kind: string;
  start: number;
  end: number;
  x: number;
  y: number;
  width: number;
  height: number;
  animated: boolean;
  text?: string | null;
  actions?: string[] | null;
}
export interface EditorEventPayload {
  readonly kind: string;
  readonly text: string;
  readonly count: number;
  readonly start: number;
  readonly end: number;
  readonly width: number;
  readonly height: number;
  readonly x: number;
  readonly y: number;
  readonly html?: string | null;
  readonly files?: string | null;
  readonly image?: string | null;
  readonly imageTooLarge?: number | null;
}
export interface EditorReceipt {
  accepted: boolean;
  count: number;
}
export interface EditorKeyEventPayload {
  action: number;
  keyCode: number;
  keyText: string;
  unicode: number;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  cmd: boolean;
  capsLock?: boolean;
  timestamp: number;
}
export interface EditorPlannedEdit {
  text: string;
  clipped: string;
  caret: number;
  changed: boolean;
}
interface KeyEventLockState {
  isCapsLockOn?: boolean;
}
export interface MenuActionPayload {
  targetId: number;
  action: string;
  kind?: string;
  generation?: number;
}
export interface WebDownloadPayload {
  state: string;
  id: string;
  filename: string;
  path: string;
  bytes: number;
  mimeType?: string;
  method?: string;
  requestId?: string;
  ok?: boolean;
  errorCode?: string;
}
export interface WebEventPayload {
  targetId: number;
  phase: string;
  url: string;
  title: string;
  loading: boolean;
  canGoBack: boolean;
  canGoForward: boolean;
  message?: string;
  code?: number;
  download?: WebDownloadPayload;
  x?: number;
  y?: number;
}
export interface NshRuntimeContext {
  isActive(): boolean;
  getUIContext(): UIContext;
  getUIContextFor?(componentId: string): UIContext;
  getWindowUIContext?(windowId: number): UIContext | undefined;
  requestFocus?(componentId: string): boolean;
  colorMode?(): ThemeColorMode;
  attachNativeNode(targetId: number, content: FrameNode | null): boolean;
  attachNativeSubtree(targetId: number, content: FrameNode | null, slot: NodeContent): boolean;
  retireNativeNode(targetId: number): boolean;
  editorEvent(targetId: number, event: EditorEventPayload): EditorReceipt;
  editorKeyEvent?(targetId: number, event: EditorKeyEventPayload): boolean;
  editorPlanEdit?(
    targetId: number,
    current: string,
    start: number,
    end: number,
    inserted: string,
    normalizePaste: boolean,
  ): EditorPlannedEdit;
  editorPlanReplace?(
    targetId: number,
    previous: string,
    candidate: string,
  ): EditorPlannedEdit;
  viewAction(action: MenuActionPayload): boolean;
  webControllerAttached(targetId: number, nativeTag: string): boolean;
  webNavigate(targetId: number, url: string): boolean;
  webDownloadAllowed(targetId: number, url: string): boolean;
  webEvent(event: WebEventPayload): boolean;
}
const inputTypes: Map<string, InputType> = new Map([
  ["normal", InputType.Normal],
  ["number", InputType.Number],
  ["decimal", InputType.NUMBER_DECIMAL],
  ["phone", InputType.PhoneNumber],
  ["email", InputType.Email],
  ["password", InputType.Password],
  ["number-password", InputType.NUMBER_PASSWORD],
]);
const areaTypes: Map<string, TextAreaType> = new Map([
  ["normal", TextAreaType.NORMAL],
  ["number", TextAreaType.NUMBER],
  ["decimal", TextAreaType.NUMBER_DECIMAL],
  ["phone", TextAreaType.PHONE_NUMBER],
  ["email", TextAreaType.EMAIL],
]);
const enterTypes: Map<string, EnterKeyType> = new Map([
  ["go", EnterKeyType.Go],
  ["next", EnterKeyType.Next],
  ["search", EnterKeyType.Search],
  ["send", EnterKeyType.Send],
  ["newline", EnterKeyType.NEW_LINE],
  ["done", EnterKeyType.Done],
]);
const contentTypes: Map<string, ContentType> = new Map([
  ["username", ContentType.USER_NAME],
  ["password", ContentType.PASSWORD],
  ["new-password", ContentType.NEW_PASSWORD],
  ["email", ContentType.EMAIL_ADDRESS],
  ["tel", ContentType.FULL_PHONE_NUMBER],
  ["name", ContentType.PERSON_FULL_NAME],
  ["given-name", ContentType.PERSON_FIRST_NAME],
  ["family-name", ContentType.PERSON_LAST_NAME],
  ["nickname", ContentType.NICKNAME],
  ["street-address", ContentType.FULL_STREET_ADDRESS],
  ["address-line2", ContentType.DETAIL_INFO_WITHOUT_STREET],
  ["city", ContentType.CITY_ADDRESS],
  ["region", ContentType.PROVINCE_ADDRESS],
  ["country", ContentType.COUNTRY_ADDRESS],
  ["credit-card", ContentType.BANK_CARD_NUMBER],
]);
// ArkUI reads a number without an alpha byte (`0x00RRGGBB`) as opaque, so a
// transparent color (`TimeField`'s and `OtpInput`'s hidden text) would show:
// a `#AARRGGBB` string keeps its alpha.
function argbColor(value: number): string {
  return "#" + (value >>> 0).toString(16).padStart(8, "0");
}

// The keyboards whose `InputType` keeps only digits of the field's text.
function digitsOnly(keyboard: string): boolean {
  return keyboard === "number" || keyboard === "number-password";
}

function inputType(value: string): InputType {
  const result = inputTypes.get(value);
  if (result === undefined) {
    throw new Error("Invalid editor keyboard");
  }
  return result;
}
function areaType(value: string): TextAreaType {
  const result = areaTypes.get(value);
  if (result === undefined) {
    throw new Error("Invalid multiline editor keyboard");
  }
  return result;
}
function enterType(value: string): EnterKeyType {
  const result = enterTypes.get(value);
  if (result === undefined) {
    throw new Error("Invalid editor return key");
  }
  return result;
}
function contentType(value?: string | null): ContentType | undefined {
  if (value == null) {
    return undefined;
  }
  const result = contentTypes.get(value);
  if (result === undefined) {
    throw new Error("Invalid editor autofill type");
  }
  return result;
}

const SINK_PAD_LEN: number = 64;
const SINK_PAD_MIN: number = 32;
const SINK_PAD: string = "\u200b".repeat(SINK_PAD_LEN);
const SINK_BYTE_LIMIT: number = 256;

function isSinkSettings(s: EditorSettings): boolean {
  return s.multiline && s.reportPaste && s.hitTest === 3 && s.color === 0 && s.caretColor === 0;
}

function isImeCursorSinkSettings(s: EditorSettings): boolean {
  return !s.secure && s.reportPaste && s.hitTest === 3 && s.color === 0 && s.caretColor === 0;
}

function utf8ByteLength(text: string): number {
  let bytes = 0;
  for (const ch of Array.from(text)) {
    const code = ch.codePointAt(0) ?? 0;
    bytes += code <= 0x7f ? 1 : code <= 0x7ff ? 2 : code <= 0xffff ? 3 : 4;
  }
  return bytes;
}

class EditorEvent implements EditorEventPayload {
  readonly kind: string;
  readonly text: string;
  readonly count: number;
  readonly start: number;
  readonly end: number;
  readonly width: number;
  readonly height: number;
  readonly x: number;
  readonly y: number;
  readonly html?: string | null;
  readonly files?: string | null;
  readonly image?: string | null;
  readonly imageTooLarge?: number | null;
  constructor(
    entry: EditorEntry,
    kind: string,
    text: string,
    start = 0,
    end = 0,
    width = 0,
    height = 0,
    x = 0,
    y = 0,
    html?: string | null,
    files?: string | null,
    image?: string | null,
    imageTooLarge?: number | null,
  ) {
    this.kind = kind;
    this.text = text;
    this.count = entry.count;
    this.start = start;
    this.end = end;
    this.width = width;
    this.height = height;
    this.x = x;
    this.y = y;
    this.html = html;
    this.files = files;
    this.image = image;
    this.imageTooLarge = imageTooLarge;
  }
}

class EditorModifier implements AttributeModifier<TextInputAttribute | TextAreaAttribute> {
  private entry: EditorEntry;
  constructor(entry: EditorEntry) {
    this.entry = entry;
  }
  applyNormalAttribute(node: TextInputAttribute | TextAreaAttribute): void {
    const s = this.entry.settings;
    // `nsh-core` already scales `fontSize` by `font_scale` (honoring `allowFontScaling`
    // and `maxFontSizeMultiplier`), so pass `vp` rather than ArkUI's default `fp` to
    // avoid double-scaling when `fontSizeScale: "followSystem"` is active.
    const fontSize = `${s.fontSize}vp`;
    const fontStyle = s.italic ? FontStyle.Italic : FontStyle.Normal;
    const placeholderFont: Font = {
      size: fontSize,
      weight: s.fontWeight,
      style: fontStyle,
    };
    if (s.fontFamily) {
      placeholderFont.family = s.fontFamily;
    }
    node
      .width(s.width)
      .height(this.entry.editorHeight())
      .padding(0)
      .borderWidth(0)
      // The outer NSH node owns corners; theme rounding would clip this content box.
      .borderRadius(0)
      .backgroundColor(Color.Transparent)
      .fontSize(fontSize)
      .fontWeight(s.fontWeight)
      .fontStyle(fontStyle)
      .fontColor(argbColor(s.color))
      .placeholderFont(placeholderFont)
      .placeholderColor(argbColor(s.placeholderColor))
      .caretColor(argbColor(s.caretColor))
      .selectedBackgroundColor(argbColor(s.selectionColor))
      .enabled(s.accessibility?.disabled !== true)
      .enableKeyboardOnFocus(s.editable)
      .enablePreviewText(s.editable)
      .barState(BarState.Off)
      .accessibilityText(s.accessibility?.label ?? "")
      .accessibilityDescription(
        s.accessibility?.api23 === true &&
          deviceInfo.sdkApiVersion >= 23 &&
          typeof (node as EditorStateAttribute).accessibilityStateDescription === "function"
          ? s.accessibility?.hint ?? ""
          : [s.accessibility?.hint ?? "", s.accessibility?.stateDescription ?? ""]
              .filter((part: string): boolean => part.length > 0)
              .join(", "),
      )
      .accessibilityLevel(
        s.accessibility?.hidden === true
          ? "no-hide-descendants"
          : s.accessibility?.accessible === false
            ? "no"
            : "yes",
      )
      .textAlign(s.align as TextAlign)
      .direction(s.rtl ? Direction.Rtl : Direction.Ltr)
      .autoCapitalizationMode(s.capitalize as AutoCapitalizationMode)
      .enterKeyType(enterType(s.enterKey))
      // Enforce maxLength via Rust Unicode-scalar + grapheme-boundary logic
      // (`willChange` / `insertText`) instead of ArkUI's UTF-16 code-unit limit.
      .maxLength(2147483647)
      .enableAutoFill(s.autoFill)
      .contentType(contentType(s.contentType))
      .hitTestBehavior(s.hitTest as HitTestMode);
    if (
      s.accessibility?.api23 === true &&
      deviceInfo.sdkApiVersion >= 23 &&
      typeof (node as EditorStateAttribute).accessibilityStateDescription === "function"
    ) {
      (node as EditorStateAttribute).accessibilityStateDescription(
        s.accessibility?.stateDescription ?? "",
      );
    }
    if (s.fontFamily) {
      node.fontFamily(s.fontFamily);
    }
    if (s.multiline && !s.secure) {
      (node as TextAreaAttribute).type(areaType(s.keyboard));
    } else {
      (node as TextInputAttribute)
        .type(inputType(s.keyboard))
        .showPasswordIcon(false)
        .showUnderline(false);
    }
  }
}

class EditorSystemMenu implements EditMenuOptions {
  private readonly entry: EditorEntry;
  constructor(entry: EditorEntry) {
    this.entry = entry;
  }
  onCreateMenu(menuItems: Array<TextMenuItem>): Array<TextMenuItem> {
    return menuItems.filter(
      (item: TextMenuItem): boolean =>
        item.id.equals(TextMenuItemId.CUT) ||
        item.id.equals(TextMenuItemId.COPY) ||
        item.id.equals(TextMenuItemId.PASTE) ||
        item.id.equals(TextMenuItemId.SELECT_ALL),
    );
  }
  onMenuItemClick(_menuItem: TextMenuItem, _range: TextRange): boolean {
    // The system performs Cut, Copy, Paste and Select All natively; the
    // native input reports the result through onChange/onPaste.
    return false;
  }
}

interface EditorContentParams {
  entry: EditorEntry;
}

@Builder
function editorContent(params: EditorContentParams) {
  if (params.entry.settings.multiline && !params.entry.settings.secure) {
    Scroll(params.entry.scroller) {
      TextArea({
        text: params.entry.text,
        placeholder: params.entry.settings.placeholder,
        controller: params.entry.area,
      })
        .id(`nsh-editor-${params.entry.settings.targetId}`)
        .attributeModifier(new EditorModifier(params.entry))
        .editMenuOptions(new EditorSystemMenu(params.entry))
        .onChange((text: string, preview?: PreviewText) => params.entry.change(text, preview))
        .onFocus(() => params.entry.emit("focus"))
        .onBlur(() => params.entry.emit("blur"))
        .onSubmit((_key: EnterKeyType, event?: SubmitEvent) => params.entry.submit(event))
        .onTextSelectionChange((start: number, end: number) => params.entry.selection(start, end))
        .onContentScroll(() => params.entry.scrolled())
        .onPaste((text: string, event: PasteEvent) => params.entry.paste(text, event))
        .onWillChange((info: EditableTextChangeValue): boolean => params.entry.willChange(info))
        .onWillInsert((value: InsertValue): boolean => params.entry.willInsert(value))
        .onWillDelete((): boolean => {
          if (!params.entry.canEdit()) return false;
          if (!params.entry.isSink() || params.entry.text.length === 0) {
            params.entry.editKey("Backspace");
          }
          return true;
        })
        .onMouse((event: MouseEvent): void => params.entry.handleMouse(event))
        .onTouch((event: TouchEvent): void => params.entry.handleTouch(event))
        .onAppear(() => params.entry.appeared())
    }
    .width(params.entry.settings.width)
    .height(params.entry.settings.height)
    .scrollable(ScrollDirection.Vertical)
    .scrollBar(params.entry.settings.scrollbarVisible ? BarState.Auto : BarState.Off)
    .clip(true)
    .hitTestBehavior(params.entry.settings.hitTest as HitTestMode)
    .onKeyEventDispatch((event: KeyEvent): boolean => params.entry.dispatchKeyEvent(event))
    .onDidScroll(() => params.entry.scrolled());
  } else {
    Stack() {
      TextInput({
        text: params.entry.text,
        placeholder: params.entry.settings.placeholder,
        controller: params.entry.line,
      })
        .id(`nsh-editor-${params.entry.settings.targetId}`)
        .attributeModifier(new EditorModifier(params.entry))
        .editMenuOptions(new EditorSystemMenu(params.entry))
        .onChange((text: string, preview?: PreviewText) => params.entry.change(text, preview))
        .onFocus(() => params.entry.emit("focus"))
        .onBlur(() => params.entry.emit("blur"))
        .onSubmit((_key: EnterKeyType, event: SubmitEvent) => params.entry.submit(event))
        .onTextSelectionChange((start: number, end: number) => params.entry.selection(start, end))
        .onContentScroll(() => params.entry.scrolled())
        .onPaste((text: string, event: PasteEvent) => params.entry.paste(text, event))
        .onWillChange((info: EditableTextChangeValue): boolean => params.entry.willChange(info))
        .onWillInsert((value: InsertValue): boolean => params.entry.willInsert(value))
        .onWillDelete((): boolean => {
          if (!params.entry.canEdit()) return false;
          params.entry.editKey("Backspace");
          return true;
        })
        .onMouse((event: MouseEvent): void => params.entry.handleMouse(event))
        .onTouch((event: TouchEvent): void => params.entry.handleTouch(event))
        .onAppear(() => params.entry.appeared());
    }
    .width(params.entry.settings.width)
    .height(params.entry.settings.height)
    .hitTestBehavior(params.entry.settings.hitTest as HitTestMode)
    .onKeyEventDispatch((event: KeyEvent): boolean => params.entry.dispatchKeyEvent(event));
  }
}

// Failed retirement cannot release a BuilderNode still borrowed by native code.
const quarantinedEditors: Set<EditorEntry> = new Set();
// Track the active editor per NshRuntimeContext so separate windows/surfaces never blur each other.
const activeEditorsByContext: Map<NshRuntimeContext, EditorEntry> = new Map();

export function syncActiveEditorImeCursor(): void {
  for (const entry of activeEditorsByContext.values()) {
    entry.refreshImeCursor();
  }
}

interface EditMenuParams {
  entry: EditorEntry;
  x: number;
  y: number;
  width: number;
  height: number;
  actions: string[];
}

function modifierPressed(event: KeyEvent, key: string): boolean {
  try {
    return event.getModifierKeyState ? event.getModifierKeyState([key]) : false;
  } catch {
    return false;
  }
}

// The system's own text selection menu labels, so every language the platform
// supports is covered and the wording matches the menu a native text view shows.
function editActionLabel(action: string): ResourceStr {
  switch (action) {
    case "cut":
      return $r("sys.string.general_cut");
    case "copy":
      return $r("sys.string.general_copy");
    case "paste":
      return $r("sys.string.general_paste");
    case "selectAll":
      return $r("sys.string.general_select_all");
    default:
      return action;
  }
}

@Component
struct EditMenuView {
  entry!: EditorEntry;
  @Prop x: number = 0;
  @Prop y: number = 0;
  @Prop selWidth: number = 0;
  @Prop selHeight: number = 0;
  @Prop actions: string[] = [];
  @State private menuW: number = 0;
  @State private menuH: number = 36;
  @State private overlayOriginX: number = 0;
  @State private overlayOriginY: number = 0;

  private posX(): number {
    const centerX = this.x + this.selWidth * 0.5;
    if (this.menuW <= 0) {
      return Math.max(8, centerX - 80 - this.overlayOriginX);
    }
    let winW = 360;
    try {
      let cur = this.entry.node.getFrameNode();
      while (cur?.getParent()) {
        cur = cur.getParent();
      }
      const size = cur?.getMeasuredSize();
      if (size && size.width > 0) {
        winW = this.getUIContext().px2vp(size.width);
      }
    } catch {}
    const left = Math.max(8, Math.min(winW - this.menuW - 8, centerX - this.menuW * 0.5));
    return left - this.overlayOriginX;
  }

  private posY(): number {
    const above = this.y - this.menuH - 10;
    const targetY = above >= 48 ? above : this.y + Math.max(this.selHeight, 20) + 26;
    return targetY - this.overlayOriginY;
  }

  build() {
    WithTheme({ colorMode: this.entry.colorMode() }) {
      Row() {
        ForEach(
          this.actions,
          (action: string, index: number) => {
            if (index > 0) {
              Divider()
                .vertical(true)
                .height(16)
                .color($r("sys.color.comp_divider"))
                .strokeWidth(0.5);
            }
            if (action === "paste") {
              PasteButton({
                text: PasteDescription.PASTE,
                buttonType: ButtonType.Capsule,
              })
                .fontSize(14)
                .fontColor($r("sys.color.font_primary"))
                .iconColor($r("sys.color.font_primary"))
                .fontWeight(FontWeight.Regular)
                .backgroundColor($r("sys.color.comp_background_primary"))
                .padding({ left: 12, right: 12, top: 8, bottom: 8 })
                .onClick((_event: ClickEvent, result: PasteButtonOnClickResult): void => {
                  if (result === PasteButtonOnClickResult.SUCCESS) {
                    this.entry.chooseEditAction("paste");
                  }
                });
            } else {
              Text(editActionLabel(action))
                .fontSize(14)
                .fontColor($r("sys.color.font_primary"))
                .padding({ left: 12, right: 12, top: 8, bottom: 8 })
                .focusable(false)
                .onClick((): void => {
                  this.entry.chooseEditAction(action);
                });
            }
          },
          (action: string): string => action,
        )
      }
      .backgroundColor($r("sys.color.comp_background_primary"))
      .borderRadius(18)
      .border({
        width: 0.5,
        color: $r("sys.color.comp_divider"),
      })
      .shadow(ShadowStyle.OUTER_DEFAULT_MD)
      .focusable(false)
      .onTouch((event: TouchEvent): void => {
        if (event.type === TouchType.Down) {
          this.entry.markMenuInteraction();
        } else if (event.type === TouchType.Cancel) {
          this.entry.clearMenuInteraction();
        } else if (event.type === TouchType.Up) {
          this.entry.scheduleClearMenuInteraction();
        }
      })
      .onAreaChange((_old: Area, area: Area): void => {
        const w = Number(area.width);
        const h = Number(area.height);
        const gx = Number(area.globalPosition.x);
        const gy = Number(area.globalPosition.y);
        const lx = Number(area.position.x);
        const ly = Number(area.position.y);
        if (w > 0) {
          this.menuW = w;
        }
        if (h > 0) {
          this.menuH = h;
        }
        if (Number.isFinite(gx) && Number.isFinite(lx)) {
          this.overlayOriginX = gx - lx;
        }
        if (Number.isFinite(gy) && Number.isFinite(ly)) {
          this.overlayOriginY = gy - ly;
        }
      })
      .position({ x: this.posX(), y: this.posY() })
    };
  }
}

@Builder
function editMenuBuilder(params: EditMenuParams) {
  EditMenuView({
    entry: params.entry,
    x: params.x,
    y: params.y,
    selWidth: params.width,
    selHeight: params.height,
    actions: params.actions,
  });
}

class EditorSizeCallback extends FrameCallback {
  private entry: EditorEntry;
  constructor(entry: EditorEntry) {
    super();
    this.entry = entry;
  }
  onIdle(_timeLeft: number): void {
    this.entry.measureSize();
  }
}
class EditorSelectAllCallback extends FrameCallback {
  private entry: EditorEntry;
  private gen: number;
  constructor(entry: EditorEntry, gen: number) {
    super();
    this.entry = entry;
    this.gen = gen;
  }
  onIdle(_timeLeft: number): void {
    this.entry.applySelectAllOnFocus(this.gen);
  }
}
class EditorEntry {
  readonly node: BuilderNode<[EditorContentParams]>;
  readonly scroller: Scroller = new Scroller();
  readonly line: TextInputController = new TextInputController();
  readonly area: TextAreaController = new TextAreaController();
  settings: EditorSettings;
  text: string;
  count: number;
  private updating: boolean = false;
  private closed: boolean = false;
  private retired: boolean = false;
  private disposed: boolean = false;
  private sdkBusy: boolean = false;
  private attached: boolean = false;
  private appearedPending: boolean = false;
  private disposePending: boolean = false;
  private focusedOnce: boolean = false;
  private focusRequested: boolean = false;
  private context: NshRuntimeContext;
  private sizePending: boolean = false;
  private contentWidth: number = -1;
  private contentHeight: number = -1;
  private focused: boolean = false;
  private preview: string = "";
  private previewStart: number = 0;
  private revealCaret: boolean = false;
  private caretIndex: number = -1;
  private caretX: number = 0;
  private caretY: number = 0;
  private caretHeight: number = 0;
  private selStart: number = 0;
  private selEnd: number = 0;
  private pendingScroll?: EditorCommand;
  private editMenuContent?: ComponentContent<EditMenuParams>;
  private editMenuUi?: UIContext;
  private menuInteracting: boolean = false;
  private menuInteractTimer: number = -1;
  private undoStack: string[] = [];
  private redoStack: string[] = [];
  private imeCursorKey: string = "";
  private consumedPasteKey: boolean = false;
  private rustText: string = "";
  private rustEmptied: string = "";
  private focusGen: number = 0;
  private userContextVersion: number = 0;
  private applyingPaste: boolean = false;
  private pasteQueue: Promise<void> = Promise.resolve();
  private pointerFocusing: boolean = false;
  private pointerActive: boolean = false;
  private pointerMoved: boolean = false;
  private pointerSuppressSelect: boolean = false;
  private pointerDownX: number = 0;
  private pointerDownY: number = 0;
  private lastClickTime: number = 0;
  private lastClickX: number = 0;
  private lastClickY: number = 0;
  private pendingSelectAllGen: number = 0;
  private explicitSelectionAfterFocusGen: number = 0;
  constructor(settings: EditorSettings, context: NshRuntimeContext) {
    this.context = context;
    this.settings = settings;
    this.rustText = settings.text;
    this.text = isSinkSettings(settings) && settings.text.length === 0 ? SINK_PAD : settings.text;
    this.count = settings.count;
    this.selStart = this.text.length;
    this.selEnd = this.text.length;
    this.node = new BuilderNode<[EditorContentParams]>(context.getUIContext());
  }
  isSink(): boolean {
    return isSinkSettings(this.settings);
  }
  isImeCursorSink(): boolean {
    return isImeCursorSinkSettings(this.settings);
  }
  private editorUIContext(): UIContext {
    const id = `nsh-editor-${this.settings.targetId}`;
    return this.context.getUIContextFor ? this.context.getUIContextFor(id) : this.context.getUIContext();
  }
  refreshImeCursor(): void {
    if (this.closed || !this.focused || !this.isImeCursorSink() || !this.context.isActive()) {
      return;
    }
    this.imeCursorKey = "";
    this.scheduleSize();
  }
  private pinSinkCaret(): void {
    const end = this.text.length;
    this.selStart = end;
    this.selEnd = end;
    try {
      this.area.setTextSelection(end, end);
    } catch {}
  }
  create(): void {
    this.updating = true;
    this.sdkBusy = true;
    try {
      this.node.build(wrapBuilder(editorContent), { entry: this });
      if (this.closed || !this.context.isActive()) {
        throw new Error("Editor creation was cancelled");
      }
      const accepted = this.context.attachNativeNode(
        this.settings.targetId,
        this.node.getFrameNode(),
      );
      if (!accepted || this.closed) {
        throw new Error("Native editor target is no longer live");
      }
      this.attached = true;
    } catch (error) {
      this.dispose();
      throw new Error(String(error));
    } finally {
      this.updating = false;
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.release();
      }
      this.autoFocus();
    }
  }
  update(settings: EditorSettings): void {
    if (this.closed) {
      throw new Error("Native editor is retired");
    }
    if (
      settings.targetId !== this.settings.targetId ||
      settings.multiline !== this.settings.multiline
    ) {
      throw new Error("Editor replacement requires a new generation");
    }
    const sink = isSinkSettings(settings);
    if (settings.count === this.count) {
      if (sink) {
        if (settings.text.length === 0) {
          if (this.rustText.length > 0) {
            this.rustEmptied = this.rustText;
            this.rustText = "";
          }
          if (this.preview.length === 0 && this.text.length < SINK_PAD_MIN) {
            this.text = SINK_PAD;
          }
        } else {
          this.rustText = settings.text;
          this.rustEmptied = "";
        }
      } else {
        const prevLen = this.text.length;
        if (this.text !== settings.text) {
          this.userContextVersion += 1;
        }
        this.text = settings.text;
        if (this.selStart >= prevLen && this.selEnd >= prevLen) {
          this.selStart = this.text.length;
          this.selEnd = this.text.length;
        } else {
          this.selStart = Math.min(this.selStart, this.text.length);
          this.selEnd = Math.min(Math.max(this.selStart, this.selEnd), this.text.length);
        }
      }
    }
    this.settings = settings;
    if (this.isSink() && this.preview.length === 0 && this.text.length === 0) {
      this.text = SINK_PAD;
    }
    this.refresh();
  }
  private refresh(): void {
    this.updating = true;
    this.sdkBusy = true;
    try {
      this.node.update({ entry: this });
      if (this.isSink() && this.focused && this.preview.length === 0) {
        this.pinSinkCaret();
      }
    } finally {
      this.updating = false;
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.release();
      }
      this.autoFocus();
    }
    this.scheduleSize();
  }
  // Both soft and hardware editing use these SDK callbacks. The native key
  // event path handles shortcuts/KeyDown independently and never emits KeyPress.
  editKey(key: string): void {
    for (const character of key === "Backspace" || key === "Enter" ? [key] : Array.from(key)) {
      this.emit("key", character === "\n" ? "Enter" : character);
    }
  }
  canEdit(): boolean {
    return (
      this.updating || (this.settings.editable && this.settings.accessibility?.disabled !== true)
    );
  }
  private planEdit(
    start: number,
    end: number,
    inserted: string,
    normalizePaste: boolean,
  ): EditorPlannedEdit {
    if (this.context.editorPlanEdit) {
      return this.context.editorPlanEdit(
        this.settings.targetId,
        this.text,
        start,
        end,
        inserted,
        normalizePaste,
      );
    }
    const clampedStart = Math.max(0, Math.min(this.text.length, start));
    const clampedEnd = Math.max(clampedStart, Math.min(this.text.length, end));
    let normalized = inserted;
    if (normalizePaste) {
      normalized = normalized.replace(/\r\n?/g, "\n");
      if (!(this.settings.multiline && !this.settings.secure)) {
        normalized = normalized.replace(/\n/g, " ");
      }
    }
    if (digitsOnly(this.settings.keyboard)) {
      normalized = normalized.replace(/\D/g, "");
    }
    const prefix = this.text.slice(0, clampedStart);
    const suffix = this.text.slice(clampedEnd);
    const maxLen = this.settings.maxLength;
    let clipped = normalized;
    if (maxLen != null) {
      const existing = Array.from(prefix).length + Array.from(suffix).length;
      const room = Math.max(0, maxLen - existing);
      clipped = Array.from(normalized).slice(0, room).join("");
    }
    const next = prefix + clipped + suffix;
    const caret = clampedStart + clipped.length;
    return { text: next, clipped, caret, changed: next !== this.text };
  }
  private planReplace(previous: string, candidate: string): EditorPlannedEdit {
    if (this.context.editorPlanReplace) {
      return this.context.editorPlanReplace(this.settings.targetId, previous, candidate);
    }
    const maxLen = this.settings.maxLength;
    if (maxLen == null || Array.from(candidate).length <= maxLen) {
      return {
        text: candidate,
        clipped: candidate,
        caret: candidate.length,
        changed: candidate !== previous,
      };
    }
    const trimmed = Array.from(candidate).slice(0, maxLen).join("");
    return {
      text: trimmed,
      clipped: trimmed,
      caret: trimmed.length,
      changed: trimmed !== previous,
    };
  }
  willChange(info: EditableTextChangeValue): boolean {
    if (!this.canEdit()) {
      return false;
    }
    if (this.updating || this.isSink()) {
      return true;
    }
    // Preserve IME pre-edit composition; enforce maxLength once committed.
    if (info.previewText && info.previewText.value.length > 0) {
      return true;
    }
    if (this.settings.maxLength == null) {
      return true;
    }
    const previous = this.text;
    const planned = this.planReplace(previous, info.content);
    if (planned.text === info.content) {
      return true;
    }
    if (planned.changed) {
      const expectedVersion = this.userContextVersion;
      const expectedFocusGen = this.focusGen;
      Promise.resolve().then((): void => {
        if (
          this.closed ||
          !this.context.isActive() ||
          this.text !== previous ||
          this.userContextVersion !== expectedVersion ||
          this.focusGen !== expectedFocusGen
        ) {
          return;
        }
        this.pushUndoState(this.text);
        this.applyProgrammaticText(planned.text, planned.caret);
      });
    }
    return false;
  }
  willInsert(value: InsertValue): boolean {
    if (!this.canEdit()) {
      return false;
    }
    if (!this.isSink()) {
      if (this.preview.length === 0 && this.settings.maxLength != null) {
        const start =
          this.selEnd > this.selStart
            ? Math.min(this.text.length, this.selStart)
            : Math.max(0, Math.min(this.text.length, value.insertOffset));
        const end =
          this.selEnd > this.selStart ? Math.min(this.text.length, this.selEnd) : start;
        const planned = this.planEdit(start, end, value.insertValue, false);
        if (planned.clipped.length === 0) {
          return true;
        }
      }
      this.editKey(value.insertValue);
    }
    return true;
  }
  handleMouse(event: MouseEvent): void {
    if (event.button !== MouseButton.Left || this.isSink()) {
      return;
    }
    if (event.action === MouseAction.Press) {
      const now = Date.now();
      const dx = event.x - this.lastClickX;
      const dy = event.y - this.lastClickY;
      const isDoubleClick = now - this.lastClickTime <= 500 && dx * dx + dy * dy <= 144;
      this.lastClickTime = now;
      this.lastClickX = event.x;
      this.lastClickY = event.y;
      let shift = false;
      try {
        shift = event.getModifierKeyState ? event.getModifierKeyState(["Shift"]) : false;
      } catch {}
      if (!this.focused && this.settings.selectTextOnFocus) {
        this.pointerFocusing = true;
        this.pointerActive = true;
        this.pointerMoved = false;
        this.pointerSuppressSelect = shift || isDoubleClick;
        this.pointerDownX = event.x;
        this.pointerDownY = event.y;
      }
    } else if (event.action === MouseAction.Move) {
      if (this.pointerActive && !this.pointerMoved) {
        const dx = event.x - this.pointerDownX;
        const dy = event.y - this.pointerDownY;
        if (dx * dx + dy * dy > 16) {
          this.pointerMoved = true;
          this.pointerSuppressSelect = true;
          this.pendingSelectAllGen = 0;
        }
      }
    } else if (event.action === MouseAction.Release) {
      if (this.pointerActive) {
        this.pointerActive = false;
        const shouldSelect =
          this.pointerFocusing && !this.pointerSuppressSelect && !this.pointerMoved;
        this.pointerFocusing = false;
        if (
          shouldSelect &&
          this.focused &&
          this.pendingSelectAllGen === this.focusGen &&
          this.explicitSelectionAfterFocusGen !== this.focusGen
        ) {
          this.scheduleSelectAllOnFocus(this.focusGen);
        } else {
          this.pendingSelectAllGen = 0;
        }
      }
    }
  }
  handleTouch(event: TouchEvent): void {
    // Mouse clicks already go through `handleMouse` (with Shift & button info).
    if (event.source === SourceType.Mouse || this.isSink()) {
      return;
    }
    const touch = event.touches.length > 0 ? event.touches[0] : undefined;
    if (event.type === TouchType.Down) {
      if (touch && event.touches.length === 1) {
        const now = Date.now();
        const dx = touch.x - this.lastClickX;
        const dy = touch.y - this.lastClickY;
        const isDoubleTap = now - this.lastClickTime <= 500 && dx * dx + dy * dy <= 576;
        this.lastClickTime = now;
        this.lastClickX = touch.x;
        this.lastClickY = touch.y;
        if (!this.focused && this.settings.selectTextOnFocus) {
          this.pointerFocusing = true;
          this.pointerActive = true;
          this.pointerMoved = false;
          this.pointerSuppressSelect = isDoubleTap;
          this.pointerDownX = touch.x;
          this.pointerDownY = touch.y;
        }
      }
    } else if (event.type === TouchType.Move) {
      if (this.pointerActive && !this.pointerMoved && touch) {
        const dx = touch.x - this.pointerDownX;
        const dy = touch.y - this.pointerDownY;
        if (dx * dx + dy * dy > 64) {
          this.pointerMoved = true;
          this.pointerSuppressSelect = true;
          this.pendingSelectAllGen = 0;
        }
      }
    } else if (event.type === TouchType.Up) {
      if (this.pointerActive) {
        this.pointerActive = false;
        const shouldSelect =
          this.pointerFocusing && !this.pointerSuppressSelect && !this.pointerMoved;
        this.pointerFocusing = false;
        if (
          shouldSelect &&
          this.focused &&
          this.pendingSelectAllGen === this.focusGen &&
          this.explicitSelectionAfterFocusGen !== this.focusGen
        ) {
          this.scheduleSelectAllOnFocus(this.focusGen);
        } else {
          this.pendingSelectAllGen = 0;
        }
      }
    } else if (event.type === TouchType.Cancel) {
      this.pointerActive = false;
      this.pointerFocusing = false;
      this.pointerMoved = false;
      this.pointerSuppressSelect = false;
      this.pendingSelectAllGen = 0;
    }
  }
  private scheduleSelectAllOnFocus(gen: number): void {
    if (this.closed || !this.context.isActive()) {
      return;
    }
    try {
      this.editorUIContext().postFrameCallback(new EditorSelectAllCallback(this, gen));
    } catch {
      setTimeout((): void => {
        this.applySelectAllOnFocus(gen);
      }, 0);
    }
  }
  applySelectAllOnFocus(gen: number): void {
    if (
      this.closed ||
      !this.context.isActive() ||
      !this.focused ||
      this.focusGen !== gen ||
      !this.settings.selectTextOnFocus ||
      this.isSink() ||
      this.pointerActive ||
      this.pointerSuppressSelect ||
      this.explicitSelectionAfterFocusGen === gen ||
      this.text.length === 0
    ) {
      return;
    }
    const end = this.text.length;
    this.selStart = 0;
    this.selEnd = end;
    this.updating = true;
    try {
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(0, end);
      } else {
        this.line.setTextSelection(0, end);
      }
    } catch {
    } finally {
      this.updating = false;
    }
    this.selection(0, end);
  }
  colorMode(): ThemeColorMode {
    return this.context.colorMode ? this.context.colorMode() : ThemeColorMode.SYSTEM;
  }
  dispatchKeyEvent(event: KeyEvent): boolean {
    if (this.keyEvent(event)) {
      return true;
    }
    if (this.canEdit() && this.focused && this.preview.length === 0) {
      if (
        event.type === KeyType.Down &&
        event.keyCode === KeyCode.KEYCODE_V &&
        modifierPressed(event, "Ctrl") &&
        !modifierPressed(event, "Alt") &&
        !modifierPressed(event, "Shift") &&
        event.metaKey !== 1
      ) {
        this.consumedPasteKey = true;
        this.pasteFromShortcut();
        return true;
      }
      if (event.type === KeyType.Up && event.keyCode === KeyCode.KEYCODE_V && this.consumedPasteKey) {
        this.consumedPasteKey = false;
        return true;
      }
    }
    try {
      return this.editorUIContext().dispatchKeyEvent(`nsh-editor-${this.settings.targetId}`, event);
    } catch {
      return false;
    }
  }
  private finishPaste(
    text: string,
    html?: string | null,
    files?: string | null,
    image?: string | null,
    tooLarge?: number | null,
  ): void {
    this.applyingPaste = true;
    try {
      this.emit("paste", text, 0, 0, 0, 0, 0, 0, html, files, image, tooLarge);
      if (!this.settings.reportPaste && this.canEdit() && text.length > 0) {
        this.insertText(text, true);
      }
    } finally {
      this.applyingPaste = false;
    }
  }
  private enqueuePaste(
    dataPromise: Promise<pasteboard.PasteData>,
    fallbackPlain: string | undefined,
    fallbackEditAction: boolean,
  ): void {
    const expectedFocusGen = this.focusGen;
    const expectedUserVersion = this.userContextVersion;
    const stillValid = (): boolean =>
      !this.closed &&
      this.context.isActive() &&
      this.focused &&
      this.focusGen === expectedFocusGen &&
      this.userContextVersion === expectedUserVersion;
    this.pasteQueue = this.pasteQueue
      .then(async (): Promise<void> => {
        let data: pasteboard.PasteData | undefined = undefined;
        try {
          data = await dataPromise;
        } catch {
          data = undefined;
        }
        if (!stillValid()) {
          return;
        }
        if (!data || data.getRecordCount() === 0) {
          if (fallbackPlain !== undefined) {
            this.finishPaste(fallbackPlain);
          } else if (fallbackEditAction) {
            this.emit("edit-action", "paste");
          }
          return;
        }
        let rich: ExtractedPasteData | undefined = undefined;
        try {
          rich = await extractPasteData(data, this.settings.pasteMaxSize ?? undefined);
        } catch {
          rich = undefined;
        }
        if (!stillValid()) {
          return;
        }
        if (!rich) {
          if (fallbackPlain !== undefined) {
            this.finishPaste(fallbackPlain);
          } else if (fallbackEditAction) {
            this.emit("edit-action", "paste");
          }
          return;
        }
        const pastedText = rich.text.length > 0 ? rich.text : (fallbackPlain ?? "");
        this.finishPaste(pastedText, rich.html, rich.files, rich.image, rich.tooLarge);
      })
      .catch((): void => {});
  }
  private pasteFromShortcut(): void {
    try {
      const board = pasteboard.getSystemPasteboard();
      if (!board.hasDataSync()) {
        return;
      }
      // Consume HarmonyOS pasteboard_service's one-shot Ctrl+V read grant
      // directly before TextArea's built-in handler reads plain text only.
      this.enqueuePaste(board.getData(), undefined, true);
    } catch {
      this.emit("edit-action", "paste");
    }
  }
  keyEvent(event: KeyEvent): boolean {
    if (!this.attached || this.closed || !this.context.isActive() || !this.context.editorKeyEvent) {
      return false;
    }
    const action = event.type === KeyType.Down ? 1 : event.type === KeyType.Up ? 2 : 0;
    if (action === 0) {
      return false;
    }
    const lockState = event as KeyEventLockState;
    const capsLock =
      typeof lockState.isCapsLockOn === "boolean" ? lockState.isCapsLockOn : undefined;
    const consumed = this.context.editorKeyEvent(this.settings.targetId, {
      action,
      keyCode: event.keyCode,
      keyText: event.keyText ?? "",
      unicode: event.unicode ?? 0,
      ctrl: modifierPressed(event, "Ctrl"),
      alt: modifierPressed(event, "Alt"),
      shift: modifierPressed(event, "Shift"),
      cmd: event.metaKey === 1,
      capsLock,
      timestamp: event.timestamp ?? 0,
    });
    if (consumed) {
      event.stopPropagation();
      return true;
    }
    return false;
  }
  editorHeight(): number {
    return this.settings.multiline && !this.settings.secure && !this.isSink()
      ? Math.max(this.settings.height, this.contentHeight)
      : this.settings.height;
  }
  paste(text: string, event: PasteEvent): void {
    event.preventDefault?.();
    // Synchronous fast path when no Paste listener is attached and default
    // paste mode only needs the plain text ArkUI already supplied.
    if (!this.settings.reportPaste && !this.settings.listensPaste && !this.isSink()) {
      if (this.canEdit() && text.length > 0) {
        this.insertText(text, true);
      }
      return;
    }
    try {
      const dataPromise = pasteboard.getSystemPasteboard().getData();
      this.enqueuePaste(dataPromise, text, false);
      return;
    } catch {}
    this.finishPaste(text);
  }
  selection(start: number, end: number): void {
    if (this.isSink()) {
      if (
        this.focused &&
        !this.updating &&
        this.preview.length === 0 &&
        (start !== this.text.length || end !== this.text.length)
      ) {
        this.pinSinkCaret();
      }
      return;
    }
    const clampedStart = Math.max(0, start);
    const clampedEnd = Math.max(clampedStart, end);
    const changed = clampedStart !== this.selStart || clampedEnd !== this.selEnd;
    if (this.pointerActive && clampedEnd > clampedStart) {
      this.pointerSuppressSelect = true;
      this.pendingSelectAllGen = 0;
    }
    this.selStart = clampedStart;
    this.selEnd = clampedEnd;
    if (changed && !this.applyingPaste) {
      this.userContextVersion += 1;
    }
    this.emit("selection", "", start, end);
    this.imeCursorKey = "";
    this.revealCaret = true;
    this.scheduleSize();
  }
  scrolled(): void {
    if (this.settings.multiline && !this.settings.secure) {
      const offset = this.scroller.currentOffset();
      this.emit(
        "scroll",
        "",
        0,
        0,
        Math.max(this.settings.width, this.contentWidth),
        Math.max(this.settings.height, this.contentHeight),
        offset.xOffset,
        offset.yOffset,
      );
    }
    this.scheduleSize();
  }
  change(text: string, preview?: PreviewText): void {
    if (this.updating || this.closed || !this.canEdit()) {
      return;
    }
    const composed = preview?.value ?? "";
    const start = Math.max(0, preview?.offset ?? 0);
    const wasComposing = this.preview.length > 0;
    const previewChanged =
      composed.length > 0 && (composed !== this.preview || start !== this.previewStart);
    this.preview = composed;
    this.previewStart = start;
    if (this.isSink()) {
      this.sinkChange(text, composed, start, wasComposing, previewChanged);
      return;
    }
    // Mark composition before ChangeText can cause a controlled update.
    if (previewChanged) {
      this.emit("composition", composed, start, start + composed.length);
    }
    let next = text;
    if (digitsOnly(this.settings.keyboard)) {
      // ArkUI's number input keeps only digits even of the text the app
      // sets (`09:00` shows as `0900`), and reports that as a change; the
      // other platforms filter only what is typed. The app's text with
      // its separators dropped is that rewrite, not an edit.
      const shown = this.text.replace(/\D/g, "");
      if (text === shown) {
        return;
      }
      // An edit of the digits shown, as an edit of the app's text: the app's
      // text up to the last digit both still agree on (all of it when every
      // digit is kept), then what the user typed after it.
      let common = 0;
      while (common < shown.length && common < text.length && shown[common] === text[common]) {
        common++;
      }
      let at = 0;
      if (common === shown.length) {
        at = this.text.length;
      } else {
        for (let i = 0, seen = 0; seen < common && i < this.text.length; i++) {
          if (/\d/.test(this.text[i]) && ++seen === common) {
            at = i + 1;
          }
        }
      }
      next = this.text.slice(0, at) + text.slice(common);
    }
    if (this.text !== next) {
      this.pushUndoState(this.text);
      this.text = next;
      this.userContextVersion += 1;
      this.emit("change", next);
    }
    if (wasComposing && composed.length === 0) {
      this.emit("composition", "", start, start);
    }
    this.imeCursorKey = "";
    this.revealCaret = true;
    this.scheduleSize();
  }
  private sinkChange(
    text: string,
    composed: string,
    start: number,
    wasComposing: boolean,
    previewChanged: boolean,
  ): void {
    if (this.text !== text) {
      let prevPad = 0;
      while (prevPad < this.text.length && this.text.charCodeAt(prevPad) === 0x200b) {
        prevPad++;
      }
      let nextPad = 0;
      while (nextPad < text.length && text.charCodeAt(nextPad) === 0x200b) {
        nextPad++;
      }
      const prevChars = Array.from(this.text);
      const nextChars = Array.from(text);
      let prefix = 0;
      const limit = Math.min(prevChars.length, nextChars.length);
      while (prefix < limit && prevChars[prefix] === nextChars[prefix]) {
        prefix++;
      }
      const deleted = prevChars.length - prefix;
      const rawInserted = nextChars.slice(prefix).join("");
      let insertedPad = 0;
      while (insertedPad < rawInserted.length && rawInserted.charCodeAt(insertedPad) === 0x200b) {
        insertedPad++;
      }
      const inserted = rawInserted.replace(/\u200b/g, "");
      this.text = text;
      if (wasComposing) {
        this.emit("composition", "", start, start);
      }
      if (nextPad === 0 && prevPad >= SINK_PAD_MIN && deleted >= prevPad) {
        if (this.rustText.length > 0) {
          this.emit("change", "");
          this.rustText = "";
          this.rustEmptied = "";
        }
        this.emit("delete-to-doc-start");
        if (inserted.length > 0) {
          this.applySinkDiff(0, inserted);
        }
      } else if (deleted === 0 && insertedPad >= SINK_PAD_MIN) {
        this.emit("undo");
        if (composed.length === 0) {
          this.text = SINK_PAD;
          this.refresh();
          return;
        }
      } else {
        this.applySinkDiff(deleted, inserted);
      }
      if (composed.length > 0) {
        this.emit("composition", composed, start, start + composed.length);
      }
    } else {
      if (previewChanged) {
        this.emit("composition", composed, start, start + composed.length);
      } else if (wasComposing && composed.length === 0) {
        this.emit("composition", "", start, start);
      }
    }
    if (composed.length === 0) {
      let pad = 0;
      while (pad < this.text.length && this.text.charCodeAt(pad) === 0x200b) {
        pad++;
      }
      if (pad < SINK_PAD_MIN) {
        this.text = SINK_PAD;
        this.refresh();
        return;
      }
    }
    this.imeCursorKey = "";
    this.revealCaret = true;
    this.scheduleSize();
  }
  private applySinkDiff(deleted: number, inserted: string): void {
    const richEditorSink = !this.settings.scrollbarVisible && this.settings.editActions === true;
    if (deleted > 0) {
      const rustChars = Array.from(this.rustText);
      const k = rustChars.length;
      if (k > 0) {
        if (deleted < k) {
          const kept = rustChars.slice(0, k - deleted).join("");
          this.emit("change", kept);
          if (richEditorSink) {
            this.emit("change", "");
            this.rustText = "";
            this.rustEmptied = "";
          } else {
            this.rustText = kept;
            this.rustEmptied = "";
          }
        } else {
          this.emit("change", "");
          this.rustText = "";
          this.rustEmptied = "";
          const rem = deleted - k;
          for (let i = 0; i < rem; i++) {
            this.emit("key", "Backspace");
          }
        }
      } else {
        for (let i = 0; i < deleted; i++) {
          this.emit("key", "Backspace");
        }
      }
    }
    if (inserted.length > 0) {
      const base = this.rustText.length > 0 ? this.rustText : this.rustEmptied;
      const sent = base + inserted;
      this.emit("change", sent);
      if ((richEditorSink && inserted.includes("\n")) || utf8ByteLength(sent) > SINK_BYTE_LIMIT) {
        this.emit("change", "");
        this.rustText = "";
        this.rustEmptied = "";
      } else {
        this.rustText = sent;
        this.rustEmptied = "";
      }
    }
  }
  submit(event?: SubmitEvent): void {
    if (!this.settings.blurOnSubmit) {
      event?.keepEditableState();
    }
    this.emit("submit");
  }
  private scheduleSize(): void {
    if (this.sizePending || this.closed) {
      return;
    }
    this.sizePending = true;
    this.editorUIContext().postFrameCallback(new EditorSizeCallback(this));
  }
  measureSize(): void {
    this.sizePending = false;
    if (this.closed || !this.context.isActive()) {
      return;
    }
    const rect =
      this.settings.multiline && !this.settings.secure
        ? this.area.getTextContentRect()
        : this.line.getTextContentRect();
    const ui = this.editorUIContext();
    const width = ui.px2vp(rect.width);
    const height = ui.px2vp(rect.height);
    if (width !== this.contentWidth || height !== this.contentHeight) {
      const oldHeight = this.editorHeight();
      this.contentWidth = width;
      this.contentHeight = height;
      this.emit("size", "", 0, 0, width, height);
      if (this.preview.length === 0 && oldHeight !== this.editorHeight()) {
        this.refresh();
        return;
      }
      this.scrolled();
    }
    if (this.pendingScroll !== undefined) {
      const command = this.pendingScroll;
      this.pendingScroll = undefined;
      this.scroller.scrollTo({
        xOffset: command.x,
        yOffset: command.y,
        animation: command.animated,
      });
      this.scrolled();
    }
    if (this.focused) {
      const controller: TextContentControllerBase =
        this.settings.multiline && !this.settings.secure ? this.area : this.line;
      const caret = controller.getCaretOffset();
      const lineHeight =
        height > 0
          ? height / Math.max(1, controller.getTextContentLineCount())
          : this.settings.fontSize * 1.2;
      let x = ui.px2vp(caret.x);
      let y = ui.px2vp(caret.y);
      if (this.settings.multiline && !this.settings.secure) {
        let offset = this.scroller.currentOffset();
        if (!this.isSink() && this.revealCaret) {
          const top = offset.yOffset;
          const bottom = top + this.settings.height;
          if (y < top || y + lineHeight > bottom) {
            this.scroller.scrollTo({
              xOffset: 0,
              yOffset: Math.max(0, y < top ? y : y + lineHeight - this.settings.height),
              animation: false,
            });
            offset = this.scroller.currentOffset();
          }
        }
        x -= offset.xOffset;
        y -= offset.yOffset;
      }
      this.revealCaret = false;
      if (this.isImeCursorSink()) {
        this.syncImeCursor(ui);
      }
      if (
        caret.index !== this.caretIndex ||
        x !== this.caretX ||
        y !== this.caretY ||
        lineHeight !== this.caretHeight
      ) {
        this.caretIndex = caret.index;
        this.caretX = x;
        this.caretY = y;
        this.caretHeight = lineHeight;
        this.emit("caret", "", caret.index, caret.index, 1.5, lineHeight, x, y);
      }
    }
  }
  private syncImeCursor(ui: UIContext): void {
    const frame = this.node.getFrameNode();
    if (!frame) {
      return;
    }
    let x = Number.NaN;
    let y = Number.NaN;
    try {
      const pos = frame.getPositionToScreenWithTransform();
      x = Number(pos.x);
      y = Number(pos.y);
    } catch {
      try {
        const pos = frame.getPositionToScreen();
        x = Number(pos.x);
        y = Number(pos.y);
      } catch {
        return;
      }
    }
    if (!Number.isFinite(x) || !Number.isFinite(y)) {
      return;
    }
    const left = Math.round(ui.vp2px(x));
    const top = Math.round(ui.vp2px(y));
    const width = Math.max(1, Math.round(ui.vp2px(this.settings.width)));
    const height = Math.max(1, Math.round(ui.vp2px(this.settings.height)));
    const key = `${left},${top},${width},${height}`;
    if (key === this.imeCursorKey) {
      return;
    }
    this.imeCursorKey = key;
    try {
      inputMethod
        .getController()
        .updateCursor({ left, top, width, height })
        .catch((): void => {});
    } catch {}
  }
  private pushUndoState(snapshot: string): void {
    if (this.undoStack.length > 0 && this.undoStack[this.undoStack.length - 1] === snapshot) {
      return;
    }
    this.undoStack.push(snapshot);
    if (this.undoStack.length > 64) {
      this.undoStack.shift();
    }
    this.redoStack.length = 0;
  }
  private applyProgrammaticText(next: string, caret: number): void {
    const clamped = Math.max(0, Math.min(next.length, caret));
    this.selStart = clamped;
    this.selEnd = clamped;
    this.text = next;
    this.userContextVersion += 1;
    this.emit("change", next);
    this.selection(clamped, clamped);
    this.refresh();
    try {
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(clamped, clamped);
      } else {
        this.line.setTextSelection(clamped, clamped);
      }
    } catch {}
  }
  markMenuInteraction(): void {
    if (this.menuInteractTimer >= 0) {
      clearTimeout(this.menuInteractTimer);
      this.menuInteractTimer = -1;
    }
    this.menuInteracting = true;
  }
  clearMenuInteraction(): void {
    if (this.menuInteractTimer >= 0) {
      clearTimeout(this.menuInteractTimer);
      this.menuInteractTimer = -1;
    }
    this.menuInteracting = false;
  }
  scheduleClearMenuInteraction(): void {
    if (this.menuInteractTimer >= 0) {
      clearTimeout(this.menuInteractTimer);
    }
    // Clear shortly after TouchType.Up so drag-out releases cannot leave the
    // editor unable to blur while still absorbing the press-induced blur.
    this.menuInteractTimer = setTimeout((): void => {
      this.menuInteractTimer = -1;
      this.menuInteracting = false;
    }, 60);
  }
  blurFromOther(): void {
    if (!this.focused || this.closed) {
      return;
    }
    this.clearMenuInteraction();
    this.emit("blur");
  }
  emit(
    kind: string,
    text = "",
    start = 0,
    end = 0,
    width = 0,
    height = 0,
    x = 0,
    y = 0,
    html?: string | null,
    files?: string | null,
    image?: string | null,
    imageTooLarge?: number | null,
  ): void {
    if (
      !this.attached ||
      this.closed ||
      !this.context.isActive() ||
      (this.updating && kind !== "focus" && kind !== "blur")
    ) {
      return;
    }
    if (kind === "blur" && this.menuInteracting) {
      this.clearMenuInteraction();
      this.requestEditorFocus();
      return;
    }
    if (kind === "focus") {
      this.clearMenuInteraction();
      const previous = activeEditorsByContext.get(this.context);
      if (previous && previous !== this) {
        previous.blurFromOther();
      }
      activeEditorsByContext.set(this.context, this);
      const wasFocused = this.focused;
      this.focused = true;
      this.imeCursorKey = "";
      if (!wasFocused) {
        this.focusGen += 1;
        this.explicitSelectionAfterFocusGen = 0;
        if (this.settings.selectTextOnFocus && !this.isSink() && this.text.length > 0) {
          if (this.pointerFocusing) {
            this.pendingSelectAllGen = this.pointerSuppressSelect ? 0 : this.focusGen;
          } else {
            this.pendingSelectAllGen = this.focusGen;
            this.scheduleSelectAllOnFocus(this.focusGen);
          }
        } else {
          this.pendingSelectAllGen = 0;
        }
      } else if (
        this.pendingSelectAllGen === this.focusGen &&
        this.explicitSelectionAfterFocusGen !== this.focusGen &&
        !this.pointerActive &&
        !this.pointerSuppressSelect &&
        this.settings.selectTextOnFocus &&
        !this.isSink() &&
        this.text.length > 0
      ) {
        // Native onFocus arrived after programmatic command("focus"); re-apply
        // select-all post-frame so ArkUI's default caret placement does not win.
        this.scheduleSelectAllOnFocus(this.focusGen);
      }
      if (this.isSink() && this.preview.length === 0) {
        this.pinSinkCaret();
      }
      this.caretIndex = -1;
      this.scheduleSize();
    } else if (kind === "blur") {
      if (activeEditorsByContext.get(this.context) === this) {
        activeEditorsByContext.delete(this.context);
      }
      if (this.focused) {
        this.focusGen += 1;
      }
      this.focused = false;
      this.pointerFocusing = false;
      this.pointerActive = false;
      this.pointerMoved = false;
      this.pointerSuppressSelect = false;
      this.pendingSelectAllGen = 0;
      this.explicitSelectionAfterFocusGen = 0;
      this.imeCursorKey = "";
      this.consumedPasteKey = false;
      this.closeEditMenu();
      this.caretIndex = -1;
      this.scheduleSize();
    }
    const response = this.context.editorEvent(
      this.settings.targetId,
      new EditorEvent(
        this,
        kind,
        text,
        start,
        end,
        width,
        height,
        x,
        y,
        html,
        files,
        image,
        imageTooLarge,
      ),
    );
    if (response.accepted) {
      this.count = response.count;
    }
  }
  appeared(): void {
    this.scheduleSize();
    this.appearedPending = true;
    this.autoFocus();
  }
  private autoFocus(): void {
    if (this.sdkBusy || !this.attached || this.closed || !this.context.isActive()) {
      return;
    }
    if (
      this.appearedPending &&
      ((!this.focusedOnce && this.settings.autoFocus) || this.focusRequested)
    ) {
      this.focusedOnce = true;
      this.focusRequested = false;
      this.requestEditorFocus();
    }
  }
  private requestEditorFocus(): void {
    const id = `nsh-editor-${this.settings.targetId}`;
    try {
      if (this.context.requestFocus) {
        this.context.requestFocus(id);
        return;
      }
      this.context.getUIContext().getFocusController().requestFocus(id);
    } catch {}
  }
  command(command: EditorCommand): void {
    if (command.kind === "focus") {
      this.focusRequested = true;
      this.requestEditorFocus();
      if (!this.focused) {
        this.emit("focus");
      }
    } else if (command.kind === "blur") {
      this.focusRequested = false;
      try {
        if (this.settings.multiline && !this.settings.secure) {
          this.area.stopEditing();
        } else {
          this.line.stopEditing();
        }
      } catch {}
      if (this.focused) {
        this.emit("blur");
      }
    } else if (command.kind === "scroll") {
      if (this.settings.multiline && !this.settings.secure) {
        this.revealCaret = false;
        this.pendingScroll = command;
        this.scheduleSize();
      }
    } else if (command.kind === "selection") {
      try {
        this.explicitSelectionAfterFocusGen = this.focusGen;
        this.pendingSelectAllGen = 0;
        this.selStart = Math.max(0, command.start);
        this.selEnd = Math.max(this.selStart, command.end);
        this.userContextVersion += 1;
        if (this.settings.multiline && !this.settings.secure) {
          this.area.setTextSelection(command.start, command.end);
        } else {
          this.line.setTextSelection(command.start, command.end);
        }
      } catch {}
    } else if (command.kind === "select-all") {
      if (this.isSink()) {
        this.emit("edit-action", "selectAll");
        return;
      }
      const end = this.text.length;
      this.selStart = 0;
      this.selEnd = end;
      this.userContextVersion += 1;
      this.updating = true;
      try {
        if (this.settings.multiline && !this.settings.secure) {
          this.area.setTextSelection(0, end);
        } else {
          this.line.setTextSelection(0, end);
        }
      } catch {
      } finally {
        this.updating = false;
      }
      this.selection(0, end);
    } else if (command.kind === "insert") {
      this.insertText(command.text ?? "");
    } else if (command.kind === "paste-insert") {
      this.applyingPaste = true;
      try {
        this.insertText(command.text ?? "", true);
      } finally {
        this.applyingPaste = false;
      }
    } else if (command.kind === "type") {
      if (!this.canEdit() || this.closed) {
        return;
      }
      let typed = command.text ?? "";
      if (digitsOnly(this.settings.keyboard)) {
        typed = typed.replace(/\D/g, "");
      }
      if (typed.length > 0) {
        this.editKey(typed);
        this.insertText(typed);
      }
    } else if (command.kind === "backspace") {
      this.deleteBackward();
    } else if (command.kind === "return-newline") {
      if (!this.canEdit() || this.closed) {
        return;
      }
      this.editKey("Enter");
      this.insertText("\n");
    } else if (command.kind === "return") {
      this.submit();
      if (this.settings.blurOnSubmit) {
        try {
          if (this.settings.multiline && !this.settings.secure) {
            this.area.stopEditing();
          } else {
            this.line.stopEditing();
          }
        } catch {}
        if (this.focused) {
          this.emit("blur");
        }
      }
    } else if (command.kind === "cursor") {
      if (this.focused && this.isImeCursorSink()) {
        this.imeCursorKey = "";
        this.scheduleSize();
      }
    } else if (command.kind === "copy" || command.kind === "cut") {
      if (this.isSink()) {
        this.emit("edit-action", command.kind);
        return;
      }
      const start = Math.min(this.text.length, this.selStart);
      const end = Math.min(this.text.length, Math.max(start, this.selEnd));
      if (end > start && !this.settings.secure) {
        const selected = this.text.slice(start, end);
        try {
          const data = pasteboard.createData(pasteboard.MIMETYPE_TEXT_PLAIN, selected);
          pasteboard.getSystemPasteboard().setDataSync(data);
        } catch {}
        if (command.kind === "cut" && this.canEdit()) {
          this.pushUndoState(this.text);
          const next = this.text.slice(0, start) + this.text.slice(end);
          this.applyProgrammaticText(next, start);
        }
      }
    } else if (command.kind === "undo") {
      if (this.isSink()) {
        this.emit("edit-action", "undo");
        return;
      }
      if (this.canEdit() && this.undoStack.length > 0) {
        const previous = this.undoStack.pop()!;
        this.redoStack.push(this.text);
        this.applyProgrammaticText(previous, previous.length);
      }
    } else if (command.kind === "redo") {
      if (this.isSink()) {
        this.emit("edit-action", "redo");
        return;
      }
      if (this.canEdit() && this.redoStack.length > 0) {
        const next = this.redoStack.pop()!;
        this.undoStack.push(this.text);
        this.applyProgrammaticText(next, next.length);
      }
    } else if (command.kind === "open-edit-menu") {
      this.openEditMenu(command.x, command.y, command.width, command.height, command.actions ?? []);
    } else if (command.kind === "close-edit-menu") {
      this.closeEditMenu();
    } else {
      throw new Error(`Unsupported editor command '${command.kind}'`);
    }
  }
  insertText(inserted: string, normalizePaste = false): void {
    if (!this.canEdit() || this.closed) {
      return;
    }
    if (this.isSink()) {
      if (inserted.length > 0) {
        this.applySinkDiff(0, inserted);
        if (this.preview.length === 0 && this.text.length < SINK_PAD_MIN) {
          this.text = SINK_PAD;
          this.refresh();
        }
      }
      return;
    }
    const controller: TextContentControllerBase =
      this.settings.multiline && !this.settings.secure ? this.area : this.line;
    const prev = this.text;
    const start = Math.min(prev.length, this.selStart);
    const end = Math.min(prev.length, Math.max(start, this.selEnd));
    const planned = this.planEdit(start, end, inserted, normalizePaste);
    if (!planned.changed) {
      return;
    }
    const next = planned.text;
    const caret = Math.max(0, Math.min(next.length, planned.caret));
    this.selStart = caret;
    this.selEnd = caret;
    this.pushUndoState(prev);
    this.text = next;
    if (!this.applyingPaste) {
      this.userContextVersion += 1;
    }
    this.updating = true;
    try {
      if (next === prev.slice(0, start) + planned.clipped + prev.slice(end)) {
        if (end > start) {
          controller.deleteText({ start, end });
        }
        if (planned.clipped.length > 0) {
          controller.addText(planned.clipped, { offset: start });
        }
      }
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(caret, caret);
      } else {
        this.line.setTextSelection(caret, caret);
      }
    } catch {
    } finally {
      this.updating = false;
    }
    this.emit("change", next);
    this.selection(caret, caret);
    this.refresh();
    try {
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(caret, caret);
      } else {
        this.line.setTextSelection(caret, caret);
      }
    } catch {}
  }
  deleteBackward(): void {
    if (!this.canEdit() || this.closed) {
      return;
    }
    if (this.isSink()) {
      this.applySinkDiff(1, "");
      if (this.preview.length === 0 && this.text.length < SINK_PAD_MIN) {
        this.text = SINK_PAD;
        this.refresh();
      }
      return;
    }
    this.editKey("Backspace");
    const controller: TextContentControllerBase =
      this.settings.multiline && !this.settings.secure ? this.area : this.line;
    const start = Math.min(this.text.length, this.selStart);
    const end = Math.min(this.text.length, Math.max(start, this.selEnd));
    let delStart = start;
    if (end === start && start > 0) {
      const before = Array.from(this.text.slice(0, start));
      const popped = before.pop() ?? "";
      delStart = start - popped.length;
    }
    if (end === delStart) {
      return;
    }
    const next = this.text.slice(0, delStart) + this.text.slice(end);
    this.selStart = delStart;
    this.selEnd = delStart;
    this.updating = true;
    try {
      controller.deleteText({ start: delStart, end });
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(delStart, delStart);
      } else {
        this.line.setTextSelection(delStart, delStart);
      }
    } catch {
    } finally {
      this.updating = false;
    }
    if (this.text !== next) {
      this.pushUndoState(this.text);
      this.text = next;
      this.userContextVersion += 1;
      this.emit("change", next);
    }
    this.selection(delStart, delStart);
    this.refresh();
  }
  chooseEditAction(action: string): void {
    this.clearMenuInteraction();
    if (!this.focused) {
      this.requestEditorFocus();
    }
    if (this.closed || !this.context.isActive()) {
      this.closeEditMenu();
      return;
    }
    if (action === "paste") {
      try {
        // Start getData() while PasteButton's temporary security grant is active,
        // before disposing the overlay ComponentContent.
        const dataPromise = pasteboard.getSystemPasteboard().getData();
        this.closeEditMenu();
        this.enqueuePaste(dataPromise, undefined, true);
      } catch {
        this.closeEditMenu();
        this.emit("edit-action", "paste");
      }
      return;
    }
    this.closeEditMenu();
    this.emit("edit-action", action);
  }
  private openEditMenu(
    x: number,
    y: number,
    width: number,
    height: number,
    actions: string[],
  ): void {
    this.closeEditMenu();
    if (this.closed || !this.context.isActive()) {
      return;
    }
    let hasClip = true;
    try {
      hasClip = pasteboard.getSystemPasteboard().hasDataSync();
    } catch {}
    const filtered = actions.filter((a: string): boolean => a !== "paste" || hasClip);
    if (filtered.length === 0) {
      return;
    }
    const ui = this.editorUIContext();
    const content = new ComponentContent<EditMenuParams>(
      ui,
      wrapBuilder(editMenuBuilder),
      {
        entry: this,
        x,
        y,
        width,
        height,
        actions: filtered,
      },
      { nestingBuilderSupported: true },
    );
    this.editMenuContent = content;
    this.editMenuUi = ui;
    try {
      ui.getOverlayManager().addComponentContent(content);
    } catch {
      if (this.editMenuContent === content) {
        this.editMenuContent = undefined;
        this.editMenuUi = undefined;
      }
      try {
        content.dispose();
      } catch {}
    }
  }
  private closeEditMenu(): void {
    const content = this.editMenuContent;
    const ui = this.editMenuUi ?? this.editorUIContext();
    if (!content) {
      return;
    }
    this.editMenuContent = undefined;
    this.editMenuUi = undefined;
    try {
      ui.getOverlayManager().removeComponentContent(content);
    } catch {}
    try {
      content.dispose();
    } catch {}
  }
  private release(): void {
    this.closeEditMenu();
    try {
      this.node.dispose();
      this.disposed = true;
      quarantinedEditors.delete(this);
    } catch (error) {
      quarantinedEditors.add(this);
      throw new Error(String(error));
    }
  }
  dispose(cleanup: NshRuntimeContext = this.context): void {
    if (this.disposed) {
      return;
    }
    this.clearMenuInteraction();
    if (activeEditorsByContext.get(this.context) === this) {
      activeEditorsByContext.delete(this.context);
    }
    this.closed = true;
    try {
      if (!this.retired) {
        if (!cleanup.retireNativeNode(this.settings.targetId)) {
          throw new Error("Native editor retirement was rejected");
        }
        this.retired = true;
      }
      if (this.sdkBusy) {
        this.disposePending = true;
      } else {
        this.release();
      }
    } catch (error) {
      quarantinedEditors.add(this);
      throw new Error(`Native editor retirement failed: ${String(error)}`);
    }
  }
}

function retryQuarantinedEditors(context: NshRuntimeContext): void {
  for (const entry of Array.from(quarantinedEditors)) {
    try {
      entry.dispose(context);
    } catch {}
  }
}

class EditorBuildCallback extends FrameCallback {
  private build: (timeLeft: number) => void;
  constructor(build: (timeLeft: number) => void) {
    super();
    this.build = build;
  }
  onIdle(timeLeft: number): void {
    this.build(timeLeft);
  }
}

export class NativeEditors {
  private pending: Map<number, EditorSettings> = new Map();
  private scheduled: boolean = false;
  private create(state: EditorSettings, context: NshRuntimeContext): EditorEntry {
    this.pending.delete(state.targetId);
    const entry = new EditorEntry(state, context);
    // Publish before build because SDK callbacks can synchronously retire the target.
    this.entries.set(state.targetId, entry);
    try {
      entry.create();
    } catch (error) {
      if (this.entries.get(state.targetId) === entry) this.entries.delete(state.targetId);
      throw error instanceof Error ? error : new Error(String(error));
    }
    return entry;
  }
  private schedule(context: NshRuntimeContext): void {
    if (this.scheduled || this.pending.size === 0 || !context.isActive()) return;
    this.scheduled = true;
    const callback = new EditorBuildCallback((timeLeft: number): void => {
      this.scheduled = false;
      if (!context.isActive()) return;
      // Allow at least one editor to progress on a busy frame. Leave the rest of
      // the interval to ArkUI to present and process the next input.
      const until = Date.now() + Math.min(3, timeLeft / 1000000);
      try {
        for (const state of this.pending.values()) {
          this.create(state, context);
          if (Date.now() >= until || !context.isActive()) break;
        }
      } finally {
        this.schedule(context);
      }
    });
    try {
      context.getUIContext().postFrameCallback(callback);
    } catch (error) {
      this.scheduled = false;
      throw error instanceof Error ? error : new Error(String(error));
    }
  }
  private entries: Map<number, EditorEntry> = new Map();
  remove(ids: number[], context: NshRuntimeContext): void {
    retryQuarantinedEditors(context);
    for (const id of ids) {
      this.pending.delete(id);
      this.entries.get(id)?.dispose(context);
      this.entries.delete(id);
    }
  }
  update(states: EditorSettings[], commands: EditorCommand[], context: NshRuntimeContext): void {
    retryQuarantinedEditors(context);
    for (const state of states) {
      if (!context.isActive()) {
        throw new Error("NSH SDK session is inactive");
      }
      const entry = this.entries.get(state.targetId);
      if (entry) {
        entry.update(state);
      } else if (state.belowViewport && !state.autoFocus) {
        this.pending.set(state.targetId, state);
      } else {
        this.create(state, context);
      }
    }
    for (const command of commands) {
      const pending = this.pending.get(command.targetId);
      const entry = pending ? this.create(pending, context) : this.entries.get(command.targetId);
      entry?.command(command);
    }
    this.schedule(context);
  }
  dispose(context: NshRuntimeContext): void {
    this.pending.clear();
    activeEditorsByContext.delete(context);
    retryQuarantinedEditors(context);
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
