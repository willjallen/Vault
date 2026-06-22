import { FinderShell } from "./components/FinderShell.js";
import { ConfirmToast } from "./components/ConfirmToast.js";
import { TransferDock } from "./components/TransferDock.js";
import { ContextMenu } from "./components/browser/ContextMenu.js";
import { MoveDialog } from "./components/browser/MoveDialog.js";
import {
  buildFileMenuItems,
  buildFolderMenuItems,
  buildMyEditMenuItems,
  buildPageMenuItems,
} from "./lib/contextMenus.js";
import { createDropHandlers } from "./lib/dropHandlers.js";
import { createFileLockActions } from "./lib/fileLockActions.js";
import {
  folderBaseName,
  folderParent,
  folderPathForName,
  isArchivePath,
  normalizeFolderName,
  toBreadcrumbs,
} from "./lib/utils.js";
import { useMouseNavigation } from "./lib/useMouseNavigation.js";
import { useMoveDialog } from "./lib/useMoveDialog.js";
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
  const [uploading, setUploading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [draggingId, setDraggingId] = useState(null);
  const [dropHint, setDropHint] = useState(null);
  const [uploadHover, setUploadHover] = useState(false);
  const [creatingFolder, setCreatingFolder] = useState(false);
  const [inlineFolderDraft, setInlineFolderDraft] = useState(null);
  const [contextMenu, setContextMenu] = useState(null);
  const [draggingFolderPath, setDraggingFolderPath] = useState(null);
  const [toast, setToast] = useState("");
  const [confirmRequest, setConfirmRequest] = useState(null);
  const uploadInput = useRef(null);
  const versionUploadInput = useRef(null);
  const versionUploadDoc = useRef(null);
  const versionUploadOptions = useRef({});
  const confirmResolver = useRef(null);
  const closeContextMenu = useCallback(() => setContextMenu(null), []);

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

  const baseDomain =
    initialBootstrap.base_domain ||
    (window.location.hostname.includes(".")
      ? window.location.hostname.split(".").slice(1).join(".")
      : "");
  const logoutUrl = useMemo(() => {
    const rd = encodeURIComponent(window.location.href);
    return baseDomain ? `https://auth.${baseDomain}/logout?rd=${rd}` : `/logout?rd=${rd}`;
  }, [baseDomain]);
  const redirectingRef = useRef(false);

  const redirectToLogin = useCallback(() => {
    if (redirectingRef.current) {
      return;
    }
    redirectingRef.current = true;
    setToast("Session expired. Redirecting to login…");
    const rd = encodeURIComponent(window.location.href);
    const loginUrl = baseDomain ? `https://auth.${baseDomain}/?rd=${rd}` : `/login?rd=${rd}`;
    window.location.href = loginUrl;
  }, [baseDomain]);
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
    setFolder(normalized);
    closeContextMenu();
  }

  function replaceFolder(nextFolder) {
    const normalized = nextFolder || "";
    if (normalized === folder) {
      return;
    }
    setSelectedId(null);
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

  function handleView(doc) {
    downloadWithProgress({
      name: doc.name,
      size: doc.size_bytes,
      url: `/documents/${doc.id}/download`,
    }).catch((err) => {
      setError(err.message || "Download failed.");
    });
  }

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

  async function handleMove(docId, newPath) {
    if (!newPath) {
      return false;
    }
    const doc = docs.find((d) => d.id === docId);
    const pathLower = (newPath || "").trim();
    const targetInArchive = isArchivePath(pathLower);
    const docArchived = doc && doc.archived;
    if (docArchived && !targetInArchive) {
      setError("Restore this file before moving it out of Archive.");
      return false;
    }
    if (!docArchived && targetInArchive) {
      setError("Use Move to Archive instead of dragging items into Archive.");
      return false;
    }
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("new_path", newPath);
    let success = false;
    try {
      const res = await apiFetch(`/documents/${docId}/move`, { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Move failed");
      }
      await refresh(undefined, { invalidateContents: true });
      success = true;
    } catch (err) {
      setError(err.message || "Move failed.");
    } finally {
      setBusy(false);
      setDraggingId(null);
      setDropHint(null);
    }
    return success;
  }

  async function handleArchive(docId) {
    const doc = docs.find((item) => item.id === docId) || selectedDoc;
    const confirmed = await requestConfirm({
      title: "Move to Archive",
      message: `Move "${doc?.name || "this file"}" to Archive?`,
      confirmLabel: "Move",
    });
    if (!confirmed) {
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${docId}/archive`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Archive failed");
      }
      await refresh(undefined, { invalidateContents: true, sidebar: true });
      setSelectedId(null);
      return true;
    } catch (err) {
      setError(err.message || "Archive failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handleUnarchive(docId) {
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${docId}/unarchive`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Unarchive failed");
      }
      const data = await res.json();
      const destFolder = data.path ? data.path.split("/").slice(0, -1).join("/") : "";
      await refresh(destFolder, { invalidateContents: true, sidebar: true });
      replaceFolder(destFolder);
      setSelectedId(null);
    } catch (err) {
      setError(err.message || "Unarchive failed.");
    } finally {
      setBusy(false);
    }
  }

  async function handlePermanentDelete(docId) {
    const doc = docs.find((item) => item.id === docId) || selectedDoc;
    const confirmed = await requestConfirm({
      title: "Delete forever",
      message: `Permanently delete "${doc?.name || "this file"}" from Archive? This cannot be undone.`,
      confirmLabel: "Delete forever",
      tone: "danger",
    });
    if (!confirmed) {
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${docId}/permanent_delete`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Delete failed");
      }
      await refresh(undefined, { invalidateContents: true, sidebar: true });
      setSelectedId(null);
      return true;
    } catch (err) {
      setError(err.message || "Delete failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handlePermanentDeleteFolder(targetFolder = folder, options = {}) {
    const selectedFolder = typeof targetFolder === "string" ? targetFolder : folder || "";
    const currentFolder = folder || "";
    const withinSelected =
      selectedFolder &&
      (currentFolder === selectedFolder || currentFolder.startsWith(`${selectedFolder}/`));
    const shouldNavigate = options.navigate ?? withinSelected;
    if (!selectedFolder) {
      setError("Choose a folder to delete.");
      return;
    }
    if (selectedFolder === "Archive") {
      setError("Cannot delete the Archive root.");
      return;
    }
    if (!isArchivePath(selectedFolder)) {
      setError("Delete forever is only available in Archive.");
      return;
    }
    const confirmed = await requestConfirm({
      title: "Delete forever",
      message: `Permanently delete "${selectedFolder}" and everything inside? This cannot be undone.`,
      confirmLabel: "Delete forever",
      tone: "danger",
    });
    if (!confirmed) {
      return;
    }
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("folder", selectedFolder);
    try {
      const res = await apiFetch("/folders/permanent_delete", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Delete failed");
      }
      const parentFolder = selectedFolder.split("/").slice(0, -1).join("/");
      const refreshTarget = shouldNavigate ? parentFolder : folder;
      await refresh(refreshTarget, { invalidateContents: true, sidebar: true });
      if (shouldNavigate) {
        replaceFolder(parentFolder);
      }
      setSelectedId(null);
    } catch (err) {
      setError(err.message || "Delete failed.");
    } finally {
      setBusy(false);
    }
  }

  async function handleArchiveFolder(targetFolder = folder, options = {}) {
    const selectedFolder = typeof targetFolder === "string" ? targetFolder : folder || "";
    const shouldNavigate = options.navigate ?? selectedFolder === folder;
    if (!selectedFolder || isArchivePath(selectedFolder)) {
      setError("Pick a Vault folder to move into Archive.");
      return;
    }
    const confirmed = await requestConfirm({
      title: "Move to Archive",
      message: `Move "${selectedFolder}" and everything inside to Archive?`,
      confirmLabel: "Move",
    });
    if (!confirmed) {
      return;
    }
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("folder", selectedFolder);
    try {
      const res = await apiFetch("/folders/archive", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Archive failed");
      }
      const payload = await res.json();
      const dest = payload.archive_folder || `Archive/${selectedFolder}`;
      const refreshTarget = shouldNavigate ? dest : folder;
      await refresh(refreshTarget, { invalidateContents: true, sidebar: true });
      if (shouldNavigate) {
        replaceFolder(dest);
      }
    } catch (err) {
      setError(err.message || "Archive failed.");
    } finally {
      setBusy(false);
    }
  }

  async function handleUnarchiveFolder(targetFolder = folder, options = {}) {
    const selectedFolder = typeof targetFolder === "string" ? targetFolder : folder || "";
    const shouldNavigate = options.navigate ?? selectedFolder === folder;
    if (!selectedFolder || !isArchivePath(selectedFolder)) {
      setError("Choose an archived folder to restore.");
      return;
    }
    const confirmed = await requestConfirm({
      title: "Restore to Vault",
      message: `Restore "${selectedFolder}" back to Vault?`,
      confirmLabel: "Restore",
    });
    if (!confirmed) {
      return;
    }
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("folder", selectedFolder);
    try {
      const res = await apiFetch("/folders/unarchive", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Unarchive failed");
      }
      const payload = await res.json();
      const dest = payload.folder || "";
      const refreshTarget = shouldNavigate ? dest : folder;
      await refresh(refreshTarget, { invalidateContents: true, sidebar: true });
      if (shouldNavigate) {
        replaceFolder(dest);
      }
    } catch (err) {
      setError(err.message || "Unarchive failed.");
    } finally {
      setBusy(false);
    }
  }

  const {
    handleDropOnFolder,
    handleCanvasDrop,
    handleCanvasDragOver,
    handleCanvasDragLeave,
    handleFileDragStart,
    handleFileDragEnd,
    handleFolderDragStart,
    handleFolderDragEnd,
    clearDropState,
  } = createDropHandlers({
    folder,
    docs,
    draggingId,
    draggingFolderPath,
    setDropHint,
    setUploadHover,
    setError,
    handleArchiveFolder,
    handleRenameFolder,
    handleUpload,
    handleMove,
    handleArchive,
    setDraggingId,
    setDraggingFolderPath,
  });

  async function handleCreateFolder(folderName, parentFolder = folder) {
    const trimmed = normalizeFolderName(folderName);
    if (!trimmed) {
      setError("Folder name is required.");
      return false;
    }
    if (trimmed.includes("/")) {
      setError("Folder name cannot contain slashes.");
      return false;
    }
    const targetPath = folderPathForName(parentFolder || "", trimmed);
    setCreatingFolder(true);
    setError("");
    const form = new FormData();
    form.append("folder", targetPath);
    try {
      const res = await apiFetch("/folders", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Could not create folder");
      }
      await refresh(parentFolder || "", { invalidateContents: true, sidebar: true });
      return true;
    } catch (err) {
      setError(err.message || "Could not create folder");
      return false;
    } finally {
      setCreatingFolder(false);
    }
  }

  function handleUploadClick() {
    if (uploadInput.current) {
      uploadInput.current.click();
    }
  }

  function beginCreateFolder(parentFolder = folder) {
    setSelectedId(null);
    setInlineFolderDraft({
      mode: "create",
      parent: parentFolder || "",
      path: "",
      value: "New Folder",
    });
  }

  function beginRenameFolder(targetFolder = folder) {
    const folderPath = typeof targetFolder === "string" ? targetFolder : "";
    if (!folderPath || folderPath === "Archive") {
      setError("Choose a folder to rename.");
      return;
    }
    const parentPath = folderParent(folderPath);
    if ((folder || "") !== parentPath) {
      replaceFolder(parentPath);
    }
    setSelectedId(null);
    setInlineFolderDraft({
      mode: "rename",
      parent: parentPath,
      path: folderPath,
      value: folderBaseName(folderPath, "Folder"),
    });
  }

  function handleInlineFolderNameChange(value) {
    setInlineFolderDraft((draft) => (draft ? { ...draft, value } : draft));
  }

  function handleCancelInlineFolder() {
    setInlineFolderDraft(null);
  }

  async function handleCommitInlineFolder(value) {
    const draft = inlineFolderDraft;
    if (!draft) {
      return;
    }
    const trimmed = normalizeFolderName(value);
    if (!trimmed) {
      setError("Folder name is required.");
      return;
    }
    if (trimmed.includes("/")) {
      setError("Folder name cannot contain slashes.");
      return;
    }
    const success =
      draft.mode === "create"
        ? await handleCreateFolder(trimmed, draft.parent)
        : await handleRenameFolder(draft.path, folderPathForName(draft.parent, trimmed), {
            navigate: false,
          });
    if (success || folderPathForName(draft.parent, trimmed) === draft.path) {
      setInlineFolderDraft(null);
    }
  }

  function handleRenameFile(doc) {
    const currentName = doc?.name || "";
    const next = window.prompt("Rename file", currentName);
    if (next === null) {
      return;
    }
    const trimmed = (next || "").trim();
    if (!trimmed) {
      setError("File name is required.");
      return;
    }
    if (trimmed.includes("/")) {
      setError("File name cannot contain slashes.");
      return;
    }
    const targetPath = doc.folder ? `${doc.folder}/${trimmed}` : trimmed;
    if (targetPath === doc.path) {
      return;
    }
    handleMove(doc.id, targetPath);
  }

  async function handleRenameFolder(targetFolder = folder, newPathOverride = null, options = {}) {
    const folderPath = typeof targetFolder === "string" ? targetFolder : "";
    if (!folderPath) {
      setError("Cannot rename the root Vault folder.");
      return false;
    }
    const parts = folderPath.split("/").filter(Boolean);
    const currentName = parts.slice(-1)[0] || folderPath;
    const parentFolder = parts.slice(0, -1).join("/");
    let newPath = newPathOverride || "";
    if (!newPath) {
      const next = window.prompt("Rename folder", currentName);
      if (next === null) {
        return;
      }
      const trimmed = (next || "").trim().replace(/^\/+|\/+$/g, "");
      if (!trimmed) {
        setError("Folder name is required.");
        return;
      }
      if (trimmed.includes("/")) {
        setError("Folder name cannot contain slashes.");
        return;
      }
      newPath = parentFolder ? `${parentFolder}/${trimmed}` : trimmed;
    }
    const normalizedNewPath = (newPath || "").trim().replace(/^\/+|\/+$/g, "");
    if (!normalizedNewPath) {
      setError("New folder path is required.");
      return false;
    }
    if (normalizedNewPath === folderPath) {
      return false;
    }
    const shouldNavigate = options.navigate ?? folder === folderPath;
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("folder", folderPath);
    form.append("new_path", normalizedNewPath);
    let success = false;
    try {
      const res = await apiFetch("/folders/rename", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Rename failed");
      }
      const payload = await res.json();
      const dest = payload.folder || normalizedNewPath;
      const refreshTarget = shouldNavigate ? dest : folder;
      await refresh(refreshTarget, { invalidateContents: true, sidebar: true });
      if (shouldNavigate) {
        replaceFolder(dest);
      }
      success = true;
    } catch (err) {
      setError(err.message || "Rename failed.");
    } finally {
      setBusy(false);
    }
    return success;
  }

  const {
    moveTarget,
    moveDestination,
    moveNewFolderName,
    creatingMoveFolder,
    openMoveDialogForDoc,
    openMoveDialogForFolder,
    closeMoveDialog,
    setMoveDestinationSafe,
    handleCreateMoveFolder,
    handleConfirmMoveTarget,
    setMoveNewFolderName,
  } = useMoveDialog({
    folder,
    handleMove,
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
      handleArchiveFolder,
      handleLock,
      handlePermanentDelete,
      handlePermanentDeleteFolder,
      handleRelease,
      handleRenameFile,
      handleRenameFolder,
      handleStartEdit,
      handleUnarchive,
      handleUnarchiveFolder,
      handleUploadClick,
      handleView,
      handleVersionUploadClick,
      isAdmin,
      openMoveDialogForDoc,
      openMoveDialogForFolder,
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
    setSelectedId(doc.id);
    const items = buildFileMenuItems(contextActions({ doc }));
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
    const items = buildFolderMenuItems(contextActions({ folderItem }));
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handlePageContextMenu(evt) {
    evt.preventDefault();
    const items = buildPageMenuItems(contextActions());
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

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
      onSearchQueryChange: setSearchQuery,
      onRecursiveSearchChange: setRecursiveSearch,
      onClearSelection: () => setSelectedId(null),
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
      onDownload: handleView,
      onDownloadVersion: handleVersionDownload,
      onLock: handleLock,
      onRename: handleRenameFile,
      onMove: openMoveDialogForDoc,
      onStartEdit: handleStartEdit,
      onRelease: handleRelease,
      onSave: handleSave,
      onArchive: handleArchive,
      onUnarchive: handleUnarchive,
      onPermanentDelete: handlePermanentDelete,
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
    h(ConfirmToast, { request: confirmRequest, onResolve: resolveConfirm }),
    contextMenu ? h(ContextMenu, { menu: contextMenu, onClose: closeContextMenu }) : null,
    error ? h("div", { className: "toast error" }, error) : null,
    busy ? h("div", { className: "toast subtle" }, "Working...") : null
  );
}
