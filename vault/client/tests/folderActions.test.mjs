import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/folderActions.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { createFolderActionHandlers } = await import(moduleUrl);

function folderHandlers(overrides = {}) {
  return createFolderActionHandlers({
    apiFetch: async () => ({ ok: true }),
    clearAllSelections: () => {},
    folder: "Project",
    handleArchiveItems: async () => true,
    inlineFolderDraft: null,
    postAction: async () => ({ failed: [] }),
    refresh: async () => {},
    refreshAfterAction: async () => {},
    replaceFolder: () => {},
    requestConfirm: async () => true,
    setBusy: () => {},
    setCreatingFolder: () => {},
    setError: () => {},
    setInlineFolderDraft: () => {},
    setSelectedId: () => {},
    ...overrides,
  });
}

test("create folder posts urlencoded form data", async () => {
  let request = null;
  const handlers = folderHandlers({
    apiFetch: async (url, options) => {
      request = { options, url };
      return { ok: true };
    },
  });

  const created = await handlers.handleCreateFolder("New Folder", "Project");

  assert.equal(created, true);
  assert.equal(request.url, "/folders");
  assert.equal(request.options.method, "POST");
  assert.ok(request.options.body instanceof URLSearchParams);
  assert.equal(request.options.body.toString(), "folder=Project%2FNew+Folder");
});

test("delete empty folder confirms irreversibility and sends its exact id and path", async () => {
  let confirmation = null;
  let request = null;
  const events = [];
  const handlers = folderHandlers({
    apiFetch: async (url, options) => {
      request = { options, url };
      events.push("request");
      return { ok: true };
    },
    clearAllSelections: () => events.push("clear"),
    folder: "Project Plans/Empty & Ready",
    refreshAfterAction: async (target) => events.push(`refresh:${target}`),
    replaceFolder: (target) => events.push(`navigate:${target}`),
    requestConfirm: async (options) => {
      confirmation = options;
      return true;
    },
  });

  const deleted = await handlers.handleDeleteEmptyFolder({
    can_delete_empty: true,
    id: 42,
    name: "ignored stale label",
    path: "Project Plans/Empty & Ready",
  });

  assert.equal(deleted, true);
  assert.deepEqual(confirmation, {
    title: "Delete empty folder",
    message: 'Permanently delete "Empty & Ready"? This cannot be undone.',
    confirmLabel: "Delete",
    tone: "danger",
  });
  const requestUrl = new URL(request.url, "https://vault.test");
  assert.equal(requestUrl.pathname, "/api/folders/42");
  assert.equal(requestUrl.searchParams.get("path"), "Project Plans/Empty & Ready");
  assert.deepEqual(request.options, { method: "DELETE" });
  assert.deepEqual(events, ["request", "refresh:Project Plans", "clear", "navigate:Project Plans"]);
});

test("delete empty folder cancellation makes no request or state change", async () => {
  let requests = 0;
  let clears = 0;
  const handlers = folderHandlers({
    apiFetch: async () => {
      requests += 1;
      return { ok: true };
    },
    clearAllSelections: () => {
      clears += 1;
    },
    requestConfirm: async () => false,
  });

  const deleted = await handlers.handleDeleteEmptyFolder({
    can_delete_empty: true,
    id: 7,
    path: "Project/Empty",
  });

  assert.equal(deleted, false);
  assert.equal(requests, 0);
  assert.equal(clears, 0);
});

test("delete empty folder requires the explicit capability and a valid identity", async () => {
  let requests = 0;
  const errors = [];
  const handlers = folderHandlers({
    apiFetch: async () => {
      requests += 1;
      return { ok: true };
    },
    setError: (error) => errors.push(error),
  });

  assert.equal(
    await handlers.handleDeleteEmptyFolder({ id: 8, path: "Project/Zero", size_bytes: 0 }),
    false
  );
  assert.equal(
    await handlers.handleDeleteEmptyFolder({
      can_delete_empty: true,
      id: null,
      path: "Project/NoId",
    }),
    false
  );
  assert.equal(requests, 0);
  assert.deepEqual(errors, [
    "Choose an empty Vault folder to delete.",
    "Choose an empty Vault folder to delete.",
  ]);
});

test("delete empty folder surfaces a stale server rejection without refreshing", async () => {
  const errors = [];
  let refreshes = 0;
  let clears = 0;
  const handlers = folderHandlers({
    apiFetch: async () => ({
      json: async () => ({ detail: "Folder is no longer empty" }),
      ok: false,
    }),
    clearAllSelections: () => {
      clears += 1;
    },
    refreshAfterAction: async () => {
      refreshes += 1;
    },
    setError: (error) => errors.push(error),
  });

  const deleted = await handlers.handleDeleteEmptyFolder({
    can_delete_empty: true,
    id: 9,
    path: "Project/WasEmpty",
  });

  assert.equal(deleted, false);
  assert.deepEqual(errors, ["", "Folder is no longer empty"]);
  assert.equal(refreshes, 0);
  assert.equal(clears, 0);
});
