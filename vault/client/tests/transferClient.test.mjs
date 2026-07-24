import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const uploadPartPolicyUrl = new URL("../src/lib/uploadPartPolicy.js", import.meta.url);
const uploadPartPolicySource = await readFile(uploadPartPolicyUrl, "utf8");
const uploadPartPolicyModuleUrl = `data:text/javascript;base64,${Buffer.from(
  uploadPartPolicySource
).toString("base64")}`;
const { shouldRetryUploadPart, uploadParallelismForLatency, uploadPart } = await import(
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

test("upload parallelism is conservative for constrained or unknown paths", () => {
  assert.equal(uploadParallelismForLatency(26), 2);
  assert.equal(uploadParallelismForLatency(null), 2);
  assert.equal(uploadParallelismForLatency(Number.NaN), 2);
});

function installWatchdogXhr({ acknowledgeAttempt }) {
  const requests = [];
  globalThis.XMLHttpRequest = class FakeXMLHttpRequest {
    constructor() {
      this.headers = {};
      this.responseText = "";
      this.status = 204;
      this.upload = {};
    }

    abort() {
      this.onabort?.();
    }

    open(method, url) {
      this.method = method;
      this.url = url;
    }

    send(chunk) {
      requests.push(this);
      const attempt = requests.length;
      queueMicrotask(() => {
        this.upload.onprogress?.({ loaded: chunk.size });
        if (acknowledgeAttempt(attempt)) {
          this.onload?.();
        }
      });
    }

    setRequestHeader(name, value) {
      this.headers[name.toLowerCase()] = value;
    }
  };
  return requests;
}

test("an acknowledgement stall times out quickly and performs a real retry", async () => {
  const requests = installWatchdogXhr({ acknowledgeAttempt: (attempt) => attempt === 2 });
  const progress = [];
  const states = [];

  await uploadPart({
    ackTimeoutMs: 12,
    chunk: new Blob(["part"]),
    idleTimeoutMs: 12,
    offset: 0,
    onAttemptStart: () => progress.push(0),
    onProgress: (loaded) => progress.push(loaded),
    onState: (state) => states.push(state),
    partNumber: 1,
    reconcileAfterFailure: async () => false,
    retryDelayForAttempt: () => 1,
    session: { id: "session", upload_token: "token" },
    sha256: "00".repeat(32),
    signal: new AbortController().signal,
    stallNoticeMs: 2,
  });

  assert.equal(requests.length, 2);
  assert.equal(
    requests.every((request) => request.timeout === 0),
    true
  );
  assert.ok(states.some((state) => state.stage === "awaiting-ack"));
  assert.ok(
    states.some((state) => state.stage === "stalled" && state.waitingForAcknowledgement === true)
  );
  assert.ok(
    states.some(
      (state) => state.stage === "retrying" && state.attempt === 2 && state.maxAttempts === 3
    )
  );
  assert.deepEqual(progress, [0, 4, 0, 4, 4]);
});

test("an ambiguous acknowledgement failure is reconciled before retransmission", async () => {
  const requests = installWatchdogXhr({ acknowledgeAttempt: () => false });
  const failures = [];

  const result = await uploadPart({
    ackTimeoutMs: 5,
    chunk: new Blob(["part"]),
    idleTimeoutMs: 5,
    offset: 0,
    partNumber: 1,
    reconcileAfterFailure: async (failure) => {
      failures.push(failure);
      return true;
    },
    retryDelayForAttempt: () => 1,
    session: { id: "session", upload_token: "token" },
    sha256: "00".repeat(32),
    signal: new AbortController().signal,
    stallNoticeMs: 1,
  });

  assert.deepEqual(result, { reconciled: true });
  assert.equal(requests.length, 1);
  assert.equal(failures[0].error.timeoutPhase, "awaiting-ack");
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
