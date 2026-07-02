import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/uploadPartPolicy.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { shouldRetryUploadPart, uploadParallelismForLatency } = await import(moduleUrl);

test("upload parallelism uses low fanout for low latency paths", () => {
  assert.equal(uploadParallelismForLatency(0), 4);
  assert.equal(uploadParallelismForLatency(25), 4);
});

test("upload parallelism uses high fanout for slow or unknown control paths", () => {
  assert.equal(uploadParallelismForLatency(26), 8);
  assert.equal(uploadParallelismForLatency(null), 8);
  assert.equal(uploadParallelismForLatency(Number.NaN), 8);
});

test("upload part retry treats transport-shaped failures as retryable", () => {
  assert.equal(shouldRetryUploadPart({ networkError: true }), true);
  assert.equal(shouldRetryUploadPart({ status: 500 }), true);
  assert.equal(shouldRetryUploadPart({ responseText: "", status: 400 }), true);
  assert.equal(
    shouldRetryUploadPart({
      detail: "Upload failed while reading request body",
      responseText: '{"detail":"Upload failed while reading request body"}',
      status: 400,
    }),
    true
  );
  assert.equal(
    shouldRetryUploadPart({
      detail: "Upload part size does not match session",
      responseText: '{"detail":"Upload part size does not match session"}',
      status: 400,
    }),
    true
  );
});

test("upload part retry does not retry semantic upload errors", () => {
  assert.equal(
    shouldRetryUploadPart({
      detail: "Upload part range does not match session",
      responseText: '{"detail":"Upload part range does not match session"}',
      status: 400,
    }),
    false
  );
  assert.equal(shouldRetryUploadPart({ status: 409 }), false);
});
