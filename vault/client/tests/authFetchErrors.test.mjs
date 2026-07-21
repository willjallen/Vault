import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/authFetchErrors.js", import.meta.url);
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
const { normalizedAuthFetchError } = await import(moduleUrl);

test("authenticated fetch preserves cancellation and authorization errors", () => {
  const cancelled = new DOMException("cancelled", "AbortError");
  const unauthorized = Object.assign(new Error("unauthorized"), { status: 401 });

  assert.equal(normalizedAuthFetchError(cancelled), cancelled);
  assert.equal(normalizedAuthFetchError(unauthorized), unauthorized);
});

test("authenticated fetch still normalizes transport failures", () => {
  const normalized = normalizedAuthFetchError(new TypeError("network failed"));

  assert.equal(normalized.message, "Lost connection to the server.");
  assert.equal(normalized.status, 0);
});
