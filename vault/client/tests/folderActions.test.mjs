import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/folderActions.js", import.meta.url);
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
const { createFolderActionHandlers } = await import(moduleUrl);

function folderHandlers(overrides = {}) {
  return createFolderActionHandlers({
    apiFetch: async () => ({ ok: true }),
    folder: "Project",
    handleArchiveItems: async () => true,
    inlineFolderDraft: null,
    postAction: async () => ({ failed: [] }),
    refresh: async () => {},
    refreshAfterAction: async () => {},
    replaceFolder: () => {},
    setBusy: () => {},
    setCreatingFolder: () => {},
    setError: () => {},
    setInlineFolderDraft: () => {},
    setSelectedId: () => {},
    ...overrides,
  });
}

test("create folder posts urlencoded form data", async () => {
  let request = null;
  const handlers = folderHandlers({
    apiFetch: async (url, options) => {
      request = { options, url };
      return { ok: true };
    },
  });

  const created = await handlers.handleCreateFolder("New Folder", "Project");

  assert.equal(created, true);
  assert.equal(request.url, "/folders");
  assert.equal(request.options.method, "POST");
  assert.ok(request.options.body instanceof URLSearchParams);
  assert.equal(request.options.body.toString(), "folder=Project%2FNew+Folder");
});
