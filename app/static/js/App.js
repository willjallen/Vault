import { FinderShell } from "./components/FinderShell.js";
import { ContextMenu } from "./components/browser/ContextMenu.js";
import { MoveDialog } from "./components/browser/MoveDialog.js";
import { createDropHandlers } from "./lib/dropHandlers.js";
import { isArchivePath, toBreadcrumbs, triggerDownload } from "./lib/utils.js";
import { useMoveDialog } from "./lib/useMoveDialog.js";

const { useEffect, useMemo, useState, useCallback, useRef } = React;
const h = React.createElement;

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
  const [addingFolder, setAddingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  const [creatingFolder, setCreatingFolder] = useState(false);
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
      .map((path) => ({
        path,
        name: path.split("/").filter(Boolean).slice(-1)[0] || (inArchiveView ? "Archive" : "Vault"),
      }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [folderChildren, folder, inArchiveView]);

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

  async function handleCreateFolder() {
    const trimmed = (newFolderName || "").trim().replace(/^\/+|\/+$/g, "");
    if (!trimmed) {
      setError("Folder name is required.");
      return;
    }
    const targetPath = folder ? `${folder}/${trimmed}` : trimmed;
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
      await refresh(targetPath);
      setFolder(targetPath);
      setNewFolderName("");
      setAddingFolder(false);
    } catch (err) {
      setError(err.message || "Could not create folder");
    } finally {
      setCreatingFolder(false);
    }
  }

  function handleUploadClick() {
    if (uploadInput.current) {
      uploadInput.current.click();
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

  function handleFileContextMenu(evt, doc) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!doc) {
      return;
    }
    setSelectedId(doc.id);
    const lock = doc.lock || {};
    const lockedByMe = lock && lock.by === currentUser.id;
    const lockedByOther = lock && lock.by && lock.by !== currentUser.id;
    const items = [
      { label: "Open", action: () => handleView(doc) },
      {
        label: "Rename",
        action: () => handleRenameFile(doc),
        disabled: busy,
      },
      {
        label: "Move…",
        action: () => openMoveDialogForDoc(doc),
        disabled: busy || lockedByOther,
      },
      doc.archived
        ? { label: "Restore to Vault", action: () => handleUnarchive(doc.id), disabled: busy }
        : { label: "Move to Archive", action: () => handleArchive(doc.id), disabled: busy },
      !doc.archived && !lockedByOther
        ? {
            label: lockedByMe ? "Re-download (locked)" : "Lock for editing",
            action: () => handleStartEdit(doc),
          }
        : null,
      lockedByMe && !doc.archived
        ? { label: "Unlock file", action: () => handleRelease(doc.id), disabled: busy }
        : null,
      isAdmin && doc.archived
        ? {
            label: "Delete forever",
            action: () => {
              const confirmed = window.confirm(
                `This will permanently delete "${doc.name}" from the archive. You cannot undo this.`
              );
              if (confirmed) {
                handlePermanentDelete(doc.id);
              }
            },
            danger: true,
            disabled: busy,
          }
        : null,
    ].filter(Boolean);
    setContextMenu({ x: evt.clientX, y: evt.clientY, items });
  }

  function handleFolderContextMenu(evt, folderItem) {
    evt.preventDefault();
    evt.stopPropagation();
    if (!folderItem) {
      return;
    }
    const isArchivedFolder = (folderItem.path || "").startsWith("Archive");
    const hasPath = Boolean(folderItem.path);
    const canPermanentDeleteFolder =
      isAdmin && isArchivedFolder && folderItem.path && folderItem.path !== "Archive";
    const items = [
      { label: "Open", action: () => setFolder(folderItem.path || "") },
      hasPath
        ? {
            label: "Rename",
            action: () => handleRenameFolder(folderItem.path),
            disabled: busy,
          }
        : null,
      hasPath
        ? {
            label: "Move…",
            action: () => openMoveDialogForFolder(folderItem),
            disabled: busy,
          }
        : null,
      hasPath
        ? isArchivedFolder
          ? {
              label: "Restore to Vault",
              action: () => handleUnarchiveFolder(folderItem.path, { navigate: false }),
              disabled: busy,
            }
          : {
              label: "Move to Archive",
              action: () => handleArchiveFolder(folderItem.path, { navigate: false }),
              disabled: busy,
            }
        : null,
      canPermanentDeleteFolder
        ? {
            label: "Delete forever",
            action: () => handlePermanentDeleteFolder(folderItem.path),
            danger: true,
            disabled: busy,
          }
        : null,
    ].filter(Boolean);
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
      addingFolder,
      creatingFolder,
      newFolderName,
      onNewFolderNameChange: setNewFolderName,
      onStartAddingFolder: () => setAddingFolder(true),
      onCancelCreateFolder: () => {
        setAddingFolder(false);
        setNewFolderName("");
      },
      onCreateFolder: handleCreateFolder,
      onSelectFolder: setFolder,
      onSelectDoc: setSelectedId,
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
