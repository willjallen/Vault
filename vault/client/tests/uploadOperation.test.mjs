import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/uploadOperation.js", import.meta.url);
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
const { createUploadOperation, describeUploadOperation } = await import(moduleUrl);

function filesWithSizes(sizes) {
  return sizes.map((size, index) => new File([new Uint8Array(size)], `file-${index + 1}.bin`));
}

function operationHarness({ controller = new AbortController(), files, folders = [] }) {
  const cancelled = [];
  const completed = [];
  const errors = [];
  const progress = [];
  const descriptor = describeUploadOperation({ files, folders });
  const operation = createUploadOperation({
    descriptor,
    onCancelled: (summary) => cancelled.push(summary),
    onComplete: (summary) => completed.push(summary),
    onError: (...args) => errors.push(args),
    onProgress: (value) => progress.push(value),
    runUpload: async ({ file, onProgress }) => {
      onProgress({ bytesPerSecond: file.size, loaded: Math.floor(file.size / 2) });
      onProgress({ bytesPerSecond: file.size, loaded: file.size });
      return { id: file.name, size: file.size };
    },
    signal: controller.signal,
  });
  return { cancelled, completed, controller, descriptor, errors, operation, progress };
}

test("upload operation descriptors aggregate files, folders, bytes, and a restrained name", () => {
  const files = filesWithSizes([3, 5]);
  const descriptor = describeUploadOperation({
    files,
    folders: ["Bundle", "Bundle/Empty"],
  });

  assert.deepEqual(descriptor, {
    grouped: true,
    name: "Bundle",
    totalBytes: 8,
    totalFiles: 2,
    totalFolders: 2,
    totalItems: 4,
  });
  assert.equal(describeUploadOperation({ files: [files[0]] }).grouped, false);
  assert.equal(describeUploadOperation({ files }).name, "2 files");
});

test("one operation aggregates folder and concurrent file progress before completing once", async () => {
  const files = filesWithSizes([4, 6]);
  const harness = operationHarness({ files, folders: ["Bundle"] });

  harness.operation.folderStarted("Bundle");
  harness.operation.folderFinished("Bundle");
  const results = await Promise.all(files.map((file) => harness.operation.upload({ file })));
  harness.operation.finish({
    attempted: files.length,
    cancelled: 0,
    failed: 0,
    outcomes: [],
    succeeded: files.length,
  });

  assert.deepEqual(
    results.map((result) => result.id),
    files.map((file) => file.name)
  );
  assert.equal(harness.completed.length, 1);
  assert.equal(harness.cancelled.length, 0);
  assert.equal(harness.errors.length, 0);
  assert.equal(harness.completed[0].processedItems, 3);
  assert.equal(harness.completed[0].totalItems, 3);
  assert.equal(harness.progress.at(-1).loaded, 10);
  assert.equal(harness.progress.at(-1).processedItems, 3);
  assert.equal(harness.progress.at(-1).percent, 100);
});

test("a retried part rolls aggregate progress back to server-committed bytes", async () => {
  const file = filesWithSizes([10])[0];
  const progress = [];
  const operation = createUploadOperation({
    descriptor: describeUploadOperation({ files: [file] }),
    onCancelled: () => assert.fail("upload was cancelled"),
    onComplete: () => {},
    onError: (error) => assert.fail(error),
    onProgress: (value) => progress.push(value),
    runUpload: async ({ onProgress }) => {
      onProgress({
        committedBytes: 4,
        inFlightBytes: 6,
        loaded: 10,
        stage: "awaiting-ack",
        waitingForAcknowledgement: true,
      });
      onProgress({
        attempt: 2,
        committedBytes: 4,
        inFlightBytes: 0,
        loaded: 4,
        maxAttempts: 3,
        stage: "retrying",
      });
      onProgress({
        committedBytes: 10,
        inFlightBytes: 0,
        loaded: 10,
        stage: "uploading",
      });
      return { id: file.name };
    },
    signal: new AbortController().signal,
  });

  await operation.upload({ file });

  const awaiting = progress.find((value) => value.stage === "awaiting-ack");
  const retrying = progress.find((value) => value.stage === "retrying");
  assert.equal(awaiting.committedBytes, 4);
  assert.equal(awaiting.inFlightBytes, 6);
  assert.equal(awaiting.percent, 99.9);
  assert.equal(retrying.committedBytes, 4);
  assert.equal(retrying.inFlightBytes, 0);
  assert.equal(retrying.loaded, 4);
  assert.equal(retrying.percent, 40);
});

