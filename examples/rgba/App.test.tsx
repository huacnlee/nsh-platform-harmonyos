// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { open, test } from "nsh/test";

test("animation-loop liveness during RGBA updates", async () => {
  const app = await open(".");
  // The counter is advanced before each image update. This proves the
  // animation loop stays live, but does not independently assert painted pixels.
  await app.waitFor("text:/^frame \\d{3,} · \\d+ fps$/");
});
