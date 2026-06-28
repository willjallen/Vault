import { createFolderRequestOptions } from "./folderRequests.js";
import { folderNameFromPath, isArchivedPath, isArchiveRootPath } from "./utils.js";

const { useState } = React;

export function useMoveDialog({
  folder,
  handleMove,
  handleMoveSelection,
  handleRenameFolder,
  apiFetch,
  refresh,
  setError,
  setSelectedId,
}) {
  const [moveTarget, setMoveTarget] = useState(null);
  const [moveDestination, setMoveDestination] = useState("");
  const [moveNewFolderName, setMoveNewFolderName] = useState("");
  const [creatingMoveFolder, setCreatingMoveFolder] = useState(false);

  function openMoveDialogForDoc(doc) {
    if (!doc) {
      return;
    }
    if (doc.archived) {
      setError("Restore this file before moving it.");
      return;
    }
    const baseFolder = doc.folder || "";
    setMoveTarget({
      type: "doc",
      id: doc.id,
      name: doc.name,
      path: doc.path,
      folder: doc.folder || "",
      archived: false,
    });
    setMoveDestination(baseFolder);
    setMoveNewFolderName("");
  }

  function openMoveDialogForFolder(folderItem) {
    if (!folderItem || !folderItem.path) {
      return;
    }
    const parentPath = folderItem.path.split("/").slice(0, -1).join("/");
    const inArchive = isArchiveRootPath(folderItem.path);
    if (inArchive) {
      setError("Archive folders cannot be moved.");
      return;
    }
    setMoveTarget({
      type: "folder",
      path: folderItem.path,
      name: folderItem.name || folderNameFromPath(folderItem.path),
      archived: false,
    });
    setMoveDestination(parentPath);
    setMoveNewFolderName("");
  }

  function openMoveDialogForSelection(items) {
    if (!items || !items.length) {
      return;
    }
    const first = items[0];
    if (items.some((item) => item.archived)) {
      setError("Restore archived files before moving them.");
      return;
    }
    const baseFolder =
      first.type === "folder" ? first.path.split("/").slice(0, -1).join("/") : first.folder || "";
    setMoveTarget({
      type: "selection",
      items,
      name: `${items.length} items`,
      path: first.path || "",
      folder: baseFolder,
      archived: false,
    });
    setMoveDestination(baseFolder);
    setMoveNewFolderName("");
  }

  function closeMoveDialog() {
    setMoveTarget(null);
    setMoveDestination("");
    setMoveNewFolderName("");
    setCreatingMoveFolder(false);
  }

  function setMoveDestinationSafe(nextPath) {
    if (!moveTarget) {
      return;
    }
    const normalized = (nextPath || "").replace(/^\/+|\/+$/g, "");
    if (isArchivedPath(normalized)) {
      return;
    }
    setMoveDestination(normalized);
  }

  async function handleCreateMoveFolder() {
    if (!moveTarget) {
      return;
    }
    const trimmed = (moveNewFolderName || "").trim().replace(/^\/+|\/+$/g, "");
    if (!trimmed) {
      setError("Folder name is required.");
      return;
    }
    if (trimmed.includes("/")) {
      setError("Folder name cannot contain slashes.");
      return;
    }
    const base = moveDestination || "";
    const newPath = base ? `${base}/${trimmed}` : trimmed;
    setCreatingMoveFolder(true);
    setError("");
    try {
      const res = await apiFetch("/folders", createFolderRequestOptions(newPath));
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Could not create folder");
      }
      await refresh(base, { invalidateContents: true, sidebar: true });
      setMoveDestination(newPath);
      setMoveNewFolderName("");
    } catch (err) {
      setError(err.message || "Could not create folder");
    } finally {
      setCreatingMoveFolder(false);
    }
  }

  async function handleConfirmMoveTarget() {
    if (!moveTarget) {
      return;
    }
    const destinationFolder = moveDestination || "";
    const targetName =
      moveTarget.type === "selection"
        ? ""
        : moveTarget.type === "folder"
          ? folderNameFromPath(moveTarget.path)
          : moveTarget.name || folderNameFromPath(moveTarget.path);
    const desiredPath = targetName
      ? destinationFolder
        ? `${destinationFolder}/${targetName}`
        : targetName
      : destinationFolder;
    if (moveTarget.type === "selection") {
      const selectionMoved = await handleMoveSelection(moveTarget.items, destinationFolder);
      if (selectionMoved) {
        closeMoveDialog();
      }
      return;
    }
    if (moveTarget.type === "folder") {
      const normalizedTarget = moveTarget.path || "";
      if (desiredPath === normalizedTarget) {
        setError("Pick a different destination for this folder.");
        return;
      }
      if (desiredPath.startsWith(`${normalizedTarget}/`)) {
        setError("Cannot move a folder into itself.");
        return;
      }
      const shouldNavigate =
        folder === normalizedTarget || (folder || "").startsWith(`${normalizedTarget}/`);
      const renamed = await handleRenameFolder(normalizedTarget, desiredPath, {
        navigate: shouldNavigate,
      });
      if (renamed) {
        closeMoveDialog();
      }
      return;
    }
    const moved = await handleMove(moveTarget.id, desiredPath);
    if (moved) {
      closeMoveDialog();
      setSelectedId(moveTarget.id);
    }
  }

  return {
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
  };
}
