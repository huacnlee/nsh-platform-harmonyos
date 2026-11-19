import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";

// Inject the SDK dependency and execute the production factory, without copying
// its lifecycle logic into the test. This module uses ordinary TypeScript.
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
  let running = false;
  let range;
  let rangeError;
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
    setExpectedFrameRateRange(value) {
      record("range", { ...value });
      if (rangeError) throw rangeError;
      range = { ...value };
    },
    start() {
      record("start");
      running = true;
    },
    stop() {
      record("stop");
      running = false;
      if (stopError) throw stopError;
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
  const request = bindDisplaySync({
    create() {
      record("create");
      return native;
    },
  })(ui, { frame });
  return {
    request,
    calls,
    get running() { return running; },
    get subscribed() { return listener !== undefined; },
    get range() { return range; },
    emit(timestamp) {
      assert.ok(running, "only a started SDK source delivers frames");
      assert.equal(typeof listener, "function");
      // SDK frame delivery already belongs to the source's UIContext.
      return ui.runScopedTask(() => listener({ timestamp }));
    },
    failRange(error) { rangeError = error; },
    failStop(error) { stopError = error; },
  };
}

const enable = Object.freeze({ enabled: true, close: false, min: 0, max: 120, expected: 120 });
const disable = Object.freeze({ enabled: false, close: false, min: 0, max: 0, expected: 0 });
const close = Object.freeze({ ...disable, close: true });
const names = (h) => h.calls.map((call) => call.name);

test("creation is idle and Rust's range reaches the system divider before start", () => {
  for (const range of [
    { min: 0, max: 30, expected: 30 },
    { min: 0, max: 60, expected: 30 },
    { min: 60, max: 120, expected: 120 },
    { min: 0, max: 144, expected: 90 },
  ]) {
    const h = harness();
    assert.deepEqual(names(h), ["create", "on"]);
    assert.equal(h.running, false);
    const request = Object.freeze({ ...enable, ...range });
    h.request(request);
    assert.deepEqual(h.range, range, "the adapter does not independently arbitrate or clamp");
    assert.deepEqual(request, { ...enable, ...range }, "the shared request is not mutated");
    assert.equal(h.running, true);
    assert.deepEqual(names(h), ["create", "on", "range", "start"]);
  }
});

test("a new Rust range reconfigures the same system source without a second clock", () => {
  const timestamps = [];
  const h = harness((timestamp) => { timestamps.push(timestamp); return true; });
  h.request(enable);
  h.emit(100000000);
  h.request({ ...enable, max: 30, expected: 30 });
  assert.deepEqual(h.range, { min: 0, max: 30, expected: 30 });
  // The framework chooses callback cadence. Every delivered timestamp goes to
  // Rust, including adjacent deliveries; the adapter has no extra divider.
  h.emit(108333333);
  h.emit(141666667);
  assert.deepEqual(timestamps, [100000000, 108333333, 141666667]);
  assert.deepEqual(names(h), ["create", "on", "range", "start", "range", "start"]);
});

test("pause and resume reuse the source and run lifecycle operations in its UIContext", () => {
  const h = harness();
  h.request(enable);
  h.request(disable);
  assert.equal(h.running, false);
  assert.equal(h.subscribed, true);
  h.request(enable);
  assert.equal(h.running, true);
  h.request(close);
  assert.deepEqual(names(h), ["create", "on", "range", "start", "stop", "range", "start", "stop", "off"]);
  for (const call of h.calls.filter((call) => call.name !== "off")) {
    assert.equal(call.scope, 1, `${call.name} must use the owning UIContext`);
  }
});

test("stop and close never call the rate setter", () => {
  const h = harness();
  h.request(enable);
  h.failRange(new Error("rate changes are forbidden while stopping"));
  h.request(disable);
  h.request({ ...enable, close: true });
  assert.equal(h.running, false);
  assert.equal(h.subscribed, false);
  assert.deepEqual(names(h), ["create", "on", "range", "start", "stop", "stop", "off"]);
});

test("a failed range setter does not start the source and the same request can retry", () => {
  const failure = new Error("SDK rate setter failed");
  const h = harness();
  h.failRange(failure);
  assert.throws(() => h.request(enable), (error) => error === failure);
  assert.equal(h.running, false);
  assert.equal(h.range, undefined);
  assert.equal(h.subscribed, true);
  assert.deepEqual(names(h), ["create", "on", "range"]);
  h.failRange(undefined);
  h.request(enable);
  assert.equal(h.running, true);
  assert.deepEqual(h.range, { min: 0, max: 120, expected: 120 });
  assert.deepEqual(names(h), ["create", "on", "range", "range", "start"]);
});

test("timestamps reach the callback unchanged and false stops until fresh demand", () => {
  const timestamps = [];
  const h = harness((timestamp) => {
    timestamps.push(timestamp);
    return timestamps.length === 1;
  });
  h.request(enable);
  h.emit(123456789);
  assert.equal(h.running, true);
  h.emit(131790122);
  assert.equal(h.running, false);
  assert.equal(h.subscribed, true);
  h.request(enable);
  h.emit(400000001);
  assert.deepEqual(timestamps, [123456789, 131790122, 400000001]);
  assert.equal(h.running, false);
  assert.equal(h.calls.filter((call) => call.name === "stop").length, 2);
});

test("a throwing callback stops delivery and preserves the error", () => {
  const failure = new Error("frame failed");
  const h = harness(() => { throw failure; });
  h.request(enable);
  assert.throws(() => h.emit(123), (error) => error === failure);
  assert.equal(h.running, false);
  assert.equal(h.calls.filter((call) => call.name === "stop").length, 1);
});

test("close unsubscribes once and takes precedence over enabled", () => {
  const h = harness();
  h.request(enable);
  h.request({ ...enable, close: true });
  assert.equal(h.running, false);
  assert.equal(h.subscribed, false);
  assert.deepEqual(names(h).slice(-2), ["stop", "off"]);
  const closedCalls = [...h.calls];
  h.request(close);
  h.request(enable);
  h.request(disable);
  assert.deepEqual(h.calls, closedCalls);
});

test("close before the first frame leaves no live subscription", () => {
  const h = harness(() => assert.fail("a closed source must not call Rust"));
  h.request(close);
  assert.equal(h.running, false);
  assert.equal(h.subscribed, false);
  h.request(enable);
  assert.deepEqual(names(h), ["create", "on", "stop", "off"]);
});

test("close still unsubscribes and cannot restart when SDK stop throws", () => {
  const failure = new Error("SDK stop failed");
  const h = harness();
  h.request(enable);
  h.failStop(failure);
  assert.throws(() => h.request(close), (error) => error === failure);
  assert.equal(h.subscribed, false);
  assert.deepEqual(names(h).slice(-2), ["stop", "off"]);
  const closedCalls = [...h.calls];
  h.request(enable);
  h.request(close);
  assert.deepEqual(h.calls, closedCalls);
});

test("synchronous close from a frame callback cannot restart or stop a disposed source", () => {
  let request;
  const h = harness(() => {
    request(close);
    return false;
  });
  request = h.request;
  request(enable);
  h.emit(123);
  assert.equal(h.running, false);
  assert.equal(h.subscribed, false);
  assert.deepEqual(names(h), ["create", "on", "range", "start", "stop", "off"]);
});
