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
    handleUpload: () => {},
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

test("dropping multiple files on a folder forwards every file in order", () => {
  const files = uploadFiles();
  const uploads = [];
  const event = dropEventFor(files);
  const handlers = dropHandlers({
    handleUpload: (droppedFiles, targetFolder) => {
      uploads.push({ droppedFiles, targetFolder });
    },
  });

  handlers.handleDropOnFolder("Shared/Review", event, false);

  assert.equal(event.prevented, 1);
  assert.equal(uploads.length, 1);
  assert.equal(Array.isArray(uploads[0].droppedFiles), true);
  assert.deepEqual(uploads[0].droppedFiles, files);
  assert.equal(uploads[0].targetFolder, "Shared/Review");
});

test("dropping multiple files on the canvas targets the current shared folder", () => {
  const files = uploadFiles();
  const uploads = [];
  const event = dropEventFor(files);
  const handlers = dropHandlers({
    handleUpload: (droppedFiles, targetFolder) => {
      uploads.push({ droppedFiles, targetFolder });
    },
  });

  handlers.handleCanvasDrop(event);

  assert.equal(event.prevented, 1);
  assert.equal(uploads.length, 1);
  assert.equal(Array.isArray(uploads[0].droppedFiles), true);
  assert.deepEqual(uploads[0].droppedFiles, files);
  assert.equal(uploads[0].targetFolder, "Shared/Incoming");
});
