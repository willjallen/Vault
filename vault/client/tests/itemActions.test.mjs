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
const { docToItem, folderToItem } = await import(moduleUrl);

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
