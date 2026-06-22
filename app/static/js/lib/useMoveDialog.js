import { folderNameFromPath, isArchivePath } from "./utils.js";

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

  const movingTargetInArchive =
    moveTarget?.archived || isArchivePath(moveTarget?.path || moveTarget?.folder || "");

  function openMoveDialogForDoc(doc) {
    if (!doc) {
      return;
    }
    const baseFolder = doc.folder || (doc.archived ? "Archive" : "");
    setMoveTarget({
      type: "doc",
      id: doc.id,
      name: doc.name,
      path: doc.path,
      folder: doc.folder || "",
      archived: Boolean(doc.archived),
    });
    setMoveDestination(baseFolder || (doc.archived ? "Archive" : ""));
    setMoveNewFolderName("");
  }

  function openMoveDialogForFolder(folderItem) {
    if (!folderItem || !folderItem.path) {
      return;
    }
    const parentPath = folderItem.path.split("/").slice(0, -1).join("/");
    const inArchive = isArchivePath(folderItem.path);
    setMoveTarget({
      type: "folder",
      path: folderItem.path,
      name: folderItem.name || folderNameFromPath(folderItem.path),
      archived: inArchive,
    });
    setMoveDestination(parentPath || (inArchive ? "Archive" : ""));
    setMoveNewFolderName("");
  }

  function openMoveDialogForSelection(items) {
    if (!items || !items.length) {
      return;
    }
    const first = items[0];
    const archived = items.every((item) => item.archived);
    const baseFolder =
      first.type === "folder" ? first.path.split("/").slice(0, -1).join("/") : first.folder || "";
    setMoveTarget({
      type: "selection",
      items,
      name: `${items.length} items`,
      path: first.path || "",
      folder: baseFolder,
      archived,
    });
    setMoveDestination(baseFolder || (archived ? "Archive" : ""));
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
    if (!movingTargetInArchive && isArchivePath(normalized)) {
      return;
    }
    if (movingTargetInArchive && normalized && !isArchivePath(normalized)) {
      setMoveDestination(`Archive/${normalized}`);
      return;
    }
    if (movingTargetInArchive && !normalized) {
      setMoveDestination("Archive");
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
    const base = moveDestination || (movingTargetInArchive ? "Archive" : "");
    const newPath = base ? `${base}/${trimmed}` : trimmed;
    setCreatingMoveFolder(true);
    setError("");
    const form = new FormData();
    form.append("folder", newPath);
    try {
      const res = await apiFetch("/folders", { method: "POST", body: form });
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
    const destinationFolder = moveDestination || (movingTargetInArchive ? "Archive" : "");
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
