// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { open, test } from "nsh/test";

test("pixel buffers reach the screen every frame", async () => {
  const app = await open(".");
  // Three digits: a hundred frames went through the image element.
  await app.waitFor("text:/^frame \\d{3,} · \\d+ fps$/");
});