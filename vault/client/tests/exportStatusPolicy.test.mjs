import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/exportStatusPolicy.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { EXPORT_NO_PROGRESS_NOTICE_MS, createExportProgressTracker, trackExportJobProgress } =
  await import(moduleUrl);

function exportJob(overrides = {}) {
  return {
    created_at: "2026-07-17T10:00:00Z",
    processed_bytes: 0,
    processed_items: 0,
    status: "queued",
    total_bytes: 1024,
    total_items: 3,
    updated_at: "2026-07-17T10:00:00Z",
    ...overrides,
  };
}

test("export progress preserves server state and reports unchanged activity", () => {
  const tracker = createExportProgressTracker(1000);
  const initial = trackExportJobProgress(exportJob(), tracker, 1000);

  assert.deepEqual(initial, {
    createdAt: "2026-07-17T10:00:00Z",
    loaded: 0,
    noProgressSeconds: 0,
    processedItems: 0,
    serverStatus: "queued",
    total: 1024,
    totalItems: 3,
    updatedAt: "2026-07-17T10:00:00Z",
  });
  assert.equal(
    trackExportJobProgress(exportJob(), tracker, 1000 + EXPORT_NO_PROGRESS_NOTICE_MS - 1)
      .noProgressSeconds,
    0
  );
  assert.equal(
    trackExportJobProgress(exportJob(), tracker, 1000 + EXPORT_NO_PROGRESS_NOTICE_MS + 2200)
      .noProgressSeconds,
    7
  );
});

test("export progress activity resets when status, bytes, items, or timestamp advance", () => {
  const tracker = createExportProgressTracker(0);
  trackExportJobProgress(exportJob(), tracker, 0);
  assert.equal(trackExportJobProgress(exportJob(), tracker, 6000).noProgressSeconds, 6);

  const running = exportJob({
    processed_bytes: 512,
    processed_items: 1,
    status: "running",
    updated_at: "2026-07-17T10:00:06Z",
  });
  const advanced = trackExportJobProgress(running, tracker, 6000);
  assert.equal(advanced.noProgressSeconds, 0);
  assert.equal(advanced.loaded, 512);
  assert.equal(advanced.processedItems, 1);
  assert.equal(advanced.serverStatus, "running");
  assert.equal(trackExportJobProgress(running, tracker, 11_999).noProgressSeconds, 5);
});

test("export progress rejects invalid numeric and timestamp fields", () => {
  const progress = trackExportJobProgress(
    exportJob({
      created_at: 12,
      processed_bytes: -1,
      processed_items: "invalid",
      total_bytes: undefined,
      total_items: Number.NaN,
      updated_at: {},
    }),
    createExportProgressTracker(0),
    0
  );

  assert.equal(progress.createdAt, null);
  assert.equal(progress.loaded, 0);
  assert.equal(progress.processedItems, null);
  assert.equal(progress.total, null);
  assert.equal(progress.totalItems, null);
  assert.equal(progress.updatedAt, null);
});
