import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/components/browser/marqueeSelection.js", import.meta.url);
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
const {
  clientPointToContent,
  clientRectToContent,
  combineMarqueeSelection,
  contentRectToVisibleClientRect,
  rectFromPoints,
  rectsIntersect,
} = await import(moduleUrl);

const listRect = { bottom: 300, left: 0, right: 300, top: 0 };

test("marquee selection keeps crossed rows after auto-scroll moves them offscreen", () => {
  const start = clientPointToContent({
    clientX: 30,
    clientY: 100,
    listRect,
    scrollLeft: 0,
    scrollTop: 0,
  });
  const current = clientPointToContent({
    clientX: 250,
    clientY: 285,
    listRect,
    scrollLeft: 0,
    scrollTop: 180,
  });
  const marquee = rectFromPoints(start, current);
  const crossedRow = clientRectToContent(
    { bottom: -20, left: 20, right: 200, top: -60 },
    listRect,
    0,
    180
  );

  assert.equal(rectsIntersect(marquee, crossedRow), true);
  assert.deepEqual(contentRectToVisibleClientRect(marquee, listRect, 0, 180), {
    bottom: 285,
    height: 285,
    left: 30,
    right: 250,
    top: 0,
    width: 220,
  });
});

test("marquee selection combines current content hits with the drag-start selection", () => {
  const options = {
    baseSelection: ["folder:1", "document:2"],
    hitKeys: ["document:2", "document:3"],
    orderedKeys: ["folder:1", "document:2", "document:3", "document:4"],
  };

  assert.deepEqual(combineMarqueeSelection({ ...options, mode: "replace" }), [
    "document:2",
    "document:3",
  ]);
  assert.deepEqual(combineMarqueeSelection({ ...options, mode: "add" }), [
    "folder:1",
    "document:2",
    "document:3",
  ]);
  assert.deepEqual(combineMarqueeSelection({ ...options, mode: "toggle" }), [
    "folder:1",
    "document:3",
  ]);
});
