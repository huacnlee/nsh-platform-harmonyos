import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";

// These classes use ordinary TypeScript. Exercise the production methods with
// failing SDK owners; no ArkUI renderer or generated copy of their logic is used.
async function load(source) {
  const js = stripTypeScriptTypes(source, { mode: "transform" });
  return import(`data:text/javascript;base64,${Buffer.from(js).toString("base64")}`);
}
const source = new URL("../src/main/ets/", import.meta.url);
const { PointerPosting } = await load(await readFile(new URL("NativePointer.ets", source), "utf8"));
const views = await readFile(new URL("NativeViews.ets", source), "utf8");
const entry = views.slice(views.indexOf("class ViewEntry {"), views.indexOf("export class NativeViews"));
assert.ok(entry.startsWith("class ViewEntry {"));
const { ViewEntry, quarantinedViews } = await load(
  `export const quarantinedViews = new Set();\nexport ${entry}`,
);

function view(posting = new PointerPosting()) {
  const calls = { detach: 0, node: 0, texture: 0 };
  const instance = Object.create(ViewEntry.prototype);
  Object.assign(instance, {
    state: { settings: { targetId: 17 } },
    closed: false, attached: true, retiredNative: false,
    nodeDisposed: false, textureDisposed: false, sdkBusy: false, disposePending: false,
    pointerPosting: posting,
    context: { retireNativeNode() { calls.detach++; return true; } },
    node: { dispose() { calls.node++; } },
    textureController: { clear() {} },
    textureNode: { dispose() { calls.texture++; } },
  });
  return { instance, calls, posting };
}

test("retry only releases unfinished builders after successful native detach", () => {
  const { instance, calls } = view();
  instance.textureNode.dispose = () => {
    if (++calls.texture === 1) throw new Error("texture disposal failed once");
  };
  assert.throws(() => instance.dispose());
  assert.ok(quarantinedViews.has(instance));
  instance.dispose();
  instance.dispose();
  assert.deepEqual(calls, { detach: 1, node: 1, texture: 2 });
  assert.ok(instance.textureDisposed);
  assert.ok(!quarantinedViews.has(instance));
});

test("native detach rejection preserves builders and allows retry", () => {
  const { instance, calls } = view();
  instance.context.retireNativeNode = () => ++calls.detach > 1;
  assert.throws(() => instance.dispose());
  assert.deepEqual(calls, { detach: 1, node: 0, texture: 0 });
  instance.dispose();
  assert.deepEqual(calls, { detach: 2, node: 1, texture: 1 });
  assert.ok(!quarantinedViews.has(instance));
});

test("posting drains every retirement even when the first builder throws", () => {
  const posting = new PointerPosting();
  posting.busy = true;
  const first = view(posting);
  const second = view(posting);
  first.instance.textureNode.dispose = () => {
    if (++first.calls.texture === 1) throw new Error("first failed");
  };
  first.instance.dispose();
  second.instance.dispose();
  assert.equal(first.calls.node, 0);
  assert.equal(second.calls.node, 0);
  assert.throws(() => posting.finish());
  assert.deepEqual(second.calls, { detach: 1, node: 1, texture: 1 });
  assert.equal(posting.retirements.length, 0);
  assert.ok(quarantinedViews.has(first.instance));
  assert.ok(!quarantinedViews.has(second.instance));
  first.instance.dispose();
  assert.equal(first.calls.texture, 2);
  assert.ok(!quarantinedViews.has(first.instance));
});

test("SDK build/update deferral retains the owner until builder disposal finishes", () => {
  const { instance, calls } = view();
  instance.sdkBusy = true;
  instance.dispose();
  assert.ok(instance.disposePending);
  assert.ok(quarantinedViews.has(instance));
  assert.deepEqual(calls, { detach: 1, node: 0, texture: 0 });
  instance.sdkBusy = false;
  instance.disposePending = false;
  instance.disposeBuilders();
  assert.deepEqual(calls, { detach: 1, node: 1, texture: 1 });
  assert.ok(!quarantinedViews.has(instance));
});

test("synchronous detach reentry does not dispose a borrowed builder", () => {
  const { instance, calls } = view();
  instance.context.retireNativeNode = () => {
    calls.detach++;
    instance.dispose();
    assert.equal(calls.node, 0);
    return true;
  };
  instance.dispose();
  assert.deepEqual(calls, { detach: 1, node: 1, texture: 1 });
});

