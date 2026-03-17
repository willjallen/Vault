import { folderNameFromPath } from "./utils.js";

function getDocFromDrag(dragEvent, docs) {
  const docId =
    dragEvent.dataTransfer.getData("application/x-doc-id") ||
    dragEvent.dataTransfer.getData("text/doc-id") ||
    dragEvent.dataTransfer.getData("text/plain");
  const parsed = parseInt(docId, 10);
  if (!parsed) {
    return null;
  }
  return docs.find((d) => d.id === parsed);
}

function getFolderFromDrag(dragEvent) {
  const path =
    dragEvent.dataTransfer.getData("application/x-folder-path") ||
    dragEvent.dataTransfer.getData("text/folder-path") ||
    "";
  return (path || "").trim();
}

function handleFolderDrop({
  target,
  targetFolder,
  draggedFolder,
  dropEvent,
  isPreview,
  folder,
  setDropHint,
  setUploadHover,
  setError,
  handleArchiveFolder,
  handleRenameFolder,
}) {
  const baseName = folderNameFromPath(draggedFolder);
  if (!baseName) {
    return;
  }
  const invalidTarget = target === draggedFolder || target.startsWith(`${draggedFolder}/`);
  if (invalidTarget) {
    setDropHint(null);
    setUploadHover(false);
    return;
  }
  const sourceArchived = draggedFolder.startsWith("Archive");
  const targetArchived = target.startsWith("Archive");
  if (target === "Archive" && !sourceArchived) {
    dropEvent.preventDefault();
    if (isPreview) {
      setDropHint(targetFolder);
      setUploadHover(false);
      return;
    }
    setDropHint(null);
    setUploadHover(false);
    handleArchiveFolder(draggedFolder, { navigate: false });
    return;
  }
  if (sourceArchived !== targetArchived) {
    if (!isPreview) {
      setError("Use Move to Archive/Restore to Vault for switching locations.");
    }
    setDropHint(null);
    setUploadHover(false);
    return;
  }
  const folderDestination = target ? `${target}/${baseName}` : baseName;
  if (folderDestination === draggedFolder) {
    setDropHint(null);
    setUploadHover(false);
    return;
  }
  dropEvent.preventDefault();
  if (isPreview) {
    setDropHint(targetFolder);
    return;
  }
  setDropHint(null);
  setUploadHover(false);
  handleRenameFolder(draggedFolder, folderDestination, { navigate: folder === draggedFolder });
}

function handleFileDrop({
  targetFolder,
  dropEvent,
  isPreview,
  setDropHint,
  setUploadHover,
  handleUpload,
}) {
  const file = dropEvent.dataTransfer.files[0];
  dropEvent.preventDefault();
  if (isPreview) {
    setDropHint(targetFolder);
    setUploadHover(true);
    return;
  }
  setDropHint(null);
  setUploadHover(false);
  handleUpload(file, targetFolder || "");
}

function handleDocDrop({
  target,
  targetFolder,
  doc,
  dropEvent,
  isPreview,
  setDropHint,
  handleArchive,
  handleMove,
}) {
  if (target === "Archive" && !doc.archived) {
    dropEvent.preventDefault();
    if (isPreview) {
      setDropHint(targetFolder);
      return;
    }
    setDropHint(null);
    handleArchive(doc.id);
    return;
  }
  dropEvent.preventDefault();
  if (isPreview) {
    setDropHint(targetFolder);
    return;
  }
  setDropHint(null);
  const docDestination = targetFolder ? `${targetFolder}/${doc.name}` : doc.name;
  if (docDestination === doc.path) {
    return;
  }
  handleMove(doc.id, docDestination);
}

export function createDropHandlers({
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
}) {
  function handleDropOnFolder(targetFolder, dropEvent, isPreview, clearOnly = false) {
    if (clearOnly) {
      setDropHint(null);
      setUploadHover(false);
      return;
    }
    const target = targetFolder || "";
    const draggedFolder = getFolderFromDrag(dropEvent);
    if (draggedFolder) {
      handleFolderDrop({
        target,
        targetFolder,
        draggedFolder,
        dropEvent,
        isPreview,
        folder,
        setDropHint,
        setUploadHover,
        setError,
        handleArchiveFolder,
        handleRenameFolder,
      });
      return;
    }
    const hasFiles =
      dropEvent.dataTransfer &&
      dropEvent.dataTransfer.files &&
      dropEvent.dataTransfer.files.length > 0;
    if (hasFiles) {
      handleFileDrop({
        targetFolder,
        dropEvent,
        isPreview,
        setDropHint,
        setUploadHover,
        handleUpload,
      });
      return;
    }
    const doc = getDocFromDrag(dropEvent, docs);
    if (!doc) {
      return;
    }
    handleDocDrop({
      target,
      targetFolder,
      doc,
      dropEvent,
      isPreview,
      setDropHint,
      handleArchive,
      handleMove,
    });
  }

  function handleCanvasDrop(canvasEvent) {
    canvasEvent.preventDefault();
    const draggedFolder = getFolderFromDrag(canvasEvent);
    const target = folder || "";
    if (draggedFolder) {
      handleFolderDrop({
        target,
        targetFolder: target,
        draggedFolder,
        dropEvent: canvasEvent,
        isPreview: false,
        folder,
        setDropHint,
        setUploadHover,
        setError,
        handleArchiveFolder,
        handleRenameFolder,
      });
      return;
    }
    const hasFiles = canvasEvent.dataTransfer.files && canvasEvent.dataTransfer.files.length > 0;
    if (hasFiles) {
      setDropHint(null);
      setUploadHover(false);
      handleUpload(canvasEvent.dataTransfer.files[0]);
      return;
    }
    const doc = getDocFromDrag(canvasEvent, docs);
    if (!doc) {
      return;
    }
    setDropHint(folder);
    const docDestination = folder ? `${folder}/${doc.name}` : doc.name;
    if (docDestination === doc.path) {
      return;
    }
    handleMove(doc.id, docDestination);
  }

  function handleCanvasDragOver(e) {
    const hasFiles = e.dataTransfer.types && Array.from(e.dataTransfer.types).includes("Files");
    const draggingFolder = Boolean(draggingFolderPath);
    if (hasFiles || draggingId || draggingFolder) {
      e.preventDefault();
      if (hasFiles) {
        setUploadHover(true);
      }
      if (draggingFolder) {
        setDropHint(folder);
      }
    }
  }

  function handleCanvasDragLeave(e) {
    if (!e.currentTarget.contains(e.relatedTarget)) {
      setUploadHover(false);
      setDropHint(null);
    }
  }

  function handleFileDragStart(e, docId) {
    e.dataTransfer.setData("application/x-doc-id", String(docId));
    e.dataTransfer.effectAllowed = "move";
    setDraggingId(docId);
  }

  function handleFileDragEnd() {
    setDraggingId(null);
    setDropHint(null);
  }

  function handleFolderDragStart(e, path) {
    if (!path) {
      return;
    }
    e.dataTransfer.setData("application/x-folder-path", path);
    e.dataTransfer.effectAllowed = "move";
    setDraggingFolderPath(path);
  }

  function handleFolderDragEnd() {
    setDraggingFolderPath(null);
    setDropHint(null);
    setUploadHover(false);
  }

  function clearDropState() {
    setDropHint(null);
    setUploadHover(false);
  }

  return {
    handleDropOnFolder,
    handleCanvasDrop,
    handleCanvasDragOver,
    handleCanvasDragLeave,
    handleFileDragStart,
    handleFileDragEnd,
    handleFolderDragStart,
    handleFolderDragEnd,
    clearDropState,
  };
}
