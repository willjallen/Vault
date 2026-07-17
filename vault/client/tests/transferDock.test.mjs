import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = { createElement: () => ({}) };
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
const { transferMeta, transferStageLabel, transferTitle } = await import(moduleUrl);

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
