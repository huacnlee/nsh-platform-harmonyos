import { expect, open, test } from "nsh/test";

test("system alert keeps the original destructive action index", async () => {
  const app = await open("./Dialogs.tsx");
  app.onDialog(() => "关闭全部");
  await app.press("Open alert");
  await app.waitFor("关闭全部连接？: close");
});

test("system alert cancel is an action and back is dismissal", async () => {
  const app = await open("./Dialogs.tsx");
  app.onDialog(() => "取消");
  await app.press("Open alert");
  await app.waitFor("关闭全部连接？: cancel");
  app.onDialog(() => null);
  await app.press("Open alert");
  await app.waitFor("关闭全部连接？: dismissed");
});

test("system sheet preserves indices when moving cancel to its footer", async () => {
  const app = await open("./Dialogs.tsx");
  app.onDialog(() => "查看详情");
  await app.press("Open sheet");
  await app.waitFor("关闭全部连接？: details");
  app.onDialog(() => "取消");
  await app.press("Open sheet");
  await app.waitFor("关闭全部连接？: cancel");
});

test("system dialogs advance the queue after disappearing", async () => {
  const app = await open("./Dialogs.tsx");
  const titles: string[] = [];
  app.onDialog((dialog) => { titles.push(dialog.title!); return "关闭全部"; });
  await app.press("Queue two alerts");
  await app.waitFor("Second: close");
  expect(titles).toEqual(["First", "Second"]);
});
