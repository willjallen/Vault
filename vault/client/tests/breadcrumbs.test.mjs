import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  Fragment: Symbol("Fragment"),
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
};

const sourceUrl = new URL("../src/components/common/Breadcrumbs.js", import.meta.url);
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
const { Breadcrumbs } = await import(moduleUrl);

function elementsOfType(node, type, result = []) {
  if (Array.isArray(node)) {
    node.forEach((child) => elementsOfType(child, type, result));
    return result;
  }
  if (!node || typeof node !== "object") {
    return result;
  }
  if (node.type === type) {
    result.push(node);
  }
  elementsOfType(node.children, type, result);
  return result;
}

test("every breadcrumb exposes its exact folder drop target", () => {
  const breadcrumbs = [
    { name: "Vault", path: "" },
    { name: "Shared", path: "Shared" },
    { name: "Incoming", path: "Shared/Incoming" },
  ];

  const tree = Breadcrumbs({
    activePath: "Shared/Incoming",
    breadcrumbs,
    onClearDrop: () => {},
    onDropOnFolder: () => {},
    onSelect: () => {},
  });
  const buttons = elementsOfType(tree, "button");

  assert.equal(buttons.length, breadcrumbs.length);
  assert.deepEqual(
    buttons.map((button) => button.props["data-vault-drop-kind"]),
    breadcrumbs.map(() => "folder")
  );
  assert.deepEqual(
    buttons.map((button) => button.props["data-drop-folder"]),
    breadcrumbs.map((crumb) => crumb.path)
  );
});
