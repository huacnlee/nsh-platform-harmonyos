import { expect, open, test } from "nsh/test";

test("native input preserves Unicode, shell characters, paste and submit", async () => {
  const app = await open("./App.tsx");
  await app.clear("id:draft");
  await app.type("id:draft", "你好 ' $HOME; $(id)");
  expect(app.value("id:draft")).toBe("你好 ' $HOME; $(id)");
  await app.clear("id:draft");
  await app.paste("id:draft", { text: "pasted" });
  expect(app.value("id:draft")).toBe("pasted");
  await app.submit("id:draft");
  await app.waitFor("Action: submit pasted");
});

test("canvas RichEditor supports typing, clearing and pasting", async () => {
  const app = await open("./App.tsx");
  await app.type("id:rich", "## Heading");
  await app.waitFor("Heading");
  await app.clear("id:rich");
  expect(app.shows("Heading")).toBe(false);
  await app.paste("id:rich", { text: "Pasted block" });
  await app.waitFor("Pasted block");
});

test("long presses use the view's delay", async () => {
  const app = await open("./App.tsx");
  await app.longPress("Hold me");
  await app.waitFor("Action: held");
});

test("drag moves a pan view", async () => {
  const app = await open("./App.tsx");
  await app.drag("id:pad", { x: 100, y: 0 });
  await app.waitFor("Action: drag right");
});

test("setColorScheme switches the device appearance", async () => {
  const app = await open("./App.tsx", { colorScheme: "dark" });
  await app.waitFor("Scheme: dark");
  await app.setColorScheme("light");
  await app.waitFor("Scheme: light");
});

test("a selector scrolls an offscreen row into view", async () => {
  const app = await open("./App.tsx");
  await app.press("Row 12");
  await app.waitFor("Action: row 12");
});

test("network routes and seeded storage reach the device", async () => {
  const app = await open("./App.tsx", {
    storage: { seed: "provided by the test" },
    network: { "https://api.test/probe": { body: "mocked" } },
  });
  await app.waitFor("Seed: provided by the test");
  await app.waitFor("Response: mocked");
  await app.press("Save");
  expect(app.storage().seed).toBe("changed");
});

test("the next open starts with empty storage", async () => {
  const app = await open("./App.tsx");
  await app.waitFor("Seed: empty");
  expect(app.storage().seed).toBe(undefined);
});

test("a manual clock controls application timers", async () => {
  const app = await open("./App.tsx", { clock: {} });
  expect(app.shows("Timer: pending")).toBe(true);
  await app.clock!.runFor(10000);
  await app.waitFor("Timer: done");
});
