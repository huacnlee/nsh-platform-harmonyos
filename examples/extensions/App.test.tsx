import { expect, open, test } from "nsh/test";

test("HarmonyBridge module works from TSX with host fallbacks", async () => {
  const app = await open("./App.tsx");
  await app.waitFor("host · unknown  (API 0)");
  await app.press("Show Native ArkTS Toast");
  await app.waitFor("Toast shown via uiContext.getPromptAction().showToast");
  await app.press("Encode Sample Image (JPEG 80%)");
  await app.waitFor("Prepared image/png (1×1, 68 bytes)");
  expect(app.shows("HarmonyOS Extensions Bridge")).toBe(true);
});
