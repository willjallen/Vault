import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/dropHandlers.js", import.meta.url);
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
const { createDropHandlers } = await import(moduleUrl);

function makeFileList(files) {
  return Object.assign(
    {
      item: (index) => files.at(index) || null,
      length: files.length,
      [Symbol.iterator]: () => files[Symbol.iterator](),
    },
    files
  );
}

function dropEventFor(files) {
  let prevented = 0;
  return {
    dataTransfer: {
      files: makeFileList(files),
      getData: () => "",
      types: ["Files"],
    },
    get prevented() {
      return prevented;
    },
    preventDefault: () => {
      prevented += 1;
    },
  };
}

function dropHandlers(overrides = {}) {
  return createDropHandlers({
    docs: [],
    draggingFolderPath: null,
    draggingId: null,
    folder: "Shared/Incoming",
    handleArchive: () => {},
    handleArchiveFolder: () => {},
    handleArchiveItems: () => {},
    handleMove: () => {},
    handleMoveSelection: () => {},
    handleRenameFolder: () => {},
    handleUploadDrop: () => {},
    setDraggingFolderPath: () => {},
    setDraggingId: () => {},
    setDropHint: () => {},
    setError: () => {},
    setUploadHover: () => {},
    ...overrides,
  });
}

function uploadFiles() {
  return [
    new File(["first"], "first.txt", { type: "text/plain" }),
    new File(["second"], "second.txt", { type: "text/plain" }),
    new File(["third"], "third.txt", { type: "text/plain" }),
  ];
}

test("dropping external items on a folder forwards the intact data transfer", () => {
  const files = uploadFiles();
  const uploads = [];
  const event = dropEventFor(files);
  const handlers = dropHandlers({
    handleUploadDrop: (dataTransfer, targetFolder) => {
      uploads.push({ dataTransfer, targetFolder });
    },
  });

  handlers.handleDropOnFolder("Shared/Review", event, false);

  assert.equal(event.prevented, 1);
  assert.equal(uploads.length, 1);
  assert.equal(uploads[0].dataTransfer, event.dataTransfer);
  assert.equal(uploads[0].targetFolder, "Shared/Review");
});

test("dropping external items on the canvas targets the current shared folder", () => {
  const files = uploadFiles();
  const uploads = [];
  const event = dropEventFor(files);
  const handlers = dropHandlers({
    handleUploadDrop: (dataTransfer, targetFolder) => {
      uploads.push({ dataTransfer, targetFolder });
    },
  });

  handlers.handleCanvasDrop(event);

  assert.equal(event.prevented, 1);
  assert.equal(uploads.length, 1);
  assert.equal(uploads[0].dataTransfer, event.dataTransfer);
  assert.equal(uploads[0].targetFolder, "Shared/Incoming");
});

test("folder drop previews use the Files type while browser file data is protected", () => {
  const uploadHover = [];
  const dropHints = [];
  const event = dropEventFor([]);
  const handlers = dropHandlers({
    setDropHint: (value) => dropHints.push(value),
    setUploadHover: (value) => uploadHover.push(value),
  });

  handlers.handleDropOnFolder("Shared/Review", event, true);

  assert.equal(event.prevented, 1);
  assert.deepEqual(dropHints, ["Shared/Review"]);
  assert.deepEqual(uploadHover, [true]);
});
