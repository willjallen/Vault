import { FinderShell } from "./components/FinderShell.js";
import { BulkDragPreview } from "./components/BulkDragPreview.js";
import { ConfirmToast } from "./components/ConfirmToast.js";
import { FolderPropertiesModal } from "./components/FolderPropertiesModal.js";
import { SettingsModal } from "./components/SettingsModal.js";
import { TransferDock } from "./components/TransferDock.js";
import { ContextMenu } from "./components/browser/ContextMenu.js";
import { MoveDialog } from "./components/browser/MoveDialog.js";
import {
  buildMyEditMenuItems,
  buildPageMenuItems,
  buildSelectionMenuItems,
} from "./lib/contextMenus.js";
import { createDropHandlers } from "./lib/dropHandlers.js";
import { createFileLockActions } from "./lib/fileLockActions.js";
import { createFolderActionHandlers } from "./lib/folderActions.js";
import {
  createBulkActionHandlers,
  docToItem,
  folderToItem,
  keyForItem,
} from "./lib/itemActions.js";
import { folderBaseName, folderParent, isArchivePath, toBreadcrumbs } from "./lib/utils.js";
import { useMouseNavigation } from "./lib/useMouseNavigation.js";
import { useMoveDialog } from "./lib/useMoveDialog.js";
import {
  applyThemePreference,
  readStoredThemePreference,
  storeThemePreference,
} from "./lib/theme.js";
import { useTransfers } from "./lib/useTransfers.js";
import { useVaultResources } from "./lib/useVaultResources.js";

const { useEffect, useMemo, useState, useCallback, useRef } = React;
const h = React.createElement;

