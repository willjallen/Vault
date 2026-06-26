function positiveId(value) {
  const id = Number(value);
  return Number.isInteger(id) && id > 0 ? id : 0;
}

function sameContextTarget(left, right) {
  if (!left || !right || left.type !== right.type) {
    return false;
  }
  if (left.type === "document") {
    return positiveId(left.id) === positiveId(right.id);
  }
  const leftId = positiveId(left.id);
  const rightId = positiveId(right.id);
  if (leftId && rightId) {
    return leftId === rightId;
  }
  return String(left.path || "") === String(right.path || "");
}

export function markFavoriteContextTarget(selectedItems, targetItem) {
  if (!targetItem?.favorite) {
    return selectedItems;
  }
  return selectedItems.map((item) =>
    sameContextTarget(item, targetItem) ? { ...item, favorite: true } : item
  );
}
