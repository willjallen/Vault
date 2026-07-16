import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

let hookSlots = new Map();
let hookIndex = 0;

function nextHookIndex() {
  const current = hookIndex;
  hookIndex += 1;
  return current;
}

globalThis.React = {
  useCallback: (callback) => callback,
  useEffect: () => {},
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
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { mergeContentsPage, mergeSidebarPage, useVaultResources } = await import(moduleUrl);

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

function resetHooks() {
  hookSlots = new Map();
  hookIndex = 0;
}

function renderResources(overrides = {}) {
  hookIndex = 0;
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
  const requestUrl = new URL(requests[0], "https://vault.invalid");
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
  const requestUrl = new URL(requests[0], "https://vault.invalid");
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
