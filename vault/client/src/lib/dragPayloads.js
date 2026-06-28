export function dragTypes(dragEvent) {
  return Array.from(dragEvent.dataTransfer?.types || []);
}

export function dragHasFiles(dragEvent) {
  return dragTypes(dragEvent).includes("Files");
}

export function dragHasVaultItems(dragEvent) {
  const types = dragTypes(dragEvent);
  return (
    types.includes("application/x-vault-selection") ||
    types.includes("application/x-doc-id") ||
    types.includes("application/x-folder-path") ||
    types.includes("text/doc-id") ||
    types.includes("text/folder-path")
  );
}

export function dragHasFavoriteItems(dragEvent) {
  const types = dragTypes(dragEvent);
  return (
    types.includes("application/x-vault-favorite-selection") ||
    types.includes("application/x-vault-selection") ||
    types.includes("application/x-doc-id") ||
    types.includes("application/x-folder-path") ||
    types.includes("text/doc-id") ||
    types.includes("text/folder-path")
  );
}

export function dragCanUseVaultDropZones(dragEvent) {
  return dragHasFiles(dragEvent) || dragHasVaultItems(dragEvent);
}

export function selectionItemsFromDrag(dragEvent) {
  const rawSelection = dragEvent.dataTransfer.getData("application/x-vault-selection");
  if (!rawSelection) {
    return [];
  }
  try {
    const parsed = JSON.parse(rawSelection);
    return Array.isArray(parsed.items) ? parsed.items : [];
  } catch (_err) {
    return [];
  }
}

export function favoriteItemsFromDrag(dragEvent) {
  const items = [];
  const directPath =
    dragEvent.dataTransfer.getData("application/x-folder-path") ||
    dragEvent.dataTransfer.getData("text/folder-path") ||
    "";
  const directDocId =
    dragEvent.dataTransfer.getData("application/x-doc-id") ||
    dragEvent.dataTransfer.getData("text/doc-id") ||
    "";
  if (directPath) {
    items.push({ type: "folder", path: directPath });
  }
  const parsedDocId = Number.parseInt(directDocId, 10);
  if (parsedDocId > 0) {
    items.push({ type: "document", id: parsedDocId });
  }
  selectionItemsFromDrag(dragEvent).forEach((item) => {
    if (item?.type === "folder" && item.id) {
      items.push({ type: "folder", id: item.id });
    } else if (item?.type === "document" && item.id) {
      items.push({
        type: "document",
        id: item.id,
        name: item.name || "",
        folder: item.folder || "",
        path: item.path || "",
      });
    }
  });
  return items;
}
