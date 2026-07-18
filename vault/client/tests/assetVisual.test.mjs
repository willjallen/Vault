import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: () => null,
  useEffect: () => {},
  useMemo: (factory) => factory(),
  useState: (value) => [value, () => {}],
};

const sourceUrl = new URL("../src/components/common/AssetVisual.js", import.meta.url);
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
const { previewRetryDelay, previewSourceSet, previewVariantForSize, readyPreviewVariants } =
  await import(moduleUrl);

test("preview variants require ready status and valid rendition URLs", () => {
  const item = {
    visual: {
      preview: {
        status: "ready",
        variants: [
          { height: 256, url: "/large", width: 256 },
          { height: 64, url: "/small", width: 64 },
          { url: "", width: 128 },
        ],
      },
    },
  };
  assert.deepEqual(
    readyPreviewVariants(item).map((variant) => variant.url),
    ["/small", "/large"]
  );
  item.visual.preview.status = "pending";
  assert.deepEqual(readyPreviewVariants(item), []);
});

test("preview source selection chooses the smallest sufficient rendition", () => {
  const variants = [
    { url: "/64", width: 64 },
    { url: "/128", width: 128 },
    { url: "/256", width: 256 },
  ];
  assert.equal(previewVariantForSize(variants, 80).url, "/128");
  assert.equal(previewVariantForSize(variants, 500).url, "/256");
  assert.equal(previewSourceSet(variants), "/64 64w, /128 128w, /256 256w");
});

test("failed preview retries use a bounded exponential delay", () => {
  assert.equal(previewRetryDelay(1), 1000);
  assert.equal(previewRetryDelay(4), 8000);
  assert.equal(previewRetryDelay(20), 8000);
});
