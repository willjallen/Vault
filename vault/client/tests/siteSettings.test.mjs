import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/siteSettings.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { normalizeSiteSettings } = await import(moduleUrl);

test("custom download streaming is disabled by default", () => {
  assert.equal(normalizeSiteSettings({}).customDownloadStreamingEnabled, false);
});

test("custom download streaming requires an explicit boolean opt-in", () => {
  assert.equal(
    normalizeSiteSettings({ customDownloadStreamingEnabled: true }).customDownloadStreamingEnabled,
    true
  );
  assert.equal(
    normalizeSiteSettings({ customDownloadStreamingEnabled: "true" })
      .customDownloadStreamingEnabled,
    false
  );
});
