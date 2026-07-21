import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/folderRequests.js", import.meta.url);
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
const { createFolderRequestOptions } = await import(moduleUrl);

test("ordinary folder creation remains strict", () => {
  const options = createFolderRequestOptions("Project/New Folder");

  assert.equal(options.method, "POST");
  assert.equal(options.body.toString(), "folder=Project%2FNew+Folder");
});

test("folder copies request idempotent directory creation", () => {
  const options = createFolderRequestOptions("Project/Existing", { allowExisting: true });

  assert.equal(options.method, "POST");
  assert.equal(options.body.toString(), "folder=Project%2FExisting&exist_ok=true");
});
