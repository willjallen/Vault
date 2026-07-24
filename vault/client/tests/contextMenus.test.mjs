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
const { buildFileMenuItems, buildFolderMenuItems, buildPageMenuItems } = await import(moduleUrl);

function fileMenuItemsFor(doc, overrides = {}) {
  return buildFileMenuItems({
    busy: false,
    currentUser: { id: "user" },
    doc,
    handleArchive: () => {},
    handleLock: () => {},
    handlePermanentDelete: () => {},
    handleRelease: () => {},
    handleRemoveFavoriteItem: () => {},
    handleRenameFile: () => {},
    handleShareItem: () => {},
    handleUnarchive: () => {},
    handleVersionUploadClick: () => {},
    handleView: () => {},
    openFileDetails: () => {},
    openMoveDialogForDoc: () => {},
    siteSettings: {},
    ...overrides,
  });
}

function folderMenuItemsFor(folderItem, overrides = {}) {
  return buildFolderMenuItems({
    beginRenameFolder: () => {},
    busy: false,
    folderItem,
    handleArchiveFolder: () => {},
    handleDeleteForeverItems: () => {},
    handleDeleteEmptyFolder: () => {},
    handleDownloadSelection: () => {},
    handleRestoreItems: () => {},
    handleShareItem: () => {},
    navigateToFolder: () => {},
    openFolderProperties: () => {},
    openMoveDialogForFolder: () => {},
    ...overrides,
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

test("active file context actions follow the requested order", () => {
  const items = fileMenuItemsFor({
    archived: false,
    favorite: false,
    id: 1,
    lock: {},
    name: "active.txt",
    type: "document",
  });

  assert.deepEqual(
    items.map((item) => item.label),
    ["Download", "Replace", "Rename", "Lock", "Share", "History", "Move...", "Archive"]
  );
});

test("file context lock action locks unlocked files and unlocks owned locks", () => {
  const doc = {
    archived: false,
    favorite: false,
    id: 7,
    lock: {},
    name: "active.txt",
    type: "document",
  };
  let locked = null;
  let released = null;
  const unlockedItems = fileMenuItemsFor(doc, {
    handleLock: (item) => {
      locked = item;
    },
  });
  unlockedItems.find((item) => item.label === "Lock").action();
  assert.equal(locked, doc);

  const ownedItems = fileMenuItemsFor(
    { ...doc, lock: { by: "user", name: "Current User" } },
    {
      handleRelease: (id) => {
        released = id;
      },
    }
  );
  ownedItems.find((item) => item.label === "Unlock").action();
  assert.equal(released, doc.id);
});

test("file context lock action explains locks owned by another user", () => {
  const doc = {
    archived: false,
    favorite: false,
    id: 7,
    lock: { by: "other", name: "Grace" },
    name: "active.txt",
    type: "document",
  };
  const items = fileMenuItemsFor(doc);
  const lockItem = items.find((item) => item.label === "Unlock");

  assert.equal(lockItem.disabled, true);
  assert.equal(lockItem.note, "Locked by Grace");

  let released = null;
  const adminItems = fileMenuItemsFor(doc, {
    handleRelease: (id) => {
      released = id;
    },
    isAdmin: true,
  });
  const adminUnlock = adminItems.find((item) => item.label === "Unlock");
  assert.equal(adminUnlock.disabled, false);
  adminUnlock.action();
  assert.equal(released, doc.id);
});

test("empty active folder uses the same archive lifecycle as every other folder", () => {
  const folderItem = {
    archived: false,
    can_delete_empty: true,
    id: 17,
    name: "Empty",
    path: "Projects/Empty",
    type: "folder",
  };
  let archived = null;
  const items = folderMenuItemsFor(folderItem, {
    handleArchiveFolder: (path) => {
      archived = path;
    },
  });

  const archiveItem = items.find((item) => item.label === "Move to Archive");
  assert.equal(archiveItem?.disabled, false);
  assert.equal(
    items.some((item) => item.label === "Delete"),
    false
  );
  archiveItem.action();
  assert.equal(archived, folderItem.path);
});

test("nonempty active folder retains Move to Archive without Delete", () => {
  const items = folderMenuItemsFor({
    archived: false,
    can_delete_empty: false,
    id: 18,
    name: "Nonempty",
    path: "Projects/Nonempty",
    type: "folder",
  });

  assert.equal(
    items.some((item) => item.label === "Move to Archive"),
    true
  );
  assert.equal(
    items.some((item) => item.label === "Delete"),
    false
  );
});

test("direct archived folder exposes aggregate restore and permanent delete", () => {
  const folderItem = {
    access: { write: true },
    archived: true,
    directly_archived: true,
    id: 5,
    name: "Project",
    path: "Archive/@5~Project",
    type: "folder",
  };
  const restored = [];
  const deleted = [];
  const items = folderMenuItemsFor(folderItem, {
    handleDeleteForeverItems: (selection) => deleted.push(selection),
    handleRestoreItems: (selection) => restored.push(selection),
    isAdmin: true,
  });

  assert.deepEqual(
    items.map((item) => item.label),
    ["Download", "Open", "Restore to Vault", "Delete forever"]
  );
  items.find((item) => item.label === "Restore to Vault").action();
  items.find((item) => item.label === "Delete forever").action();
  assert.deepEqual(restored, [[folderItem]]);
  assert.deepEqual(deleted, [[folderItem]]);
});

test("inherited archived folder cannot be restored independently", () => {
  const items = folderMenuItemsFor({
    archived: true,
    directly_archived: false,
    id: 6,
    name: "Nested",
    path: "Archive/@5~Project/@6~Nested",
    type: "folder",
  });

  assert.equal(
    items.some((item) => item.label === "Restore to Vault"),
    false
  );
  assert.equal(
    items.some((item) => item.label === "Delete forever"),
    false
  );
});

test("picker capability keeps a single Download menu action", () => {
  const items = buildFileMenuItems({
    busy: false,
    currentUser: { id: "user" },
    doc: { archived: false, favorite: false, id: 1, lock: {}, name: "active.txt" },
    filePickerDownloadsAvailable: true,
    handleArchive: () => {},
    handleRenameFile: () => {},
    handleShareItem: () => {},
    handleVersionUploadClick: () => {},
    handleView: () => {},
    openMoveDialogForDoc: () => {},
    siteSettings: {},
  });

  assert.equal(items[0].label, "Download");
  assert.equal(items.filter((item) => /download/i.test(item.label)).length, 1);
});

test("page upload stays available during another background upload", () => {
  const selected = [];
  const items = buildPageMenuItems({
    beginCreateFolder: () => {},
    busy: false,
    creatingFolder: false,
    folder: "",
    handleUploadClick: () => selected.push("file"),
    handleUploadFolderClick: () => selected.push("folder"),
    uploading: true,
  });

  assert.equal(items.find((item) => item.label === "Upload file")?.disabled, false);
  assert.equal(items.find((item) => item.label === "Upload folder")?.disabled, false);
  items.find((item) => item.label === "Upload file").action();
  items.find((item) => item.label === "Upload folder").action();
  assert.deepEqual(selected, ["file", "folder"]);
});

test("page actions remain disabled during foreground busy operations", () => {
  const items = buildPageMenuItems({
    beginCreateFolder: () => {},
    busy: true,
    creatingFolder: false,
    folder: "",
    handleUploadClick: () => {},
    handleUploadFolderClick: () => {},
  });

  assert.equal(items.find((item) => item.label === "Upload file")?.disabled, true);
  assert.equal(items.find((item) => item.label === "Upload folder")?.disabled, true);
  assert.equal(items.find((item) => item.label === "New folder")?.disabled, true);
});

test("page mutations remain disabled throughout an archived folder subtree", () => {
  const items = buildPageMenuItems({
    beginCreateFolder: () => {},
    busy: false,
    creatingFolder: false,
    folder: "Archive/@5~Project/@6~Nested",
    handleUploadClick: () => {},
    handleUploadFolderClick: () => {},
  });

  assert.equal(
    items.every((item) => item.disabled),
    true
  );
});
