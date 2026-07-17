/* global AbortController, URL */

import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/fileLockActions.js", import.meta.url);
const bundle = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(
  bundle.outputFiles.at(0).text
).toString("base64")}`;
const { createFileLockActions } = await import(moduleUrl);

function jsonResponse(body, status = 200) {
  return {
    json: async () => body,
    ok: status >= 200 && status < 300,
    status,
  };
}

function createActions(overrides = {}) {
  return createFileLockActions({
    apiFetch: async () => {
      throw new Error("Unexpected API request");
    },
    currentUser: { id: "writer-1", name: "Writer One" },
    downloadWithProgress: async () => {
      throw new Error("Unexpected download");
    },
    refresh: () => {},
    setBusy: () => {},
    setError: () => {},
    updateDocument: () => {},
    uploadWithProgress: async () => ({}),
    ...overrides,
  });
}

const documentRecord = {
  archived: false,
  id: 7,
  lock: { by: null, name: null },
  name: "design.blend",
  size_bytes: 42,
};

test("start edit prepares a pinned checkout URL before optimistic lock update", async () => {
  const events = [];
  const signal = new AbortController().signal;
  let updated = null;
  const actions = createActions({
    apiFetch: async (url, options) => {
      events.push("checkout-post");
      assert.equal(url, "/documents/7/checkout");
      assert.equal(options.method, "POST");
      assert.equal(options.signal, signal);
      return jsonResponse({
        download_url: "/documents/7/versions/version-9/download",
      });
    },
    downloadWithProgress: async (options) => {
      events.push("download-start");
      assert.equal(options.name, "design.blend");
      assert.equal(options.size, 42);
      assert.equal(Object.hasOwn(options, "url"), false);
      const preparedUrl = await options.prepare(signal);
      events.push(`prepared:${preparedUrl}`);
      events.push("handoff-complete");
      return { browserManaged: true, status: 202 };
    },
    updateDocument: (docId, updater) => {
      events.push("optimistic-lock");
      assert.equal(docId, 7);
      updated = updater(documentRecord);
    },
  });

  assert.equal(await actions.handleStartEdit(documentRecord), true);
  assert.deepEqual(events, [
    "download-start",
    "checkout-post",
    "prepared:/documents/7/versions/version-9/download",
    "handoff-complete",
    "optimistic-lock",
  ]);
  assert.equal(updated.lock.by, "writer-1");
  assert.equal(updated.lock.name, "Writer One");
});

test("picker cancellation returns without preparing or optimistically locking", async () => {
  let apiCalls = 0;
  let updates = 0;
  let errors = 0;
  let refreshes = 0;
  const actions = createActions({
    apiFetch: async () => {
      apiCalls += 1;
      return jsonResponse({});
    },
    downloadWithProgress: async (options) => {
      assert.equal(typeof options.prepare, "function");
      return { cancelled: true, status: 0 };
    },
    refresh: () => {
      refreshes += 1;
    },
    setError: () => {
      errors += 1;
    },
    updateDocument: () => {
      updates += 1;
    },
  });

  assert.equal(await actions.handleStartEdit(documentRecord), false);
  assert.equal(apiCalls, 0);
  assert.equal(updates, 0);
  assert.equal(errors, 0);
  assert.equal(refreshes, 0);
});

test("checkout preparation surfaces the exact server error and refreshes lock state", async () => {
  const errors = [];
  let refreshes = 0;
  let updates = 0;
  const actions = createActions({
    apiFetch: async () => jsonResponse({ detail: "Document is locked by another user" }, 403),
    downloadWithProgress: async (options) => {
      await options.prepare(new AbortController().signal);
      throw new Error("preparation unexpectedly succeeded");
    },
    refresh: () => {
      refreshes += 1;
    },
    setError: (error) => errors.push(error),
    updateDocument: () => {
      updates += 1;
    },
  });

  assert.equal(await actions.handleStartEdit(documentRecord), false);
  assert.deepEqual(errors, ["Document is locked by another user"]);
  assert.equal(refreshes, 1);
  assert.equal(updates, 0);
});

test("checkout preparation rejects a success response without a pinned URL", async () => {
  const errors = [];
  const actions = createActions({
    apiFetch: async () => jsonResponse({ download_url: "" }),
    downloadWithProgress: async (options) => {
      await options.prepare(new AbortController().signal);
      return { browserManaged: true };
    },
    setError: (error) => errors.push(error),
  });

  assert.equal(await actions.handleStartEdit(documentRecord), false);
  assert.deepEqual(errors, ["Checkout did not return a download URL."]);
});
