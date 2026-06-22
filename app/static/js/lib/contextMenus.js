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
          label: lockedByMe ? "Unlock file" : "Lock for editing",
          action: () => (lockedByMe ? actions.handleRelease(doc.id) : actions.handleLock(doc)),
          disabled: busy,
        }
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

export function buildFolderMenuItems(actions) {
  const { folderItem, busy, isAdmin } = actions;
  const folderPath = folderItem.path || "";
  const isArchivedFolder = folderPath.startsWith("Archive");
  const hasPath = Boolean(folderPath);
  const canPermanentDeleteFolder = isAdmin && isArchivedFolder && folderPath !== "Archive";
  return compactMenuItems([
    { label: "Open", action: () => actions.navigateToFolder(folderPath) },
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
