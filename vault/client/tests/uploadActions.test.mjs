import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/uploadActions.js", import.meta.url);
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
const { UploadFileScheduler, uploadFileBatch } = await import(moduleUrl);

function filesWithNames(names) {
  return names.map((name) => new File([name], name, { type: "application/octet-stream" }));
}

function nextTurn() {
  return new Promise((resolve) => setImmediate(resolve));
}

function nonemptyErrors() {
  const messages = [];
  return {
    messages,
    setError: (message) => {
      if (String(message || "").trim()) {
        messages.push(message);
      }
    },
  };
}

test("batch uploads use bounded default concurrency and refresh once after success", async () => {
  const files = filesWithNames(
    Array.from({ length: 12 }, (_, index) => `success-${index + 1}.bin`)
  );
  let active = 0;
  let maxActive = 0;
  const calls = [];
  const errors = nonemptyErrors();
  const refreshes = [];

  const result = await uploadFileBatch({
    files,
    targetFolder: "Shared/Incoming",
    uploadWithProgress: async (options) => {
      calls.push(options);
      active += 1;
      maxActive = Math.max(maxActive, active);
      await nextTurn();
      active -= 1;
      return { id: options.file.name, size: options.file.size };
    },
    refresh: async (...args) => refreshes.push(args),
    setError: errors.setError,
  });

  assert.deepEqual(
    calls.map((call) => call.file),
    files
  );
  assert.equal(maxActive, 2);
  assert.deepEqual(refreshes, [["Shared/Incoming", { invalidateContents: true }]]);
  assert.deepEqual(errors.messages, []);
  assert.equal(result.attempted, files.length);
  assert.equal(result.blocked, 0);
  assert.equal(result.cancelled, 0);
  assert.equal(result.failed, 0);
  assert.equal(result.succeeded, files.length);
  assert.equal(result.outcomes.length, files.length);
  assert.deepEqual(
    result.outcomes.map((outcome) => outcome.status),
    files.map(() => "fulfilled")
  );
});

test("batch uploads hard-cap caller-requested concurrency", async () => {
  const files = filesWithNames(Array.from({ length: 12 }, (_, index) => `capped-${index + 1}.bin`));
  let active = 0;
  let maxActive = 0;

  const result = await uploadFileBatch({
    concurrency: 99,
    files,
    targetFolder: "Shared/Incoming",
    uploadWithProgress: async ({ file }) => {
      active += 1;
      maxActive = Math.max(maxActive, active);
      await nextTurn();
      active -= 1;
      return { id: file.name };
    },
    refresh: async () => {},
    setError: () => {},
  });

  assert.equal(maxActive, 4);
  assert.equal(result.blocked, 0);
  assert.equal(result.succeeded, files.length);
});

test("batch uploads isolate failures and cancellations while preserving later successes", async () => {
  const files = filesWithNames(["first.bin", "broken.bin", "cancelled.bin", "last.bin"]);
  let active = 0;
  let maxActive = 0;
  const started = [];
  const completed = [];
  const errors = nonemptyErrors();
  const refreshes = [];

  const result = await uploadFileBatch({
    concurrency: 2,
    files,
    targetFolder: "Shared/Review",
    uploadWithProgress: async ({ file, ...options }) => {
      started.push(file.name);
      assert.equal(options.folder, "Shared/Review");
      assert.equal(options.mode, "create");
      assert.equal(options.name, file.name);
      assert.equal(options.size, file.size);
      active += 1;
      maxActive = Math.max(maxActive, active);
      await nextTurn();
      active -= 1;
      if (file.name === "broken.bin") {
        throw new Error("network refused upload");
      }
      if (file.name === "cancelled.bin") {
        return { cancelled: true };
      }
      completed.push(file.name);
      return { id: file.name, size: file.size };
    },
    refresh: async (...args) => refreshes.push(args),
    setError: errors.setError,
  });

  assert.deepEqual(
    started,
    files.map((file) => file.name)
  );
  assert.deepEqual(completed, ["first.bin", "last.bin"]);
  assert.equal(maxActive, 2);
  assert.deepEqual(refreshes, [["Shared/Review", { invalidateContents: true }]]);
  assert.equal(errors.messages.length, 1);
  assert.match(String(errors.messages[0]), /broken\.bin/i);
  assert.match(String(errors.messages[0]), /network refused upload/i);

  assert.equal(result.attempted, 4);
  assert.equal(result.blocked, 0);
  assert.equal(result.cancelled, 1);
  assert.equal(result.failed, 1);
  assert.equal(result.succeeded, 2);
  const outcomes = result.outcomes;
  assert.equal(outcomes.length, files.length);
  assert.deepEqual(
    outcomes.map((outcome) => outcome.file),
    files
  );
  assert.deepEqual(
    outcomes.map((outcome) => outcome.status),
    ["fulfilled", "rejected", "cancelled", "fulfilled"]
  );
  assert.equal(outcomes[0].value.id, "first.bin");
  assert.match(outcomes[1].reason.message, /network refused upload/i);
  assert.equal(outcomes[2].value.cancelled, true);
  assert.equal(outcomes[3].value.id, "last.bin");
});

