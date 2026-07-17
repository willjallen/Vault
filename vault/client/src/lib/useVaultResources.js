import {
  BoundedPrefetchScheduler,
  CONTENTS_CACHE_LIMIT,
  ContentsPageCache,
  PREFETCH_PRIORITY_ROOT,
  PREFETCH_PRIORITY_SIDEBAR,
  PREFETCH_PRIORITY_VISIBLE,
  SEARCH_DEBOUNCE_MS,
} from "./vaultResourceBounds.js";

const { useCallback, useEffect, useMemo, useRef, useState } = React;

function contentsKey(folder, q, recursive) {
  return JSON.stringify([folder || "", (q || "").trim(), Boolean(recursive)]);
}

function normalizeContentsPage(contents) {
  return {
    ...contents,
    next_cursor: contents.next_cursor ?? null,
  };
}

function mergeByStableId(currentItems = [], nextItems = []) {
  const merged = [];
  const positions = new Map();

  function add(item) {
    const id = item?.id;
    if (id === null || id === undefined) {
      merged.push(item);
      return;
    }
    if (positions.has(id)) {
      merged[positions.get(id)] = item;
      return;
    }
    positions.set(id, merged.length);
    merged.push(item);
  }

  currentItems.forEach(add);
  nextItems.forEach(add);
  return merged;
}

export function mergeContentsPage(current, next) {
  return {
    ...current,
    ...next,
    documents: mergeByStableId(current.documents, next.documents),
    folders: mergeByStableId(current.folders, next.folders),
    next_cursor: next.next_cursor ?? null,
  };
}

function mergeUniquePaths(currentPaths = [], nextPaths = []) {
  return [...new Set([...currentPaths, ...nextPaths])];
}

function mergeFolderChildrenMaps(currentChildren = {}, nextChildren = {}) {
  const merged = new Map(Object.entries(currentChildren));
  Object.entries(nextChildren).forEach(([parentPath, paths]) => {
    merged.set(parentPath, mergeUniquePaths(merged.get(parentPath) || [], paths || []));
  });
  return Object.fromEntries(merged);
}

function normalizeSidebarPage(sidebar) {
  return {
    ...sidebar,
    next_cursor: sidebar.next_cursor ?? null,
  };
}

export function mergeSidebarPage(current, next) {
  const currentChildren = current.folder_children || {};
  const nextChildren = next.folder_children || {};
  return {
    ...current,
    ...next,
    folder_children: {
      ...currentChildren,
      ...nextChildren,
      "": mergeUniquePaths(currentChildren[""] || [], nextChildren[""] || []),
    },
    folder_metadata: {
      ...(current.folder_metadata || {}),
      ...(next.folder_metadata || {}),
    },
    next_cursor: next.next_cursor ?? null,
  };
}

function emptyContents(folder, q, recursive) {
  return {
    folder: folder || "",
    q: q || "",
    recursive: Boolean(recursive),
    folders: [],
    documents: [],
    next_cursor: null,
  };
}

function isContentsPending({
  activeContentsCached,
  activeContentsKey,
  contents,
  folder,
  recursiveSearch,
  searchQuery,
  storedContentsKey,
}) {
  return Boolean(
    (searchQuery || recursiveSearch) &&
    storedContentsKey !== activeContentsKey &&
    !activeContentsCached &&
    (contents.folder || "") === (folder || "")
  );
}

function isPendingEmptySearch({ contents, contentsPending, recursiveSearch, searchQuery }) {
  return Boolean(
    contentsPending &&
    (searchQuery || recursiveSearch) &&
    (contents.q || contents.recursive) &&
    !(contents.documents || []).length &&
    !(contents.folders || []).length
  );
}

function criticalRefresh(promise) {
  return promise.then(
    () => ({ ok: true }),
    (error) => ({ error, ok: Boolean(error?.suppressRefreshError) })
  );
}

function optionalRefresh(promise) {
  return promise.catch(() => null);
}