test("aborting a bulk operation cancels every child and settles one operation popup", async () => {
  const files = filesWithSizes([2, 3, 4]);
  const controller = new AbortController();
  const cancelled = [];
  const descriptor = describeUploadOperation({ files });
  const operation = createUploadOperation({
    descriptor,
    onCancelled: (summary) => cancelled.push(summary),
    onComplete: () => assert.fail("cancelled operation completed"),
    onError: () => assert.fail("cancelled operation failed"),
    onProgress: () => {},
    runUpload: ({ signal }) =>
      new Promise((_resolve, reject) => {
        signal.addEventListener(
          "abort",
          () => reject(Object.assign(new Error("cancelled"), { cancelled: true })),
          { once: true }
        );
      }),
    signal: controller.signal,
  });

  const pending = files.map((file) => operation.upload({ file }));
  controller.abort();
  const results = await Promise.all(pending);
  const afterAbort = await operation.upload({ file: files[0] });
  operation.finish({
    attempted: files.length,
    cancelled: files.length,
    failed: 0,
    outcomes: [],
    succeeded: 0,
  });

  assert.equal(
    results.every((result) => result.cancelled),
    true
  );
  assert.equal(afterAbort.cancelled, true);
  assert.equal(cancelled.length, 1);
  assert.equal(cancelled[0].cancelled, files.length);
});

test("partial failures settle once with a concise aggregate error", async () => {
  const files = filesWithSizes([1, 1]);
  const errors = [];
  const descriptor = describeUploadOperation({ files });
  const operation = createUploadOperation({
    descriptor,
    onCancelled: () => {},
    onComplete: () => {},
    onError: (...args) => errors.push(args),
    onProgress: () => {},
    runUpload: async () => ({}),
    signal: new AbortController().signal,
  });
  const failure = new Error("network unavailable");

  operation.finish({
    attempted: 2,
    cancelled: 0,
    failed: 1,
    outcomes: [{ status: "fulfilled" }, { reason: failure, status: "rejected" }],
    succeeded: 1,
  });
  operation.finish({ failed: 1 });

  assert.equal(errors.length, 1);
  assert.match(errors[0][0].message, /1 of 2 files failed: network unavailable/);
  assert.equal(errors[0][1].processedItems, 2);
});

test("an authorization failure aborts siblings and survives operation aggregation", async () => {
  const files = filesWithSizes([2, 3]);
  const controller = new AbortController();
  const errors = [];
  const descriptor = describeUploadOperation({ files });
  const operation = createUploadOperation({
    abort: () => controller.abort(),
    descriptor,
    onCancelled: () => assert.fail("authorization failure was reported as cancellation"),
    onComplete: () => assert.fail("authorization failure completed"),
    onError: (...args) => errors.push(args),
    onProgress: () => {},
    runUpload: ({ file, signal }) => {
      if (file === files[0]) {
        return Promise.reject(Object.assign(new Error("authentication required"), { status: 401 }));
      }
      return new Promise((_resolve, reject) => {
        signal.addEventListener(
          "abort",
          () => reject(Object.assign(new Error("cancelled"), { cancelled: true })),
          { once: true }
        );
      });
    },
    signal: controller.signal,
  });

  const outcomes = await Promise.all(
    files.map((file) =>
      operation
        .upload({ file })
        .then((value) => ({ status: value.cancelled ? "cancelled" : "fulfilled", value }))
        .catch((reason) => ({ reason, status: "rejected" }))
    )
  );
  operation.finish({
    attempted: files.length,
    cancelled: outcomes.filter((outcome) => outcome.status === "cancelled").length,
    failed: outcomes.filter((outcome) => outcome.status === "rejected").length,
    outcomes,
    succeeded: 0,
  });

  assert.equal(controller.signal.aborted, true);
  assert.deepEqual(
    outcomes.map((outcome) => outcome.status),
    ["rejected", "cancelled"]
  );
  assert.equal(errors.length, 1);
  assert.equal(errors[0][0].status, 401);
  assert.match(errors[0][0].message, /authentication required/);
});
