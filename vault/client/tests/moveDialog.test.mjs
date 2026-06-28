import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/useMoveDialog.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});

const initialStates = [
  {
    folder: "Project",
    id: 1,
    name: "Asset.fbx",
    path: "Project/Asset.fbx",
    type: "doc",
  },
  "Project",
  "New Folder",
  false,
];

globalThis.React = {
  useState(initialValue) {
    const index = globalThis.React.stateIndex;
    globalThis.React.stateIndex += 1;
    return [
      initialStates[index] ?? initialValue,
      (nextValue) => {
        globalThis.React.stateUpdates[index] = nextValue;
      },
    ];
  },
  stateIndex: 0,
  stateUpdates: [],
};

const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { useMoveDialog } = await import(moduleUrl);

test("move dialog creates folders with urlencoded form data", async () => {
  let request = null;
  const dialog = useMoveDialog({
    apiFetch: async (url, options) => {
      request = { options, url };
      return { ok: true };
    },
    folder: "Project",
    handleMove: async () => true,
    handleMoveSelection: async () => true,
    handleRenameFolder: async () => true,
    refresh: async () => {},
    setError: () => {},
    setSelectedId: () => {},
  });

  await dialog.handleCreateMoveFolder();

  assert.equal(request.url, "/folders");
  assert.equal(request.options.method, "POST");
  assert.ok(request.options.body instanceof URLSearchParams);
  assert.equal(request.options.body.toString(), "folder=Project%2FNew+Folder");
});
