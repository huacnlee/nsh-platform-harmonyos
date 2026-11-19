import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";

// Execute production NativeMobileControl & NativeAlerts logic without duplicating it.
const mobileSrc = await readFile(
  new URL("../src/main/ets/NativeMobileControl.ets", import.meta.url),
  "utf8",
);
const mobileHelpers = mobileSrc.slice(
  mobileSrc.indexOf("interface FieldChange"),
  mobileSrc.indexOf("@Builder\nfunction mobileControl"),
);
const mobileClass = mobileSrc.slice(mobileSrc.indexOf("export class NativeMobileControl"));
const mobileJs = stripTypeScriptTypes(
  `
  const $r = (s) => ({ resource: s });
  const wrapBuilder = (fn) => fn;
  const mobileControl = () => {};
  const ThemeColorMode = { SYSTEM: 0, LIGHT: 1, DARK: 2 };
  const BlurStyle = { COMPONENT_ULTRA_THICK: 5 };
  const DialogAlignment = { Default: 3, Bottom: 2 };
  const FontWeight = { Regular: 400, Medium: 500 };
  const ButtonStyleMode = { NORMAL: 0, EMPHASIZED: 1, TEXTUAL: 2 };
  let localeValue = "en-US";
  let is24Hour = true;
  const i18n = {
    System: {
      getSystemLocaleInstance: () => ({ toString: () => localeValue }),
      is24HourClock: () => is24Hour,
    },
  };
  class BuilderNode {
    constructor(ui) {
      this.ui = ui;
      this.updates = [];
      this.disposed = false;
      this.frameNode = {
        measures: 0,
        measure() { this.measures += 1; },
        getMeasuredSize() { return { width: 200, height: 80 }; },
      };
    }
    build(_builder, state) { this.updates.push(["build", state]); }
    update(state) { this.updates.push(["update", state]); }
    updateConfiguration() { this.updates.push(["config"]); }
    getFrameNode() { return this.frameNode; }
    dispose() { this.disposed = true; }
  }
  ${mobileHelpers}
  ${mobileClass}
  export { ThemeColorMode, BlurStyle, DialogAlignment, FontWeight, ButtonStyleMode, clampDate };
`,
  { mode: "transform" },
);
const { NativeMobileControl, ThemeColorMode, BlurStyle, DialogAlignment, FontWeight, ButtonStyleMode } = await import(
  `data:text/javascript;base64,${Buffer.from(mobileJs).toString("base64")}`
);

function makeMobileHost(name) {
  const sizes = [];
  const changes = [];
  const dialogs = [];
  const ui = {
    px2vp: (px) => px / 2,
    showDatePickerDialog: (opts) => dialogs.push(["date", opts]),
    showTimePickerDialog: (opts) => dialogs.push(["time", opts]),
  };
  const context = {
    name,
    size: (w, h) => sizes.push([w, h]),
    valueChanged: (v) => changes.push(v),
    change: () => {},
    height: () => {},
  };
  const hostWindow = {
    getWindowProperties: () => ({ windowRect: { width: 1080, height: 2400 } }),
  };
  const mounted = new Set();
  const control = new NativeMobileControl(ui, context, hostWindow, mounted, ThemeColorMode.SYSTEM);
  return { control, sizes, changes, dialogs, mounted };
}

test("switch measures once on mount, skips measure on checked/revision/theme flips, and remeasures on refresh", () => {
  const { control, sizes } = makeMobileHost("nsh/switch");
  assert.equal(control.node.measures, 1);
  assert.deepEqual(sizes, [[100, 40]]);

  control.updateProps([
    { name: "checked", value: true, removed: false },
    { name: "revision", value: 1, removed: false },
  ]);
  control.setColorMode(ThemeColorMode.DARK);
  assert.equal(control.node.measures, 1);

  control.refresh();
  assert.equal(control.node.measures, 2);
});

test("date-time-picker measures once when initial updateProps arrives synchronously in the mount batch", async () => {
  const { control } = makeMobileHost("nsh/date-time-picker");
  assert.equal(control.node.measures, 0);

  control.updateProps([
    { name: "mode", value: "date", removed: false },
    { name: "value", value: { year: 2026, month: 11, day: 19 }, removed: false },
  ]);
  assert.equal(control.node.measures, 1);

  // Microtask fallback must not re-measure once already measured.
  await Promise.resolve();
  assert.equal(control.node.measures, 1);

  // Unchanged formatted text skips redundant measure.
  control.updateProps([{ name: "disabled", value: true, removed: false }]);
  assert.equal(control.node.measures, 1);

  // Changed date text triggers a new measure.
  control.updateProps([
    { name: "value", value: { year: 2026, month: 12, day: 25 }, removed: false },
  ]);
  assert.equal(control.node.measures, 2);
});

test("date-time-picker falls back to microtask measure if no updateProps arrives", async () => {
  const { control } = makeMobileHost("nsh/date-time-picker");
  assert.equal(control.node.measures, 0);
  await Promise.resolve();
  assert.equal(control.node.measures, 1);
});