function prepareContentsRequest({
  beginForegroundContentsRequest,
  contentRequestRef,
  folderRef,
  nextFolder,
  options,
  recursiveSearchRef,
  searchQueryRef,
}) {
  const background = Boolean(options.background);
  const controller = background ? null : (options.controller ?? beginForegroundContentsRequest());
  const signal = background ? options.signal : controller.signal;
  const targetFolder = nextFolder ?? folderRef.current;
  const q = options.q ?? searchQueryRef.current;
  const recursive = options.recursive ?? recursiveSearchRef.current;
  const requestId = background ? null : contentRequestRef.current + 1;
  const key = contentsKey(targetFolder, q, recursive);
  const params = new URLSearchParams({
    folder: targetFolder || "",
    q: q || "",
    recursive: recursive ? "true" : "false",
  });
  return {
    background,
    controller,
    key,
    requestId,
    signal,
    targetFolder,
    url: `/api/folders/contents?${params.toString()}`,
  };
}

function contentsRequestIsDiscarded({
  background,
  cacheGeneration,
  contentsCacheGenerationRef,
  contentRequestRef,
  requestId,
  signal,
}) {
  return Boolean(
    signal?.aborted ||
    cacheGeneration !== contentsCacheGenerationRef.current ||
    (!background && requestId !== contentRequestRef.current)
  );
}

async function readContentsResponse({
  background,
  onMissingFolder,
  options,
  response,
  targetFolder,
}) {
  if (response.status === 404) {
    if (!background && onMissingFolder) {
      onMissingFolder(targetFolder, {
        fallbackFolder: options.missingFolderFallback || "",
        suppressError: Boolean(options.suppressMissingFolderError),
      });
    }
    const error = new Error("Folder not found");
    error.suppressRefreshError = Boolean(options.suppressMissingFolderError);
    throw error;
  }
  if (!response.ok) {
    throw new Error("Could not refresh contents");
  }
  return response.json();
}

