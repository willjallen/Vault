import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const uploadPartPolicyUrl = new URL("../src/lib/uploadPartPolicy.js", import.meta.url);
const uploadPartPolicySource = await readFile(uploadPartPolicyUrl, "utf8");
const uploadPartPolicyModuleUrl = `data:text/javascript;base64,${Buffer.from(
  uploadPartPolicySource
).toString("base64")}`;
const { shouldRetryUploadPart, uploadParallelismForLatency } = await import(
  uploadPartPolicyModuleUrl
);

const uploadStatusPolicyUrl = new URL("../src/lib/uploadStatusPolicy.js", import.meta.url);
const uploadStatusPolicySource = await readFile(uploadStatusPolicyUrl, "utf8");
const uploadStatusPolicyModuleUrl = `data:text/javascript;base64,${Buffer.from(
  uploadStatusPolicySource
).toString("base64")}`;
const { nextUploadVerificationPollDelay, UPLOAD_VERIFICATION_POLL_INITIAL_MS } = await import(
  uploadStatusPolicyModuleUrl
);

test("upload parallelism uses low fanout for low latency paths", () => {
  assert.equal(uploadParallelismForLatency(0), 4);
  assert.equal(uploadParallelismForLatency(25), 4);
});

test("upload parallelism uses high fanout for slow or unknown control paths", () => {
  assert.equal(uploadParallelismForLatency(26), 8);
  assert.equal(uploadParallelismForLatency(null), 8);
  assert.equal(uploadParallelismForLatency(Number.NaN), 8);
});

test("verification polling backs off and resets after forward progress", () => {
  assert.equal(UPLOAD_VERIFICATION_POLL_INITIAL_MS, 500);
  let delay = UPLOAD_VERIFICATION_POLL_INITIAL_MS;
  delay = nextUploadVerificationPollDelay(delay, false);
  assert.equal(delay, 1000);
  delay = nextUploadVerificationPollDelay(delay, false);
  assert.equal(delay, 2000);
  assert.equal(nextUploadVerificationPollDelay(delay, false), 2000);
  assert.equal(nextUploadVerificationPollDelay(delay, true), 500);
  assert.equal(nextUploadVerificationPollDelay(Number.NaN, false), 1000);
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