export function App({ initial }) {
  const initialBootstrap = initial.bootstrap || initial;
  const [folder, setFolder] = useState(initialBootstrap.current_folder || "");
  const [folderBackStack, setFolderBackStack] = useState([]);
  const [folderForwardStack, setFolderForwardStack] = useState([]);
  const [selectedId, setSelectedId] = useState(null);
  const [contentsSelection, setContentsSelection] = useState([]);
  const [contentsAnchor, setContentsAnchor] = useState(null);
  const [folderSelection, setFolderSelection] = useState([]);
  const [folderAnchor, setFolderAnchor] = useState(null);
  const [uploading, setUploading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [draggingId, setDraggingId] = useState(null);
  const [dragBundle, setDragBundle] = useState(null);
  const [dropHint, setDropHint] = useState(null);
  const [uploadHover, setUploadHover] = useState(false);
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [inlineFolderDraft, setInlineFolderDraft] = useState(null);
  const [contextMenu, setContextMenu] = useState(null);
  const [draggingFolderPath, setDraggingFolderPath] = useState(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [themePreference, setThemePreference] = useState(readStoredThemePreference);
  const [folderPropertiesTarget, setFolderPropertiesTarget] = useState(null);
  const [toast, setToast] = useState("");
  const [confirmRequest, setConfirmRequest] = useState(null);
  const uploadInput = useRef(null);
  const versionUploadInput = useRef(null);
  const versionUploadDoc = useRef(null);
  const versionUploadOptions = useRef({});
  const settingsButtonRef = useRef(null);
  const confirmResolver = useRef(null);
  const closeContextMenu = useCallback(() => setContextMenu(null), []);

  useEffect(() => {
    applyThemePreference(themePreference);
    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!media) {
      return undefined;
    }
    function handleSystemThemeChange() {
      if (themePreference === "system") {
        applyThemePreference("system");
      }
    }
    media.addEventListener("change", handleSystemThemeChange);
    return () => media.removeEventListener("change", handleSystemThemeChange);
  }, [themePreference]);

  const handleThemePreferenceChange = useCallback((preference) => {
    storeThemePreference(preference);
    setThemePreference(preference);
  }, []);

  const resolveConfirm = useCallback((confirmed) => {
    const resolver = confirmResolver.current;
    confirmResolver.current = null;
    setConfirmRequest(null);
    if (resolver) {
      resolver(confirmed);
    }
  }, []);

  const requestConfirm = useCallback((request) => {
    if (confirmResolver.current) {
      confirmResolver.current(false);
    }
    return new Promise((resolve) => {
      confirmResolver.current = resolve;
      setConfirmRequest(request);
    });
  }, []);

  const openSettings = useCallback(() => {
    setSettingsOpen(true);
    closeContextMenu();
  }, [closeContextMenu]);

  const closeSettings = useCallback(() => {
    setSettingsOpen(false);
    window.setTimeout(() => settingsButtonRef.current?.focus(), 0);
  }, []);

  const openFolderProperties = useCallback(
    (folderItem) => {
      setFolderPropertiesTarget(folderItem);
      closeContextMenu();
    },
    [closeContextMenu]
  );

  const closeFolderProperties = useCallback(() => {
    setFolderPropertiesTarget(null);
  }, []);

  const baseDomain =
    initialBootstrap.base_domain ||
    (window.location.hostname.includes(".")
      ? window.location.hostname.split(".").slice(1).join(".")
      : "");
  const authMode = initialBootstrap.auth_mode || "headers";
  const logoutUrl = useMemo(() => {
    const rd = encodeURIComponent(window.location.href);
    if (authMode === "headers" && baseDomain) {
      return `https://auth.${baseDomain}/logout?rd=${rd}`;
    }
    return `/logout?rd=${rd}`;
  }, [authMode, baseDomain]);
  const redirectingRef = useRef(false);

  const redirectToLogin = useCallback(() => {
    if (redirectingRef.current) {
      return;
    }
    redirectingRef.current = true;
    setToast("Session expired. Redirecting to login…");
    const rd = encodeURIComponent(window.location.href);
    const loginUrl =
      authMode === "headers" && baseDomain
        ? `https://auth.${baseDomain}/?rd=${rd}`
        : `/login?rd=${rd}`;
    window.location.href = loginUrl;
  }, [authMode, baseDomain]);
  const { downloadWithProgress, transfers, uploadWithProgress } = useTransfers({
    onUnauthorized: redirectToLogin,
  });

  const apiFetch = useCallback(
    async (url, options = {}) => {
      try {
        const res = await fetch(url, { credentials: "include", ...options });
        const redirectedToAuth =
          res.redirected && res.url && res.url.includes("auth.") && res.url.includes("://auth.");
        if (res.type === "opaqueredirect" || res.status === 401 || redirectedToAuth) {
          redirectToLogin();
          throw new Error("Redirecting to login");
        }
        return res;
      } catch (err) {
        // Network-level failures (CORS when session expired, etc.) should trigger login redirect.
        redirectToLogin();
        throw err;
      }
    },
    [redirectToLogin]
  );

  const currentUser = initialBootstrap.user || {};
  const isAdmin = Boolean(currentUser.is_admin);

  const breadcrumbs = useMemo(() => toBreadcrumbs(folder || ""), [folder]);
  const canGoBack = folderBackStack.length > 0;
  const canGoForward = folderForwardStack.length > 0;
  const canGoUp = Boolean(folder);

  function navigateToFolder(nextFolder) {
    const normalized = nextFolder || "";
    if (normalized === folder) {
      return;
    }
    setFolderBackStack((prev) => [...prev, folder || ""]);
    setFolderForwardStack([]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(normalized);
    closeContextMenu();
  }

  function replaceFolder(nextFolder) {
    const normalized = nextFolder || "";
    if (normalized === folder) {
      return;
    }
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(normalized);
    closeContextMenu();
  }

  function navigateBack() {
    if (!folderBackStack.length) {
      return;
    }
    const nextFolder = folderBackStack[folderBackStack.length - 1] || "";
    setFolderBackStack(folderBackStack.slice(0, -1));
    setFolderForwardStack([folder || "", ...folderForwardStack]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(nextFolder);
    closeContextMenu();
  }

  function navigateForward() {
    if (!folderForwardStack.length) {
      return;
    }
    const nextFolder = folderForwardStack[0] || "";
    setFolderForwardStack(folderForwardStack.slice(1));
    setFolderBackStack([...folderBackStack, folder || ""]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(nextFolder);
    closeContextMenu();
  }

  function navigateUp() {
    if (!folder) {
      return;
    }
    navigateToFolder(folderParent(folder));
  }

  useMouseNavigation({ onBack: navigateBack, onForward: navigateForward });

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    if (folder) {
      params.set("folder", folder);
    } else {
      params.delete("folder");
    }
    const newUrl = `${window.location.pathname}${params.toString() ? "?" + params.toString() : ""}`;
    window.history.replaceState({}, "", newUrl);
  }, [folder]);

  useEffect(() => {
    closeContextMenu();
  }, [folder, closeContextMenu]);

  const {
    docs,
    folderChildren,
    folderMetadata,
    myEdits,
    recursiveSearch,
    refresh,
    searchQuery,
    selectedDoc,
    setRecursiveSearch,
    setSearchQuery,
    subfolders,
    updateDocumentInViews,
  } = useVaultResources({
    initial,
    apiFetch,
    folder,
    selectedId,
    setError,
    setSelectedId,
  });

  const contentsItems = useMemo(
    () => [...subfolders.map(folderToItem), ...docs.map(docToItem)],
    [docs, subfolders]
  );
  const contentsByKey = useMemo(
    () => new Map(contentsItems.map((item) => [keyForItem(item), item])),
    [contentsItems]
  );
  const folderPaneItems = useMemo(() => {
    const childrenFor = (parentPath, predicate) =>
      // eslint-disable-next-line security/detect-object-injection
      (folderChildren[parentPath] || [])
        .filter(predicate)
        .map((path) => {
          // eslint-disable-next-line security/detect-object-injection
          const metadata = folderMetadata[path] || {};
          return folderToItem({
            color: metadata.color,
            icon: metadata.icon,
            name: folderBaseName(path, path === "Archive" ? "Archive" : "Folder"),
            path,
          });
        })
        .sort((a, b) => a.name.localeCompare(b.name));
    const vaultMetadata = folderMetadata[""] || {};
    const archiveMetadata = folderMetadata.Archive || {};
    return [
      folderToItem({
        color: vaultMetadata.color,
        icon: vaultMetadata.icon || "house",
        name: "Vault",
        path: "",
      }),
      ...childrenFor("", (path) => !isArchivePath(path)),
      folderToItem({
        color: archiveMetadata.color,
        icon: archiveMetadata.icon || "box-archive",
        name: "Archive",
        path: "Archive",
      }),
      ...childrenFor("Archive", (path) => isArchivePath(path)),
    ];
  }, [folderChildren, folderMetadata]);
  const folderByKey = useMemo(
    () => new Map(folderPaneItems.map((item) => [keyForItem(item), item])),
    [folderPaneItems]
  );
  const selectedContentsItems = useMemo(
    () => contentsSelection.map((key) => contentsByKey.get(key)).filter(Boolean),
    [contentsByKey, contentsSelection]
  );
  const selectedFolderItems = useMemo(
    () => folderSelection.map((key) => folderByKey.get(key)).filter(Boolean),
    [folderByKey, folderSelection]
  );

  const applyPaneSelection = useCallback(
    (pane, item, pointerEvent, orderedItems) => {
      const key = keyForItem(item);
      const setSelection = pane === "contents" ? setContentsSelection : setFolderSelection;
      const anchor = pane === "contents" ? contentsAnchor : folderAnchor;
      const setAnchor = pane === "contents" ? setContentsAnchor : setFolderAnchor;
      const orderedKeys = orderedItems.map(keyForItem);
      const isToggle = pointerEvent?.ctrlKey || pointerEvent?.metaKey;
      const isRange = pointerEvent?.shiftKey && anchor && orderedKeys.includes(anchor);
      setSelection((current) => {
        if (isRange) {
          const start = orderedKeys.indexOf(anchor);
          const end = orderedKeys.indexOf(key);
          const [from, to] = start < end ? [start, end] : [end, start];
          return orderedKeys.slice(from, to + 1);
        }
        if (isToggle) {
          return current.includes(key)
            ? current.filter((itemKey) => itemKey !== key)
            : [...current, key];
        }
        return [key];
      });
      if (!isRange) {
        setAnchor(key);
      }
    },
    [contentsAnchor, folderAnchor]
  );

  function clearAllSelections() {
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolderSelection([]);
    setFolderAnchor(null);
  }

  function isSelectionClick(pointerEvent) {
    return Boolean(pointerEvent?.ctrlKey || pointerEvent?.metaKey || pointerEvent?.shiftKey);
  }

  useEffect(() => {
    const docItems = selectedContentsItems.filter((item) => item.type === "document");
    setSelectedId(
      selectedContentsItems.length === 1 && docItems.length === 1 ? docItems[0].id : null
    );
  }, [selectedContentsItems]);

  function handleSelectContentItem(rawItem, type, pointerEvent, orderedItems) {
    const item = type === "document" ? docToItem(rawItem) : folderToItem(rawItem);
    if (!item) {
      return;
    }
    const key = keyForItem(item);
    if (isSelectionClick(pointerEvent)) {
      setFolderSelection([]);
      setFolderAnchor(null);
      applyPaneSelection("contents", item, pointerEvent, orderedItems);
      return;
    }
    if (contentsSelection.includes(key)) {
      clearAllSelections();
      return;
    }
    if (item.type === "folder") {
      clearAllSelections();
      navigateToFolder(item.path || "");
      return;
    }
    setFolderSelection([]);
    setFolderAnchor(null);
    applyPaneSelection("contents", item, pointerEvent, orderedItems);
  }

  function handleSelectFolderItem(item, pointerEvent, orderedItems) {
    const key = keyForItem(item);
    if (isSelectionClick(pointerEvent)) {
      setContentsSelection([]);
      setContentsAnchor(null);
      applyPaneSelection("folders", item, pointerEvent, orderedItems);
      return;
    }
    if (folderSelection.includes(key)) {
      clearAllSelections();
      return;
    }
    clearAllSelections();
    navigateToFolder(item.path || "");
  }

  const { handleLock, handleRelease, handleSave, handleStartEdit, handleVersionUpload } =
    createFileLockActions({
      apiFetch,
      currentUser,
      refresh,
      setBusy,
      setError,
      updateDocument: updateDocumentInViews,
      uploadWithProgress,
      downloadWithProgress,
    });

  function handleVersionUploadClick(doc, options = {}) {
    versionUploadDoc.current = doc;
    versionUploadOptions.current = options;
    if (versionUploadInput.current) {
      versionUploadInput.current.click();
    }
  }

  async function handleVersionUploadInput(file) {
    const doc = versionUploadDoc.current;
    const options = versionUploadOptions.current;
    versionUploadDoc.current = null;
    versionUploadOptions.current = {};
    if (!doc || !file) {
      return;
    }
    await handleVersionUpload(doc, file, options);
    if (versionUploadInput.current) {
      versionUploadInput.current.value = "";
    }
  }

  async function handleUpload(file, targetFolder = folder) {
    if (!file) {
      return;
    }
    setUploading(true);
    setError("");
    const form = new FormData();
    form.append("file", file);
    form.append("folder", targetFolder || "");
    try {
      await uploadWithProgress({
        formData: form,
        name: file.name,
        size: file.size,
        url: "/documents",
      });
      await refresh(targetFolder || "", { invalidateContents: true });
    } catch (err) {
      setError(err.message || "Upload failed. Please try again.");
    } finally {
      setUploading(false);
      setUploadHover(false);
      if (uploadInput.current) {
        uploadInput.current.value = "";
      }
    }
  }

  const {
    handleArchive,
    handleArchiveItems,
    handleDeleteForeverItems,
    handleDownloadSelection,
    handleLockItems,
    handleMove,
    handleMoveSelection,
    handlePermanentDelete,
    handleRestoreItems,
    handleUnarchive,
    handleUnlockItems,
    handleView,
    postAction,
    refreshAfterAction,
  } = createBulkActionHandlers({
    apiFetch,
    clearAllSelections: () => {
      setContentsSelection([]);
      setContentsAnchor(null);
      setFolderSelection([]);
      setFolderAnchor(null);
    },
    docs,
    downloadWithProgress,
    refresh,
    requestConfirm,
    selectedDoc,
    setBusy,
    setDraggingFolderPath,
    setDraggingId,
    setDropHint,
    setError,
  });

  function handleVersionDownload(item) {
    if (!item.download_url) {
      return;
    }
    downloadWithProgress({
      name: item.original_filename || selectedDoc?.name || "download",
      size: item.size_bytes,
      url: item.download_url,
    }).catch((err) => {
      setError(err.message || "Download failed.");
    });
  }

  const {
    beginCreateFolder,
    beginRenameFolder,
    handleArchiveFolder,
    handleCancelInlineFolder,
    handleCommitInlineFolder,
    handleInlineFolderNameChange,
    handlePermanentDeleteFolder,
    handleRenameFile,
    handleRenameFolder,
    handleUnarchiveFolder,
  } = createFolderActionHandlers({
    apiFetch,
    folder,
    handleArchiveItems,
    handleDeleteForeverItems,
    handleRestoreItems,
    inlineFolderDraft,
    postAction,
    refresh,
    refreshAfterAction,
    replaceFolder,
    setBusy,
    setCreatingFolder,
    setError,
    setInlineFolderDraft,
    setSelectedId,
  });

  const {
    handleDropOnFolder,
    handleCanvasDrop,
    handleCanvasDragOver,
    handleCanvasDragLeave,
    handleFileDragStart: startFileDrag,
    handleFileDragEnd: endFileDrag,
    handleFolderDragStart: startFolderDrag,
    handleFolderDragEnd: endFolderDrag,
    clearDropState,
  } = createDropHandlers({
    folder,
    docs,
    draggingId,
    draggingFolderPath,
    setDropHint,
    setUploadHover,
    setError,
    handleArchiveItems,
    handleArchiveFolder,
    handleMoveSelection,
    handleRenameFolder,
    handleUpload,
    handleMove,
    handleArchive,
    setDraggingId,
    setDraggingFolderPath,
  });

  function beginDragPreview(evt, items) {
    if (!items || items.length <= 1) {
      setDragBundle(null);
      return;
    }
    const transparent = document.createElement("canvas");
    transparent.width = 1;
    transparent.height = 1;
    evt.dataTransfer.setDragImage(transparent, 0, 0);
    setDragBundle({ items, phase: "entering", x: evt.clientX, y: evt.clientY });
    window.requestAnimationFrame(() =>
      setDragBundle((current) => (current ? { ...current, phase: "visible" } : current))
    );
  }

  function handleFileDragStart(evt, docId, items = []) {
    beginDragPreview(evt, items);
    startFileDrag(evt, docId, items);
  }

  function handleFolderDragStart(evt, path, items = []) {
    beginDragPreview(evt, items);
    startFolderDrag(evt, path, items);
  }

  function handleFileDragEnd() {
    endFileDrag();
    setDragBundle(null);
  }

  function handleFolderDragEnd() {
    endFolderDrag();
    setDragBundle(null);
  }

  useEffect(() => {
    if (!dragBundle) {
      return undefined;
    }
    function updateDragPosition(evt) {
      setDragBundle((current) =>
        current ? { ...current, x: evt.clientX, y: evt.clientY } : current
      );
    }
    window.addEventListener("dragover", updateDragPosition);
    return () => window.removeEventListener("dragover", updateDragPosition);
  }, [dragBundle]);

  function handleUploadClick() {
    if (uploadInput.current) {
      uploadInput.current.click();
    }
  }

  const {
    moveTarget,
    moveDestination,
    moveNewFolderName,
    creatingMoveFolder,
    openMoveDialogForDoc,
    openMoveDialogForFolder,
    openMoveDialogForSelection,
    closeMoveDialog,
    setMoveDestinationSafe,
    handleCreateMoveFolder,
    handleConfirmMoveTarget,
    setMoveNewFolderName,
  } = useMoveDialog({
    folder,
    handleMove,
    handleMoveSelection,
    handleRenameFolder,
    apiFetch,
    refresh,
    setError,
    setSelectedId,
  });

  function contextActions(extra = {}) {
    return {
      beginCreateFolder,
      beginRenameFolder,
      busy,
      creatingFolder,
      currentUser,
      folder,
      handleArchive,
      handleArchiveItems,
      handleArchiveFolder,
      handleDeleteForeverItems,
      handleDownloadSelection,
      handleLockItems,
      handleLock,
      handlePermanentDelete,
      handlePermanentDeleteFolder,
      handleRelease,
      handleRestoreItems,
      handleRenameFile,
      handleRenameFolder,
      handleStartEdit,
      handleUnarchive,
      handleUnarchiveFolder,
      handleUnlockItems,
      handleUploadClick,
      openFolderProperties,
      handleView,
      handleVersionUploadClick,
      isAdmin,
      openMoveDialogForDoc,
      openMoveDialogForFolder,
      openMoveDialogForSelection,
      selectedDoc,
      navigateToFolder,
      uploading,
      ...extra,
    };
  }

  function handleFileContextMenu(evt, doc) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!doc) {
      return;
    }
    const item = docToItem(doc);
    const key = keyForItem(item);
    const selectedItems = contentsSelection.includes(key) ? selectedContentsItems : [item];
    if (!contentsSelection.includes(key)) {
      setContentsSelection([key]);
      setContentsAnchor(key);
    }
    const items = buildSelectionMenuItems(contextActions({ selectedItems }));
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handleMyEditContextMenu(evt, doc) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!doc) {
      return;
    }
    setSelectedId(doc.id);
    const items = buildMyEditMenuItems(contextActions({ doc }));
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handleFolderContextMenu(evt, folderItem) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!folderItem) {
      return;
    }
    const item = folderToItem(folderItem);
    const key = keyForItem(item);
    const useFolderPane = folderItem.sourcePane === "folders" || !contentsByKey.has(key);
    const paneSelection = useFolderPane ? folderSelection : contentsSelection;
    const paneItems = useFolderPane ? selectedFolderItems : selectedContentsItems;
    const selectedItems = paneSelection.includes(key) ? paneItems : [item];
    if (!paneSelection.includes(key)) {
      if (useFolderPane) {
        setFolderSelection([key]);
        setFolderAnchor(key);
      } else {
        setContentsSelection([key]);
        setContentsAnchor(key);
      }
    }
    const items = buildSelectionMenuItems(contextActions({ selectedItems }));
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handlePageContextMenu(evt) {
    evt.preventDefault();
    const items = buildPageMenuItems(contextActions());
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  const infoSelectionItems = selectedContentsItems.length
    ? selectedContentsItems
    : selectedFolderItems;

  return h(
    React.Fragment,
    null,
    toast
      ? h(
          "div",
          {
            className: "toast",
            style: {
              position: "fixed",
              top: 12,
              right: 12,
              background: "rgba(0,0,0,0.85)",
              color: "#fff",
              padding: "10px 14px",
              borderRadius: "6px",
              zIndex: 9999,
              boxShadow: "0 2px 8px rgba(0,0,0,0.3)",
            },
          },
          toast
        )
      : null,
    h(FinderShell, {
      folder,
      breadcrumbs,
      myEdits,
      folderChildren,
      subfolders,
      files: docs,
      selectedId,
      contentsItems,
      contentsSelection,
      folderItems: folderPaneItems,
      folderSelection,
      selectedDoc,
      searchQuery,
      recursiveSearch,
      dropHint,
      uploadHover,
      draggingId,
      draggingFolderPath,
      currentUser,
      isAdmin,
      canGoBack,
      canGoForward,
      canGoUp,
      inlineFolderDraft,
      onInlineFolderNameChange: handleInlineFolderNameChange,
      onCommitInlineFolder: handleCommitInlineFolder,
      onCancelInlineFolder: handleCancelInlineFolder,
      onNavigateBack: navigateBack,
      onNavigateForward: navigateForward,
      onNavigateUp: navigateUp,
      onSelectFolder: navigateToFolder,
      onSelectDoc: setSelectedId,
      onSelectContentItem: handleSelectContentItem,
      onSelectFolderItem: handleSelectFolderItem,
      onSearchQueryChange: setSearchQuery,
      onRecursiveSearchChange: setRecursiveSearch,
      onClearSelection: () => {
        setContentsSelection([]);
        setContentsAnchor(null);
        setFolderSelection([]);
        setFolderAnchor(null);
      },
      onOpenDoc: handleView,
      onDropOnFolder: handleDropOnFolder,
      onClearDropHint: clearDropState,
      onCanvasDrop: handleCanvasDrop,
      onCanvasDragOver: handleCanvasDragOver,
      onCanvasDragLeave: handleCanvasDragLeave,
      onFileDragStart: handleFileDragStart,
      onFileDragEnd: handleFileDragEnd,
      onFolderDragStart: handleFolderDragStart,
      onFolderDragEnd: handleFolderDragEnd,
      onFileContextMenu: handleFileContextMenu,
      onFolderContextMenu: handleFolderContextMenu,
      onMyEditContextMenu: handleMyEditContextMenu,
      onPageContextMenu: handlePageContextMenu,
      onTriggerUpload: handleUploadClick,
      logoutUrl,
      onOpenSettings: openSettings,
      settingsButtonRef,
      onDownload: handleView,
      selectionItems: infoSelectionItems,
      onDownloadSelection: handleDownloadSelection,
      onDownloadVersion: handleVersionDownload,
      onLock: handleLock,
      onLockSelection: handleLockItems,
      onRename: handleRenameFile,
      onMove: openMoveDialogForDoc,
      onMoveSelection: openMoveDialogForSelection,
      onStartEdit: handleStartEdit,
      onRelease: handleRelease,
      onReleaseSelection: handleUnlockItems,
      onSave: handleSave,
      onArchive: handleArchive,
      onArchiveSelection: handleArchiveItems,
      onUnarchive: handleUnarchive,
      onRestoreSelection: handleRestoreItems,
      onPermanentDelete: handlePermanentDelete,
      onDeleteSelection: handleDeleteForeverItems,
      onOpenFolder: navigateToFolder,
      busy,
    }),
    moveTarget
      ? h(MoveDialog, {
          target: moveTarget,
          destination: moveDestination,
          folderChildren,
          newFolderName: moveNewFolderName,
          creatingFolder: creatingMoveFolder,
          onDestinationChange: setMoveDestinationSafe,
          onClose: closeMoveDialog,
          onConfirm: handleConfirmMoveTarget,
          onCreateFolder: handleCreateMoveFolder,
          onNewFolderNameChange: setMoveNewFolderName,
        })
      : null,
    h("input", {
      type: "file",
      ref: uploadInput,
      className: "hidden-input",
      onChange: (e) => handleUpload(e.target.files[0]),
    }),
    h("input", {
      type: "file",
      ref: versionUploadInput,
      className: "hidden-input",
      onChange: (e) => handleVersionUploadInput(e.target.files[0]),
    }),
    h(TransferDock, { transfers }),
    h(BulkDragPreview, { drag: dragBundle }),
    settingsOpen
      ? h(SettingsModal, {
          apiFetch,
          currentUser,
          onClose: closeSettings,
          onThemePreferenceChange: handleThemePreferenceChange,
          themePreference,
        })
      : null,
    folderPropertiesTarget
      ? h(FolderPropertiesModal, {
          apiFetch,
          folder: folderPropertiesTarget,
          onClose: closeFolderProperties,
          onUpdated: () => refresh(folder, { invalidateContents: true, sidebar: true }),
        })
      : null,
    h(ConfirmToast, { request: confirmRequest, onResolve: resolveConfirm }),
    contextMenu ? h(ContextMenu, { menu: contextMenu, onClose: closeContextMenu }) : null,
    error ? h("div", { className: "toast error" }, error) : null,
    busy ? h("div", { className: "toast subtle" }, "Working...") : null
  );
}