export function useVaultResources({
  initial,
  apiFetch,
  folder,
  onMissingFolder,
  onPreferencesRefresh,
  onSiteSettingsChange,
  selectedId,
  setSelectedId,
  setError,
  showNotice,
}) {
  const initialContents = normalizeContentsPage(initial.contents || { folders: [], documents: [] });
  const initialSidebar = normalizeSidebarPage(initial.sidebar || { folder_children: {} });
  const initialMyEdits = initial.my_edits || { documents: [] };
  const initialContentsKey = contentsKey(
    initialContents.folder || "",
    initialContents.q || "",
    initialContents.recursive
  );
  const [contents, setContents] = useState(initialContents);
  const [sidebar, setSidebar] = useState(initialSidebar);
  const [contentsCache] = useState(
    () => new ContentsPageCache(CONTENTS_CACHE_LIMIT, [[initialContentsKey, initialContents]])
  );
  const [contentsFolderData, setContentsFolderData] = useState(() => contentsCache.folderData());
  const [prefetchScheduler] = useState(() => new BoundedPrefetchScheduler());
  const [myEditsState, setMyEditsState] = useState(initialMyEdits);
  const [selectedDocDetail, setSelectedDocDetail] = useState(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [recursiveSearch, setRecursiveSearch] = useState(false);
  const [loadingMoreRequest, setLoadingMoreRequest] = useState(null);
  const [sidebarLoadingMoreRequest, setSidebarLoadingMoreRequest] = useState(null);
  const contentRequestRef = useRef(0);
  const contentsCacheGenerationRef = useRef(0);
  const foregroundContentsControllerRef = useRef(null);
  const loadingMoreRequestRef = useRef(null);
  const sidebarRef = useRef(initialSidebar);
  const sidebarRequestRef = useRef(0);
  const sidebarGenerationRef = useRef(0);
  const sidebarLoadingMoreRequestRef = useRef(null);
  const detailRequestRef = useRef(0);
  const onMissingFolderRef = useRef(onMissingFolder);
  const onPreferencesRefreshRef = useRef(onPreferencesRefresh);
  const folderRef = useRef(folder || "");
  const searchQueryRef = useRef(searchQuery);
  const recursiveSearchRef = useRef(recursiveSearch);
  const selectedIdRef = useRef(selectedId);

  folderRef.current = folder || "";
  onMissingFolderRef.current = onMissingFolder;
  onPreferencesRefreshRef.current = onPreferencesRefresh;
  searchQueryRef.current = searchQuery;
  recursiveSearchRef.current = recursiveSearch;
  selectedIdRef.current = selectedId;
  sidebarRef.current = sidebar;

  const activeContentsKey = contentsKey(folder, searchQuery, recursiveSearch);
  const contentsContextRef = useRef({ generation: 0, key: activeContentsKey });
  if (contentsContextRef.current.key !== activeContentsKey) {
    contentsContextRef.current = {
      generation: contentsContextRef.current.generation + 1,
      key: activeContentsKey,
    };
    contentRequestRef.current += 1;
  }
  const activeCachedContents = contentsCache.get(activeContentsKey);
  const storedContentsKey = contentsKey(
    contents.folder || "",
    contents.q || "",
    contents.recursive
  );
  const activeContentsCached = Boolean(activeCachedContents);
  const contentsPending = isContentsPending({
    activeContentsCached,
    activeContentsKey,
    contents,
    folder,
    recursiveSearch,
    searchQuery,
    storedContentsKey,
  });
  const contentsPendingEmptySearch = isPendingEmptySearch({
    contents,
    contentsPending,
    recursiveSearch,
    searchQuery,
  });
  const displayedContents = useMemo(() => {
    if (storedContentsKey === activeContentsKey) {
      return contents;
    }
    if (activeCachedContents) {
      return activeCachedContents;
    }
    if (contentsPending) {
      return contents;
    }
    return emptyContents(folder, searchQuery, recursiveSearch);
  }, [
    activeContentsKey,
    activeCachedContents,
    contents,
    contentsPending,
    folder,
    recursiveSearch,
    searchQuery,
    storedContentsKey,
  ]);

  const docs = useMemo(() => displayedContents.documents || [], [displayedContents.documents]);
  const subfolders = useMemo(() => displayedContents.folders || [], [displayedContents.folders]);
  const sidebarChildren = useMemo(() => sidebar.folder_children || {}, [sidebar.folder_children]);
  const sidebarMetadata = useMemo(() => sidebar.folder_metadata || {}, [sidebar.folder_metadata]);
  const { children: contentsChildren, metadata: contentsMetadata } = contentsFolderData;
  const folderChildren = useMemo(() => {
    return mergeFolderChildrenMaps(sidebarChildren, contentsChildren);
  }, [contentsChildren, sidebarChildren]);
  const folderMetadata = useMemo(() => {
    return { ...sidebarMetadata, ...contentsMetadata };
  }, [contentsMetadata, sidebarMetadata]);
  const selectedDoc = selectedDocDetail || docs.find((doc) => doc.id === selectedId) || null;
  const myEdits = myEditsState.documents || [];
  const displayedContentsKey = contentsKey(
    displayedContents.folder || "",
    displayedContents.q || "",
    displayedContents.recursive
  );
  const contentsHasMore = Boolean(
    displayedContentsKey === activeContentsKey && displayedContents.next_cursor
  );
  const contentsLoadingMore = Boolean(
    loadingMoreRequest &&
    loadingMoreRequest.key === activeContentsKey &&
    loadingMoreRequest.contextGeneration === contentsContextRef.current.generation
  );
  const sidebarHasMore = Boolean(sidebar.next_cursor);
  const sidebarLoadingMore = Boolean(
    sidebarLoadingMoreRequest &&
    sidebarLoadingMoreRequest.requestId === sidebarRequestRef.current &&
    sidebarLoadingMoreRequest.generation === sidebarGenerationRef.current
  );

  const abortForegroundContentsRequest = useCallback(() => {
    foregroundContentsControllerRef.current?.abort();
    foregroundContentsControllerRef.current = null;
  }, []);

  const beginForegroundContentsRequest = useCallback(() => {
    abortForegroundContentsRequest();
    const controller = new AbortController();
    foregroundContentsControllerRef.current = controller;
    return controller;
  }, [abortForegroundContentsRequest]);

  const finishForegroundContentsRequest = useCallback((controller) => {
    if (foregroundContentsControllerRef.current === controller) {
      foregroundContentsControllerRef.current = null;
    }
  }, []);

  const invalidateContentsCache = useCallback(() => {
    abortForegroundContentsRequest();
    prefetchScheduler.clear();
    contentsCacheGenerationRef.current += 1;
    contentRequestRef.current += 1;
    contentsCache.clear();
    loadingMoreRequestRef.current = null;
    setLoadingMoreRequest(null);
    setContents((previous) => ({ ...previous, next_cursor: null }));
    setContentsFolderData({ children: {}, metadata: {} });
  }, [abortForegroundContentsRequest, contentsCache, prefetchScheduler]);

  const invalidateSidebar = useCallback(() => {
    sidebarGenerationRef.current += 1;
    sidebarRequestRef.current += 1;
    sidebarLoadingMoreRequestRef.current = null;
    setSidebarLoadingMoreRequest(null);
    const invalidated = { ...sidebarRef.current, next_cursor: null };
    sidebarRef.current = invalidated;
    setSidebar(invalidated);
  }, []);

  const rememberContents = useCallback(
    (data) => {
      const normalized = normalizeContentsPage(data);
      const key = contentsKey(normalized.folder || "", normalized.q || "", normalized.recursive);
      contentsCache.set(key, normalized, [
        contentsContextRef.current.key,
        loadingMoreRequestRef.current?.key,
      ]);
      setContentsFolderData(contentsCache.folderData());
      return normalized;
    },
    [contentsCache]
  );

  const fetchContents = useCallback(
    async (nextFolder, options = {}) => {
      const request = prepareContentsRequest({
        beginForegroundContentsRequest,
        contentRequestRef,
        folderRef,
        nextFolder,
        options,
        recursiveSearchRef,
        searchQueryRef,
      });
      const cacheGeneration = contentsCacheGenerationRef.current;
      if (!request.background) {
        contentRequestRef.current = request.requestId;
      }
      const discarded = () =>
        contentsRequestIsDiscarded({
          background: request.background,
          cacheGeneration,
          contentsCacheGenerationRef,
          contentRequestRef,
          requestId: request.requestId,
          signal: request.signal,
        });
      try {
        const response = await apiFetch(request.url, { signal: request.signal });
        if (discarded()) {
          return null;
        }
        const responseData = await readContentsResponse({
          background: request.background,
          onMissingFolder: onMissingFolderRef.current,
          options,
          response,
          targetFolder: request.targetFolder,
        });
        if (discarded()) {
          return null;
        }
        const data = rememberContents(responseData);
        if (request.background) {
          return null;
        }
        if (
          request.key ===
          contentsKey(folderRef.current, searchQueryRef.current, recursiveSearchRef.current)
        ) {
          setContents(data);
        }
        return data;
      } catch (error) {
        if (request.signal?.aborted) {
          return null;
        }
        throw error;
      } finally {
        if (request.controller) {
          finishForegroundContentsRequest(request.controller);
        }
      }
    },
    [apiFetch, beginForegroundContentsRequest, finishForegroundContentsRequest, rememberContents]
  );

  const loadMoreContents = useCallback(async () => {
    const targetFolder = folderRef.current;
    const q = searchQueryRef.current;
    const recursive = recursiveSearchRef.current;
    const key = contentsKey(targetFolder, q, recursive);
    const current = contentsCache.get(key);
    const cursor = current?.next_cursor;
    const contextGeneration = contentsContextRef.current.generation;
    const activeLoad = loadingMoreRequestRef.current;
    if (
      !cursor ||
      (activeLoad?.key === key && activeLoad.contextGeneration === contextGeneration)
    ) {
      return null;
    }

    const controller = beginForegroundContentsRequest();
    const requestId = contentRequestRef.current + 1;
    contentRequestRef.current = requestId;
    const cacheGeneration = contentsCacheGenerationRef.current;
    const request = { contextGeneration, key, requestId };
    loadingMoreRequestRef.current = request;
    setLoadingMoreRequest(request);

    const isCurrentRequest = () =>
      requestId === contentRequestRef.current &&
      cacheGeneration === contentsCacheGenerationRef.current &&
      contentsContextRef.current.key === key &&
      contentsContextRef.current.generation === contextGeneration;

    try {
      const params = new URLSearchParams({
        folder: targetFolder || "",
        q: q || "",
        recursive: recursive ? "true" : "false",
        cursor,
      });
      const res = await apiFetch(`/api/folders/contents?${params.toString()}`, {
        signal: controller.signal,
      });
      if (!isCurrentRequest()) {
        return null;
      }
      if (!res.ok) {
        throw new Error("Could not load more contents");
      }
      const page = normalizeContentsPage(await res.json());
      if (!isCurrentRequest()) {
        return null;
      }
      if (contentsKey(page.folder || "", page.q || "", page.recursive) !== key) {
        throw new Error("Contents page did not match the current view");
      }
      const latest = contentsCache.get(key);
      if (!latest || latest.next_cursor !== cursor) {
        return null;
      }
      const merged = mergeContentsPage(latest, page);
      rememberContents(merged);
      if (!isCurrentRequest()) {
        return null;
      }
      setContents(merged);
      return merged;
    } catch {
      if (isCurrentRequest() && !controller.signal.aborted) {
        setError("Could not load more contents.");
      }
      return null;
    } finally {
      if (loadingMoreRequestRef.current?.requestId === requestId) {
        loadingMoreRequestRef.current = null;
        setLoadingMoreRequest(null);
      }
      finishForegroundContentsRequest(controller);
    }
  }, [
    apiFetch,
    beginForegroundContentsRequest,
    contentsCache,
    finishForegroundContentsRequest,
    rememberContents,
    setError,
  ]);

  prefetchScheduler.setRunner(async ({ key, targetFolder }, signal) => {
    if (signal.aborted || contentsCache.has(key)) {
      return;
    }
    await fetchContents(targetFolder, {
      background: true,
      q: "",
      recursive: false,
      signal,
    });
  });

  const prefetchContents = useCallback(
    (targetFolder, priority = PREFETCH_PRIORITY_SIDEBAR) => {
      const key = contentsKey(targetFolder, "", false);
      if (contentsCache.has(key)) {
        return false;
      }
      return prefetchScheduler.enqueue(key, { key, targetFolder }, priority);
    },
    [contentsCache, prefetchScheduler]
  );

  const fetchSidebar = useCallback(async () => {
    const requestId = sidebarRequestRef.current + 1;
    sidebarRequestRef.current = requestId;
    sidebarLoadingMoreRequestRef.current = null;
    setSidebarLoadingMoreRequest(null);
    const generation = sidebarGenerationRef.current;
    const res = await apiFetch("/api/folders/sidebar");
    if (requestId !== sidebarRequestRef.current || generation !== sidebarGenerationRef.current) {
      return null;
    }
    if (!res.ok) {
      throw new Error("Could not refresh folders");
    }
    const data = normalizeSidebarPage(await res.json());
    if (requestId !== sidebarRequestRef.current || generation !== sidebarGenerationRef.current) {
      return null;
    }
    sidebarRef.current = data;
    setSidebar(data);
    return data;
  }, [apiFetch]);

  const loadMoreSidebar = useCallback(async () => {
    const cursor = sidebarRef.current.next_cursor;
    const activeLoad = sidebarLoadingMoreRequestRef.current;
    if (!cursor || activeLoad) {
      return null;
    }

    const requestId = sidebarRequestRef.current + 1;
    sidebarRequestRef.current = requestId;
    const generation = sidebarGenerationRef.current;
    const request = { generation, requestId };
    sidebarLoadingMoreRequestRef.current = request;
    setSidebarLoadingMoreRequest(request);

    const isCurrentRequest = () =>
      requestId === sidebarRequestRef.current && generation === sidebarGenerationRef.current;

    try {
      const params = new URLSearchParams({ cursor });
      const res = await apiFetch(`/api/folders/sidebar?${params.toString()}`);
      if (!isCurrentRequest()) {
        return null;
      }
      if (!res.ok) {
        throw new Error("Could not load more folders");
      }
      const page = normalizeSidebarPage(await res.json());
      if (!isCurrentRequest()) {
        return null;
      }
      const latest = sidebarRef.current;
      if (latest.next_cursor !== cursor) {
        return null;
      }
      const merged = mergeSidebarPage(latest, page);
      sidebarRef.current = merged;
      setSidebar(merged);
      return merged;
    } catch {
      if (isCurrentRequest()) {
        setError("Could not load more folders.");
      }
      return null;
    } finally {
      if (sidebarLoadingMoreRequestRef.current?.requestId === requestId) {
        sidebarLoadingMoreRequestRef.current = null;
        setSidebarLoadingMoreRequest(null);
      }
    }
  }, [apiFetch, setError]);

  const fetchMyEdits = useCallback(async () => {
    const res = await apiFetch("/api/my-edits");
    if (!res.ok) {
      throw new Error("Could not refresh edits");
    }
    const data = await res.json();
    setMyEditsState(data);
    return data;
  }, [apiFetch]);

  const fetchSettings = useCallback(async () => {
    if (!onSiteSettingsChange) {
      return null;
    }
    const res = await apiFetch("/api/settings");
    if (!res.ok) {
      throw new Error("Could not refresh settings");
    }
    const data = await res.json();
    onSiteSettingsChange(data.settings || {});
    return data;
  }, [apiFetch, onSiteSettingsChange]);

  const fetchDocumentDetail = useCallback(
    async (docId) => {
      if (!docId) {
        setSelectedDocDetail(null);
        return null;
      }
      const requestId = detailRequestRef.current + 1;
      detailRequestRef.current = requestId;
      const res = await apiFetch(`/api/documents/${docId}/detail`);
      if (requestId !== detailRequestRef.current || selectedIdRef.current !== docId) {
        return null;
      }
      if (res.status === 404) {
        setSelectedDocDetail(null);
        if (selectedIdRef.current === docId) {
          setSelectedId(null);
        }
        return null;
      }
      if (!res.ok) {
        throw new Error("Could not refresh document");
      }
      const data = await res.json();
      if (requestId !== detailRequestRef.current || selectedIdRef.current !== docId) {
        return null;
      }
      setSelectedDocDetail(data);
      return data;
    },
    [apiFetch, setSelectedId]
  );

  const refresh = useCallback(
    async (nextFolder, options = {}) => {
      if (options.invalidateContents) {
        invalidateContentsCache();
      }
      if (options.sidebar) {
        invalidateSidebar();
      }
      const [contentsRefresh, sidebarRefresh] = await Promise.all([
        criticalRefresh(
          fetchContents(nextFolder, {
            missingFolderFallback: options.missingFolderFallback,
            suppressMissingFolderError: options.suppressMissingFolderError,
          })
        ),
        options.sidebar ? criticalRefresh(fetchSidebar()) : Promise.resolve({ ok: true }),
        optionalRefresh(fetchMyEdits()),
        selectedIdRef.current
          ? optionalRefresh(fetchDocumentDetail(selectedIdRef.current))
          : Promise.resolve(null),
      ]);
      if (!contentsRefresh.ok || !sidebarRefresh.ok) {
        const failedRefresh = contentsRefresh.ok ? sidebarRefresh : contentsRefresh;
        setError(
          failedRefresh.error?.status === 0
            ? failedRefresh.error.message
            : "Could not refresh data."
        );
      }
    },
    [
      fetchContents,
      fetchDocumentDetail,
      fetchMyEdits,
      fetchSidebar,
      invalidateContentsCache,
      invalidateSidebar,
      setError,
    ]
  );

  const updateDocumentInViews = useCallback(
    (docId, updater) => {
      const updateList = (items = []) =>
        items.map((item) => (item.id === docId ? updater(item) : item));
      contentsCache.updateDocument(docId, updater);
      setContents((prev) => ({ ...prev, documents: updateList(prev.documents) }));
      setMyEditsState((prev) => ({ ...prev, documents: updateList(prev.documents) }));
      setSelectedDocDetail((prev) => (prev && prev.id === docId ? updater(prev) : prev));
    },
    [contentsCache]
  );

  useEffect(() => {
    abortForegroundContentsRequest();
    const key = contentsKey(folder, searchQuery, recursiveSearch);
    const cached = contentsCache.get(key);
    if (cached) {
      setContents(cached);
      return undefined;
    }
    if (!searchQuery && !recursiveSearch) {
      setContents(emptyContents(folder, searchQuery, recursiveSearch));
    }
    const controller = beginForegroundContentsRequest();
    let timer = null;
    const request = () => {
      fetchContents(folder, {
        controller,
        q: searchQuery,
        recursive: recursiveSearch,
      }).catch(() => {
        if (!controller.signal.aborted) {
          setError("Could not refresh contents.");
        }
      });
    };
    if (searchQuery || recursiveSearch) {
      timer = setTimeout(request, SEARCH_DEBOUNCE_MS);
    } else {
      request();
    }
    return () => {
      if (timer) {
        clearTimeout(timer);
      }
      controller.abort();
      finishForegroundContentsRequest(controller);
    };
  }, [
    beginForegroundContentsRequest,
    abortForegroundContentsRequest,
    contentsCache,
    fetchContents,
    finishForegroundContentsRequest,
    folder,
    recursiveSearch,
    searchQuery,
    setError,
  ]);

  useEffect(() => {
    if (displayedContents.q || displayedContents.recursive) {
      return;
    }
    (displayedContents.folders || []).forEach((item) =>
      prefetchContents(item.path, PREFETCH_PRIORITY_VISIBLE)
    );
  }, [displayedContents, prefetchContents]);

  useEffect(() => {
    prefetchContents("", PREFETCH_PRIORITY_ROOT);
    prefetchContents("Archive", PREFETCH_PRIORITY_ROOT);
    Object.values(sidebarChildren)
      .flat()
      .forEach((path) => prefetchContents(path, PREFETCH_PRIORITY_SIDEBAR));
  }, [prefetchContents, sidebarChildren]);

  useEffect(() => {
    if (!selectedId) {
      setSelectedDocDetail(null);
      return;
    }
    fetchDocumentDetail(selectedId).catch(() => setError("Could not refresh document."));
  }, [fetchDocumentDetail, selectedId, setError]);

  useEffect(() => {
    const events = new EventSource("/api/events/stream");
    const pendingResources = new Set();
    let connectionReported = false;
    let refreshTimer = null;

    function flushPendingRefreshes() {
      const resources = new Set(pendingResources);
      pendingResources.clear();
      refreshTimer = null;
      if (resources.has("contents") || resources.has("sidebar")) {
        invalidateContentsCache();
      }
      if (resources.has("contents")) {
        fetchContents().catch(() => setError("Could not refresh contents."));
      }
      if (resources.has("sidebar")) {
        invalidateSidebar();
        fetchSidebar().catch(() => setError("Could not refresh folders."));
      }
      if (resources.has("my_edits")) {
        fetchMyEdits().catch(() => setError("Could not refresh edits."));
      }
      if (resources.has("settings")) {
        fetchSettings().catch(() => setError("Could not refresh settings."));
      }
      if (resources.has("preferences") && onPreferencesRefreshRef.current) {
        onPreferencesRefreshRef.current().catch(() => setError("Could not refresh preferences."));
      }
      if (resources.has("document_detail") && selectedIdRef.current) {
        fetchDocumentDetail(selectedIdRef.current).catch(() =>
          setError("Could not refresh document.")
        );
      }
    }

    function queueRefresh(resources) {
      resources.forEach((resource) => pendingResources.add(resource));
      if (!refreshTimer) {
        refreshTimer = window.setTimeout(flushPendingRefreshes, 80);
      }
    }

    events.addEventListener("state", (evt) => {
      try {
        const payload = JSON.parse(evt.data || "{}");
        queueRefresh(payload.resources || []);
      } catch {
        queueRefresh(["contents", "sidebar", "document_detail", "my_edits"]);
      }
    });
    events.addEventListener("open", () => {
      if (connectionReported) {
        showNotice({
          detail: "Live updates are back online.",
          kind: "success",
          title: "Reconnected to server",
        });
      }
      connectionReported = false;
    });
    events.onerror = () => {
      if (connectionReported) {
        return;
      }
      connectionReported = true;
      showNotice({
        detail: "Lost connection to the server.",
        dismissible: false,
        duration: null,
        kind: "error",
        progress: "indeterminate",
        title: "Trying to reconnect",
      });
    };
    return () => {
      events.close();
      if (refreshTimer) {
        window.clearTimeout(refreshTimer);
      }
    };
  }, [
    fetchContents,
    fetchDocumentDetail,
    fetchMyEdits,
    fetchSettings,
    fetchSidebar,
    invalidateContentsCache,
    invalidateSidebar,
    setError,
    showNotice,
  ]);

  useEffect(
    () => () => {
      contentsCacheGenerationRef.current += 1;
      contentRequestRef.current += 1;
      abortForegroundContentsRequest();
      prefetchScheduler.clear();
    },
    [abortForegroundContentsRequest, prefetchScheduler]
  );

  return {
    docs,
    folderChildren,
    folderMetadata,
    contentsHasMore,
    contentsLoadingMore,
    contentsPending,
    contentsPendingEmptySearch,
    loadMoreContents,
    myEdits,
    recursiveSearch,
    refresh,
    searchQuery,
    selectedDoc,
    setRecursiveSearch,
    setSearchQuery,
    sidebarPagination: {
      hasMore: sidebarHasMore,
      loadMore: loadMoreSidebar,
      loadingMore: sidebarLoadingMore,
    },
    subfolders,
    updateDocumentInViews,
  };
}
