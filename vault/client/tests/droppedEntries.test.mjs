import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/droppedEntries.js", import.meta.url);
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
const { readDroppedUploadTree } = await import(moduleUrl);

function fileEntry(name, contents = name) {
  const file = new File([contents], name, { type: "application/octet-stream" });
  return {
    file: (resolve) => queueMicrotask(() => resolve(file)),
    isDirectory: false,
    isFile: true,
    name,
  };
}

function failingFileEntry(name) {
  return {
    file: (_resolve, reject) => queueMicrotask(() => reject(new Error("filesystem denied"))),
    isDirectory: false,
    isFile: true,
    name,
  };
}

function directoryEntry(name, batches, reads = { count: 0 }) {
  return {
    createReader: () => {
      let index = 0;
      return {
        readEntries: (resolve) => {
          reads.count += 1;
          const batch = batches.at(index) || [];
          index += 1;
          queueMicrotask(() => resolve(batch));
        },
      };
    },
    isDirectory: true,
    isFile: false,
    name,
    reads,
  };
}

function entryItem(entry, placeholder = null, onCapture = () => {}) {
  return {
    getAsFile: () => placeholder,
    kind: "file",
    webkitGetAsEntry: () => {
      onCapture(entry);
      return entry;
    },
  };
}

test("directory drops traverse every reader batch and never upload the zero-byte placeholder", async () => {
  const nestedReads = { count: 0 };
  const rootReads = { count: 0 };
  const actualEmptyFile = fileEntry("empty.txt", "");
  const emptyDirectory = directoryEntry("Empty", [[]]);
  const nestedDirectory = directoryEntry(
    "Nested",
    [[fileEntry("one.txt")], [actualEmptyFile, emptyDirectory], []],
    nestedReads
  );
  const root = directoryEntry(
    "Project",
    [[fileEntry("root.txt"), nestedDirectory], [directoryEntry("Blank", [[]])], []],
    rootReads
  );
  const directoryPlaceholder = new File([], "Project");
  let captured = false;

  const promise = readDroppedUploadTree({
    files: [directoryPlaceholder],
    items: [entryItem(root, directoryPlaceholder, () => (captured = true))],
  });

  assert.equal(captured, true, "entry handles must be captured before the drop event expires");
  const tree = await promise;

  assert.deepEqual(tree.directories, [
    "Project",
    "Project/Nested",
    "Project/Nested/Empty",
    "Project/Blank",
  ]);
  assert.deepEqual(
    tree.files.map(({ file, relativePath }) => ({
      name: file.name,
      relativePath,
      size: file.size,
    })),
    [
      { name: "root.txt", relativePath: "Project/root.txt", size: 8 },
      { name: "one.txt", relativePath: "Project/Nested/one.txt", size: 7 },
      { name: "empty.txt", relativePath: "Project/Nested/empty.txt", size: 0 },
    ]
  );
  assert.equal(
    tree.files.some(({ file }) => file === directoryPlaceholder),
    false
  );
  assert.equal(rootReads.count, 3);
  assert.equal(nestedReads.count, 3);
});

test("mixed file and folder roots preserve top-level order and hierarchy", async () => {
  const loose = fileEntry("loose.txt");
  const folder = directoryEntry("Assets", [[fileEntry("texture.png")], []]);

  const tree = await readDroppedUploadTree({
    files: [],
    items: [entryItem(loose), entryItem(folder)],
  });

  assert.deepEqual(tree.directories, ["Assets"]);
  assert.deepEqual(
    tree.files.map(({ relativePath }) => relativePath),
    ["loose.txt", "Assets/texture.png"]
  );
});

test("file-list fallback preserves webkitRelativePath folder structure", async () => {
  const loose = new File(["loose"], "loose.txt");
  const selected = new File(["selected"], "selected.txt");
  Object.defineProperty(selected, "webkitRelativePath", {
    value: "Bundle/Nested/selected.txt",
  });

  const tree = await readDroppedUploadTree({ files: [loose, selected] });

  assert.deepEqual(tree.directories, ["Bundle", "Bundle/Nested"]);
  assert.deepEqual(
    tree.files.map(({ relativePath }) => relativePath),
    ["loose.txt", "Bundle/Nested/selected.txt"]
  );
});

test("unreadable directory members reject the entire copy before upload", async () => {
  const root = directoryEntry("Project", [[failingFileEntry("locked.txt")], []]);

  await assert.rejects(
    readDroppedUploadTree({ files: [], items: [entryItem(root)] }),
    /Could not read dropped item locked\.txt/
  );
});
