import { folderToItem } from "./itemActions.js";
import {
  folderBaseName,
  folderParent,
  folderPathForName,
  isArchivedPath,
  normalizeFolderName,
} from "./utils.js";

export function createFolderActionHandlers({
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
}) {
  async function handleArchiveFolder(targetFolder = folder, options = {}) {
    const selectedFolder = typeof targetFolder === "string" ? targetFolder : folder || "";
    const shouldNavigate = options.navigate ?? selectedFolder === folder;
    if (!selectedFolder || isArchivedPath(selectedFolder)) {
      setError("Pick a Vault folder to move into Archive.");
      return;
    }
    const success = await handleArchiveItems([folderToItem({ path: selectedFolder })]);
    if (success) {
      if (shouldNavigate) {
        replaceFolder("Archive");
      }
    }
  }

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
    if (draft.mode === "renameFile") {
      await handleCommitInlineFile(draft, value);
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
    const targetPath = folderPathForName(draft.parent, trimmed);
    const success =
      draft.mode === "create"
        ? await handleCreateFolder(trimmed, draft.parent)
        : await handleRenameFolder(draft.path, targetPath, { navigate: false });
    if (success || targetPath === draft.path) {
      setInlineFolderDraft(null);
    }
  }

  function handleRenameFile(doc) {
    if (!doc) {
      setError("Document not found.");
      return;
    }
    const parentPath = doc.folder || "";
    if ((folder || "") !== parentPath) {
      replaceFolder(parentPath);
    }
    setSelectedId(doc.id);
    setInlineFolderDraft({
      docId: doc.id,
      mode: "renameFile",
      parent: parentPath,
      path: doc.path || (parentPath ? `${parentPath}/${doc.name}` : doc.name),
      value: doc.name || "",
    });
  }

  async function handleCommitInlineFile(draft, value) {
    const trimmed = (value || "").trim();
    if (!trimmed) {
      setError("File name is required.");
      return false;
    }
    if (trimmed.includes("/")) {
      setError("File name cannot contain slashes.");
      return false;
    }
    const targetPath = draft.parent ? `${draft.parent}/${trimmed}` : trimmed;
    if (targetPath === draft.path) {
      setInlineFolderDraft(null);
      return true;
    }
    setBusy(true);
    setError("");
    try {
      const payload = await postAction("rename", [{ type: "document", id: draft.docId }], {
        name: trimmed,
      });
      if (payload.failed?.length) {
        throw new Error(payload.failed[0].detail || "Rename failed");
      }
      await refreshAfterAction(draft.parent);
      setInlineFolderDraft(null);
      return true;
    } catch (err) {
      setError(err.message || "Rename failed.");
      return false;
    } finally {
      setBusy(false);
    }
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
        return false;
      }
      const trimmed = (next || "").trim().replace(/^\/+|\/+$/g, "");
      if (!trimmed) {
        setError("Folder name is required.");
        return false;
      }
      if (trimmed.includes("/")) {
        setError("Folder name cannot contain slashes.");
        return false;
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
    try {
      const payload = await postAction("rename", [folderToItem({ path: folderPath })], {
        destination_folder: folderParent(normalizedNewPath),
        name: folderBaseName(normalizedNewPath, "Folder"),
      });
      if (payload.failed?.length) {
        throw new Error(payload.failed[0].detail || "Rename failed");
      }
      const refreshTarget = shouldNavigate ? normalizedNewPath : folder;
      await refreshAfterAction(refreshTarget);
      if (shouldNavigate) {
        replaceFolder(normalizedNewPath);
      }
      return true;
    } catch (err) {
      setError(err.message || "Rename failed.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  return {
    beginCreateFolder,
    beginRenameFolder,
    handleArchiveFolder,
    handleCancelInlineFolder,
    handleCommitInlineFolder,
    handleCreateFolder,
    handleInlineFolderNameChange,
    handleRenameFile,
    handleRenameFolder,
  };
}
