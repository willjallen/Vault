import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/itemActions.js", import.meta.url);
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
const { createBulkActionHandlers, docToItem, folderToItem } = await import(moduleUrl);

function bulkHandlers(downloadWithProgress) {
  return createBulkActionHandlers({
    apiFetch: async () => ({ json: async () => ({}), ok: true }),
    clearAllSelections: () => {},
    docs: [],
    downloadWithProgress,
    folder: "",
    refresh: async () => {},
    requestConfirm: async () => true,
    selectedDoc: null,
    setBusy: () => {},
    setDraggingFolderPath: () => {},
    setDraggingId: () => {},
    setDropHint: () => {},
    setError: () => {},
  });
}

test("document items preserve the namespaced visual descriptor", () => {
  const visual = {
    icon_key: "file",
    preview: {
      recipe: "raster-v1",
      status: "pending",
      variants: [],
      version_id: "version-1",
    },
  };
  assert.equal(docToItem({ id: 7, name: "asset.png", visual }).visual, visual);
  assert.equal(docToItem({ id: 8, name: "fallback.txt" }).visual, null);
});

test("folder items preserve only an explicit empty-folder delete capability", () => {
  assert.equal(
    folderToItem({ can_delete_empty: true, id: 1, path: "Empty" }).can_delete_empty,
    true
  );
  assert.equal(
    folderToItem({ can_delete_empty: false, id: 2, path: "Nonempty" }).can_delete_empty,
    false
  );
  assert.equal(
    folderToItem({ can_delete_empty: "true", id: 3, path: "Untrusted" }).can_delete_empty,
    false
  );
  assert.equal(folderToItem({ id: 4, path: "Unknown", size_bytes: 0 }).can_delete_empty, false);
});

test("archived folder items preserve stable identity and origin metadata", () => {
  const item = folderToItem({
    archived_at: "2026-07-24T12:00:00Z",
    archived_origin_path: "Projects/Incoming",
    directly_archived: true,
    id: 17,
    path: "Archive/@17~Incoming",
  });

  assert.equal(item.id, 17);
  assert.equal(item.name, "Incoming");
  assert.equal(item.archived, true);
  assert.equal(item.directly_archived, true);
  assert.equal(item.archived_origin_path, "Projects/Incoming");
});

test("each multi-item download action creates one export operation", async () => {
  const downloads = [];
  const handlers = bulkHandlers(async (options) => {
    downloads.push(options);
    return { status: 200 };
  });
  const firstSelection = [
    { id: 1, name: "one.txt", type: "document" },
    { id: 2, name: "two.txt", type: "document" },
    { id: 3, name: "Folder", path: "Folder", type: "folder" },
  ];
  const secondSelection = [
    { id: 4, name: "four.txt", type: "document" },
    { id: 5, name: "five.txt", type: "document" },
  ];

  await Promise.all([
    handlers.handleDownloadSelection(firstSelection),
    handlers.handleDownloadSelection(secondSelection),
  ]);

  assert.equal(downloads.length, 2);
  assert.deepEqual(
    downloads.map((download) => download.exportPayload.items.length),
    [3, 2]
  );
  assert.equal(
    downloads.every((download) => download.name === "vault-download.zip"),
    true
  );
});
