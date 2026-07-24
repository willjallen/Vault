import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
};
const sourceUrl = new URL("../src/components/TransferDock.js", import.meta.url);
const bundle = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString(
  "base64"
)}`;
const { TransferDock, transferCanCancel, transferMeta, transferStageLabel, transferTitle } =
  await import(moduleUrl);

test("queued exports show their queue state and zero-byte progress", () => {
  const transfer = {
    bytesPerSecond: 0,
    kind: "download",
    loaded: 0,
    noProgressSeconds: 6,
    percent: 0,
    serverStatus: "queued",
    stage: "queued",
    status: "active",
    total: 1024,
    totalItems: 3,
  };

  assert.equal(transferTitle(transfer), "Waiting to prepare download");
  assert.equal(transferStageLabel(transfer), "Export queued");
  assert.equal(transferMeta(transfer), "Waiting for worker for 6s - 0% - 0 B of 1.0 KB");
});

test("unknown export totals remain indeterminate", () => {
  assert.equal(
    transferMeta({
      bytesPerSecond: 0,
      kind: "download",
      loaded: 0,
      noProgressSeconds: 0,
      percent: null,
      serverStatus: "queued",
      stage: "queued",
      status: "active",
      total: null,
    }),
    "Starting"
  );
});

test("running exports identify the item being packaged", () => {
  const transfer = {
    bytesPerSecond: 128,
    etaSeconds: 2,
    kind: "download",
    loaded: 512,
    noProgressSeconds: 0,
    percent: 50,
    processedItems: 1,
    serverStatus: "running",
    stage: "preparing",
    status: "active",
    total: 1024,
    totalItems: 3,
  };

  assert.equal(transferTitle(transfer), "Preparing download");
  assert.equal(transferStageLabel(transfer), "Packaging item 2 of 3");
  assert.equal(transferMeta(transfer), "50% - 512 B of 1.0 KB - 128 B/s - 2s left");
});

test("stale export progress suppresses cumulative rate and ETA", () => {
  const transfer = {
    bytesPerSecond: 4096,
    etaSeconds: 1,
    kind: "download",
    loaded: 512,
    noProgressSeconds: 9,
    percent: 50,
    processedItems: 0,
    serverStatus: "running",
    stage: "preparing",
    status: "active",
    total: 1024,
    totalItems: 1,
  };

  assert.equal(transferMeta(transfer), "No progress reported for 9s - 50% - 512 B of 1.0 KB");
});

test("finalizing exports retain activity freshness", () => {
  const transfer = {
    kind: "download",
    noProgressSeconds: 12,
    serverStatus: "finalizing",
    stage: "server-finalizing",
    status: "active",
    total: 1024,
  };

  assert.equal(transferTitle(transfer), "Finalizing export");
  assert.equal(transferStageLabel(transfer), "Server finalization");
  assert.equal(transferMeta(transfer), "No progress reported for 12s - 1.0 KB packaged");
});

test("grouped uploads show aggregate progress and the current item with little copy", () => {
  const transfer = {
    bytesPerSecond: 100,
    currentItem: "photo-41.jpg",
    etaSeconds: 6,
    grouped: true,
    kind: "upload",
    loaded: 400,
    noProgressSeconds: 0,
    percent: 40,
    processedItems: 40,
    stage: "uploading",
    status: "active",
    total: 1000,
    totalItems: 100,
  };

  assert.equal(transferTitle(transfer), "Uploading");
  assert.equal(transferStageLabel(transfer), "photo-41.jpg");
  assert.equal(
    transferMeta(transfer),
    "40 of 100 items - 40% - 400 B of 1000 B - 100 B/s - 6s left"
  );
  assert.equal(transferCanCancel(transfer), true);
  assert.equal(transferCanCancel({ ...transfer, grouped: false, stage: "verifying" }), false);
});

test("uploads distinguish secured bytes from bytes awaiting acknowledgment", () => {
  const transfer = {
    bytesPerSecond: 100,
    committedBytes: 917,
    inFlightBytes: 83,
    kind: "upload",
    percent: 99.9,
    stage: "awaiting-ack",
    status: "active",
    total: 1000,
  };

  assert.equal(transferTitle(transfer), "Awaiting Vault");
  assert.equal(transferStageLabel(transfer), "Awaiting server acknowledgment");
  assert.equal(
    transferMeta(transfer),
    "91.7% secured - 917 B of 1000 B - 83 B awaiting confirmation"
  );
});

test("stalled and retrying uploads expose their state without preserving speculative progress", () => {
  const transfer = {
    attempt: 2,
    committedBytes: 400,
    inFlightBytes: 0,
    kind: "upload",
    maxAttempts: 3,
    noProgressSeconds: 0,
    retryDelayMs: 3000,
    stage: "retrying",
    status: "active",
    total: 1000,
  };

  assert.equal(transferTitle(transfer), "Retrying upload");
  assert.equal(transferStageLabel(transfer), "Retrying part - attempt 2 of 3");
  assert.equal(transferMeta(transfer), "40% secured - 400 B of 1000 B - Retrying in 3s");

  const stalled = {
    ...transfer,
    attempt: 1,
    inFlightBytes: 600,
    noProgressSeconds: 15,
    retryDelayMs: null,
    stage: "stalled",
    waitingForAcknowledgement: true,
  };
  assert.equal(transferTitle(stalled), "Upload stalled");
  assert.equal(transferStageLabel(stalled), "Server acknowledgment stalled");
  assert.equal(
    transferMeta(stalled),
    "Upload stalled for 15s - 40% secured - 400 B of 1000 B - 600 B awaiting confirmation"
  );
});

test("grouped completion and partial failure stay concise", () => {
  assert.equal(
    transferMeta({
      grouped: true,
      kind: "upload",
      processedItems: 5,
      status: "complete",
      total: 1024,
      totalItems: 5,
    }),
    "5 items - 1.0 KB complete"
  );
  assert.equal(
    transferTitle({
      grouped: true,
      kind: "upload",
      status: "error",
      succeededItems: 4,
    }),
    "Upload incomplete"
  );
});

test("multi-item downloads use the same aggregate visual language", () => {
  const transfer = {
    bytesPerSecond: 256,
    etaSeconds: 4,
    grouped: true,
    kind: "download",
    loaded: 512,
    noProgressSeconds: 0,
    percent: 50,
    processedItems: 3,
    serverStatus: "running",
    stage: "preparing",
    status: "active",
    total: 1024,
    totalItems: 8,
  };

  assert.equal(transferTitle(transfer), "Preparing download");
  assert.equal(transferStageLabel(transfer), "Packaging item 4 of 8");
  assert.equal(transferMeta(transfer), "3 of 8 items - 50% - 512 B of 1.0 KB - 256 B/s - 4s left");
  assert.equal(transferCanCancel(transfer), true);
});

test("the dock retains one popup for each independently started operation", () => {
  const transfers = [
    { id: 1, kind: "upload", name: "100 files", status: "active" },
    { id: 2, kind: "download", name: "archive.zip", status: "active" },
  ];
  const dock = TransferDock({ onCancelTransfer: () => {}, transfers });
  const rows = dock.children.flat();

  assert.equal(rows.length, 2);
  assert.deepEqual(
    rows.map((row) => row.props.transfer.id),
    [1, 2]
  );
});
