import { isArchivePath } from "./utils.js";

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

export function buildFileMenuItems(actions) {
  const { doc, currentUser, busy, isAdmin } = actions;
  const lock = doc.lock || {};
  const lockedByMe = lock && lock.by === currentUser.id;
  const lockedByOther = lock && lock.by && lock.by !== currentUser.id;
  return compactMenuItems([
    { label: "Download", action: () => actions.handleView(doc) },
    !doc.archived && !lockedByOther
      ? {
          label: lockedByMe ? "Upload" : "Replace",
          action: () =>
            actions.handleVersionUploadClick(doc, { renameToUploadedName: !lockedByMe }),
          disabled: busy,
        }
      : null,
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
          label: lockedByMe ? "Unlock file" : "Lock for editing",
          action: () => (lockedByMe ? actions.handleRelease(doc.id) : actions.handleLock(doc)),
          disabled: busy,
        }
      : null,
    isAdmin && doc.archived
      ? {
          label: "Delete forever",
          action: () => actions.handlePermanentDelete(doc.id),
          danger: true,
          disabled: busy,
        }
      : null,
  ]);
}

export function buildMyEditMenuItems(actions) {
  const { doc, busy } = actions;
  return compactMenuItems([
    { label: "Download", action: () => actions.handleView(doc) },
    {
      label: "Upload",
      action: () => actions.handleVersionUploadClick(doc),
      disabled: busy || doc.archived,
    },
    {
      label: "Unlock file",
      action: () => actions.handleRelease(doc.id),
      disabled: busy,
    },
  ]);
}

export function buildFolderMenuItems(actions) {
  const { folderItem, busy, isAdmin } = actions;
  const folderPath = folderItem.path || "";
  const isArchivedFolder = isArchivePath(folderPath);
  const hasPath = Boolean(folderPath);
  const isRoot = !folderPath || folderPath === "Archive";
  const canPermanentDeleteFolder = isAdmin && isArchivedFolder && folderPath !== "Archive";
  return compactMenuItems([
    { label: "Open", action: () => actions.navigateToFolder(folderPath) },
    {
      label: "Properties",
      action: () => actions.openFolderProperties(folderItem),
      disabled: busy,
    },
    hasPath && !isRoot
      ? { label: "Rename", action: () => actions.beginRenameFolder(folderPath), disabled: busy }
      : null,
    hasPath && !isRoot
      ? {
          label: "Move...",
          action: () => actions.openMoveDialogForFolder(folderItem),
          disabled: busy,
        }
      : null,
    hasPath && !isRoot
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

function isLockedByOther(doc, currentUser) {
  return doc.lock?.by && doc.lock.by !== currentUser.id;
}

function isLockedByMeOrAdmin(doc, currentUser, isAdmin) {
  return doc.lock?.by && (doc.lock.by === currentUser.id || isAdmin);
}

function isRootFolder(item) {
  return item.type === "folder" && (!item.path || item.path === "Archive");
}

export function buildSelectionMenuItems(actions) {
  const { selectedItems = [], busy, currentUser, isAdmin } = actions;
  if (selectedItems.length === 1) {
    const item = selectedItems[0];
    if (item.type === "document") {
      return buildFileMenuItems({ ...actions, doc: item });
    }
    return buildFolderMenuItems({
      ...actions,
      folderItem: { name: item.name, path: item.path },
    });
  }

  const docs = selectedItems.filter((item) => item.type === "document");
  const allDocs = docs.length === selectedItems.length;
  const noRoots = selectedItems.every((item) => !isRootFolder(item));
  const allArchived = selectedItems.every((item) => item.archived);
  const noneArchived = selectedItems.every((item) => !item.archived);
  const sameLocationScope = allArchived || noneArchived;
  const canMove =
    noRoots &&
    sameLocationScope &&
    selectedItems.every((item) => item.type === "folder" || !isLockedByOther(item, currentUser));
  const canLock = allDocs && docs.every((doc) => !doc.archived && !doc.lock?.by);
  const canUnlock = allDocs && docs.every((doc) => isLockedByMeOrAdmin(doc, currentUser, isAdmin));
  const canDelete = isAdmin && allArchived && noRoots;

  return compactMenuItems([
    {
      label: "Download",
      action: () => actions.handleDownloadSelection(selectedItems),
      disabled: busy || !noRoots,
    },
    {
      label: "Move...",
      action: () => actions.openMoveDialogForSelection(selectedItems),
      disabled: busy || !canMove,
    },
    noneArchived && noRoots
      ? {
          label: "Move to Archive",
          action: () => actions.handleArchiveItems(selectedItems),
          disabled: busy,
        }
      : null,
    allArchived && noRoots
      ? {
          label: "Restore to Vault",
          action: () => actions.handleRestoreItems(selectedItems),
          disabled: busy,
        }
      : null,
    canLock
      ? {
          label: "Lock files",
          action: () => actions.handleLockItems(selectedItems),
          disabled: busy,
        }
      : null,
    canUnlock
      ? {
          label: "Unlock files",
          action: () => actions.handleUnlockItems(selectedItems),
          disabled: busy,
        }
      : null,
    canDelete
      ? {
          label: "Delete forever",
          action: () => actions.handleDeleteForeverItems(selectedItems),
          danger: true,
          disabled: busy,
        }
      : null,
  ]);
}

export function buildPageMenuItems(actions) {
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
