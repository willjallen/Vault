const { useCallback, useEffect, useMemo, useRef, useState } = React;

function contentsKey(folder, q, recursive) {
  return JSON.stringify([folder || "", q || "", Boolean(recursive)]);
}

function childrenFromContents(contents) {
  return (contents.folders || []).map((item) => item.path);
}

function metadataFromContents(contents) {
  return Object.fromEntries(
    (contents.folders || []).map((item) => [
      item.path || "",
      {
        color: item.color || "",
        icon: item.icon || "",
      },
    ])
  );
}

function emptyContents(folder, q, recursive) {
  return {
    folder: folder || "",
    q: q || "",
    recursive: Boolean(recursive),
    folders: [],
    documents: [],
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

export function useVaultResources({
  initial,
  apiFetch,
  folder,
  selectedId,
  setSelectedId,
  setError,
}) {
  const initialContents = initial.contents || { folders: [], documents: [] };
  const initialSidebar = initial.sidebar || { folder_children: {} };
  const initialMyEdits = initial.my_edits || { documents: [] };
  const initialContentsKey = contentsKey(
    initialContents.folder || "",
    initialContents.q || "",
    initialContents.recursive
  );
  const initialContentsChildren = initialContents.folder
    ? { [initialContents.folder]: childrenFromContents(initialContents) }
    : { "": childrenFromContents(initialContents) };
  const initialContentsMetadata = metadataFromContents(initialContents);
  const [contents, setContents] = useState(initialContents);
  const [sidebar, setSidebar] = useState(initialSidebar);
  const [contentsChildren, setContentsChildren] = useState(initialContentsChildren);
  const [contentsMetadata, setContentsMetadata] = useState(initialContentsMetadata);
  const [myEditsState, setMyEditsState] = useState(initialMyEdits);
  const [selectedDocDetail, setSelectedDocDetail] = useState(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [recursiveSearch, setRecursiveSearch] = useState(false);
  const contentsCacheRef = useRef(new Map([[initialContentsKey, initialContents]]));
  const prefetchingKeysRef = useRef(new Set());
  const contentRequestRef = useRef(0);
  const detailRequestRef = useRef(0);
  const folderRef = useRef(folder || "");
  const searchQueryRef = useRef(searchQuery);
  const recursiveSearchRef = useRef(recursiveSearch);
  const selectedIdRef = useRef(selectedId);

  folderRef.current = folder || "";
  searchQueryRef.current = searchQuery;
  recursiveSearchRef.current = recursiveSearch;
  selectedIdRef.current = selectedId;

  const activeContentsKey = contentsKey(folder, searchQuery, recursiveSearch);
  const storedContentsKey = contentsKey(
    contents.folder || "",
    contents.q || "",
    contents.recursive
  );
  const activeContentsCached = contentsCacheRef.current.has(activeContentsKey);
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
    const cached = contentsCacheRef.current.get(activeContentsKey);
    if (cached) {
      return cached;
    }
    if (contentsPending) {
      return contents;
    }
    return emptyContents(folder, searchQuery, recursiveSearch);
  }, [
    activeContentsKey,
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
  const folderChildren = useMemo(() => {
    return { ...sidebarChildren, ...contentsChildren };
  }, [contentsChildren, sidebarChildren]);
  const folderMetadata = useMemo(() => {
    return { ...sidebarMetadata, ...contentsMetadata };
  }, [contentsMetadata, sidebarMetadata]);
  const selectedDoc = selectedDocDetail || docs.find((doc) => doc.id === selectedId) || null;
  const myEdits = myEditsState.documents || [];

  const invalidateContentsCache = useCallback(() => {
    contentsCacheRef.current.clear();
    prefetchingKeysRef.current.clear();
    setContentsChildren({});
  }, []);

  const rememberContents = useCallback((data) => {
    const key = contentsKey(data.folder || "", data.q || "", data.recursive);
    contentsCacheRef.current.set(key, data);
    if (!data.q && !data.recursive) {
      setContentsChildren((prev) => ({
        ...prev,
        [data.folder || ""]: childrenFromContents(data),
      }));
      setContentsMetadata((prev) => ({
        ...prev,
        ...metadataFromContents(data),
      }));
    }
  }, []);

  const fetchContents = useCallback(
    async (nextFolder, options = {}) => {
      const background = Boolean(options.background);
      const requestId = background ? null : contentRequestRef.current + 1;
      if (!background) {
        contentRequestRef.current = requestId;
      }
      const targetFolder = nextFolder ?? folderRef.current;
      const q = options.q ?? searchQueryRef.current;
      const recursive = options.recursive ?? recursiveSearchRef.current;
      const key = contentsKey(targetFolder, q, recursive);
      const params = new URLSearchParams({
        folder: targetFolder || "",
        q: q || "",
        recursive: recursive ? "1" : "0",
      });
      const res = await apiFetch(`/api/folders/contents?${params.toString()}`);
      if (!background && requestId !== contentRequestRef.current) {
        return null;
      }
      if (!res.ok) {
        throw new Error("Could not refresh contents");
      }
      const data = await res.json();
      rememberContents(data);
      if (background) {
        return null;
      }
      if (requestId !== contentRequestRef.current) {
        return null;
      }
      if (
        key === contentsKey(folderRef.current, searchQueryRef.current, recursiveSearchRef.current)
      ) {
        setContents(data);
      }
      return data;
    },
    [apiFetch, rememberContents]
  );

  const prefetchContents = useCallback(
    (targetFolder) => {
      const key = contentsKey(targetFolder, "", false);
      if (contentsCacheRef.current.has(key) || prefetchingKeysRef.current.has(key)) {
        return;
      }
      prefetchingKeysRef.current.add(key);
      fetchContents(targetFolder, { q: "", recursive: false, background: true })
        .catch(() => {})
        .finally(() => prefetchingKeysRef.current.delete(key));
    },
    [fetchContents]
  );

  const fetchSidebar = useCallback(async () => {
    const res = await apiFetch("/api/folders/sidebar");
    if (!res.ok) {
      throw new Error("Could not refresh folders");
    }
    const data = await res.json();
    setSidebar(data);
    return data;
  }, [apiFetch]);

  const fetchMyEdits = useCallback(async () => {
    const res = await apiFetch("/api/my-edits");
    if (!res.ok) {
      throw new Error("Could not refresh edits");
    }
    const data = await res.json();
    setMyEditsState(data);
    return data;
  }, [apiFetch]);

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
      try {
        const requests = [
          fetchContents(nextFolder),
          options.sidebar ? fetchSidebar() : Promise.resolve(null),
          fetchMyEdits(),
          selectedIdRef.current
            ? fetchDocumentDetail(selectedIdRef.current)
            : Promise.resolve(null),
        ];
        await Promise.all(requests);
      } catch {
        setError("Could not refresh data.");
      }
    },
    [
      fetchContents,
      fetchDocumentDetail,
      fetchMyEdits,
      fetchSidebar,
      invalidateContentsCache,
      setError,
    ]
  );

  const updateDocumentInViews = useCallback((docId, updater) => {
    const updateList = (items = []) =>
      items.map((item) => (item.id === docId ? updater(item) : item));
    contentsCacheRef.current.forEach((cached, key) => {
      if ((cached.documents || []).some((item) => item.id === docId)) {
        contentsCacheRef.current.set(key, {
          ...cached,
          documents: updateList(cached.documents),
        });
      }
    });
    setContents((prev) => ({ ...prev, documents: updateList(prev.documents) }));
    setMyEditsState((prev) => ({ ...prev, documents: updateList(prev.documents) }));
    setSelectedDocDetail((prev) => (prev && prev.id === docId ? updater(prev) : prev));
  }, []);

  useEffect(() => {
    const key = contentsKey(folder, searchQuery, recursiveSearch);
    const cached = contentsCacheRef.current.get(key);
    if (cached) {
      setContents(cached);
      return undefined;
    }
    if (!searchQuery && !recursiveSearch) {
      setContents(emptyContents(folder, searchQuery, recursiveSearch));
    }
    const timer = setTimeout(() => {
      fetchContents(folder).catch(() => setError("Could not refresh contents."));
    }, 0);
    return () => clearTimeout(timer);
  }, [fetchContents, folder, recursiveSearch, searchQuery, setError]);

  useEffect(() => {
    if (displayedContents.q || displayedContents.recursive) {
      return;
    }
    (displayedContents.folders || []).forEach((item) => prefetchContents(item.path));
  }, [displayedContents, prefetchContents]);

  useEffect(() => {
    prefetchContents("");
    prefetchContents("Archive");
    Object.values(sidebarChildren)
      .flat()
      .forEach((path) => prefetchContents(path));
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
        fetchSidebar().catch(() => setError("Could not refresh folders."));
      }
      if (resources.has("my_edits")) {
        fetchMyEdits().catch(() => setError("Could not refresh edits."));
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
    events.onerror = () => {};
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
    fetchSidebar,
    invalidateContentsCache,
    setError,
  ]);

  return {
    docs,
    folderChildren,
    folderMetadata,
    contentsPending,
    contentsPendingEmptySearch,
    myEdits,
    recursiveSearch,
    refresh,
    searchQuery,
    selectedDoc,
    setRecursiveSearch,
    setSearchQuery,
    subfolders,
    updateDocumentInViews,
  };
}