test("coordinate restoration failure still drains retirement callbacks", () => {
  const posting = new PointerPosting();
  posting.busy = true;
  let retired = false;
  posting.retirements.push(() => { retired = true; });
  assert.throws(() => posting.finish({ restore() { throw new Error("restore failed"); } }));
  assert.ok(retired);
  assert.equal(posting.busy, false);
});

test("texture wrapper update avoids rebuilding NodeContainer when capture dimensions match", () => {
  const { instance } = view();
  let outerUpdates = 0;
  let textureUpdates = 0;
  instance.node.update = () => { outerUpdates++; };
  instance.textureNode.update = () => { textureUpdates++; };
  instance.state.settings = {
    targetId: 17,
    width: 100,
    height: 80,
    textureInsets: { left: 4, top: 4, right: 4, bottom: 4 },
  };
  instance.update({
    targetId: 17,
    width: 100,
    height: 80,
    textureInsets: { left: 4, top: 4, right: 4, bottom: 4 },
    label: "changed-semantics-only",
  });
  assert.equal(textureUpdates, 1);
  assert.equal(outerUpdates, 0);

  instance.update({
    targetId: 17,
    width: 120,
    height: 80,
    textureInsets: { left: 4, top: 4, right: 4, bottom: 4 },
  });
  assert.equal(textureUpdates, 2);
  assert.equal(outerUpdates, 1);
});

const webSource = await readFile(new URL("NativeWebViews.ets", source), "utf8");
const webClasses = webSource.slice(webSource.indexOf("class WebMountFrame"), webSource.indexOf("export class NativeWebViews"));
const { WebEntry, resetWebMount } = await load(`class FrameCallback {}
  const webContent = () => {}, wrapBuilder = builder => builder;
  function ensureWebEngineInitialized() {}
  let webComponentMounted = false;
  const quarantinedWebs = new Set();
  export function resetWebMount() { webComponentMounted = false; }
  ${webClasses}
  export { WebEntry };`);

function coldWeb() {
  resetWebMount();
  const instance = Object.create(WebEntry.prototype);
  const calls = [];
  const frames = [];
  Object.assign(instance, {
    context: {
      getUIContext: () => ({ postFrameCallback: frame => frames.push(frame) }),
      isActive: () => true,
      attachNativeNode: () => { calls.push("attach"); return true; },
      retireNativeNode: () => { calls.push("retire"); return true; },
    },
    settings: { targetId: 17, sharedCookies: true },
    node: { build: () => calls.push("build"), getFrameNode: () => ({}), dispose: () => calls.push("dispose") },
    controller: { stop() {}, loadUrl() {} }, fileSelectors: new Set(), downloads: new Map(), epoch: 0,
  });
  return { instance, frames, calls };
}

test("a cold WebView attaches after the navigation frame returns", async () => {
  const { instance, frames, calls } = coldWeb();
  instance.create();
  frames[0].onFrame(0);
  assert.deepEqual(calls, []);
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(calls, ["build", "attach"]);
});

test("removal between the navigation frame and deferred WebView build cancels attachment", async () => {
  const { instance, frames, calls } = coldWeb();
  instance.create();
  frames[0].onFrame(0);
  instance.dispose();
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(calls, ["retire", "dispose"]);
});

test("a failed WebView mount retires its builder even if the error handler fails", () => {
  const { instance, calls } = coldWeb();
  instance.node.build = () => { throw new Error("build failed"); };
  instance.context.webEvent = () => { throw new Error("failed to report error"); };
  assert.throws(() => instance.mount(), /failed to report error/);
  assert.deepEqual(calls, ["retire", "dispose"]);
});

test("standalone ArkWeb initialization retries on first mount and runs once after success", async () => {
  const warmup = webSource.slice(webSource.indexOf("let webEngineInitialized"), webSource.indexOf("// Clear"));
  const sdk = await load(`
    export let calls = 0;
    const webview = { WebviewController: { initializeWebEngine() {
      if (++calls === 1) throw new Error("unavailable during startup");
    } } };
    const hiTraceMeter = { startTrace() {}, finishTrace() {} };
    const console = { warn() {} };
    ${warmup}
    export { ensureWebEngineInitialized };`);
  sdk.prepareWebEngine();
  assert.equal(sdk.calls, 1);
  sdk.ensureWebEngineInitialized();
  sdk.prepareWebEngine();
  sdk.ensureWebEngineInitialized();
  assert.equal(sdk.calls, 2);
});
