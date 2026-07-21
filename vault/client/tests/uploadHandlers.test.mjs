import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/uploadHandlers.js", import.meta.url);
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
const { createUploadHandlers } = await import(moduleUrl);

function fileEntry(name, contents) {
  const file = new File([contents], name, { type: "text/plain" });
  return {
    file: (resolve) => resolve(file),
    isDirectory: false,
    isFile: true,
    name,
  };
}

function directoryEntry(name, children) {
  return {
    createReader: () => {
      let finished = false;
      return {
        readEntries: (resolve) => {
          const batch = finished ? [] : children;
          finished = true;
          resolve(batch);
        },
      };
    },
    isDirectory: true,
    isFile: false,
    name,
  };
}

test("a browser-style directory drop creates its tree and uploads only real files", async () => {
  const empty = directoryEntry("Empty", []);
  const nested = directoryEntry("Nested", [fileEntry("asset.txt", "asset")]);
  const root = directoryEntry("Package", [nested, empty]);
  const zeroByteDirectoryPlaceholder = new File([], "Package");
  const requests = [];
  const uploads = [];
  const refreshes = [];
  const errors = [];
  const uploadHover = [];
  const { handleUploadDrop } = createUploadHandlers({
    apiFetch: async (url, options) => {
      requests.push({ options, url });
      return { json: async () => ({ id: requests.length }), ok: true };
    },
    refresh: async (...args) => refreshes.push(args),
    setError: (message) => errors.push(message),
    setUploadHover: (value) => uploadHover.push(value),
    targetFolder: "Shared/Incoming",
    uploadInput: { current: null },
    uploadWithProgress: async (options) => {
      uploads.push(options);
      return { id: 99 };
    },
  });

  const result = await handleUploadDrop({
    files: [zeroByteDirectoryPlaceholder],
    items: [
      {
        getAsFile: () => zeroByteDirectoryPlaceholder,
        kind: "file",
        webkitGetAsEntry: () => root,
      },
    ],
  });

  assert.deepEqual(
    requests.map(({ options, url }) => ({
      body: options.body.toString(),
      method: options.method,
      url,
    })),
    [
      {
        body: "folder=Shared%2FIncoming%2FPackage&exist_ok=true",
        method: "POST",
        url: "/folders",
      },
      {
        body: "folder=Shared%2FIncoming%2FPackage%2FEmpty&exist_ok=true",
        method: "POST",
        url: "/folders",
      },
      {
        body: "folder=Shared%2FIncoming%2FPackage%2FNested&exist_ok=true",
        method: "POST",
        url: "/folders",
      },
    ]
  );
  assert.equal(uploads.length, 1);
  assert.equal(uploads[0].file.name, "asset.txt");
  assert.equal(uploads[0].file.size, 5);
  assert.equal(uploads[0].folder, "Shared/Incoming/Package/Nested");
  assert.equal(uploads[0].file === zeroByteDirectoryPlaceholder, false);
  assert.deepEqual(refreshes, [["Shared/Incoming", { sidebar: true }]]);
  assert.deepEqual(errors.filter(Boolean), []);
  assert.deepEqual(uploadHover, [false]);
  assert.equal(result.succeeded, 1);
  assert.equal(result.foldersCreated, 3);
});
