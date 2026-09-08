import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";
import { build } from "esbuild";

let effects = [];
let stateOverride;
let stateChanges = [];
globalThis.React = {
  Fragment: Symbol("Fragment"),
  createElement: (type, props, ...children) => ({ type, props: props || {}, children }),
  useEffect: (effect) => effects.push(effect),
  useCallback: (callback) => callback,
  useMemo: (factory) => factory(),
  useRef: () => ({ current: null }),
  useState: (initial) => [stateOverride ?? initial, (value) => stateChanges.push(value)],
};

async function importBundled(path) {
  const bundle = await build({
    bundle: true,
    entryPoints: [new URL(path, import.meta.url).pathname],
    format: "esm",
    platform: "node",
    write: false,
  });
  return import(
    `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString("base64")}`
  );
}

const [{ filePreview }, { FileRow }, { FilePreviewModal, PreviewContent }, { buildFileMenuItems }] =
  await Promise.all([
    importBundled("../src/lib/filePreview.js"),
    importBundled("../src/components/browser/FileRow.js"),
    importBundled("../src/components/browser/FilePreviewModal.js"),
    importBundled("../src/lib/contextMenus.js"),
  ]);
const { useFileDialogs } = await importBundled("../src/lib/useFileDialogs.js");

function mediaDoc(kind = "image") {
  return {
    id: 7,
    name: "Example",
    visual: { media: { kind, url: "/api/documents/7/versions/original/content" } },
  };
}

function findElement(tree, predicate) {
  if (Array.isArray(tree)) {
    return tree.map((node) => findElement(node, predicate)).find(Boolean);
  }
  if (!tree || typeof tree !== "object") {
    return undefined;
  }
  return predicate(tree) ? tree : findElement(tree.children, predicate);
}

test("only authorized server-provided media can open a preview", () => {
  for (const kind of ["image", "audio", "video"]) {
    assert.equal(filePreview(mediaDoc(kind)).kind, kind);
  }
  assert.equal(filePreview({ name: "misleading.png" }), null);
  assert.equal(filePreview({ ...mediaDoc(), access: { read: false } }), null);
  assert.equal(filePreview(mediaDoc("iframe")), null);
  assert.equal(
    filePreview({ visual: { media: { kind: "image", url: "https://external/image" } } }),
    null
  );
});

test("media opens on double click and Enter even when double-click downloads are disabled", () => {
  const opened = [];
  for (const kind of ["image", "audio", "video"]) {
    const doc = mediaDoc(kind);
    const row = FileRow({ currentUser: {}, doc, onOpen: (item) => opened.push(item) });
    row.props.onDoubleClick({ target: { closest: () => null } });
    row.props.onKeyDown({ key: "Enter", preventDefault() {} });
    assert.deepEqual(opened.slice(-2), [doc, doc]);
    assert.equal(FileRow({ currentUser: {}, doc, editing: true }).props.onDoubleClick, undefined);
  }
  const doc = { id: 8, name: "document.txt" };
  assert.equal(FileRow({ currentUser: {}, doc }).props.onDoubleClick, undefined);
  FileRow({
    currentUser: {},
    doc,
    doubleClickDownload: true,
    onOpen: (item) => opened.push(item),
  }).props.onDoubleClick({ target: { closest: () => null } });
  assert.equal(opened.at(-1), doc);
});

test("file action buttons keep their own double-click and keyboard behavior", () => {
  const opened = [];
  const row = FileRow({ currentUser: {}, doc: mediaDoc(), onOpen: (doc) => opened.push(doc) });
  row.props.onDoubleClick({ target: { closest: () => ({}) } });
  row.props.onKeyDown({ key: "Enter", target: {}, currentTarget: {}, preventDefault() {} });
  assert.deepEqual(opened, []);
});

test("context menu exposes preview without changing the download action", () => {
  const doc = mediaDoc("audio");
  const actions = [];
  const menu = buildFileMenuItems({
    doc,
    currentUser: {},
    handleOpenFile: (item) => actions.push(["preview", item]),
    handleView: (item) => actions.push(["download", item]),
  });
  menu.find((item) => item.label === "Preview").action();
  menu.find((item) => item.label === "Download").action();
  assert.deepEqual(actions, [
    ["preview", doc],
    ["download", doc],
  ]);
});

