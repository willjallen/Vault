import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

let hookSlots = new Map();
let hookIndex = 0;
let effectCallbacks = new Map();
let effectCleanups = new Map();
let effectIndex = 0;

function nextHookIndex() {
  const current = hookIndex;
  hookIndex += 1;
  return current;
}

globalThis.React = {
  useCallback: (callback) => callback,
  useEffect: (callback) => {
    const index = effectIndex;
    effectIndex += 1;
    effectCallbacks.set(index, callback);
  },
  useMemo: (factory) => factory(),
  useRef(initialValue) {
    const index = nextHookIndex();
    if (!hookSlots.has(index)) {
      hookSlots.set(index, { current: initialValue });
    }
    return hookSlots.get(index);
  },
  useState(initialValue) {
    const index = nextHookIndex();
    if (!hookSlots.has(index)) {
      hookSlots.set(index, typeof initialValue === "function" ? initialValue() : initialValue);
    }
    return [
      hookSlots.get(index),
      (nextValue) => {
        const current = hookSlots.get(index);
        hookSlots.set(index, typeof nextValue === "function" ? nextValue(current) : nextValue);
      },
    ];
  },
};

const sourceUrl = new URL("../src/lib/useVaultResources.js", import.meta.url);
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
const { mergeContentsPage, mergeSidebarPage, useVaultResources } = await import(moduleUrl);

