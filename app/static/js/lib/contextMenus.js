import { isArchiveRootPath } from "./utils.js";
import { canDeleteForeverItem } from "./siteSettings.js";

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
  const { doc, currentUser, busy } = actions;
  const lock = doc.lock || {};
  const lockedByOther = lock && lock.by && lock.by !== currentUser.id;
  const lockedByMe = lock && lock.by && lock.by === currentUser.id;
  return compactMenuItems([
    { label: "Download", action: () => actions.handleView(doc) },
    {
      label: "Replace",
      action: () => actions.handleVersionUploadClick(doc, { renameToUploadedName: !lockedByMe }),
      disabled: busy || doc.archived || lockedByOther,
    },
    actions.openFileDetails
      ? { label: "History", action: () => actions.openFileDetails(doc) }
      : null,
    { label: "Rename", action: () => actions.handleRenameFile(doc), disabled: busy },
    doc.favorite && actions.handleRemoveFavoriteItem
      ? {
          label: "Remove from Favorites",
          action: () => actions.handleRemoveFavoriteItem(doc),
          disabled: busy,
        }
      : null,
    { label: "Share", action: () => actions.handleShareItem(doc), disabled: busy },
    doc.archived
      ? null
      : {
          label: "Move...",
          action: () => actions.openMoveDialogForDoc(doc),
          disabled: busy || lockedByOther,
        },
    doc.archived
      ? { label: "Restore to Vault", action: () => actions.handleUnarchive(doc.id), disabled: busy }
      : { label: "Archive", action: () => actions.handleArchive(doc.id), disabled: busy },
    canDeleteForeverItem(doc, actions)
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
  const { folderItem, busy } = actions;
  const folderPath = folderItem.path || "";
  const hasPath = Boolean(folderPath);
  const isArchivedFolder = Boolean(folderItem.archived) || isArchiveRootPath(folderPath);
  const isRoot = !folderPath || folderPath === "Archive";
  const canMoveFolder = hasPath && !isRoot && !isArchivedFolder;
  return compactMenuItems([
    { label: "Open", action: () => actions.navigateToFolder(folderPath) },
    canMoveFolder
      ? { label: "Rename", action: () => actions.beginRenameFolder(folderPath), disabled: busy }
      : null,
    folderItem.favorite && actions.handleRemoveFavoriteItem
      ? {
          label: "Remove from Favorites",
          action: () => actions.handleRemoveFavoriteItem(folderItem),
          disabled: busy,
        }
      : null,
    { label: "Share", action: () => actions.handleShareItem(folderItem), disabled: busy },
    {
      label: "Properties",
      action: () => actions.openFolderProperties(folderItem),
      disabled: busy,
    },
    canMoveFolder
      ? {
          label: "Move...",
          action: () => actions.openMoveDialogForFolder(folderItem),
          disabled: busy,
        }
      : null,
    canMoveFolder
      ? {
          label: "Move to Archive",
          action: () => actions.handleArchiveFolder(folderPath, { navigate: false }),
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
      folderItem: item,
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
    noneArchived &&
    sameLocationScope &&
    selectedItems.every((item) => item.type === "folder" || !isLockedByOther(item, currentUser));
  const canLock = allDocs && docs.every((doc) => !doc.archived && !doc.lock?.by);
  const canUnlock = allDocs && docs.every((doc) => isLockedByMeOrAdmin(doc, currentUser, isAdmin));
  const canDelete =
    allArchived && noRoots && selectedItems.every((item) => canDeleteForeverItem(item, actions));

  return compactMenuItems([
    {
      label: "Download files",
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
          label: "Archive files",
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
  const inArchive = isArchiveRootPath(currentFolder);
  return [
    {
      label: "Upload file",
      action: actions.handleUploadClick,
      disabled: actions.busy || actions.uploading || inArchive,
    },
    {
      label: "New folder",
      action: () => actions.beginCreateFolder(currentFolder),
      disabled: actions.busy || actions.creatingFolder || inArchive,
    },
  ];
}
