import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/browserDownload.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { canUseFileSystemDownloadWriter } = await import(moduleUrl);

test("picker availability requires both browser support and the site gate", () => {
  globalThis.window = { showSaveFilePicker: async () => {} };

  assert.equal(canUseFileSystemDownloadWriter(false), false);
  assert.equal(canUseFileSystemDownloadWriter(true), true);

  delete window.showSaveFilePicker;
  assert.equal(canUseFileSystemDownloadWriter(true), false);
});
