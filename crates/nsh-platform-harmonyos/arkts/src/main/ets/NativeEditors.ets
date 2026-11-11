import { BuilderNode, ComponentContent, FrameNode, NodeContent } from "@ohos.arkui.node";
import { FrameCallback, UIContext } from "@ohos.arkui.UIContext";
import { uiAppearance } from "@kit.ArkUI";
import pasteboard from "@ohos.pasteboard";
import image from "@ohos.multimedia.image";
import util from "@ohos.util";

export interface EditorSettings {
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
  hidden: boolean;
  accessible?: boolean | null;
  disabled: boolean;
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
}
export interface EditorReceipt {
  accepted: boolean;
  count: number;
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
}
export interface NshRuntimeContext {
  isActive(): boolean;
  getUIContext(): UIContext;
  attachNativeNode(targetId: number, content: FrameNode | null): boolean;
  attachNativeSubtree(targetId: number, content: FrameNode | null, slot: NodeContent): boolean;
  retireNativeNode(targetId: number): boolean;
  editorEvent(targetId: number, event: EditorEventPayload): EditorReceipt;
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
  }
}

class EditorModifier implements AttributeModifier<TextInputAttribute | TextAreaAttribute> {
  private entry: EditorEntry;
  constructor(entry: EditorEntry) {
    this.entry = entry;
  }
  applyNormalAttribute(node: TextInputAttribute | TextAreaAttribute): void {
    const s = this.entry.settings;
    node
      .width(s.width)
      .height(this.entry.editorHeight())
      .padding(0)
      .borderWidth(0)
      // The outer NSH node owns corners; theme rounding would clip this content box.
      .borderRadius(0)
      .backgroundColor(Color.Transparent)
      .fontSize(s.fontSize)
      .fontWeight(s.fontWeight)
      .fontFamily(s.fontFamily ?? "HarmonyOS Sans")
      .fontStyle(s.italic ? FontStyle.Italic : FontStyle.Normal)
      .fontColor(argbColor(s.color))
      .placeholderColor(argbColor(s.placeholderColor))
      .caretColor(argbColor(s.caretColor))
      .selectedBackgroundColor(argbColor(s.selectionColor))
      .enabled(s.accessibility?.disabled !== true)
      .enableKeyboardOnFocus(s.editable)
      .enablePreviewText(s.editable)
      .barState(BarState.Off)
      .accessibilityText(s.accessibility?.label ?? "")
      .accessibilityDescription(s.accessibility?.hint ?? "")
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
      .maxLength(s.maxLength ?? 2147483647)
      .enableAutoFill(s.autoFill)
      .contentType(contentType(s.contentType))
      .hitTestBehavior(s.hitTest as HitTestMode);
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

@Builder
function editorContent(entry: EditorEntry) {
  if (entry.settings.multiline && !entry.settings.secure) {
    Scroll(entry.scroller) {
      TextArea({
        text: entry.text,
        placeholder: entry.settings.placeholder,
        controller: entry.area,
      })
        .id(`nsh-editor-${entry.settings.targetId}`)
        .attributeModifier(new EditorModifier(entry))
        .editMenuOptions(new EditorSystemMenu(entry))
        .onKeyPreIme((): boolean => false)
        .onChange((text: string, preview?: PreviewText) => entry.change(text, preview))
        .onFocus(() => entry.emit("focus"))
        .onBlur(() => entry.emit("blur"))
        .onSubmit((_key: EnterKeyType, event?: SubmitEvent) => entry.submit(event))
        .onTextSelectionChange((start: number, end: number) => entry.selection(start, end))
        .onContentScroll(() => entry.scrolled())
        .onPaste((text: string, event: PasteEvent) => entry.paste(text, event))
        .onWillChange(() => entry.canEdit())
        .onWillInsert((value: InsertValue): boolean => {
          if (!entry.canEdit()) return false;
          if (!entry.isSink()) {
            entry.editKey(value.insertValue);
          }
          return true;
        })
        .onWillDelete((): boolean => {
          if (!entry.canEdit()) return false;
          if (!entry.isSink() || entry.text.length === 0) {
            entry.editKey("Backspace");
          }
          return true;
        })
        .onAppear(() => entry.appeared())
    }
    .width(entry.settings.width)
    .height(entry.settings.height)
    .scrollable(ScrollDirection.Vertical)
    .scrollBar(entry.settings.scrollbarVisible ? BarState.Auto : BarState.Off)
    .clip(true)
    .onDidScroll(() => entry.scrolled());
  } else {
    TextInput({ text: entry.text, placeholder: entry.settings.placeholder, controller: entry.line })
      .id(`nsh-editor-${entry.settings.targetId}`)
      .attributeModifier(new EditorModifier(entry))
      .editMenuOptions(new EditorSystemMenu(entry))
      .onKeyPreIme((): boolean => false)
      .onChange((text: string, preview?: PreviewText) => entry.change(text, preview))
      .onFocus(() => entry.emit("focus"))
      .onBlur(() => entry.emit("blur"))
      .onSubmit((_key: EnterKeyType, event: SubmitEvent) => entry.submit(event))
      .onTextSelectionChange((start: number, end: number) => entry.selection(start, end))
      .onContentScroll(() => entry.scrolled())
      .onPaste((text: string, event: PasteEvent) => entry.paste(text, event))
      .onWillChange(() => entry.canEdit())
      .onWillInsert((value: InsertValue): boolean => {
        if (!entry.canEdit()) return false;
        entry.editKey(value.insertValue);
        return true;
      })
      .onWillDelete((): boolean => {
        if (!entry.canEdit()) return false;
        entry.editKey("Backspace");
        return true;
      })
      .onAppear(() => entry.appeared());
  }
}

// Failed retirement cannot release a BuilderNode still borrowed by native code.
const quarantinedEditors: Set<EditorEntry> = new Set();
let activeEditor: EditorEntry | undefined;

interface EditMenuParams {
  entry: EditorEntry;
  x: number;
  y: number;
  width: number;
  height: number;
  actions: string[];
}

interface ExtractedPasteData {
  text: string;
  html?: string;
  files?: string;
  image?: string;
}

async function extractPasteData(data: pasteboard.PasteData): Promise<ExtractedPasteData> {
  const text = data.getPrimaryText() ?? "";
  let html: string | undefined;
  try {
    const primaryHtml = data.getPrimaryHtml();
    if (primaryHtml && primaryHtml.length > 0) {
      html = primaryHtml;
    }
  } catch {}
  const uris: string[] = [];
  try {
    const count = data.getRecordCount();
    for (let i = 0; i < count; i++) {
      const record = data.getRecord(i);
      if (!html && record.htmlText && record.htmlText.length > 0) {
        html = record.htmlText;
      }
      if (record.uri && record.uri.startsWith("file:")) {
        uris.push(record.uri);
      }
    }
  } catch {}
  const files = uris.length > 0 ? uris.join("\n") : undefined;
  let imgUri: string | undefined;
  try {
    const pixels = data.getPrimaryPixelMap();
    if (pixels) {
      const packer = image.createImagePacker();
      try {
        const bytes = await packer.packing(pixels, { format: "image/png", quality: 100 });
        if (bytes.byteLength > 0 && bytes.byteLength <= 48 * 1024 * 1024) {
          imgUri = `data:image/png;base64,${new util.Base64Helper().encodeToStringSync(new Uint8Array(bytes))}`;
        }
      } finally {
        packer.release();
        pixels.release();
      }
    }
  } catch {}
  return { text, html, files, image: imgUri };
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

// The system keeps a separate resource for each dark color, so the menu picks
// the pair that matches the appearance the system is showing (API 20).
function darkMode(): boolean {
  try {
    return uiAppearance.getDarkMode() === uiAppearance.DarkMode.ALWAYS_DARK;
  } catch {
    return false;
  }
}

function menuBackgroundColor(): ResourceColor {
  return darkMode()
    ? $r("sys.color.ohos_id_color_card_bg_dark")
    : $r("sys.color.ohos_id_color_card_bg");
}

function menuTextColor(): ResourceColor {
  return darkMode()
    ? $r("sys.color.ohos_id_color_text_primary_dark")
    : $r("sys.color.ohos_id_color_text_primary");
}

function menuDividerColor(): ResourceColor {
  return darkMode()
    ? $r("sys.color.ohos_id_color_component_normal_dark")
    : $r("sys.color.ohos_id_color_component_normal");
}

function menuShadowColor(): ResourceColor {
  return darkMode()
    ? $r("sys.color.ohos_id_color_mask_thin_dark")
    : $r("sys.color.ohos_id_color_mask_thin");
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
    Row() {
      ForEach(
        this.actions,
        (action: string, index: number) => {
          if (index > 0) {
            Divider().vertical(true).height(16).color(menuDividerColor()).strokeWidth(0.5);
          }
          if (action === "paste") {
            PasteButton({
              text: PasteDescription.PASTE,
              buttonType: ButtonType.Capsule,
            })
              .fontSize(14)
              .fontColor(menuTextColor())
              .iconColor(menuTextColor())
              .fontWeight(FontWeight.Regular)
              .backgroundColor(menuBackgroundColor())
              .padding({ left: 12, right: 12, top: 8, bottom: 8 })
              .onClick((_event: ClickEvent, result: PasteButtonOnClickResult): void => {
                if (result === PasteButtonOnClickResult.SUCCESS) {
                  this.entry.chooseEditAction("paste");
                }
              });
          } else {
            Text(editActionLabel(action))
              .fontSize(14)
              .fontColor(menuTextColor())
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
    .backgroundColor(menuBackgroundColor())
    .borderRadius(18)
    .border({
      width: 0.5,
      color: menuDividerColor(),
    })
    .shadow({
      radius: 16,
      color: menuShadowColor(),
      offsetX: 0,
      offsetY: 4,
    })
    .focusable(false)
    .onTouch((event: TouchEvent): void => {
      if (event.type === TouchType.Down) {
        this.entry.markMenuInteraction();
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
    .position({ x: this.posX(), y: this.posY() });
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
class EditorEntry {
  readonly node: BuilderNode<[EditorEntry]>;
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
  private menuInteracting: boolean = false;
  private rustText: string = "";
  private rustEmptied: string = "";
  constructor(settings: EditorSettings, context: NshRuntimeContext) {
    this.context = context;
    this.settings = settings;
    this.rustText = settings.text;
    this.text = isSinkSettings(settings) && settings.text.length === 0 ? SINK_PAD : settings.text;
    this.count = settings.count;
    this.selStart = this.text.length;
    this.selEnd = this.text.length;
    this.node = new BuilderNode<[EditorEntry]>(context.getUIContext());
  }
  isSink(): boolean {
    return isSinkSettings(this.settings);
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
      this.node.build(wrapBuilder(editorContent), this);
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
      this.node.update(this);
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
  editorHeight(): number {
    return this.settings.multiline && !this.settings.secure
      ? Math.max(this.settings.height, this.contentHeight)
      : this.settings.height;
  }
  paste(text: string, event: PasteEvent): void {
    if (!this.canEdit() || this.settings.reportPaste) {
      event.preventDefault?.();
    }
    if (this.settings.reportPaste) {
      try {
        pasteboard
          .getSystemPasteboard()
          .getData()
          .then((data: pasteboard.PasteData): void => {
            if (this.closed || !this.context.isActive()) {
              return;
            }
            if (data.getRecordCount() === 0) {
              this.emit("paste", text);
              return;
            }
            extractPasteData(data)
              .then((rich: ExtractedPasteData): void => {
                if (this.closed || !this.context.isActive()) {
                  return;
                }
                this.emit(
                  "paste",
                  rich.text.length > 0 ? rich.text : text,
                  0,
                  0,
                  0,
                  0,
                  0,
                  0,
                  rich.html,
                  rich.files,
                  rich.image,
                );
              })
              .catch((): void => {
                this.emit("paste", text);
              });
          })
          .catch((): void => {
            this.emit("paste", text);
          });
        return;
      } catch {}
    }
    this.emit("paste", text);
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
    this.selStart = Math.max(0, start);
    this.selEnd = Math.max(this.selStart, end);
    this.emit("selection", "", start, end);
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
      this.text = next;
      this.emit("change", next);
    }
    if (wasComposing && composed.length === 0) {
      this.emit("composition", "", start, start);
    }
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
    this.revealCaret = true;
    this.scheduleSize();
  }
  private applySinkDiff(deleted: number, inserted: string): void {
    const richEditorSink = !this.settings.scrollbarVisible;
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
    this.context.getUIContext().postFrameCallback(new EditorSizeCallback(this));
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
    const ui = this.context.getUIContext();
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
        if (this.revealCaret) {
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
  markMenuInteraction(): void {
    this.menuInteracting = true;
  }
  blurFromOther(): void {
    if (!this.focused || this.closed) {
      return;
    }
    this.menuInteracting = false;
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
      this.menuInteracting = false;
      try {
        this.context
          .getUIContext()
          .getFocusController()
          .requestFocus(`nsh-editor-${this.settings.targetId}`);
      } catch {}
      return;
    }
    if (kind === "focus") {
      this.menuInteracting = false;
      if (activeEditor && activeEditor !== this) {
        activeEditor.blurFromOther();
      }
      activeEditor = this;
      this.focused = true;
      if (this.isSink() && this.preview.length === 0) {
        this.pinSinkCaret();
      }
      this.caretIndex = -1;
      this.scheduleSize();
    } else if (kind === "blur") {
      if (activeEditor === this) {
        activeEditor = undefined;
      }
      this.focused = false;
      this.closeEditMenu();
      this.caretIndex = -1;
      this.scheduleSize();
    }
    const response = this.context.editorEvent(
      this.settings.targetId,
      new EditorEvent(this, kind, text, start, end, width, height, x, y, html, files, image),
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
      try {
        this.context
          .getUIContext()
          .getFocusController()
          .requestFocus(`nsh-editor-${this.settings.targetId}`);
      } catch {}
    }
  }
  command(command: EditorCommand): void {
    if (command.kind === "focus") {
      this.focusRequested = true;
      try {
        this.context
          .getUIContext()
          .getFocusController()
          .requestFocus(`nsh-editor-${this.settings.targetId}`);
      } catch {}
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
        this.selStart = Math.max(0, command.start);
        this.selEnd = Math.max(this.selStart, command.end);
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
      this.updating = true;
      try {
        if (this.settings.multiline && !this.settings.secure) {
          this.area.setTextSelection(0, end);
        } else {
          this.line.setTextSelection(0, end);
        }
      } catch {} finally {
        this.updating = false;
      }
      this.selection(0, end);
    } else if (command.kind === "insert") {
      this.insertText(command.text ?? "");
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
    } else if (command.kind === "open-edit-menu") {
      this.openEditMenu(command.x, command.y, command.width, command.height, command.actions ?? []);
    } else if (command.kind === "close-edit-menu") {
      this.closeEditMenu();
    } else {
      throw new Error(`Unsupported editor command '${command.kind}'`);
    }
  }
  insertText(inserted: string): void {
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
    const start = Math.min(this.text.length, this.selStart);
    const end = Math.min(this.text.length, Math.max(start, this.selEnd));
    const maxLen = this.settings.maxLength;
    const room =
      maxLen != null ? Math.max(0, maxLen - (this.text.length - (end - start))) : inserted.length;
    const clipped = inserted.slice(0, room);
    const next = this.text.slice(0, start) + clipped + this.text.slice(end);
    const caret = start + clipped.length;
    this.selStart = caret;
    this.selEnd = caret;
    this.updating = true;
    try {
      if (end > start) {
        controller.deleteText({ start, end });
      }
      if (clipped.length > 0) {
        controller.addText(clipped, { offset: start });
      }
      if (this.settings.multiline && !this.settings.secure) {
        this.area.setTextSelection(caret, caret);
      } else {
        this.line.setTextSelection(caret, caret);
      }
    } catch {} finally {
      this.updating = false;
    }
    if (this.text !== next) {
      this.text = next;
      this.emit("change", next);
    }
    this.selection(caret, caret);
    this.refresh();
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
    } catch {} finally {
      this.updating = false;
    }
    if (this.text !== next) {
      this.text = next;
      this.emit("change", next);
    }
    this.selection(delStart, delStart);
    this.refresh();
  }
  chooseEditAction(action: string): void {
    this.menuInteracting = false;
    if (!this.focused) {
      try {
        this.context
          .getUIContext()
          .getFocusController()
          .requestFocus(`nsh-editor-${this.settings.targetId}`);
      } catch {}
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
        dataPromise
          .then((data: pasteboard.PasteData): void => {
            if (this.closed || !this.context.isActive()) {
              return;
            }
            if (data.getRecordCount() === 0 && pasteboard.getSystemPasteboard().hasDataSync()) {
              this.emit("edit-action", "paste");
              return;
            }
            extractPasteData(data)
              .then((rich: ExtractedPasteData): void => {
                if (this.closed || !this.context.isActive()) {
                  return;
                }
                this.emit("paste", rich.text, 0, 0, 0, 0, 0, 0, rich.html, rich.files, rich.image);
                if (!this.settings.reportPaste && this.canEdit() && rich.text.length > 0) {
                  this.insertText(rich.text);
                }
              })
              .catch((): void => {
                this.emit("edit-action", "paste");
              });
          })
          .catch((): void => {
            this.emit("edit-action", "paste");
          });
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
    const ui = this.context.getUIContext();
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
    try {
      ui.getOverlayManager().addComponentContent(content);
    } catch {
      if (this.editMenuContent === content) {
        this.editMenuContent = undefined;
      }
      try {
        content.dispose();
      } catch {}
    }
  }
  private closeEditMenu(): void {
    const content = this.editMenuContent;
    if (!content) {
      return;
    }
    this.editMenuContent = undefined;
    try {
      this.context.getUIContext().getOverlayManager().removeComponentContent(content);
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
    if (activeEditor === this) {
      activeEditor = undefined;
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

export class NativeEditors {
  private entries: Map<number, EditorEntry> = new Map();
  remove(ids: number[], context: NshRuntimeContext): void {
    for (const id of ids) {
      this.entries.get(id)?.dispose(context);
      this.entries.delete(id);
    }
  }
  update(states: EditorSettings[], commands: EditorCommand[], context: NshRuntimeContext): void {
    for (const state of states) {
      if (!context.isActive()) {
        throw new Error("NSH SDK session is inactive");
      }
      let entry = this.entries.get(state.targetId);
      if (entry) {
        entry.update(state);
      } else {
        entry = new EditorEntry(state, context);
        // Publish before SDK build: onAppear/onClosing may reenter synchronously.
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
