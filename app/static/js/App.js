/* eslint-disable max-lines */
import { FinderShell } from "./components/FinderShell.js";
import { DragPreview } from "./components/DragPreview.js";
import { ConfirmToast } from "./components/ConfirmToast.js";
import { FolderPropertiesModal } from "./components/FolderPropertiesModal.js";
import { NotificationDock } from "./components/NotificationDock.js";
import { SettingsModal } from "./components/SettingsModal.js";
import { TransferDock } from "./components/TransferDock.js";
import { ContextMenu } from "./components/browser/ContextMenu.js";
import { FileDetailsModal } from "./components/browser/FileDetailsModal.js";
import { MoveDialog } from "./components/browser/MoveDialog.js";
import {
  buildMyEditMenuItems,
  buildPageMenuItems,
  buildSelectionMenuItems,
} from "./lib/contextMenus.js";
import { createDropHandlers } from "./lib/dropHandlers.js";
import {
  compareContentsItems,
  DEFAULT_CONTENTS_SORT,
  nextContentsSort,
} from "./lib/contentSort.js";
import { createFileLockActions } from "./lib/fileLockActions.js";
import { createFolderActionHandlers } from "./lib/folderActions.js";
import {
  createBulkActionHandlers,
  docToItem,
  folderToItem,
  keyForItem,
} from "./lib/itemActions.js";
import { favoriteItemsToSidebarItems } from "./lib/favoriteItems.js";
import { folderBaseName, isArchiveRootPath } from "./lib/utils.js";
import {
  initialFolderForApp,
  shareCodeFromLocation,
  useFolderHistory,
  useShareActions,
  useShareLinkResolution,
} from "./lib/shareLinks.js";
import { useAuthFetch } from "./lib/useAuthFetch.js";
import { useFolderNavigation } from "./lib/useFolderNavigation.js";
import { useFavoritePreferenceActions } from "./lib/useFavoritePreferenceActions.js";
import { useMoveDialog } from "./lib/useMoveDialog.js";
import { useNotifications } from "./lib/useNotifications.js";
import { normalizeSiteSettings } from "./lib/siteSettings.js";
import { useAppearancePreferences } from "./lib/theme.js";
import { useVaultResources } from "./lib/useVaultResources.js";

const { useEffect, useMemo, useState, useCallback, useRef } = React;
const h = React.createElement;

