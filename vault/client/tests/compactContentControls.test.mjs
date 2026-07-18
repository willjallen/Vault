import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

let stateOverride;
globalThis.React = {
  Fragment: Symbol("Fragment"),
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
  useEffect: () => {},
  useMemo: (factory) => factory(),
  useRef: () => ({ current: null }),
  useState: (initial) => [stateOverride === undefined ? initial : stateOverride, () => {}],
};

async function importBundled(relativePath) {
  const sourceUrl = new URL(relativePath, import.meta.url);
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
  return import(moduleUrl);
}

const [{ FileRow }, { FolderRow }, { ContentsCompactSort }] = await Promise.all([
  importBundled("../src/components/browser/FileRow.js"),
  importBundled("../src/components/browser/FolderRow.js"),
  importBundled("../src/components/browser/ContentsViewControl.js"),
]);
const styles = await readFile(new URL("../styles/styles.css", import.meta.url), "utf8");

function elementsMatching(node, predicate, result = []) {
  if (Array.isArray(node)) {
    node.forEach((child) => elementsMatching(child, predicate, result));
    return result;
  }
  if (!node || typeof node !== "object") {
    return result;
  }
  if (predicate(node)) {
    result.push(node);
  }
  elementsMatching(node.children, predicate, result);
  return result;
}

function elementsOfType(node, type) {
  return elementsMatching(node, (item) => item.type === type);
}

function elementsWithClass(node, className) {
  return elementsMatching(node, (item) =>
    String(item.props?.className || "")
      .split(/\s+/)
      .includes(className)
  );
}

function cssRule(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return styles.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`))?.[1] || "";
}

function fileRow(lock) {
  return FileRow({
    currentUser: { id: "user-1", name: "Ada" },
    doc: {
      id: 7,
      lock,
      modified_at: "2026-07-18T00:00:00Z",
      name: "moon.png",
      size_display: "20 MB",
    },
  });
}

test("compact file rows expose lock state as a non-layout icon badge", () => {
  const locked = elementsWithClass(
    fileRow({ by: "user-2", name: "Grace" }),
    "compact-lock-indicator"
  );
  assert.equal(locked.length, 1);
  assert.equal(locked[0].props.title, "Checked out by Grace");
  assert.match(locked[0].props.className, /locked-other/);
  assert.equal(elementsWithClass(fileRow({}), "compact-lock-indicator").length, 0);

  assert.match(cssRule(".compact-lock-indicator"), /position:\s*absolute/);
  assert.match(cssRule(".contents-view-list .file-cell.row-actions"), /position:\s*absolute/);
  assert.match(cssRule(".contents-view-icons .file-cell.row-actions"), /position:\s*absolute/);
});

test("folder names use the same bounded name line as files", () => {
  const tree = FolderRow({
    folder: { modified_at: "2026-07-18T00:00:00Z", name: "A very long folder name" },
  });
  const nameLines = elementsWithClass(tree, "file-name-line");
  assert.equal(nameLines.length, 1);
  assert.equal(elementsWithClass(nameLines[0], "name").length, 1);
});

test("compact sorting uses a palette menu instead of a native select", () => {
  stateOverride = true;
  const changes = [];
  const tree = ContentsCompactSort({
    onSortChange: (key) => changes.push(key),
    sort: { direction: "desc", key: "modified" },
  });
  stateOverride = undefined;

  assert.equal(elementsOfType(tree, "select").length, 0);
  const trigger = elementsMatching(
    tree,
    (item) => item.props?.className === "contents-compact-sort-trigger"
  );
  assert.equal(trigger.length, 1);
  assert.equal(trigger[0].props["aria-haspopup"], "menu");
  assert.equal(trigger[0].props["aria-expanded"], true);

  const menus = elementsMatching(tree, (item) => item.props?.role === "menu");
  const options = elementsMatching(tree, (item) => item.props?.role === "menuitemradio");
  assert.equal(menus.length, 1);
  assert.deepEqual(
    options.map((option) => option.children[0]),
    ["Name", "Modified", "User", "Size", "Status"]
  );
  assert.equal(options.find((option) => option.props["aria-checked"]).children[0], "Modified");

  options.find((option) => option.children[0] === "User").props.onClick();
  elementsMatching(
    tree,
    (item) => item.props?.["aria-label"] === "Sort ascending"
  )[0].props.onClick();
  assert.deepEqual(changes, ["user", "modified"]);

  assert.match(cssRule(".contents-compact-sort-menu"), /background:\s*var\(--surface-context\)/);
  assert.match(cssRule(".contents-compact-sort-menu"), /color:\s*var\(--text-context\)/);
});