test("batch uploads do not refresh when no file succeeds", async () => {
  const files = filesWithNames(["broken.bin", "cancelled.bin"]);
  const errors = nonemptyErrors();
  let refreshes = 0;

  const result = await uploadFileBatch({
    files,
    targetFolder: "Shared/Incoming",
    uploadWithProgress: async ({ file }) => {
      if (file.name === "cancelled.bin") {
        return { cancelled: true };
      }
      throw new Error("upload failed");
    },
    refresh: async () => {
      refreshes += 1;
    },
    setError: errors.setError,
  });

  assert.equal(refreshes, 0);
  assert.equal(errors.messages.length, 1);
  assert.ok(String(errors.messages[0]).trim());
  assert.equal(result.attempted, 2);
  assert.equal(result.blocked, 0);
  assert.equal(result.cancelled, 1);
  assert.equal(result.failed, 1);
  assert.equal(result.succeeded, 0);
  assert.deepEqual(
    result.outcomes.map((outcome) => outcome.status),
    ["rejected", "cancelled"]
  );
});

test("a blocked batch reports every blocked file without starting or refreshing", async () => {
  const files = filesWithNames(["first.bin", "second.bin", "third.bin"]);
  const blockedReason = "Uploads are disabled for this destination.";
  const errors = nonemptyErrors();
  let refreshes = 0;
  let starts = 0;

  const result = await uploadFileBatch({
    blockedReason,
    files,
    targetFolder: "Archive",
    uploadWithProgress: async () => {
      starts += 1;
      return {};
    },
    refresh: async () => {
      refreshes += 1;
    },
    setError: errors.setError,
  });

  assert.equal(starts, 0);
  assert.equal(refreshes, 0);
  assert.deepEqual(errors.messages, [blockedReason]);
  assert.equal(result.attempted, 0);
  assert.equal(result.blocked, files.length);
  assert.equal(result.cancelled, 0);
  assert.equal(result.failed, 0);
  assert.equal(result.succeeded, 0);
  assert.equal(result.outcomes.length, files.length);
});

test("overlapping batches share one global scheduler concurrency bound", async () => {
  const scheduler = new UploadFileScheduler(2);
  const firstFiles = filesWithNames(["first-a.bin", "first-b.bin", "first-c.bin"]);
  const secondFiles = filesWithNames(["second-a.bin", "second-b.bin", "second-c.bin"]);
  let active = 0;
  let maxActive = 0;
  const started = [];
  const refreshes = [];

  async function uploadWithProgress({ file, folder }) {
    started.push({ file, folder });
    active += 1;
    maxActive = Math.max(maxActive, active);
    await nextTurn();
    active -= 1;
    return { id: file.name };
  }

  const firstBatch = uploadFileBatch({
    files: firstFiles,
    refresh: async (folder) => refreshes.push(folder),
    scheduler,
    setError: () => {},
    targetFolder: "Shared/First",
    uploadWithProgress,
  });
  const secondBatch = uploadFileBatch({
    files: secondFiles,
    refresh: async (folder) => refreshes.push(folder),
    scheduler,
    setError: () => {},
    targetFolder: "Shared/Second",
    uploadWithProgress,
  });

  const [firstResult, secondResult] = await Promise.all([firstBatch, secondBatch]);

  assert.equal(maxActive, 2);
  assert.equal(started.length, firstFiles.length + secondFiles.length);
  assert.equal(firstResult.blocked, 0);
  assert.equal(firstResult.succeeded, firstFiles.length);
  assert.equal(secondResult.blocked, 0);
  assert.equal(secondResult.succeeded, secondFiles.length);
  assert.deepEqual(refreshes.sort(), ["Shared/First", "Shared/Second"]);
});

