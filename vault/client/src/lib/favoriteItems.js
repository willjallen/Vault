import { docToItem, folderToItem, keyForItem } from "./itemActions.js";
import { folderBaseName } from "./utils.js";

function positiveId(value) {
  const id = Number(value);
  return Number.isInteger(id) && id > 0 ? id : 0;
}

function cleanOptionalString(value) {
  return typeof value === "string" ? value.replace(/\\/g, "/").trim() : "";
}

export function favoriteItemKey(item) {
  if (!item) {
    return "";
  }
  const id = positiveId(item.id);
  if (!id) {
    return "";
  }
  return item.type === "document" ? `document:${id}` : `folder:${id}`;
}

export function itemToFavorite(item) {
  if (!item || (item.type !== "document" && item.type !== "folder")) {
    return null;
  }
  const id = positiveId(item.id);
  return id ? { type: item.type, id } : null;
}

function normalizeFavoriteItems(items) {
  const seen = new Set();
  const next = [];
  (items || []).forEach((item) => {
    const favorite = itemToFavorite(item);
    const key = favoriteItemKey(favorite);
    if (!favorite || !key || seen.has(key)) {
      return;
    }
    seen.add(key);
    next.push(favorite);
  });
  return next;
}

function normalizeResolvedFavoriteItems(items) {
  const seen = new Set();
  const next = [];
  (items || []).forEach((item) => {
    if (!item || (item.type !== "document" && item.type !== "folder")) {
      return;
    }
    const id = positiveId(item.id);
    if (!id) {
      return;
    }
    const favorite = { ...item, id, type: item.type };
    const key = favoriteItemKey(favorite);
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    next.push(favorite);
  });
  return next;
}

export function sameFavoriteItems(left, right) {
  return (
    JSON.stringify(normalizeFavoriteItems(left)) === JSON.stringify(normalizeFavoriteItems(right))
  );
}

export function mergeFavoriteItems(currentItems, incomingItems, options = {}) {
  const current = normalizeFavoriteItems(currentItems);
  const incoming = normalizeFavoriteItems(incomingItems);
  if (!incoming.length) {
    return current;
  }
  const incomingKeys = new Set(incoming.map(favoriteItemKey));
  const withoutIncoming = current.filter((item) => !incomingKeys.has(favoriteItemKey(item)));
  const beforeKey = options.beforeKey || "";
  const beforeIndex = beforeKey
    ? withoutIncoming.findIndex((item) => favoriteItemKey(item) === beforeKey)
    : -1;
  const boundedIndex = beforeIndex >= 0 ? beforeIndex : withoutIncoming.length;
  return [
    ...withoutIncoming.slice(0, boundedIndex),
    ...incoming,
    ...withoutIncoming.slice(boundedIndex),
  ];
}

export function removeFavoriteItem(currentItems, item) {
  const key = typeof item === "string" ? item : favoriteItemKey(item);
  return key
    ? normalizeFavoriteItems(currentItems).filter((favorite) => favoriteItemKey(favorite) !== key)
    : normalizeFavoriteItems(currentItems);
}

function documentFavoriteFallback(favorite) {
  const displayName =
    cleanOptionalString(favorite.name) || folderBaseName(favorite.path || "", "File");
  const folder = cleanOptionalString(favorite.folder);
  return docToItem({
    id: favorite.id,
    name: displayName,
    folder,
    path: cleanOptionalString(favorite.path) || (folder ? `${folder}/${displayName}` : displayName),
  });
}

function folderFavoriteFallback(favorite) {
  return folderToItem({
    ...favorite,
    favorite: true,
    name: cleanOptionalString(favorite.name) || folderBaseName(favorite.path || "", "Folder"),
    path: cleanOptionalString(favorite.path),
  });
}

export function favoriteItemsToSidebarItems(
  favoriteItems,
  { contentsItems = [], folderPaneItems = [] } = {}
) {
  const contentByKey = new Map(contentsItems.map((item) => [keyForItem(item), item]));
  const folderByKey = new Map(folderPaneItems.map((item) => [keyForItem(item), item]));
  return normalizeResolvedFavoriteItems(favoriteItems)
    .map((favorite) => {
      const key = favoriteItemKey(favorite);
      if (favorite.type === "document") {
        return { ...(contentByKey.get(key) || documentFavoriteFallback(favorite)), favorite: true };
      }
      const folderItem = folderByKey.get(key) || folderFavoriteFallback(favorite);
      return folderItem ? { ...folderItem, favorite: true } : null;
    })
    .filter(Boolean);
}
