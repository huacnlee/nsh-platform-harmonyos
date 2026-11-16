import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";

// Inject only the SDK dependency. The factory and its callbacks below are the
// production implementation, stripped of types rather than copied into a test.
const source = await readFile(new URL("../src/main/ets/NativeFrames.ets", import.meta.url), "utf8");
const body = source.replace(/^import .*;\r?\n/gm, "").replace(/^export /gm, "");
const js = stripTypeScriptTypes(
  `export function bindDisplaySync(displaySync) {\n${body}\nreturn createFrameSource;\n}`,
  { mode: "transform" },
);
const { bindDisplaySync } = await import(`data:text/javascript;base64,${Buffer.from(js).toString("base64")}`);

function harness(frame = () => true) {
  const calls = [];
  let scope = 0;
  let listener;
  let stopError;
  const record = (name, value) => calls.push({ name, scope, value });
  const native = {
    on(event, callback) {
      assert.equal(event, "frame");
      record("on");
      listener = callback;
    },
    off(event) {
      assert.equal(event, "frame");
      record("off");
      listener = undefined;
    },
    setExpectedFrameRateRange(range) { record("range", range); },
    start() { record("start"); },
    stop() {
      record("stop");
      if (stopError) throw stopError;
    },
  };
  const displaySync = {
    create() {
      record("create");
      return native;
    },
  };
  const ui = {
    runScopedTask(task) {
      scope++;
      try {
        return task();
      } finally {
        scope--;
      }
    },
  };
  const request = bindDisplaySync(displaySync)(ui, { frame });
  return {
    request,
    calls,
    emit(timestamp) {
      assert.equal(typeof listener, "function");
      listener({ timestamp });
    },
    failStop(error) { stopError = error; },
  };
}

function enabled(range = { min: 0, max: 120, expected: 120 }) {
  return { enabled: true, close: false, ...range };
}

test("displaySync accepts supported rates and caps every range field at 144 Hz", () => {
  for (const [requested, expected] of [[15, 15], [60, 60], [120, 120], [144, 144], [165, 144], [240, 144]]) {
    const h = harness();
    const request = enabled({ min: requested, max: requested, expected: requested });
    const original = { ...request };
    h.request(request);
    assert.deepEqual(h.calls.find((call) => call.name === "range").value, {
      min: expected, max: expected, expected,
    }, `${requested} Hz`);
    assert.deepEqual(request, original, "the shared frame demand is not changed");
    assert.deepEqual(h.calls.map((call) => call.name), ["create", "on", "range", "start"]);
  }
  const h = harness();
  h.request(enabled({ min: 15, max: 240, expected: 120 }));
  assert.deepEqual(h.calls.find((call) => call.name === "range").value, {
    min: 15, max: 144, expected: 120,
  });
});

test("creation, range updates, start, and requested stops use the owning UIContext", () => {
  const h = harness();
  h.request(enabled());
  h.request({ ...enabled(), enabled: false });
  h.request(enabled({ min: 60, max: 60, expected: 60 }));
  h.request({ ...enabled(), close: true });
  assert.deepEqual(h.calls.map((call) => call.name), [
    "create", "on", "range", "start", "stop", "range", "start", "stop", "off",
  ]);
  for (const call of h.calls.filter((call) => call.name !== "off")) {
    assert.equal(call.scope, 1, `${call.name} must run in the supplied context`);
  }
});

test("frame timestamps reach the callback and a false result stops the source", () => {
  const timestamps = [];
  const h = harness((timestamp) => {
    timestamps.push(timestamp);
    return timestamps.length === 1;
  });
  h.request(enabled());
  h.emit(123456789);
  assert.equal(h.calls.filter((call) => call.name === "stop").length, 0);
  h.emit(131790122);
  assert.deepEqual(timestamps, [123456789, 131790122]);
  assert.equal(h.calls.filter((call) => call.name === "stop").length, 1);
});

test("a throwing frame callback still stops the source and preserves the error", () => {
  const failure = new Error("frame failed");
  const h = harness(() => { throw failure; });
  h.request(enabled());
  assert.throws(() => h.emit(123), (error) => error === failure);
  assert.equal(h.calls.filter((call) => call.name === "stop").length, 1);
});

test("close stops and unsubscribes once, and later requests cannot restart it", () => {
  const h = harness();
  h.request(enabled());
  h.request({ ...enabled(), close: true });
  assert.deepEqual(h.calls.slice(-2).map((call) => call.name), ["stop", "off"]);
  const closedCalls = [...h.calls];
  h.request({ ...enabled(), close: true });
  h.request(enabled());
  h.request({ ...enabled(), enabled: false });
  assert.deepEqual(h.calls, closedCalls);
});

test("close unsubscribes and remains closed even when SDK stop throws", () => {
  const failure = new Error("SDK stop failed");
  const h = harness();
  h.request(enabled());
  h.failStop(failure);
  assert.throws(() => h.request({ ...enabled(), close: true }), (error) => error === failure);
  assert.deepEqual(h.calls.slice(-2).map((call) => call.name), ["stop", "off"]);
  const closedCalls = [...h.calls];
  h.request(enabled());
  h.request({ ...enabled(), close: true });
  assert.deepEqual(h.calls, closedCalls);
});