const boundsSourceUrl = new URL("../src/lib/vaultResourceBounds.js", import.meta.url);
const boundsBundle = await build({
  bundle: true,
  entryPoints: [boundsSourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const boundsModuleUrl = `data:text/javascript;base64,${Buffer.from(
  boundsBundle.outputFiles.at(0).text
).toString("base64")}`;
const { BoundedPrefetchScheduler, ContentsPageCache, contentsScopeAffectedByUpload } = await import(
  boundsModuleUrl
);

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

function resetHooks() {
  effectCleanups.forEach((cleanup) => cleanup?.());
  effectCallbacks = new Map();
  effectCleanups = new Map();
  effectIndex = 0;
  hookSlots = new Map();
  hookIndex = 0;
}

function commitEffect(index) {
  effectCleanups.get(index)?.();
  effectCleanups.set(index, effectCallbacks.get(index)?.() || null);
}

function cleanupEffect(index) {
  effectCleanups.get(index)?.();
  effectCleanups.set(index, null);
}

function renderResources(overrides = {}) {
  hookIndex = 0;
  effectIndex = 0;
  return useVaultResources({
    apiFetch: async () => {
      throw new Error("Unexpected request");
    },
    folder: "Projects",
    initial: {
      contents: {
        documents: [],
        folder: "Projects",
        folders: [],
        next_cursor: null,
        q: "",
        recursive: false,
      },
      my_edits: { documents: [] },
      sidebar: { folder_children: {} },
    },
    onMissingFolder: () => {},
    onPreferencesRefresh: () => Promise.resolve(),
    onSiteSettingsChange: () => {},
    selectedId: null,
    setError: () => {},
    setSelectedId: () => {},
    showNotice: () => {},
    ...overrides,
  });
}

function cachePage(index) {
  return {
    documents: [],
    folder: `Parent-${index}`,
    folders: [{ id: index, path: `Child-${index}` }],
    next_cursor: null,
    q: "",
    recursive: false,
  };
}

async function flushMicrotasks() {
  await Promise.resolve();
  await Promise.resolve();
}

test("contents cache is a bounded LRU and evicts derived folder data with its page", () => {
  const cache = new ContentsPageCache(32);
  for (let index = 0; index < 32; index += 1) {
    cache.set(`key-${index}`, cachePage(index));
  }

  assert.equal(cache.get("key-0").folder, "Parent-0");
  cache.set("key-32", cachePage(32));

  assert.equal(cache.size, 32);
  assert.equal(cache.has("key-0"), true);
  assert.equal(cache.has("key-1"), false);
  const folderData = cache.folderData();
  assert.deepEqual(folderData.children["Parent-0"], ["Child-0"]);
  assert.equal(folderData.children["Parent-1"], undefined);
  assert.equal(folderData.metadata["Child-1"], undefined);
  assert.equal(folderData.metadata["Child-32"].id, 32);

  const protectedCache = new ContentsPageCache(3, [
    ["active", cachePage("active")],
    ["load-more", cachePage("load-more")],
    ["old", cachePage("old")],
  ]);
  protectedCache.set("incoming", cachePage("incoming"), ["active", "load-more"]);
  assert.equal(protectedCache.size, 3);
  assert.equal(protectedCache.has("active"), true);
  assert.equal(protectedCache.has("load-more"), true);
  assert.equal(protectedCache.has("old"), false);
  assert.equal(protectedCache.has("incoming"), true);
});

test("deleting a folder evicts all of only that folder's base and search pages", () => {
  const targetFolder = "Shared/Target";
  const otherFolder = "Shared/Other";
  const targetBase = {
    documents: [],
    folder: targetFolder,
    folders: [{ id: 101, path: `${targetFolder}/Child` }],
    next_cursor: null,
    q: "",
    recursive: false,
  };
  const otherBase = {
    documents: [],
    folder: otherFolder,
    folders: [{ id: 202, path: `${otherFolder}/Child` }],
    next_cursor: null,
    q: "",
    recursive: false,
  };
  const cache = new ContentsPageCache(10, [
    ["target-base", targetBase],
    ["target-search", { ...targetBase, q: "needle" }],
    ["target-recursive", { ...targetBase, recursive: true }],
    ["other-base", otherBase],
    ["other-search", { ...otherBase, q: "needle" }],
  ]);

  assert.deepEqual(cache.folderData().children[targetFolder], [`${targetFolder}/Child`]);
  assert.equal(cache.deleteFolder(targetFolder), true);

  assert.equal(cache.has("target-base"), false);
  assert.equal(cache.has("target-search"), false);
  assert.equal(cache.has("target-recursive"), false);
  assert.equal(cache.has("other-base"), true);
  assert.equal(cache.has("other-search"), true);
  assert.equal(cache.size, 2);
  const folderData = cache.folderData();
  assert.equal(folderData.children[targetFolder], undefined);
  assert.equal(folderData.metadata[`${targetFolder}/Child`], undefined);
  assert.deepEqual(folderData.children[otherFolder], [`${otherFolder}/Child`]);
  assert.equal(folderData.metadata[`${otherFolder}/Child`].id, 202);
  assert.equal(cache.deleteFolder(targetFolder), false);
});

test("upload invalidation evicts exact scopes and recursive ancestors only", () => {
  const uploadFolder = "Shared/Target";
  const page = (folder, { id, q = "", recursive = false } = {}) => ({
    documents: [],
    folder,
    folders: [{ id, path: `${folder ? `${folder}/` : ""}Child-${id}` }],
    next_cursor: null,
    q,
    recursive,
  });
  const cache = new ContentsPageCache(20, [
    ["target-base", page(uploadFolder, { id: 1 })],
    ["target-search", page(uploadFolder, { id: 2, q: "needle" })],
    ["ancestor-recursive", page("Shared", { id: 3, q: "needle", recursive: true })],
    ["root-recursive", page("", { id: 4, q: "needle", recursive: true })],
    ["ancestor-base", page("Shared", { id: 5 })],
    ["root-base", page("", { id: 6 })],
    ["descendant", page("Shared/Target/Child", { id: 7 })],
    ["prefix-only", page("Share", { id: 8, recursive: true })],
    ["unrelated", page("Other", { id: 9, recursive: true })],
  ]);

  assert.equal(cache.deleteUploadAffected(uploadFolder), true);

  for (const key of ["target-base", "target-search", "ancestor-recursive", "root-recursive"]) {
    assert.equal(cache.has(key), false, `${key} should be invalidated`);
  }
  for (const key of ["ancestor-base", "root-base", "descendant", "prefix-only", "unrelated"]) {
    assert.equal(cache.has(key), true, `${key} should be retained`);
  }
  const folderData = cache.folderData();
  assert.equal(folderData.children[uploadFolder], undefined);
  assert.deepEqual(folderData.children.Shared, ["Shared/Child-5"]);
  assert.deepEqual(folderData.children["Shared/Target/Child"], ["Shared/Target/Child/Child-7"]);
});

test("upload scope matching honors root and folder-segment boundaries", () => {
  const cases = [
    { expected: true, recursive: false, scope: "Shared/Target", upload: "Shared/Target" },
    { expected: true, recursive: true, scope: "Shared", upload: "Shared/Target" },
    { expected: true, recursive: true, scope: "", upload: "Shared/Target" },
    { expected: true, recursive: false, scope: "", upload: "" },
    { expected: false, recursive: false, scope: "Shared", upload: "Shared/Target" },
    { expected: false, recursive: true, scope: "Share", upload: "Shared/Target" },
    { expected: false, recursive: true, scope: "Shared/Target/Child", upload: "Shared/Target" },
    { expected: false, recursive: true, scope: "Other", upload: "Shared/Target" },
  ];

  for (const { expected, recursive, scope, upload } of cases) {
    assert.equal(
      contentsScopeAffectedByUpload(scope, recursive, upload),
      expected,
      JSON.stringify({ recursive, scope, upload })
    );
  }
});

test("prefetch scheduler bounds work, deduplicates, prioritizes, and aborts on clear", async () => {
  const scheduler = new BoundedPrefetchScheduler({ concurrency: 3, maxQueued: 32 });
  const runs = [];
  scheduler.setRunner(
    (payload, signal) =>
      new Promise((resolve) => {
        const run = { payload, resolve, signal };
        runs.push(run);
        signal.addEventListener("abort", resolve, { once: true });
      })
  );

  for (let index = 0; index < 35; index += 1) {
    assert.equal(scheduler.enqueue(`task-${index}`, { index }, 0), true);
  }
  assert.equal(scheduler.enqueue("overflow", {}, 0), false);
  assert.equal(scheduler.enqueue("task-0", {}, 2), false);
  assert.equal(scheduler.enqueue("priority", { priority: true }, 2), true);
  await flushMicrotasks();

  assert.equal(scheduler.activeCount, 3);
  assert.equal(scheduler.queuedCount, 32);
  assert.equal(scheduler.has("priority"), true);
  assert.equal(runs.length, 3);

  runs.at(0).resolve();
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(scheduler.activeCount, 3);
  assert.equal(runs.length, 4);

  scheduler.clear();
  assert.equal(scheduler.queuedCount, 0);
  assert.equal(scheduler.has("priority"), false);
  assert.equal(
    runs.filter((run) => run !== runs.at(0)).every((run) => run.signal.aborted),
    true
  );
  await flushMicrotasks();
});

test("prefetch completion from an old generation cannot remove a replacement task", async () => {
  const scheduler = new BoundedPrefetchScheduler({ concurrency: 1, maxQueued: 1 });
  const runs = [];
  scheduler.setRunner(
    (_payload, signal) =>
      new Promise((resolve) => {
        runs.push({ resolve, signal });
      })
  );

  scheduler.enqueue("same-key", { generation: 0 });
  await flushMicrotasks();
  scheduler.clear();
  assert.equal(runs.at(0).signal.aborted, true);
  scheduler.enqueue("same-key", { generation: 1 });
  assert.equal(scheduler.has("same-key"), true);
  assert.equal(scheduler.queuedCount, 1);

  runs.at(0).resolve();
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(runs.length, 2);
  assert.equal(scheduler.has("same-key"), true);

  runs.at(1).resolve();
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(scheduler.activeCount, 0);
  assert.equal(scheduler.has("same-key"), false);
});

test("targeted prefetch cancellation aborts active and queued work without losing replacement", async () => {
  const scheduler = new BoundedPrefetchScheduler({ concurrency: 1, maxQueued: 4 });
  const runs = [];
  scheduler.setRunner(
    (payload, signal) =>
      new Promise((resolve) => {
        runs.push({ payload, resolve, signal });
      })
  );

  assert.equal(scheduler.enqueue("same-key", { task: "active-old" }), true);
  assert.equal(scheduler.enqueue("queued-key", { task: "queued-cancelled" }), true);
  await flushMicrotasks();
  assert.equal(runs.length, 1);
  const queuedSignal = scheduler.tasks.get("queued-key").controller.signal;

  assert.equal(scheduler.cancel("queued-key"), true);
  assert.equal(queuedSignal.aborted, true);
  assert.equal(scheduler.queuedCount, 0);
  assert.equal(scheduler.has("queued-key"), false);
  assert.equal(scheduler.cancel("missing-key"), false);

  assert.equal(scheduler.cancel("same-key"), true);
  assert.equal(runs[0].signal.aborted, true);
  assert.equal(scheduler.has("same-key"), false);
  assert.equal(scheduler.enqueue("same-key", { task: "active-replacement" }), true);
  assert.equal(scheduler.has("same-key"), true);
  assert.equal(scheduler.queuedCount, 1);

  runs[0].resolve();
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(runs.length, 2);
  assert.deepEqual(runs[1].payload, { task: "active-replacement" });
  assert.equal(runs[1].signal.aborted, false);
  assert.equal(scheduler.has("same-key"), true);

  runs[1].resolve();
  await flushMicrotasks();
  await flushMicrotasks();
  assert.equal(scheduler.activeCount, 0);
  assert.equal(scheduler.has("same-key"), false);
  assert.equal(
    runs.some((run) => run.payload.task === "queued-cancelled"),
    false
  );
});

test("contents pages merge by stable ID and replace duplicate metadata", () => {
  const merged = mergeContentsPage(
    {
      documents: [
        { id: "document-1", name: "old" },
        { id: "document-1", name: "duplicate old" },
      ],
      folders: [{ id: "folder-1", name: "old folder" }],
      next_cursor: "first",
    },
    {
      documents: [
        { id: "document-1", name: "new" },
        { id: "document-2", name: "second" },
      ],
      folders: [
        { id: "folder-1", name: "new folder" },
        { id: "folder-2", name: "second folder" },
      ],
      next_cursor: "second",
    }
  );

  assert.deepEqual(merged.documents, [
    { id: "document-1", name: "new" },
    { id: "document-2", name: "second" },
  ]);
  assert.deepEqual(merged.folders, [
    { id: "folder-1", name: "new folder" },
    { id: "folder-2", name: "second folder" },
  ]);
  assert.equal(merged.next_cursor, "second");
});

test("sidebar pages deduplicate root children and merge keyed metadata", () => {
  const merged = mergeSidebarPage(
    {
      folder_children: { "": ["Projects", "Projects"], Projects: ["Projects/Old"] },
      folder_metadata: { Projects: { id: 1, icon: "folder" } },
      next_cursor: "first",
    },
    {
      folder_children: { "": ["Projects", "Teams"], Teams: ["Teams/Shared"] },
      folder_metadata: {
        Projects: { id: 1, icon: "briefcase" },
        Teams: { id: 2, icon: "users" },
      },
      next_cursor: "second",
    }
  );

  assert.deepEqual(merged.folder_children[""], ["Projects", "Teams"]);
  assert.deepEqual(merged.folder_children.Projects, ["Projects/Old"]);
  assert.deepEqual(merged.folder_children.Teams, ["Teams/Shared"]);
  assert.deepEqual(merged.folder_metadata, {
    Projects: { id: 1, icon: "briefcase" },
    Teams: { id: 2, icon: "users" },
  });
  assert.equal(merged.next_cursor, "second");
});

test("root contents cannot overwrite a larger paginated sidebar child list", () => {
  resetHooks();
  const resources = renderResources({
    folder: "",
    initial: {
      contents: {
        documents: [],
        folder: "",
        folders: [{ id: 1, name: "Projects", path: "Projects" }],
        next_cursor: "more-contents",
        q: "",
        recursive: false,
      },
      my_edits: { documents: [] },
      sidebar: {
        folder_children: { "": ["Projects", "Teams"] },
        folder_metadata: { Projects: { id: 1 }, Teams: { id: 2 } },
        next_cursor: null,
      },
    },
  });

  assert.deepEqual(resources.folderChildren[""], ["Projects", "Teams"]);
});

test("load more folders sends one opaque cursor request and merges the sidebar page", async () => {
  resetHooks();
  const response = deferred();
  const requests = [];
  const initial = {
    contents: {
      documents: [],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: {
      folder_children: { "": ["Projects"] },
      folder_metadata: { Projects: { id: 1, icon: "folder" } },
      next_cursor: "opaque sidebar+/=",
    },
  };
  const props = {
    apiFetch: async (url) => {
      requests.push(url);
      return response.promise;
    },
    initial,
  };

  let resources = renderResources(props);
  const loading = resources.sidebarPagination.loadMore();
  resources = renderResources(props);
  assert.equal(resources.sidebarPagination.hasMore, true);
  assert.equal(resources.sidebarPagination.loadingMore, true);
  assert.equal(requests.length, 1);
  const requestUrl = new URL(requests.at(0), "https://vault.invalid");
  assert.equal(requestUrl.pathname, "/api/folders/sidebar");
  assert.equal(requestUrl.searchParams.get("cursor"), "opaque sidebar+/=");

  response.resolve({
    json: async () => ({
      folder_children: { "": ["Projects", "Teams"] },
      folder_metadata: {
        Projects: { id: 1, icon: "briefcase" },
        Teams: { id: 2, icon: "users" },
      },
      next_cursor: null,
    }),
    ok: true,
  });
  await loading;

  resources = renderResources(props);
  assert.equal(resources.sidebarPagination.hasMore, false);
  assert.equal(resources.sidebarPagination.loadingMore, false);
  assert.deepEqual(resources.folderChildren[""], ["Projects", "Teams"]);
  assert.deepEqual(resources.folderMetadata.Projects, { id: 1, icon: "briefcase" });
  assert.deepEqual(resources.folderMetadata.Teams, { id: 2, icon: "users" });
});

test("a sidebar refresh invalidates an in-flight next page and replaces its rows", async () => {
  resetHooks();
  const stalePage = deferred();
  const freshPage = deferred();
  const initial = {
    contents: {
      documents: [],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: {
      folder_children: { "": ["Projects"] },
      folder_metadata: { Projects: { id: 1 } },
      next_cursor: "stale-cursor",
    },
  };
  const props = {
    apiFetch: async (url) => {
      if (url.startsWith("/api/folders/sidebar?")) {
        return stalePage.promise;
      }
      if (url === "/api/folders/sidebar") {
        return freshPage.promise;
      }
      if (url.startsWith("/api/folders/contents?")) {
        return { json: async () => initial.contents, ok: true };
      }
      if (url === "/api/my-edits") {
        return { json: async () => initial.my_edits, ok: true };
      }
      throw new Error(`Unexpected request: ${url}`);
    },
    initial,
  };

  let resources = renderResources(props);
  const staleLoading = resources.sidebarPagination.loadMore();
  const refreshing = resources.refresh("Projects", {
    invalidateContents: true,
    sidebar: true,
  });
  resources = renderResources(props);
  assert.equal(resources.sidebarPagination.hasMore, false);
  assert.equal(resources.sidebarPagination.loadingMore, false);

  stalePage.resolve({
    json: async () => ({
      folder_children: { "": ["Stale"] },
      folder_metadata: { Stale: { id: 99 } },
      next_cursor: null,
    }),
    ok: true,
  });
  assert.equal(await staleLoading, null);

  freshPage.resolve({
    json: async () => ({
      folder_children: { "": ["Fresh"] },
      folder_metadata: { Fresh: { id: 2 } },
      next_cursor: null,
    }),
    ok: true,
  });
  await refreshing;

  resources = renderResources(props);
  assert.deepEqual(resources.folderChildren[""], ["Fresh"]);
  assert.deepEqual(resources.folderMetadata, { Fresh: { id: 2 } });
});

test("load more sends the opaque cursor and merges one requested page", async () => {
  resetHooks();
  const response = deferred();
  const requests = [];
  const initial = {
    contents: {
      documents: [{ id: "document-1", name: "first" }],
      folder: "Projects",
      folders: [{ id: "folder-1", name: "First folder" }],
      next_cursor: "opaque cursor+/=",
      q: "needle",
      recursive: true,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: async (url) => {
      requests.push(url);
      return response.promise;
    },
    initial,
  };

  let resources = renderResources(props);
  resources.setSearchQuery(" needle ");
  resources.setRecursiveSearch(true);
  resources = renderResources(props);
  const loading = resources.loadMoreContents();

  resources = renderResources(props);
  assert.equal(resources.contentsLoadingMore, true);
  assert.equal(resources.contentsHasMore, true);
  assert.equal(requests.length, 1);
  const requestUrl = new URL(requests.at(0), "https://vault.invalid");
  assert.equal(requestUrl.searchParams.get("folder"), "Projects");
  assert.equal(requestUrl.searchParams.get("q"), " needle ");
  assert.equal(requestUrl.searchParams.get("recursive"), "true");
  assert.equal(requestUrl.searchParams.get("cursor"), "opaque cursor+/=");

  response.resolve({
    json: async () => ({
      documents: [
        { id: "document-1", name: "updated first" },
        { id: "document-2", name: "second" },
      ],
      folder: "Projects",
      folders: [{ id: "folder-2", name: "Second folder" }],
      next_cursor: null,
      q: "needle",
      recursive: true,
    }),
    ok: true,
  });
  await loading;

  resources = renderResources(props);
  assert.equal(resources.contentsLoadingMore, false);
  assert.equal(resources.contentsHasMore, false);
  assert.deepEqual(
    resources.docs.map(({ id, name }) => ({ id, name })),
    [
      { id: "document-1", name: "updated first" },
      { id: "document-2", name: "second" },
    ]
  );
  assert.deepEqual(
    resources.subfolders.map(({ id, name }) => ({ id, name })),
    [
      { id: "folder-1", name: "First folder" },
      { id: "folder-2", name: "Second folder" },
    ]
  );
});

test("a next page cannot merge after navigation away and back", async () => {
  resetHooks();
  const response = deferred();
  const initial = {
    contents: {
      documents: [{ id: "document-1", name: "first" }],
      folder: "Projects",
      folders: [],
      next_cursor: "next-projects",
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: async () => response.promise,
    initial,
  };

  let resources = renderResources(props);
  const loading = resources.loadMoreContents();
  renderResources({ ...props, folder: "Another folder" });
  response.resolve({
    json: async () => ({
      documents: [{ id: "document-2", name: "stale" }],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    }),
    ok: true,
  });
  assert.equal(await loading, null);

  resources = renderResources(props);
  assert.deepEqual(
    resources.docs.map((document) => document.id),
    ["document-1"]
  );
  assert.equal(resources.contentsHasMore, true);
  assert.equal(resources.contentsLoadingMore, false);
});

test("invalidating contents clears the cursor and a first page replaces cached rows", async () => {
  resetHooks();
  const contentsResponse = deferred();
  const initial = {
    contents: {
      documents: [{ id: "document-1", name: "old" }],
      folder: "Projects",
      folders: [],
      next_cursor: "old-cursor",
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: async (url) => {
      if (url.startsWith("/api/folders/contents?")) {
        return contentsResponse.promise;
      }
      if (url === "/api/my-edits") {
        return { json: async () => ({ documents: [] }), ok: true };
      }
      throw new Error(`Unexpected request: ${url}`);
    },
    initial,
  };

  let resources = renderResources(props);
  const refreshing = resources.refresh("Projects", { invalidateContents: true });
  resources = renderResources(props);
  assert.equal(resources.contentsHasMore, false);

  contentsResponse.resolve({
    json: async () => ({
      documents: [{ id: "document-2", name: "new" }],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    }),
    ok: true,
  });
  await refreshing;

  resources = renderResources(props);
  assert.deepEqual(
    resources.docs.map((document) => document.id),
    ["document-2"]
  );
  assert.equal(resources.contentsHasMore, false);
});

test("refresh after upload refreshes the active recursive ancestor scope", async () => {
  resetHooks();
  const requests = [];
  const errors = [];
  const initial = {
    contents: {
      documents: [],
      folder: "Shared",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: async (url, options = {}) => {
      requests.push({ options, url });
      if (url.startsWith("/api/folders/contents?")) {
        const requestUrl = new URL(url, "https://vault.invalid");
        return {
          json: async () => ({
            documents: [],
            folder: requestUrl.searchParams.get("folder"),
            folders: [],
            next_cursor: null,
            q: requestUrl.searchParams.get("q"),
            recursive: requestUrl.searchParams.get("recursive") === "true",
          }),
          ok: true,
          status: 200,
        };
      }
      if (url === "/api/my-edits") {
        return { json: async () => ({ documents: [] }), ok: true };
      }
      throw new Error(`Unexpected request: ${url}`);
    },
    folder: "Shared",
    initial,
    setError: (message) => errors.push(message),
  };

  let resources = renderResources(props);
  resources.setRecursiveSearch(true);
  resources = renderResources(props);
  await resources.refreshAfterUpload("Shared/Target");

  const contentsRequests = requests.filter(({ url }) => url.startsWith("/api/folders/contents?"));
  assert.equal(contentsRequests.length, 1);
  const requestUrl = new URL(contentsRequests[0].url, "https://vault.invalid");
  assert.equal(requestUrl.searchParams.get("folder"), "Shared");
  assert.equal(requestUrl.searchParams.get("q"), "");
  assert.equal(requestUrl.searchParams.get("recursive"), "true");
  assert.equal(contentsRequests[0].options.signal.aborted, false);
  assert.equal(requests.filter(({ url }) => url === "/api/my-edits").length, 1);
  assert.deepEqual(errors, []);
});

test("unrelated upload invalidation preserves foreground work and evicts affected cache", async () => {
  resetHooks();
  const targetFolder = "Shared/Target";
  const requests = [];
  const errors = [];
  const initial = {
    contents: {
      documents: [{ id: "cached-target", name: "cached.txt" }],
      folder: targetFolder,
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: (url, options = {}) => {
      const request = { options, url };
      requests.push(request);
      return new Promise((_resolve, reject) => {
        options.signal.addEventListener(
          "abort",
          () => {
            const error = new Error("aborted");
            error.name = "AbortError";
            reject(error);
          },
          { once: true }
        );
      });
    },
    initial,
    setError: (message) => errors.push(message),
  };

  renderResources({ ...props, folder: targetFolder });
  let resources = renderResources({ ...props, folder: "Unrelated" });
  commitEffect(0);
  assert.equal(requests.length, 1);
  assert.equal(
    new URL(requests[0].url, "https://vault.invalid").searchParams.get("folder"),
    "Unrelated"
  );
  assert.equal(requests[0].options.signal.aborted, false);

  await resources.refreshAfterUpload(targetFolder);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.signal.aborted, false);

  resources = renderResources({ ...props, folder: targetFolder });
  commitEffect(0);
  assert.equal(requests[0].options.signal.aborted, true);
  assert.equal(requests.length, 2);
  assert.equal(
    new URL(requests[1].url, "https://vault.invalid").searchParams.get("folder"),
    targetFolder
  );
  cleanupEffect(0);
  await flushMicrotasks();
  assert.equal(requests[1].options.signal.aborted, true);
  assert.deepEqual(errors, []);
});

test("search waits 250ms, captures exact parameters, and aborts superseded work", async () => {
  resetHooks();
  const requests = [];
  const errors = [];
  const initial = {
    contents: {
      documents: [],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: (url, options = {}) => {
      const request = { options, url };
      requests.push(request);
      if (requests.length === 1) {
        return new Promise((_resolve, reject) => {
          options.signal.addEventListener(
            "abort",
            () => {
              const error = new Error("aborted");
              error.name = "AbortError";
              reject(error);
            },
            { once: true }
          );
        });
      }
      const requestUrl = new URL(url, "https://vault.invalid");
      return Promise.resolve({
        json: async () => ({
          documents: [],
          folder: requestUrl.searchParams.get("folder"),
          folders: [],
          next_cursor: null,
          q: requestUrl.searchParams.get("q"),
          recursive: requestUrl.searchParams.get("recursive") === "true",
        }),
        ok: true,
        status: 200,
      });
    },
    initial,
    setError: (message) => errors.push(message),
  };

  let resources = renderResources(props);
  commitEffect(0);
  resources.setSearchQuery("n");
  resources = renderResources(props);
  commitEffect(0);
  resources.setSearchQuery("ne");
  resources = renderResources(props);
  commitEffect(0);

  await new Promise((resolve) => setTimeout(resolve, 275));
  assert.equal(requests.length, 1);
  assert.equal(new URL(requests.at(0).url, "https://vault.invalid").searchParams.get("q"), "ne");
  assert.equal(requests.at(0).options.signal.aborted, false);

  resources.setSearchQuery("needle");
  resources.setRecursiveSearch(true);
  resources = renderResources(props);
  commitEffect(0);
  assert.equal(requests.at(0).options.signal.aborted, true);

  await new Promise((resolve) => setTimeout(resolve, 275));
  assert.equal(requests.length, 2);
  const finalUrl = new URL(requests.at(1).url, "https://vault.invalid");
  assert.equal(finalUrl.searchParams.get("folder"), "Projects");
  assert.equal(finalUrl.searchParams.get("q"), "needle");
  assert.equal(finalUrl.searchParams.get("recursive"), "true");
  assert.deepEqual(errors, []);
  cleanupEffect(0);
});

test("ordinary folder navigation starts immediately without the search debounce", async () => {
  resetHooks();
  const requests = [];
  const initial = {
    contents: {
      documents: [],
      folder: "Projects",
      folders: [],
      next_cursor: null,
      q: "",
      recursive: false,
    },
    my_edits: { documents: [] },
    sidebar: { folder_children: {} },
  };
  const props = {
    apiFetch: async (url) => {
      requests.push(url);
      const requestUrl = new URL(url, "https://vault.invalid");
      return {
        json: async () => ({
          documents: [],
          folder: requestUrl.searchParams.get("folder"),
          folders: [],
          next_cursor: null,
          q: "",
          recursive: false,
        }),
        ok: true,
        status: 200,
      };
    },
    initial,
  };

  renderResources(props);
  commitEffect(0);
  renderResources({ ...props, folder: "Other" });
  commitEffect(0);

  assert.equal(requests.length, 1);
  assert.equal(
    new URL(requests.at(0), "https://vault.invalid").searchParams.get("folder"),
    "Other"
  );
  await flushMicrotasks();
  cleanupEffect(0);
});
