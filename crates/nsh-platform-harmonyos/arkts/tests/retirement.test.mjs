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
