import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
  useEffect: () => {},
  useRef: () => ({ current: null }),
};

async function importBundled(relativePath) {
  const bundle = await build({
    bundle: true,
    entryPoints: [new URL(relativePath, import.meta.url).pathname],
    format: "esm",
    platform: "node",
    write: false,
  });
  return import(
    `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString("base64")}`
  );
}

const [columns, sizing, { FileRow }, { FolderRow }] = await Promise.all([
  importBundled("../src/lib/contentsColumns.js"),
  importBundled("../src/components/browser/contentColumns.js"),
  importBundled("../src/components/browser/FileRow.js"),
  importBundled("../src/components/browser/FolderRow.js"),
]);

test("column orders stay isolated by canonical folder path, including root", () => {
  const root = ["size", "name", "user", "modified"];
  const nested = ["user", "modified", "size", "name"];
  let orders = columns.setContentsColumnOrderForFolder({}, "", root);
  orders = columns.setContentsColumnOrderForFolder(orders, "/Projects/Alpha/", nested);
  assert.deepEqual(columns.contentsColumnOrderForFolder(orders, ""), root);
  assert.deepEqual(columns.contentsColumnOrderForFolder(orders, "Projects/Alpha"), nested);
  assert.deepEqual(
    columns.contentsColumnOrderForFolder(orders, "Projects/Beta"),
    columns.DEFAULT_CONTENTS_COLUMN_ORDER
  );
  assert.deepEqual(
    columns.normalizeContentsColumnOrderByFolder({
      bad: ["name", "name", "user", "size"],
      good: root,
    }),
    { good: root }
  );
});

test("columns can move in either direction without accepting fixed columns", () => {
  assert.deepEqual(columns.moveContentsColumn(undefined, "size", "name"), [
    "size",
    "name",
    "modified",
    "user",
  ]);
  assert.deepEqual(columns.moveContentsColumn(undefined, "name", "size", true), [
    "modified",
    "user",
    "size",
    "name",
  ]);
  assert.deepEqual(
    columns.moveContentsColumn(undefined, "name", "status"),
    columns.DEFAULT_CONTENTS_COLUMN_ORDER
  );
  assert.deepEqual(
    columns.moveContentsColumn(undefined, "actions", "size"),
    columns.DEFAULT_CONTENTS_COLUMN_ORDER
  );
});

test("file and folder cells follow the reordered headers", () => {
  const columnOrder = ["size", "user", "name", "modified"];
  const rows = [
    FileRow({ columnOrder, currentUser: {}, doc: { id: 1, name: "drawing.png", lock: {} } }),
    FolderRow({ columnOrder, folder: { path: "Art", name: "Art" } }),
  ];
  for (const row of rows) {
    const cells = row.children.flat();
    assert.deepEqual(
      cells.map((cell) => cell.props.className),
      [
        "file-cell icon",
        "file-cell size",
        "file-cell user",
        "file-cell main",
        "file-cell meta",
        "file-cell status-col",
        "file-cell row-actions",
      ]
    );
  }
});

test("resize handles and grid widths follow their new neighbors", () => {
  const order = ["size", "name", "user", "modified"];
  assert.deepEqual(sizing.columnResizeHandle(order, "size"), { left: "size", right: "name" });
  assert.deepEqual(sizing.columnResizeHandle(order, "modified"), {
    left: "modified",
    right: "status",
  });
  const widths = { actions: 162, modified: 210, size: 96, status: 236, user: 126 };
  const resized = sizing.columnWidthsForResize(
    {
      left: "size",
      right: "name",
      startX: 100,
      startWidths: { size: 96, name: 240 },
      startColumnWidths: widths,
    },
    120
  );
  assert.equal(resized.size, 116);
  assert.equal(resized.modified, 210);
  assert.match(
    sizing.contentColumnStyle(widths, order)["--contents-ordered-grid-template"],
    /^32px minmax\(64px, var\(--contents-size-width\)\) minmax\(196px, 1fr\)/
  );
});
