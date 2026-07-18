import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/components/browser/contentLayout.js", import.meta.url);
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
const { applyLayoutSnapshot, shouldAnimateLayoutChange } = await import(moduleUrl);

test("layout FLIP includes icon-size reflow within icon mode", () => {
  assert.equal(
    shouldAnimateLayoutChange({ iconSize: 80, mode: "icons" }, { iconSize: 112, mode: "icons" }),
    true
  );
  assert.equal(
    shouldAnimateLayoutChange({ iconSize: 80, mode: "icons" }, { iconSize: 80, mode: "icons" }),
    false
  );
  assert.equal(
    shouldAnimateLayoutChange({ iconSize: 80, mode: "list" }, { iconSize: 112, mode: "list" }),
    false
  );
  assert.equal(
    shouldAnimateLayoutChange({ iconSize: 80, mode: "list" }, { iconSize: 80, mode: "icons" }),
    true
  );
});

test("layout FLIP still honors the reduced-motion preference", () => {
  let animations = 0;
  const item = {
    animate: () => {
      animations += 1;
    },
    dataset: { selectionKey: "document:1" },
    getBoundingClientRect: () => ({ bottom: 96, left: 48, right: 96, top: 48 }),
  };
  const list = {
    getBoundingClientRect: () => ({ bottom: 200, left: 0, right: 200, top: 0 }),
    querySelectorAll: () => [item],
    scrollTop: 0,
  };
  const snapshot = {
    anchorKey: "",
    anchorTop: 0,
    animate: true,
    rects: new Map([["document:1", { bottom: 48, left: 0, right: 48, top: 0 }]]),
  };

  globalThis.window = { matchMedia: () => ({ matches: true }) };
  applyLayoutSnapshot(list, snapshot);
  assert.equal(animations, 0);

  globalThis.window = { matchMedia: () => ({ matches: false }) };
  applyLayoutSnapshot(list, snapshot);
  assert.equal(animations, 1);
});