test("date-time-picker clamps selected date into [min, max] and forwards HOS dialog options and colorMode", () => {
  const { control, dialogs } = makeMobileHost("nsh/date-time-picker");
  control.setColorMode(ThemeColorMode.DARK);
  control.updateProps([
    { name: "mode", value: "dateTime", removed: false },
    { name: "value", value: { year: 2020, month: 1, day: 1, hour: 8, minute: 0 }, removed: false },
    { name: "min", value: { year: 2025, month: 6, day: 15, hour: 9, minute: 30 }, removed: false },
    { name: "max", value: { year: 2027, month: 12, day: 31, hour: 18, minute: 0 }, removed: false },
  ]);
  control.pick();
  assert.equal(dialogs.length, 1);
  const [kind, opts] = dialogs[0];
  assert.equal(kind, "date");
  assert.equal(opts.showTime, true);
  assert.equal(opts.alignment, DialogAlignment.Bottom);
  assert.deepEqual(opts.offset, { dx: 0, dy: -16 });
  assert.equal(opts.enableHapticFeedback, true);
  assert.equal(opts.backgroundBlurStyle, BlurStyle.COMPONENT_ULTRA_THICK);
  assert.deepEqual(opts.backgroundBlurStyleOptions, { colorMode: ThemeColorMode.DARK });
  assert.deepEqual(opts.disappearTextStyle, {
    color: { resource: "sys.color.font_tertiary" },
    font: { size: { resource: "sys.float.Body_S" }, weight: FontWeight.Regular },
  });
  assert.deepEqual(opts.textStyle, {
    color: { resource: "sys.color.font_primary" },
    font: { size: { resource: "sys.float.Body_M" }, weight: FontWeight.Regular },
  });
  assert.deepEqual(opts.selectedTextStyle, {
    color: { resource: "sys.color.font_emphasize" },
    font: { size: { resource: "sys.float.Body_L" }, weight: FontWeight.Medium },
  });
  assert.deepEqual(opts.acceptButtonStyle, {
    fontColor: { resource: "sys.color.font_emphasize" },
    fontSize: { resource: "sys.float.Body_M" },
    style: ButtonStyleMode.TEXTUAL,
  });
  assert.deepEqual(opts.cancelButtonStyle, opts.acceptButtonStyle);
  assert.equal(opts.selected.getFullYear(), 2025);
  assert.equal(opts.selected.getMonth(), 5);
  assert.equal(opts.selected.getDate(), 15);
});

const alertsSrc = await readFile(
  new URL("../src/main/ets/NativeAlerts.ets", import.meta.url),
  "utf8",
);
const alertsTail =
  alertsSrc.slice(alertsSrc.indexOf("function parseColorMode"), alertsSrc.indexOf("@ComponentV2")) +
  "\n" +
  alertsSrc.slice(alertsSrc.indexOf("export function showNativeAlert"));
const alertsJs = stripTypeScriptTypes(
  `
  const $r = (s) => ({ resource: s });
  const wrapBuilder = (fn) => fn;
  const nativeAlert = () => {};
  const ThemeColorMode = { SYSTEM: 0, LIGHT: 1, DARK: 2 };
  const ButtonRole = { NORMAL: 0, ERROR: 1 };
  const SheetSize = { FIT_CONTENT: "FIT_CONTENT" };
  class AdvancedDialogV2Button { constructor(cfg) { Object.assign(this, cfg); } }
  class ComponentContent {
    constructor(_ui, _builder, content) { this.content = content; this.disposed = false; }
    dispose() { this.disposed = true; }
  }
  ${alertsTail}
  export { ThemeColorMode, ButtonRole };
`,
  { mode: "transform" },
);
const {
  showNativeAlert,
  ButtonRole,
  ThemeColorMode: AlertColorMode,
} = await import(`data:text/javascript;base64,${Buffer.from(alertsJs).toString("base64")}`);

test("showNativeAlert configures HOS ActionSheet options and maps button style and appearance", () => {
  const opened = [];
  const ui = {
    getPromptAction: () => ({
      openCustomDialog: (view, opts) => {
        opened.push(["dialog", view, opts]);
        return Promise.resolve();
      },
      closeCustomDialog: () => Promise.resolve(),
    }),
    openBindSheet: (view, opts) => {
      opened.push(["sheet", view, opts]);
      return Promise.resolve();
    },
    closeBindSheet: () => Promise.resolve(),
  };

  showNativeAlert({
    ui,
    title: "Share photo",
    message: "Choose a destination",
    autoCancel: true,
    sheet: true,
    appearance: "dark",
    sheets: [{ title: "Delete", style: "destructive", action: () => {} }],
    confirm: { value: "Cancel", style: "cancel", action: () => {} },
    onDidDisappear: () => {},
  });

  assert.equal(opened.length, 1);
  const [kind, view, opts] = opened[0];
  assert.equal(kind, "sheet");
  assert.equal(opts.title, undefined);
  assert.equal(opts.dragBar, false);
  assert.deepEqual(opts.backgroundColor, { resource: "sys.color.background_secondary" });
  assert.equal(opts.showClose, false);
  assert.equal(view.content.hasHeader, true);
  assert.equal(view.content.colorMode, AlertColorMode.DARK);
  assert.equal(view.content.actions.length, 1);
  assert.equal(view.content.cancel.value, "Cancel");

  showNativeAlert({
    ui,
    title: "Remove item?",
    message: "This cannot be undone.",
    autoCancel: false,
    sheet: false,
    appearance: "light",
    buttons: [
      { value: "Cancel", style: "cancel", action: () => {} },
      { value: "Delete", style: "destructive", action: () => {} },
    ],
    onDidDisappear: () => {},
  });

  const [dKind, dView] = opened[1];
  assert.equal(dKind, "dialog");
  assert.equal(dView.content.colorMode, AlertColorMode.LIGHT);
  assert.equal(dView.content.buttons[0].role, ButtonRole.NORMAL);
  assert.equal(dView.content.buttons[1].role, ButtonRole.ERROR);
});
