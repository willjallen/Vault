import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
  useCallback: (callback) => callback,
  useState: (initial) => [typeof initial === "function" ? initial() : initial, () => {}],
};

const sourceUrl = new URL("../src/components/settings/DebugPanel.js", import.meta.url);
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
const { DebugPanel } = await import(moduleUrl);

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

test("debug What's New can simulate a selected previously seen release", () => {
  /*
   * Renders the development panel at v2.2.0 with v2.0.0 selected, verifies every prior bundled
   * release is offered, and confirms the action writes that selected acknowledgement before the
   * modal opens.
   */
  const selectedVersions = [];
  const tree = DebugPanel({
    acknowledgedVersion: "2.0.0",
    currentVersion: "2.2.0",
    onShowWhatsNew: (version) => selectedVersions.push(version),
    releaseNotes: [
      { version: "2.2.0", entries: [{ kind: "feat", text: "Newest feature." }] },
      { version: "2.1.0", entries: [{ kind: "fix", text: "Middle improvement." }] },
      { version: "2.0.0", entries: [{ kind: "note", text: "Initial release." }] },
    ],
  });

  const [versionSelect] = elementsMatching(tree, (node) => node.type === "select");
  assert.equal(versionSelect.props.value, "2.0.0");
  assert.deepEqual(
    elementsMatching(versionSelect, (node) => node.type === "option").map(
      (option) => option.props.value
    ),
    ["", "2.1.0", "2.0.0"]
  );

  const [showButton] = elementsMatching(tree, (node) => node.props?.label === "Show What's New");
  showButton.props.onClick();
  assert.deepEqual(selectedVersions, ["2.0.0"]);
});
