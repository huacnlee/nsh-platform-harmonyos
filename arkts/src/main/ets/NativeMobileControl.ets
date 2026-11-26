import { BuilderNode, FrameNode } from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import { window } from "@kit.ArkUI";
import { i18n } from "@kit.LocalizationKit";
import { TabBarContext } from "./NativeTabBar";

interface FieldChange {
  name: string;
  value: ESObject;
  removed: boolean;
}
interface Fields {
  year?: number;
  month?: number;
  day?: number;
  hour?: number;
  minute?: number;
}
class ControlSettings {
  disabled: boolean = false;
  label: string = "";
  colorMode: ThemeColorMode = ThemeColorMode.SYSTEM;
  mode: string = "date";
  value?: Fields;
  minimum?: Fields;
  maximum?: Fields;
  interval: number = 1;
  text: string = "";
  open: () => void;
  constructor(open: () => void) {
    this.open = open;
  }
}

let cachedSystemLocale: string | undefined;
let cached24HourClock: boolean | undefined;

function invalidateLocaleCache(): void {
  cachedSystemLocale = undefined;
  cached24HourClock = undefined;
}

function systemLocale(): string {
  if (cachedSystemLocale !== undefined) {
    return cachedSystemLocale;
  }
  try {
    cachedSystemLocale = i18n.System.getSystemLocaleInstance().toString();
  } catch {
    cachedSystemLocale = "en-US";
  }
  return cachedSystemLocale;
}

function is24HourClock(): boolean {
  if (cached24HourClock !== undefined) {
    return cached24HourClock;
  }
  try {
    cached24HourClock = i18n.System.is24HourClock();
  } catch {
    cached24HourClock = false;
  }
  return cached24HourClock;
}

function date(fields?: Fields, now: Date = new Date()): Date {
  return new Date(
    fields?.year ?? now.getFullYear(),
    (fields?.month ?? now.getMonth() + 1) - 1,
    fields?.day ?? now.getDate(),
    fields?.hour ?? now.getHours(),
    fields?.minute ?? now.getMinutes(),
  );
}

function snapTimeToInterval(picked: Date, interval: number): Date {
  const step = Number.isFinite(interval) && interval >= 1 ? Math.min(60, Math.floor(interval)) : 1;
  const rawMinutes = picked.getMinutes();
  const snapped = rawMinutes - (rawMinutes % step);
  const adjusted = new Date(picked.getTime());
  adjusted.setMinutes(snapped, 0, 0);
  return adjusted;
}

function clampDate(value: Date, start?: Date, end?: Date): Date {
  if (start && value < start) {
    return new Date(start.getTime());
  }
  if (end && value > end) {
    return new Date(end.getTime());
  }
  return value;
}

function display(mode: string, value: Date): string {
  const locale = systemLocale();
  if (mode === "time") {
    return value.toLocaleTimeString(locale, {
      hour: "2-digit",
      minute: "2-digit",
      hour12: !is24HourClock(),
    });
  }
  const day = value.toLocaleDateString(locale);
  if (mode === "dateTime") {
    const time = value.toLocaleTimeString(locale, {
      hour: "2-digit",
      minute: "2-digit",
      hour12: !is24HourClock(),
    });
    return day + " " + time;
  }
  return day;
}

@Builder
function mobileControl(s: ControlSettings) {
  WithTheme({ colorMode: s.colorMode }) {
    Button(s.text, { type: ButtonType.ROUNDED_RECTANGLE })
      .controlSize(ControlSize.SMALL)
      .buttonStyle(ButtonStyleMode.NORMAL)
      .fontSize($r("sys.float.Body_S"))
      .fontWeight(FontWeight.Regular)
      .fontColor($r("sys.color.font_primary"))
      .backgroundColor($r("sys.color.comp_background_tertiary"))
      .borderRadius($r("sys.float.corner_radius_level4"))
      .padding({ left: 10, right: 10, top: 4, bottom: 4 })
      .opacity(s.disabled ? $r("sys.float.interactive_disable") : 1)
      .enabled(!s.disabled)
      .accessibilityText(s.label)
      .onClick(s.open);
  };
}

