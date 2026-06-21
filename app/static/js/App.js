import { FinderShell } from "./components/FinderShell.js";
import { ContextMenu } from "./components/browser/ContextMenu.js";
import { MoveDialog } from "./components/browser/MoveDialog.js";
import { createDropHandlers } from "./lib/dropHandlers.js";
import { isArchivePath, toBreadcrumbs, triggerDownload } from "./lib/utils.js";
import { useMoveDialog } from "./lib/useMoveDialog.js";

const { useEffect, useMemo, useState, useCallback, useRef } = React;
const h = React.createElement;

function compactMenuItems(items) {
  const compacted = items.filter(Boolean).reduce((acc, item) => {
    const previous = acc[acc.length - 1];
    if (item.type === "separator" && (!previous || previous.type === "separator")) {
      return acc;
    }
    acc.push(item);
    return acc;
  }, []);
  if (compacted[compacted.length - 1]?.type === "separator") {
    compacted.pop();
  }
  return compacted;
}

function buildFileMenuItems(actions) {
  const { doc, currentUser, busy, isAdmin } = actions;
  const lock = doc.lock || {};
  const lockedByMe = lock && lock.by === currentUser.id;
  const lockedByOther = lock && lock.by && lock.by !== currentUser.id;
  return compactMenuItems([
    { label: "Open", action: () => actions.handleView(doc) },
    { label: "Rename", action: () => actions.handleRenameFile(doc), disabled: busy },
    {
      label: "Move...",
      action: () => actions.openMoveDialogForDoc(doc),
      disabled: busy || lockedByOther,
    },
    doc.archived
      ? { label: "Restore to Vault", action: () => actions.handleUnarchive(doc.id), disabled: busy }
      : { label: "Move to Archive", action: () => actions.handleArchive(doc.id), disabled: busy },
    !doc.archived && !lockedByOther
      ? {
          label: lockedByMe ? "Re-download (locked)" : "Lock for editing",
          action: () => actions.handleStartEdit(doc),
        }
      : null,
    lockedByMe && !doc.archived
      ? { label: "Unlock file", action: () => actions.handleRelease(doc.id), disabled: busy }
      : null,
    isAdmin && doc.archived
      ? {
          label: "Delete forever",
          action: () => {
            const confirmed = window.confirm(
              `This will permanently delete "${doc.name}" from the archive. You cannot undo this.`
            );
            if (confirmed) {
              actions.handlePermanentDelete(doc.id);
            }
          },
          danger: true,
          disabled: busy,
        }
      : null,
  ]);
}

function buildFolderMenuItems(actions) {
  const { folderItem, busy, isAdmin } = actions;
  const folderPath = folderItem.path || "";
  const isArchivedFolder = folderPath.startsWith("Archive");
  const hasPath = Boolean(folderPath);
  const canPermanentDeleteFolder = isAdmin && isArchivedFolder && folderPath !== "Archive";
  return compactMenuItems([
    { label: "Open", action: () => actions.setFolder(folderPath) },
    hasPath
      ? { label: "Rename", action: () => actions.beginRenameFolder(folderPath), disabled: busy }
      : null,
    hasPath
      ? {
          label: "Move...",
          action: () => actions.openMoveDialogForFolder(folderItem),
          disabled: busy,
        }
      : null,
    hasPath
      ? isArchivedFolder
        ? {
            label: "Restore to Vault",
            action: () => actions.handleUnarchiveFolder(folderPath, { navigate: false }),
            disabled: busy,
          }
        : {
            label: "Move to Archive",
            action: () => actions.handleArchiveFolder(folderPath, { navigate: false }),
            disabled: busy,
          }
      : null,
    canPermanentDeleteFolder
      ? {
          label: "Delete forever",
          action: () => actions.handlePermanentDeleteFolder(folderPath),
          danger: true,
          disabled: busy,
        }
      : null,
  ]);
}

function buildPageMenuItems(actions) {
  const currentFolder = actions.folder || "";
  return [
    {
      label: "Upload file",
      action: actions.handleUploadClick,
      disabled: actions.busy || actions.uploading,
    },
    {
      label: "New folder",
      action: () => actions.beginCreateFolder(currentFolder),
      disabled: actions.busy || actions.creatingFolder,
    },
  ];
}

function folderParts(path) {
  return (path || "").split("/").filter(Boolean);
}

function folderParent(path) {
  return folderParts(path).slice(0, -1).join("/");
}

function folderBaseName(path, fallback = "New Folder") {
  return folderParts(path).slice(-1)[0] || fallback;
}

