import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/IconLibrary.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(
  bundled.outputFiles.at(0).text
).toString("base64")}`;
const { findIconEntry } = await import(moduleUrl);

test("unknown semantic icon keys use the requested utility fallback", () => {
  assert.equal(findIconEntry("future-backend-key", "file").icon.iconName, "file");
  assert.equal(findIconEntry("future-backend-key", "folder").icon.iconName, "folder");
});
