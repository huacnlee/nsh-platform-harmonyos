import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";

const ets = await readFile(new URL("../src/main/ets/NativeEditors.ets", import.meta.url), "utf8");
const production = ets.slice(ets.indexOf("class EditorBuildCallback extends FrameCallback"));
assert.ok(production.startsWith("class EditorBuildCallback"));
const js = stripTypeScriptTypes(`
  class FrameCallback {}
  class EditorEntry {
    constructor(state, context) { this.state = state; this.calls = context.calls; }
    create() { this.calls.push(["create", this.state.targetId, this.state.text]); }
    update(state) { this.calls.push(["update", state.targetId, state.text]); }
    command(cmd) { this.calls.push([cmd.kind, cmd.targetId]); }
    dispose() { this.calls.push(["dispose", this.state.targetId]); }
  }
  ${production}`, { mode: "transform" });
const { NativeEditors } = await import(`data:text/javascript;base64,${Buffer.from(js).toString("base64")}`);
function session() {
  const callbacks = [];
  const calls = [];
  const sdk = { postFrameCallback(cb) { callbacks.push(cb); } };
  const context = { calls, isActive: () => true, getUIContext: () => sdk };
  return { editors: new NativeEditors(), calls, callbacks, context };
}
const field = (id, belowViewport = true, text = "") => ({ targetId: id, belowViewport, text });

test("visible and autofocus editors mount immediately, with the latest offscreen state built at idle", () => {
  const { editors, calls, callbacks, context } = session();
  editors.update([field(1, false), field(2), { ...field(3), autoFocus: true }], [], context);
  editors.update([field(2, true, "new")], [], context);
  assert.deepEqual(calls, [["create", 1, ""], ["create", 3, ""]]);
  assert.equal(callbacks.length, 1);
  callbacks.shift().onIdle(0);
  assert.deepEqual(calls.at(-1), ["create", 2, "new"]);
  assert.equal(callbacks.length, 0);
});

test("a command promotes its editor before delivery and retirement cancels pending work", () => {
  const { editors, calls, callbacks, context } = session();
  editors.update([field(1), field(2), field(3)], [{ targetId: 1, kind: "focus" }], context);
  editors.remove([2], context);
  editors.dispose(context);
  callbacks.shift().onIdle(4000000);
  assert.deepEqual(calls, [["create", 1, ""], ["focus", 1], ["dispose", 1]]);
  assert.equal(callbacks.length, 0);
});

test("a busy frame makes bounded progress and scrolling a pending editor into view builds it once", () => {
  const { editors, calls, callbacks, context } = session();
  editors.update([field(1), field(2), field(3)], [], context);
  callbacks.shift().onIdle(0);
  assert.deepEqual(calls, [["create", 1, ""]]);
  editors.update([field(3, false)], [], context);
  callbacks.shift().onIdle(0);
  assert.deepEqual(calls, [["create", 1, ""], ["create", 3, ""], ["create", 2, ""]]);
});