test("opening media preserves its version snapshot while other files still download", () => {
  const downloads = [];
  const doc = mediaDoc("audio");
  const other = { id: 8, name: "document.txt" };
  stateChanges = [];
  const dialogs = useFileDialogs({
    closeContextMenu() {},
    docs: [doc, other],
    onDownload: (item) => downloads.push(item),
    selectedDoc: null,
    setSelectedId() {},
  });
  dialogs.handleOpenFile(doc);
  dialogs.handleOpenFile(other);
  assert.deepEqual(stateChanges, [doc]);
  assert.deepEqual(downloads, [other]);
});

test("preview renders original images and native media controls", () => {
  for (const kind of ["image", "audio", "video"]) {
    const preview = filePreview(mediaDoc(kind));
    const tree = PreviewContent({ fileName: "Example", preview });
    const content = findElement(tree, (node) => node.type === (kind === "image" ? "img" : kind));
    assert.equal(content.props.src, preview.url);
    if (kind === "image") {
      assert.equal(content.props.alt, "Example");
    } else {
      assert.equal(content.props.controls, true);
      assert.equal(content.props.preload, "metadata");
      assert.equal(content.props.autoPlay, undefined);
    }
    stateChanges = [];
    content.props.onError();
    assert.deepEqual(stateChanges, ["error"]);
  }
});

test("failed previews explain browser support and can be retried", () => {
  stateOverride = "error";
  let retried = false;
  const tree = PreviewContent({
    fileName: "Example",
    preview: filePreview(mediaDoc("audio")),
    onRetry: () => {
      retried = true;
    },
  });
  stateOverride = undefined;
  assert.equal(tree.props.role, "alert");
  assert.ok(findElement(tree, (node) => node.type === "p").children[0].includes("not supported"));
  findElement(tree, (node) => node.type === "button").props.onClick();
  assert.equal(retried, true);
});

test("closing or retrying a player stops playback and releases the media source", () => {
  effects = [];
  const tree = PreviewContent({ fileName: "Example", preview: filePreview(mediaDoc("audio")) });
  const calls = [];
  findElement(tree, (node) => node.type === "audio").props.ref.current = {
    pause: () => calls.push("pause"),
    removeAttribute: (name) => calls.push(`remove:${name}`),
    load: () => calls.push("load"),
  };
  const cleanup = effects[0]();
  cleanup();
  assert.deepEqual(calls, ["pause", "remove:src", "load"]);
});

test("modal opens with focus, closes on Escape or backdrop, and hands downloads back to the app", () => {
  effects = [];
  const doc = mediaDoc();
  const calls = [];
  globalThis.document = {
    activeElement: { isConnected: true, focus: () => calls.push("restoreFocus") },
    querySelector: () => null,
  };
  globalThis.CSS = { escape: String };
  const tree = FilePreviewModal({
    doc,
    onClose: () => calls.push("close"),
    onDownload: (item) => calls.push(item),
  });
  assert.equal(tree.type, "dialog");
  tree.props.ref.current = {
    showModal: () => calls.push("showModal"),
    close: () => calls.push("dialog.close"),
  };
  const close = findElement(tree, (node) => node.props["aria-label"] === "Close preview");
  close.props.ref.current = { focus: () => calls.push("focus") };
  const cleanup = effects[0]();
  tree.props.onCancel({ preventDefault: () => calls.push("preventDefault") });
  const backdrop = {};
  tree.props.onPointerDown({ target: {}, currentTarget: backdrop });
  tree.props.onClick({ target: backdrop, currentTarget: backdrop });
  tree.props.onPointerDown({ target: backdrop, currentTarget: backdrop });
  tree.props.onClick({ target: backdrop, currentTarget: backdrop });
  findElement(tree, (node) => node.props.key === "download").props.onClick();
  cleanup();
  delete globalThis.document;
  delete globalThis.CSS;
  assert.deepEqual(calls, [
    "showModal",
    "focus",
    "preventDefault",
    "close",
    "close",
    "close",
    doc,
    "dialog.close",
    "restoreFocus",
  ]);
});

test("preview opened from a dismissed context menu restores focus to its file row", () => {
  effects = [];
  const body = {};
  let focused = false;
  const sourceRow = {
    isConnected: true,
    focus: () => {
      focused = true;
    },
  };
  globalThis.document = {
    activeElement: body,
    body,
    querySelector: (selector) => {
      assert.equal(selector, '[data-selection-key="document:7"]');
      return sourceRow;
    },
  };
  globalThis.CSS = { escape: String };
  const tree = FilePreviewModal({ doc: mediaDoc(), onClose() {}, onDownload() {} });
  tree.props.ref.current = { showModal() {}, close() {} };
  const cleanup = effects[0]();
  cleanup();
  delete globalThis.document;
  delete globalThis.CSS;
  assert.equal(focused, true);
});