test("a batch rejects later duplicate normalized names before starting them", async () => {
  const first = new File(["first"], "report.txt", { type: "text/plain" });
  const duplicate = new File(["duplicate"], "staging\\report.txt ", { type: "text/plain" });
  const errors = nonemptyErrors();
  const started = [];
  let refreshes = 0;

  const result = await uploadFileBatch({
    files: [first, duplicate],
    refresh: async () => {
      refreshes += 1;
    },
    scheduler: new UploadFileScheduler(2),
    setError: errors.setError,
    targetFolder: "Shared/Incoming",
    uploadWithProgress: async ({ file }) => {
      started.push(file);
      await nextTurn();
      return { id: file.name };
    },
  });

  assert.deepEqual(started, [first]);
  assert.equal(refreshes, 1);
  assert.equal(errors.messages.length, 1);
  assert.equal(result.attempted, 2);
  assert.equal(result.blocked, 0);
  assert.equal(result.failed, 1);
  assert.equal(result.succeeded, 1);
  assert.equal(result.outcomes[0].file, first);
  assert.equal(result.outcomes[0].status, "fulfilled");
  assert.equal(result.outcomes[1].file, duplicate);
  assert.equal(result.outcomes[1].status, "rejected");
  assert.match(result.outcomes[1].reason.message, /report\.txt.*already queued/i);
});

test("the same normalized filename can upload to different folders", async () => {
  const scheduler = new UploadFileScheduler(2);
  const first = new File(["first"], "report.txt", { type: "text/plain" });
  const second = new File(["second"], "source\\report.txt", { type: "text/plain" });
  const started = [];

  function batch(file, targetFolder) {
    return uploadFileBatch({
      files: [file],
      refresh: async () => {},
      scheduler,
      setError: () => {},
      targetFolder,
      uploadWithProgress: async ({ file: uploadingFile, folder }) => {
        started.push({ file: uploadingFile, folder });
        await nextTurn();
        return { id: uploadingFile.name };
      },
    });
  }

  const [firstResult, secondResult] = await Promise.all([
    batch(first, "Shared/First"),
    batch(second, "Shared/Second"),
  ]);

  assert.deepEqual(started, [
    { file: first, folder: "Shared/First" },
    { file: second, folder: "Shared/Second" },
  ]);
  assert.equal(firstResult.blocked, 0);
  assert.equal(firstResult.succeeded, 1);
  assert.equal(secondResult.blocked, 0);
  assert.equal(secondResult.succeeded, 1);
});

test("a sustained upload stream compacts consumed scheduler entries", async () => {
  const scheduler = new UploadFileScheduler(1);
  const releases = [];
  const tasks = [];
  let maxBackingEntries = 0;

  function enqueue() {
    tasks.push(
      scheduler.run(
        () =>
          new Promise((resolve) => {
            releases.push(resolve);
          })
      )
    );
  }

  enqueue();
  await nextTurn();
  enqueue();
  enqueue();
  for (let index = 0; index < 320; index += 1) {
    enqueue();
    releases.shift()();
    await nextTurn();
    maxBackingEntries = Math.max(maxBackingEntries, scheduler.queue.length);
    assert.equal(scheduler.activeCount, 1);
    assert.equal(scheduler.queuedCount, 2);
  }

  while (scheduler.activeCount || scheduler.queuedCount) {
    releases.shift()();
    await nextTurn();
  }
  await Promise.all(tasks);

  assert.ok(maxBackingEntries <= 66, `scheduler retained ${maxBackingEntries} queue entries`);
  assert.equal(scheduler.queue.length, 0);
  assert.equal(scheduler.queuedCount, 0);
});
