import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/transferClient.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { uploadParallelismForLatency } = await import(moduleUrl);

test("upload parallelism uses low fanout for low latency paths", () => {
  assert.equal(uploadParallelismForLatency(0), 8);
  assert.equal(uploadParallelismForLatency(25), 8);
});

test("upload parallelism uses high fanout for slow or unknown control paths", () => {
  assert.equal(uploadParallelismForLatency(26), 16);
  assert.equal(uploadParallelismForLatency(null), 16);
  assert.equal(uploadParallelismForLatency(Number.NaN), 16);
});
