import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/contextMenus.js", import.meta.url);
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
const { buildFileMenuItems, buildPageMenuItems } = await import(moduleUrl);

function fileMenuItemsFor(doc) {
  return buildFileMenuItems({
    busy: false,
    currentUser: { id: "user" },
    doc,
    handleArchive: () => {},
    handlePermanentDelete: () => {},
    handleRemoveFavoriteItem: () => {},
    handleRenameFile: () => {},
    handleShareItem: () => {},
    handleUnarchive: () => {},
    handleVersionUploadClick: () => {},
    handleView: () => {},
    openFileDetails: () => {},
    openMoveDialogForDoc: () => {},
    siteSettings: {},
  });
}

test("archived file rename action is disabled", () => {
  const items = fileMenuItemsFor({
    access: {},
    archived: true,
    favorite: false,
    id: 1,
    lock: {},
    name: "archived.txt",
    type: "document",
  });

  assert.equal(items.find((item) => item.label === "Rename")?.disabled, true);
});

test("active file rename action remains enabled", () => {
  const items = fileMenuItemsFor({
    archived: false,
    favorite: false,
    id: 1,
    lock: {},
    name: "active.txt",
    type: "document",
  });

  assert.equal(items.find((item) => item.label === "Rename")?.disabled, false);
});

test("page upload stays available during another background upload", () => {
  const items = buildPageMenuItems({
    beginCreateFolder: () => {},
    busy: false,
    creatingFolder: false,
    folder: "",
    handleUploadClick: () => {},
    uploading: true,
  });

  assert.equal(items.find((item) => item.label === "Upload file")?.disabled, false);
});

test("page actions remain disabled during foreground busy operations", () => {
  const items = buildPageMenuItems({
    beginCreateFolder: () => {},
    busy: true,
    creatingFolder: false,
    folder: "",
    handleUploadClick: () => {},
  });

  assert.equal(items.find((item) => item.label === "Upload file")?.disabled, true);
  assert.equal(items.find((item) => item.label === "New folder")?.disabled, true);
});