export function App({ initial }) {
  const initialBootstrap = initial.bootstrap || initial;
  const initialShareCode = initial.share_code || shareCodeFromLocation();
  const [shareResolving, setShareResolving] = useState(Boolean(initialShareCode));
  const [folder, setFolder] = useState(() =>
    initialFolderForApp(initialBootstrap.current_folder || "", initialShareCode)
  );
  const [folderBackStack, setFolderBackStack] = useState([]);
  const [folderForwardStack, setFolderForwardStack] = useState([]);
  const [selectedId, setSelectedId] = useState(null);
  const [contentsSelection, setContentsSelection] = useState([]);
  const [contentsAnchor, setContentsAnchor] = useState(null);
  const [folderSelection, setFolderSelection] = useState([]);
  const [folderAnchor, setFolderAnchor] = useState(null);
  const [uploading, setUploading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [draggingId, setDraggingId] = useState(null);
  const [dragBundle, setDragBundle] = useState(null);
  const [dropHint, setDropHint] = useState(null);
  const [uploadHover, setUploadHover] = useState(false);
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [inlineFolderDraft, setInlineFolderDraft] = useState(null);
  const [contextMenu, setContextMenu] = useState(null);
  const [draggingFolderPath, setDraggingFolderPath] = useState(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [folderPropertiesTarget, setFolderPropertiesTarget] = useState(null);
  const [fileDetailsTarget, setFileDetailsTarget] = useState(null);
  const [contentsSort, setContentsSort] = useState(DEFAULT_CONTENTS_SORT);
  const [confirmRequest, setConfirmRequest] = useState(null);
  const [siteSettings, setSiteSettings] = useState(() =>
    normalizeSiteSettings(initialBootstrap.settings)
  );
  const uploadInput = useRef(null);
  const versionUploadInput = useRef(null);
  const versionUploadDoc = useRef(null);
  const versionUploadOptions = useRef({});
  const settingsButtonRef = useRef(null);
  const confirmResolver = useRef(null);
  const shareCodeRef = useRef(initialShareCode);
  const historyModeRef = useRef("replace");
  const closeContextMenu = useCallback(() => setContextMenu(null), []);
  const { dismissNotice, notice, showError: setError, showNotice } = useNotifications();

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

  const openFileDetails = useCallback(
    (doc) => {
      setFileDetailsTarget(doc);
      setSelectedId(doc.id);
      closeContextMenu();
    },
    [closeContextMenu]
  );

  const closeFileDetails = useCallback(() => {
    setFileDetailsTarget(null);
  }, []);

  const { apiFetch, downloadWithProgress, logoutUrl, transfers, uploadWithProgress } = useAuthFetch(
    {
      initialBootstrap,
      showNotice,
    }
  );
  const {
    alternateRows,
    doubleClickDownload,
    favoriteItems,
    handleAlternateRowsChange,
    handleDoubleClickDownloadChange,
    handleFavoriteItemsChange,
    handleOpenFoldersOnClickChange,
    handlePalettePreferenceChange,
    handleSidebarLayoutChange,
    handleSidebarSectionSizesChange,
    handleThemePreferenceChange,
    openFoldersOnClick,
    palettePreference,
    refreshUserPreferences,
    sidebarSectionCollapsed,
    sidebarSectionSizes,
    themePreference,
  } = useAppearancePreferences({
    apiFetch,
    initialPreferences: initialBootstrap.preferences,
  });

  const currentUser = initialBootstrap.user || {};
  const isAdmin = Boolean(currentUser.is_admin);
  const devMode = Boolean(initialBootstrap.dev_mode);

  const {
    breadcrumbs,
    canGoBack,
    canGoForward,
    canGoUp,
    navigateBack,
    navigateForward,
    navigateToFolder,
    navigateUp,
    replaceFolder,
  } = useFolderNavigation({
    closeContextMenu,
    folder,
    folderBackStack,
    folderForwardStack,
    historyModeRef,
    setContentsAnchor,
    setContentsSelection,
    setFolder,
    setFolderBackStack,
    setFolderForwardStack,
    setSelectedId,
  });

  useFolderHistory({
    closeContextMenu,
    folder,
    historyModeRef,
    setContentsAnchor,
    setContentsSelection,
    setFolder,
    setFolderAnchor,
    setFolderBackStack,
    setFolderForwardStack,
    setFolderSelection,
    setSelectedId,
    shareResolving,
  });

  useEffect(() => {
    closeContextMenu();
  }, [folder, closeContextMenu]);

  const {
    docs,
    folderChildren,
    folderMetadata,
    myEdits,
    contentsPending,
    contentsPendingEmptySearch,
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
    onMissingFolder: (_missingFolder, options = {}) => {
      if (!options.suppressError) {
        setError("Folder not found.");
      }
      replaceFolder(options.fallbackFolder || "");
    },
    onPreferencesRefresh: refreshUserPreferences,
    onSiteSettingsChange: setSiteSettings,
    selectedId,
    setError,
    setSelectedId,
    showNotice,
  });

  useShareLinkResolution({
    apiFetch,
    historyModeRef,
    setContentsAnchor,
    setContentsSelection,
    setFolder,
    setFolderAnchor,
    setFolderBackStack,
    setFolderForwardStack,
    setFolderSelection,
    setRecursiveSearch,
    setSearchQuery,
    setSelectedId,
    setShareResolving,
    shareCodeRef,
    showNotice,
  });

  const unsortedContentsItems = useMemo(
    () => [...subfolders.map(folderToItem), ...docs.map(docToItem)],
    [docs, subfolders]
  );
  const contentsItems = useMemo(
    () =>
      unsortedContentsItems
        .filter(Boolean)
        .slice()
        .sort((a, b) => compareContentsItems(a, b, contentsSort)),
    [contentsSort, unsortedContentsItems]
  );
  const contentsByKey = useMemo(
    () => new Map(contentsItems.map((item) => [keyForItem(item), item])),
    [contentsItems]
  );
  const activeFileDetailsDoc = useMemo(() => {
    if (!fileDetailsTarget) {
      return null;
    }
    if (selectedDoc?.id === fileDetailsTarget.id) {
      return selectedDoc;
    }
    return docs.find((doc) => doc.id === fileDetailsTarget.id) || fileDetailsTarget;
  }, [docs, fileDetailsTarget, selectedDoc]);
  const folderPaneItems = useMemo(() => {
    const childrenFor = (parentPath, predicate) =>
      // eslint-disable-next-line security/detect-object-injection
      (folderChildren[parentPath] || [])
        .filter(predicate)
        .map((path) => {
          // eslint-disable-next-line security/detect-object-injection
          const metadata = folderMetadata[path] || {};
          return folderToItem({
            ...metadata,
            name: folderBaseName(path, path === "Archive" ? "Archive" : "Folder"),
            path,
          });
        })
        .sort((a, b) => a.name.localeCompare(b.name));
    const vaultMetadata = folderMetadata[""] || {};
    const archiveMetadata = folderMetadata.Archive || {};
    return [
      folderToItem({
        ...vaultMetadata,
        icon: vaultMetadata.icon || "house",
        name: initialBootstrap.site_name || "Vault",
        path: "",
      }),
      ...childrenFor("", (path) => !isArchiveRootPath(path)),
      folderToItem({
        ...archiveMetadata,
        icon: archiveMetadata.icon || "box-archive",
        name: "Archive",
        path: "Archive",
      }),
    ];
  }, [folderChildren, folderMetadata, initialBootstrap.site_name]);
  const favoriteSidebarItems = useMemo(
    () =>
      favoriteItemsToSidebarItems(favoriteItems, {
        contentsItems,
        folderMetadata,
        folderPaneItems,
      }),
    [contentsItems, favoriteItems, folderMetadata, folderPaneItems]
  );
  const folderByKey = useMemo(
    () =>
      new Map(
        [...folderPaneItems, ...favoriteSidebarItems.filter((item) => item.type === "folder")].map(
          (item) => [keyForItem(item), item]
        )
      ),
    [favoriteSidebarItems, folderPaneItems]
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

  function sameKeys(left, right) {
    return left.length === right.length && left.join("\u0000") === right.join("\u0000");
  }

  const { handleAddFavoriteItems, handleRemoveFavoriteItem } = useFavoritePreferenceActions({
    closeContextMenu,
    favoriteItems,
    onFavoriteItemsChange: handleFavoriteItemsChange,
  });

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
      if (openFoldersOnClick) {
        clearAllSelections();
        navigateToFolder(item.path || "");
        return;
      }
      setFolderSelection([]);
      setFolderAnchor(null);
      applyPaneSelection("contents", item, pointerEvent, orderedItems);
      return;
    }
    setFolderSelection([]);
    setFolderAnchor(null);
    applyPaneSelection("contents", item, pointerEvent, orderedItems);
  }

  function handleContentsMarqueeSelectionChange(nextKeys, anchorKey) {
    setFolderSelection((current) => (current.length ? [] : current));
    setFolderAnchor(null);
    setContentsSelection((current) => (sameKeys(current, nextKeys) ? current : nextKeys));
    setContentsAnchor(anchorKey || null);
    closeContextMenu();
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
    if (openFoldersOnClick) {
      clearAllSelections();
      navigateToFolder(item.path || "");
      return;
    }
    setContentsSelection([]);
    setContentsAnchor(null);
    applyPaneSelection("folders", item, pointerEvent, orderedItems);
  }

  function handleSelectFavoriteDocument(item) {
    if (!item?.id) {
      return;
    }
    const key = keyForItem(item);
    const targetFolder = item.folder || "";
    setFolderSelection([]);
    setFolderAnchor(null);
    navigateToFolder(targetFolder);
    setContentsSelection([key]);
    setContentsAnchor(key);
    setSelectedId(item.id);
  }

  function handleContentsSortChange(key) {
    setContentsSort((current) => nextContentsSort(current, key));
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
    folder,
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
    handleRenameFile,
    handleRenameFolder,
  } = createFolderActionHandlers({
    apiFetch,
    folder,
    handleArchiveItems,
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
    if (!items || items.length === 0) {
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

  function handleUploadClick() {
    if (uploadInput.current) {
      uploadInput.current.click();
    }
  }

  const handleShareItem = useShareActions({ apiFetch, setError, showNotice });

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
      handleRelease,
      handleRemoveFavoriteItem,
      handleRestoreItems,
      handleRenameFile,
      handleRenameFolder,
      handleSave,
      handleShareItem,
      handleStartEdit,
      handleUnarchive,
      handleUnlockItems,
      handleUploadClick,
      openFolderProperties,
      openFileDetails,
      handleView,
      handleVersionDownload,
      handleVersionUploadClick,
      isAdmin,
      openMoveDialogForDoc,
      openMoveDialogForFolder,
      openMoveDialogForSelection,
      selectedDoc,
      siteSettings,
      navigateToFolder,
      uploading,
      ...extra,
    };
  }

  function handleFileContextMenu(evt, doc, options = {}) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!doc) {
      return;
    }
    const item = docToItem(doc);
    const key = keyForItem(item);
    const selectedItems = contentsSelection.includes(key) ? selectedContentsItems : [item];
    if (options.select !== false && !contentsSelection.includes(key)) {
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

  function handleFolderContextMenu(evt, folderItem, options = {}) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!folderItem) {
      return;
    }
    const item = folderToItem(folderItem);
    const key = keyForItem(item);
    const useFolderPane =
      folderItem.sourcePane === "folders" ||
      folderItem.sourcePane === "favorites" ||
      !contentsByKey.has(key);
    const paneSelection = useFolderPane ? folderSelection : contentsSelection;
    const paneItems = useFolderPane ? selectedFolderItems : selectedContentsItems;
    const selectedItems = paneSelection.includes(key) && paneItems.length ? paneItems : [item];
    const menuItems = item.favorite
      ? selectedItems.map((selectedItem) =>
          selectedItem.type === "folder" && selectedItem.path === item.path
            ? { ...selectedItem, favorite: true }
            : selectedItem
        )
      : selectedItems;
    if (options.select !== false && !paneSelection.includes(key)) {
      if (useFolderPane) {
        setFolderSelection([key]);
        setFolderAnchor(key);
      } else {
        setContentsSelection([key]);
        setContentsAnchor(key);
      }
    }
    const items = buildSelectionMenuItems(contextActions({ selectedItems: menuItems }));
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handlePageContextMenu(evt) {
    evt.preventDefault();
    const items = buildPageMenuItems(contextActions());
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  const notices = [
    ...(notice ? [notice] : []),
    ...(busy
      ? [
          {
            dismissible: false,
            id: "busy",
            kind: "busy",
            phase: "visible",
            title: "Working",
          },
        ]
      : []),
  ];

  return h(
    React.Fragment,
    null,
    devMode
      ? h("div", { className: "dev-mode-warning", role: "status" }, [
          h("strong", { key: "title" }, "DEVELOPMENT MODE"),
          h("span", { key: "copy" }, "Debug tools are enabled. Do not use real data."),
        ])
      : null,
    h(FinderShell, {
      folder,
      breadcrumbs,
      myEdits,
      folderChildren,
      favoriteItems: favoriteSidebarItems,
      subfolders,
      files: docs,
      selectedId,
      contentsItems,
      contentsSort,
      contentsSelection,
      folderItems: folderPaneItems,
      folderSelection,
      sidebarSectionCollapsed,
      sidebarSectionSizes,
      searchQuery,
      recursiveSearch,
      contentsPending,
      contentsPendingEmptySearch,
      dropHint,
      uploadHover,
      draggingId,
      draggingFolderPath,
      currentUser,
      doubleClickDownload,
      isAdmin,
      openFoldersOnClick,
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
      onSelectFavoriteDocument: handleSelectFavoriteDocument,
      onSelectContentItem: handleSelectContentItem,
      onSelectFolderItem: handleSelectFolderItem,
      onAddFavoriteItems: handleAddFavoriteItems,
      onSidebarLayoutChange: handleSidebarLayoutChange,
      onSidebarSectionSizesChange: handleSidebarSectionSizesChange,
      onSearchQueryChange: setSearchQuery,
      onRecursiveSearchChange: setRecursiveSearch,
      onContentsSortChange: handleContentsSortChange,
      onClearSelection: clearAllSelections,
      onContentsMarqueeSelectionChange: handleContentsMarqueeSelectionChange,
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
      onFavoriteFileContextMenu: (evt, item) => handleFileContextMenu(evt, item, { select: false }),
      onFolderContextMenu: handleFolderContextMenu,
      onMyEditContextMenu: handleMyEditContextMenu,
      onPageContextMenu: handlePageContextMenu,
      onTriggerUpload: handleUploadClick,
      logoutUrl,
      onOpenSettings: openSettings,
      settingsButtonRef,
      actions: contextActions(),
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
    h(NotificationDock, { notices, onDismiss: dismissNotice }),
    h(DragPreview, { drag: dragBundle }),
    settingsOpen
      ? h(SettingsModal, {
          apiFetch,
          appVersion: initialBootstrap.version,
          currentUser,
          devMode,
          doubleClickDownload,
          onAlternateRowsChange: handleAlternateRowsChange,
          onDoubleClickDownloadChange: handleDoubleClickDownloadChange,
          onOpenFoldersOnClickChange: handleOpenFoldersOnClickChange,
          onClose: closeSettings,
          onDebugError: setError,
          onPalettePreferenceChange: handlePalettePreferenceChange,
          onSiteSettingsChange: setSiteSettings,
          onThemePreferenceChange: handleThemePreferenceChange,
          openFoldersOnClick,
          alternateRows,
          palettePreference,
          siteName: initialBootstrap.site_name || "Vault",
          siteSettings,
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
    activeFileDetailsDoc
      ? h(FileDetailsModal, {
          actions: contextActions(),
          doc: activeFileDetailsDoc,
          onClose: closeFileDetails,
        })
      : null,
    h(ConfirmToast, { request: confirmRequest, onResolve: resolveConfirm }),
    contextMenu ? h(ContextMenu, { menu: contextMenu, onClose: closeContextMenu }) : null
  );
}
