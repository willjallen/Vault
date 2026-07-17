import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/dragPayloads.js", import.meta.url);
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
const { vaultDropEffect } = await import(moduleUrl);

function dragEvent(...types) {
  return { dataTransfer: { types } };
}

test("external files are copied into vault folder targets", () => {
  assert.equal(vaultDropEffect(dragEvent("Files"), "folder"), "copy");
});

test("favorite targets copy internal vault items", () => {
  assert.equal(vaultDropEffect(dragEvent("application/x-vault-selection"), "favorites"), "copy");
});

test("folder targets move internal vault items", () => {
  assert.equal(vaultDropEffect(dragEvent("application/x-vault-selection"), "folder"), "move");
});

test("a drag with no target advertises no drop", () => {
  assert.equal(vaultDropEffect(dragEvent("Files"), null), "none");
  assert.equal(vaultDropEffect(dragEvent("application/x-vault-selection"), null), "none");
});