export class NativeMobileControl {
  readonly node: FrameNode;
  private readonly builder: BuilderNode<[ControlSettings]>;
  private readonly ui: UIContext;
  private readonly context: TabBarContext;
  private settings: ControlSettings;
  private closed: boolean = false;
  private presenting: boolean = false;
  private hasMeasured: boolean = false;
  private width: number = 0;
  private height: number = 0;
  private readonly hostWindow: window.Window;
  private readonly mounted: Set<NativeMobileControl>;
  constructor(
    ui: UIContext,
    context: TabBarContext,
    hostWindow: window.Window,
    mounted: Set<NativeMobileControl>,
    colorMode: ThemeColorMode = ThemeColorMode.SYSTEM,
  ) {
    this.mounted = mounted;
    this.hostWindow = hostWindow;
    this.ui = ui;
    this.context = context;
    this.settings = new ControlSettings((): void => this.pick());
    this.settings.colorMode = colorMode;
    this.builder = new BuilderNode<[ControlSettings]>(ui);
    this.builder.build(wrapBuilder(mobileControl), this.settings);
    this.node = this.builder.getFrameNode()!;
    // Avoid measuring a placeholder date when initial updateProps arrives in the same mount batch.
    Promise.resolve().then((): void => {
      if (!this.closed && !this.hasMeasured) {
        this.settings.text = display(this.settings.mode, date(this.settings.value));
        this.builder.update(this.settings);
        this.measure();
      }
    });
    this.mounted.add(this);
  }
  private measure(): void {
    let rect: window.Rect;
    try {
      rect = this.hostWindow.getWindowProperties().windowRect;
    } catch {
      return;
    }
    if (rect.width <= 0 || rect.height <= 0) {
      return;
    }
    // ArkUI measures the themed button or switch, including its label and system padding.
    this.node.measure({
      minSize: { width: 0, height: 0 },
      maxSize: rect,
      percentReference: rect,
    });
    const measured = this.node.getMeasuredSize();
    const width = this.ui.px2vp(measured.width);
    const height = this.ui.px2vp(measured.height);
    this.hasMeasured = true;
    if (width > 0 && height > 0 && (width !== this.width || height !== this.height)) {
      this.width = width;
      this.height = height;
      this.context.size(width, height);
    }
  }
  setColorMode(colorMode: ThemeColorMode): void {
    if (this.closed || this.settings.colorMode === colorMode) {
      return;
    }
    const next = this.cloneSettings();
    next.colorMode = colorMode;
    this.settings = next;
    this.builder.update(next);
  }
  refresh(): void {
    if (this.closed) {
      return;
    }
    invalidateLocaleCache();
    this.builder.updateConfiguration();
    const next = this.cloneSettings();
    next.text = display(next.mode, date(next.value));
    this.settings = next;
    this.builder.update(next);
    this.measure();
  }
  private cloneSettings(): ControlSettings {
    const old = this.settings;
    const s = new ControlSettings(old.open);
    s.disabled = old.disabled;
    s.label = old.label;
    s.colorMode = old.colorMode;
    s.mode = old.mode;
    s.value = old.value;
    s.minimum = old.minimum;
    s.maximum = old.maximum;
    s.interval = old.interval;
    s.text = old.text;
    return s;
  }
  updateProps(changes: FieldChange[]): void {
    if (this.closed) {
      return;
    }
    const previousText = this.settings.text;
    const s = this.cloneSettings();
    for (const c of changes) {
      switch (c.name) {
        case "disabled":
          s.disabled = !c.removed && c.value === true;
          break;
        case "accessibilityLabel":
          s.label = c.removed || c.value == null ? "" : (c.value as string);
          break;
        case "mode":
          s.mode = c.removed ? "date" : (c.value as string);
          break;
        case "value":
          s.value = c.removed ? undefined : (c.value as Fields);
          break;
        case "min":
          s.minimum = c.removed ? undefined : (c.value as Fields);
          break;
        case "max":
          s.maximum = c.removed ? undefined : (c.value as Fields);
          break;
        case "minuteInterval": {
          const raw = c.removed ? 1 : Number(c.value);
          s.interval = Number.isFinite(raw) && raw >= 1 ? Math.min(60, Math.floor(raw)) : 1;
          break;
        }
      }
    }
    s.text = display(s.mode, date(s.value));
    this.settings = s;
    this.builder.update(s);
    if (!this.hasMeasured || s.text !== previousText) {
      this.measure();
    }
  }
  private pick(): void {
    const s = this.settings;
    if (this.closed || s.disabled || this.presenting) {
      return;
    }
    this.presenting = true;
    const mode = s.mode;
    const now = new Date();
    const start = s.minimum ? date(s.minimum, now) : undefined;
    const end = s.maximum ? date(s.maximum, now) : undefined;
    const selected = clampDate(date(s.value, now), start, end);
    const done = (): void => {
      this.presenting = false;
    };
    const accept = (picked: Date): void => {
      if (this.closed || this.settings.disabled || this.settings.mode !== mode) {
        return;
      }
      const adjusted = mode !== "date" ? snapTimeToInterval(picked, s.interval) : picked;
      const chosen = clampDate(adjusted, start, end);
      const fields: Fields = {};
      if (mode !== "time") {
        fields.year = chosen.getFullYear();
        fields.month = chosen.getMonth() + 1;
        fields.day = chosen.getDate();
      }
      if (mode !== "date") {
        fields.hour = chosen.getHours();
        fields.minute = chosen.getMinutes();
      }
      this.context.valueChanged(fields);
    };
    const blurOptions: BackgroundBlurStyleOptions = { colorMode: s.colorMode };
    const accent: ResourceColor = $r("sys.color.font_emphasize");
    const disappearStyle: PickerTextStyle = {
      color: $r("sys.color.font_tertiary"),
      font: { size: $r("sys.float.Body_S"), weight: FontWeight.Regular },
    };
    const normalStyle: PickerTextStyle = {
      color: $r("sys.color.font_primary"),
      font: { size: $r("sys.float.Body_M"), weight: FontWeight.Regular },
    };
    const selectedStyle: PickerTextStyle = {
      color: accent,
      font: { size: $r("sys.float.Body_L"), weight: FontWeight.Medium },
    };
    const buttonStyle: PickerDialogButtonStyle = {
      fontColor: accent,
      fontSize: $r("sys.float.Body_M"),
      style: ButtonStyleMode.TEXTUAL,
    };
    const offset: Offset = { dx: 0, dy: -16 };
    try {
      if (mode === "time") {
        const timeOptions: TimePickerDialogOptions = {
          selected,
          start,
          end,
          alignment: DialogAlignment.Bottom,
          offset,
          useMilitaryTime: is24HourClock(),
          enableCascade: true,
          enableHapticFeedback: true,
          dateTimeOptions: { hour: "2-digit", minute: "2-digit" },
          backgroundBlurStyle: BlurStyle.COMPONENT_ULTRA_THICK,
          backgroundBlurStyleOptions: blurOptions,
          disappearTextStyle: disappearStyle,
          textStyle: normalStyle,
          selectedTextStyle: selectedStyle,
          acceptButtonStyle: buttonStyle,
          cancelButtonStyle: buttonStyle,
          onAccept: (picked: TimePickerResult): void => {
            selected.setHours(picked.hour ?? 0, picked.minute ?? 0);
            accept(selected);
          },
          onDidDisappear: done,
        };
        this.ui.showTimePickerDialog(timeOptions);
      } else {
        const dateOptions: DatePickerDialogOptions = {
          selected,
          start,
          end,
          showTime: mode === "dateTime",
          alignment: DialogAlignment.Bottom,
          offset,
          useMilitaryTime: is24HourClock(),
          enableHapticFeedback: true,
          dateTimeOptions: { hour: "2-digit", minute: "2-digit" },
          backgroundBlurStyle: BlurStyle.COMPONENT_ULTRA_THICK,
          backgroundBlurStyleOptions: blurOptions,
          disappearTextStyle: disappearStyle,
          textStyle: normalStyle,
          selectedTextStyle: selectedStyle,
          acceptButtonStyle: buttonStyle,
          cancelButtonStyle: buttonStyle,
          onDateAccept: accept,
          onDidDisappear: done,
        };
        this.ui.showDatePickerDialog(dateOptions);
      }
    } catch (error) {
      done();
      console.error("NSH native date/time picker", (error as Error).message);
    }
  }
  dispose(): void {
    this.closed = true;
    this.mounted.delete(this);
    this.builder.dispose();
  }
}