function normalizeFolderName(value) {
  return (value || "").trim().replace(/^\/+|\/+$/g, "");
}

function folderPathForName(parentPath, folderName) {
  return parentPath ? `${parentPath}/${folderName}` : folderName;
}

export function App({ initial }) {
  const [folder, setFolder] = useState(initial.current_folder || "");
  const [state, setState] = useState(initial);
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
  const uploadInput = useRef(null);
  const closeContextMenu = useCallback(() => setContextMenu(null), []);

  const baseDomain =
    state.base_domain ||
    (window.location.hostname.includes(".")
      ? window.location.hostname.split(".").slice(1).join(".")
      : "");
  const redirectingRef = useRef(false);
  const docs = useMemo(() => state.doc_payloads || [], [state.doc_payloads]);
  const folderChildren = useMemo(() => state.folder_children || {}, [state.folder_children]);
  const folderPayloads = useMemo(() => state.folder_payloads || {}, [state.folder_payloads]);

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

  const currentUser = state.user || {};
  const isAdmin = Boolean(currentUser.is_admin);

  const inArchiveView = isArchivePath(folder);

  const visibleDocs = useMemo(() => {
    const targetFolder = folder || "";
    return docs
      .filter((d) => {
        const docFolder = d.folder || "";
        if (inArchiveView) {
          return docFolder === targetFolder;
        }
        if (isArchivePath(docFolder)) {
          return false;
        }
        return docFolder === targetFolder;
      })
      .slice()
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [docs, folder, inArchiveView]);

  const subfolders = useMemo(() => {
    const targetFolder = folder || "";
    // eslint-disable-next-line security/detect-object-injection
    const raw = (folderChildren[targetFolder] || []).filter((path) =>
      inArchiveView ? isArchivePath(path) : !isArchivePath(path)
    );
    return raw
      .map((path) => {
        // eslint-disable-next-line security/detect-object-injection
        const payload = folderPayloads[path] || {};
        return {
          path,
          name:
            payload.name ||
            path.split("/").filter(Boolean).slice(-1)[0] ||
            (inArchiveView ? "Archive" : "Vault"),
          latest_updated_display: payload.latest_updated_display || "Not updated yet",
          latest_updated_at: payload.latest_updated_at || null,
          size_bytes: payload.size_bytes || 0,
          size_display: payload.size_display || "0 B",
        };
      })
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [folderChildren, folder, folderPayloads, inArchiveView]);

  const breadcrumbs = useMemo(() => toBreadcrumbs(folder || ""), [folder]);
  const selectedDoc = docs.find((d) => d.id === selectedId) || null;
  const myEdits = docs.filter((d) => d.lock && d.lock.by === currentUser.id);

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
    if (!selectedId) {
      return;
    }
    const doc = docs.find((d) => d.id === selectedId);
    const targetFolder = folder || "";
    if (!doc || doc.folder !== targetFolder) {
      setSelectedId(null);
    }
  }, [docs, selectedId, folder]);

  useEffect(() => {
    closeContextMenu();
  }, [folder, closeContextMenu]);

  useEffect(() => {
    const interval = setInterval(() => {
      apiFetch(`/api/state?folder=${encodeURIComponent(folder || "")}`).catch(() => {});
    }, 45000);
    return () => clearInterval(interval);
  }, [apiFetch, folder]);

  const refresh = useCallback(
    async (nextFolder = folder) => {
      try {
        const res = await apiFetch(`/api/state?folder=${encodeURIComponent(nextFolder || "")}`);
        const data = await res.json();
        setState((prev) => ({ ...prev, ...data }));
      } catch (err) {
        setError("Could not refresh data.");
      }
    },
    [apiFetch, folder]
  );

  useEffect(() => {
    refresh(folder);
  }, [folder, refresh]);

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
      const res = await apiFetch("/documents", { method: "POST", body: form });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Upload failed");
      }
      await refresh(folder);
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

  async function handleRelease(docId) {
    setBusy(true);
    setError("");
    const form = new FormData();
    try {
      const res = await apiFetch(`/documents/${docId}/release?mode=json`, {
        method: "POST",
        body: form,
      });
      if (!res.ok) {
        throw new Error("Release failed");
      }
      await refresh(folder);
      setSelectedId(null);
    } catch (err) {
      setError("Could not release the file.");
    } finally {
      setBusy(false);
    }
  }

  async function handleSave(docId, file, note) {
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("file", file);
    if (note) {
      form.append("note", note);
    }
    try {
      const res = await apiFetch(`/documents/${docId}/checkin`, { method: "POST", body: form });
      if (!res.ok) {
        throw new Error("Save failed");
      }
      await refresh(folder);
    } catch (err) {
      setError("Save failed. Please try again.");
    } finally {
      setBusy(false);
    }
  }

  function handleView(doc) {
    triggerDownload(`/documents/${doc.id}/download`);
  }

  function handleStartEdit(doc) {
    if (doc.archived) {
      setError("Restore this file from Archive before editing.");
      return;
    }
    triggerDownload(`/documents/${doc.id}/checkout`);
    const optimisticLock = {
      by: currentUser.id,
      name: currentUser.name,
      at: new Date().toISOString(),
    };
    setState((prev) => ({
      ...prev,
      doc_payloads: (prev.doc_payloads || []).map((d) =>
        d.id === doc.id ? { ...d, lock: optimisticLock } : d
      ),
    }));
    setTimeout(() => refresh(folder), 800);
  }

  async function handleMove(docId, newPath) {
    if (!newPath) {
      return false;
    }
    const doc = docs.find((d) => d.id === docId);
    const pathLower = (newPath || "").trim();
    const targetInArchive = pathLower.startsWith("Archive");
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
      await refresh(folder);
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
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${docId}/archive`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Archive failed");
      }
      await refresh(folder);
      setSelectedId(null);
    } catch (err) {
      setError(err.message || "Archive failed.");
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
      await refresh(destFolder);
      setFolder(destFolder);
      setSelectedId(null);
    } catch (err) {
      setError(err.message || "Unarchive failed.");
    } finally {
      setBusy(false);
    }
  }

  async function handlePermanentDelete(docId) {
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${docId}/permanent_delete`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Delete failed");
      }
      await refresh(folder);
      setSelectedId(null);
    } catch (err) {
      setError(err.message || "Delete failed.");
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
    if (!selectedFolder.startsWith("Archive")) {
      setError("Delete forever is only available in Archive.");
      return;
    }
    const confirmed = window.confirm(
      `This will permanently delete "${selectedFolder}" and everything inside. You cannot undo this.`
    );
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
      await refresh(refreshTarget);
      if (shouldNavigate) {
        setFolder(parentFolder);
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
    if (!selectedFolder || selectedFolder.startsWith("Archive")) {
      setError("Pick a Vault folder to move into Archive.");
      return;
    }
    const confirmed = window.confirm(`Move "${selectedFolder}" and everything inside to Archive?`);
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
      await refresh(refreshTarget);
      if (shouldNavigate) {
        setFolder(dest);
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
    if (!selectedFolder || !selectedFolder.startsWith("Archive")) {
      setError("Choose an archived folder to restore.");
      return;
    }
    const confirmed = window.confirm(`Restore "${selectedFolder}" back to Vault?`);
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
      await refresh(refreshTarget);
      if (shouldNavigate) {
        setFolder(dest);
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
      await refresh(parentFolder || "");
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
      setFolder(parentPath);
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
      await refresh(refreshTarget);
      if (shouldNavigate) {
        setFolder(dest);
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
      isAdmin,
      openMoveDialogForDoc,
      openMoveDialogForFolder,
      selectedDoc,
      setFolder,
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
      files: visibleDocs,
      selectedId,
      selectedDoc,
      dropHint,
      uploadHover,
      draggingId,
      draggingFolderPath,
      currentUser,
      isAdmin,
      creatingFolder,
      inlineFolderDraft,
      onInlineFolderNameChange: handleInlineFolderNameChange,
      onCommitInlineFolder: handleCommitInlineFolder,
      onCancelInlineFolder: handleCancelInlineFolder,
      onStartAddingFolder: () => beginCreateFolder(folder),
      onSelectFolder: setFolder,
      onSelectDoc: setSelectedId,
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
      onPageContextMenu: handlePageContextMenu,
      onUploadFile: (file) => handleUpload(file),
      onTriggerUpload: handleUploadClick,
      uploadInputRef: uploadInput,
      onDownload: handleView,
      onRename: handleRenameFile,
      onMove: openMoveDialogForDoc,
      onStartEdit: handleStartEdit,
      onRelease: handleRelease,
      onSave: handleSave,
      onArchive: handleArchive,
      onUnarchive: handleUnarchive,
      onPermanentDelete: handlePermanentDelete,
      onOpenFolder: setFolder,
      busy,
      uploading,
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
    contextMenu ? h(ContextMenu, { menu: contextMenu, onClose: closeContextMenu }) : null,
    error ? h("div", { className: "toast error" }, error) : null,
    busy ? h("div", { className: "toast subtle" }, "Working...") : null
  );
}
