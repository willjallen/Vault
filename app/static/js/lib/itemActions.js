import { folderBaseName, folderParent, isArchivePath } from "./utils.js";

export function keyForItem(item) {
  if (item.type === "document") {
    return `document:${item.id}`;
  }
  return item.id ? `folder:${item.id}` : `folder:${item.path || ""}`;
}

export function docToItem(doc) {
  if (!doc) {
    return null;
  }
  return {
    archived: Boolean(doc.archived),
    access: doc.access || {},
    folder: doc.folder || "",
    favorite: Boolean(doc.favorite),
    id: doc.id,
    latest_by: doc.latest_by || "",
    modified_at: doc.modified_at || null,
    modified_display: doc.modified_display || "",
    latest_version_number: doc.latest_version_number || 0,
    lock: doc.lock || {},
    name: doc.name,
    path: doc.path || (doc.folder ? `${doc.folder}/${doc.name}` : doc.name),
    expires_at: doc.expires_at || null,
    expiry_action: doc.expiry_action || "",
    size_bytes: doc.size_bytes || 0,
    size_display: doc.size_display || "",
    type: "document",
    version_count: doc.version_count || 0,
    versions: doc.versions || [],
  };
}

export function folderToItem(folderItem) {
  return {
    archived: isArchivePath(folderItem.path || ""),
    access: folderItem.access || {},
    color: folderItem.color || "",
    default_ttl_action: folderItem.default_ttl_action || "none",
    default_ttl_days: folderItem.default_ttl_days || null,
    effective_ttl_action: folderItem.effective_ttl_action || "none",
    effective_ttl_days: folderItem.effective_ttl_days || null,
    effective_ttl_inherited: Boolean(folderItem.effective_ttl_inherited),
    effective_ttl_source_id: folderItem.effective_ttl_source_id || null,
    icon: folderItem.icon || "",
    id: folderItem.id || null,
    latest_by: folderItem.latest_by || "",
    modified_at: folderItem.modified_at || null,
    modified_display: folderItem.modified_display || "",
    name: folderItem.name || folderBaseName(folderItem.path || "", "Folder"),
    path: folderItem.path || "",
    favorite: Boolean(folderItem.favorite),
    size_bytes: folderItem.size_bytes || 0,
    size_display: folderItem.size_display || "",
    type: "folder",
  };
}

function apiItem(item) {
  if (item.type === "document") {
    return { type: "document", id: item.id };
  }
  return item.id
    ? { type: "folder", id: item.id, path: item.path || "" }
    : { type: "folder", path: item.path || "" };
}

function selectionLabel(items) {
  if (!items.length) {
    return "this selection";
  }
  if (items.length === 1) {
    return `"${items[0].name || "this item"}"`;
  }
  const files = items.filter((item) => item.type === "document").length;
  const folders = items.length - files;
  return `${items.length} items${files ? `, ${files} files` : ""}${folders ? `, ${folders} folders` : ""}`;
}

function firstFailureMessage(payload, fallback) {
  if (payload?.failed?.length) {
    return payload.failed[0].detail || fallback;
  }
  return "";
}

function successSummary(action, payload) {
  const ok = payload?.ok?.length || 0;
  const failed = payload?.failed?.length || 0;
  if (!failed) {
    return "";
  }
  return `${action}: ${ok} succeeded, ${failed} failed. ${firstFailureMessage(payload, "")}`;
}

export function createBulkActionHandlers({
  apiFetch,
  clearAllSelections,
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
}) {
  async function postAction(action, items, extra = {}) {
    const actionItems = (items || []).filter(Boolean);
    if (!actionItems.length) {
      throw new Error("Select at least one item");
    }
    const res = await apiFetch(`/api/${action}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ items: actionItems.map(apiItem), ...extra }),
    });
    if (!res.ok) {
      const detail = await res.json().catch(() => ({}));
      throw new Error(detail.detail || `${action} failed`);
    }
    return res.json();
  }

  async function refreshAfterAction(nextFolder = undefined, options = {}) {
    await refresh(nextFolder, { invalidateContents: true, sidebar: true, ...options });
  }

  function missingFolderFallbackForDelete() {
    const currentFolder = folder || "";
    if (!isArchivePath(currentFolder) || currentFolder === "Archive") {
      return "";
    }
    return folderParent(currentFolder) || "Archive";
  }

  function handleDownloadItems(items) {
    const actionItems = (items || []).filter(Boolean);
    if (!actionItems.length) {
      return Promise.resolve();
    }
    const label = actionItems.length === 1 ? actionItems[0].name : "vault-download.zip";
    const size =
      actionItems.length === 1 && actionItems[0].type === "document"
        ? actionItems[0].size_bytes || null
        : null;
    return downloadWithProgress({
      body: JSON.stringify({ items: actionItems.map(apiItem) }),
      headers: { "Content-Type": "application/json" },
      method: "POST",
      name: label,
      size,
      url: "/api/download",
    }).catch((err) => {
      setError(err.message || "Download failed.");
    });
  }

  function handleView(doc) {
    if (!doc) {
      return Promise.resolve();
    }
    return handleDownloadItems([docToItem(doc)]);
  }

  async function handleMove(docId, newPath) {
    if (!newPath) {
      return false;
    }
    const doc = docs.find((item) => item.id === docId);
    if (!doc) {
      setError("Document not found.");
      return false;
    }
    const targetInArchive = isArchivePath((newPath || "").trim());
    if (doc.archived && !targetInArchive) {
      setError("Restore this file before moving it out of Archive.");
      return false;
    }
    if (!doc.archived && targetInArchive) {
      setError("Use Move to Archive instead of dragging items into Archive.");
      return false;
    }
    const destinationFolder = newPath.split("/").slice(0, -1).join("/");
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("move", [docToItem(doc)], {
        destination_folder: destinationFolder,
      });
      if (payload.failed?.length) {
        throw new Error(payload.failed[0].detail || "Move failed");
      }
      await refreshAfterAction();
      return true;
    } catch (err) {
      setError(err.message || "Move failed.");
      return false;
    } finally {
      setBusy(false);
      setDraggingId(null);
      setDropHint(null);
    }
  }

  async function handleMoveSelection(items, destinationFolder) {
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("move", items, {
        destination_folder: destinationFolder || "",
      });
      const warning = successSummary("Move", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction();
      clearAllSelections();
      return true;
    } catch (err) {
      setError(err.message || "Move failed.");
      return false;
    } finally {
      setBusy(false);
      setDraggingId(null);
      setDraggingFolderPath(null);
      setDropHint(null);
    }
  }

  async function handleArchiveItems(items) {
    const confirmed = await requestConfirm({
      title: "Move to Archive",
      message: `Move ${selectionLabel((items || []).filter(Boolean))} to Archive?`,
      confirmLabel: "Move",
    });
    if (!confirmed) {
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("archive", items);
      const warning = successSummary("Archive", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction();
      clearAllSelections();
      return true;
    } catch (err) {
      setError(err.message || "Archive failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handleRestoreItems(items) {
    const confirmed = await requestConfirm({
      title: "Restore to Vault",
      message: `Restore ${selectionLabel((items || []).filter(Boolean))} back to Vault?`,
      confirmLabel: "Restore",
    });
    if (!confirmed) {
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("restore", items);
      const warning = successSummary("Restore", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction();
      clearAllSelections();
      return true;
    } catch (err) {
      setError(err.message || "Unarchive failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handleDeleteForeverItems(items) {
    const confirmed = await requestConfirm({
      title: "Delete forever",
      message: `Permanently delete ${selectionLabel((items || []).filter(Boolean))}? This cannot be undone.`,
      confirmLabel: "Delete forever",
      tone: "danger",
    });
    if (!confirmed) {
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("delete-forever", items);
      const warning = successSummary("Delete", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction(undefined, {
        missingFolderFallback: missingFolderFallbackForDelete(),
        suppressMissingFolderError: true,
      });
      clearAllSelections();
      return true;
    } catch (err) {
      setError(err.message || "Delete failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handleLockItems(items) {
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("lock", items);
      const warning = successSummary("Lock", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction();
      return true;
    } catch (err) {
      setError(err.message || "Could not lock the files.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function handleUnlockItems(items) {
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("unlock", items);
      const warning = successSummary("Unlock", payload);
      if (warning) {
        setError(warning);
      }
      await refreshAfterAction();
      return true;
    } catch (err) {
      setError(err.message || "Could not unlock the files.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  function docById(docId) {
    return docs.find((item) => item.id === docId) || selectedDoc;
  }

  async function handleArchive(docId) {
    const doc = docById(docId);
    if (!doc) {
      setError("Document not found.");
      return false;
    }
    return handleArchiveItems([docToItem(doc)]);
  }

  async function handleUnarchive(docId) {
    const doc = docById(docId);
    if (!doc) {
      setError("Document not found.");
      return false;
    }
    return handleRestoreItems([docToItem(doc)]);
  }

  async function handlePermanentDelete(docId) {
    const doc = docById(docId);
    if (!doc) {
      setError("Document not found.");
      return false;
    }
    return handleDeleteForeverItems([docToItem(doc)]);
  }

  return {
    handleArchive,
    handleArchiveItems,
    handleDeleteForeverItems,
    handleDownloadItems,
    handleDownloadSelection: handleDownloadItems,
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
  };
}
