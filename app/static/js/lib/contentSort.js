import { keyForItem } from "./itemActions.js";

export const DEFAULT_CONTENTS_SORT = { key: "name", direction: "asc" };

const DESC_FIRST_SORT_KEYS = new Set(["date", "size"]);

export function nextContentsSort(current, key) {
  if (current.key === key) {
    return { key, direction: current.direction === "asc" ? "desc" : "asc" };
  }
  return { key, direction: DESC_FIRST_SORT_KEYS.has(key) ? "desc" : "asc" };
}

function parseSortDate(value) {
  const timestamp = Date.parse(value || "");
  return Number.isFinite(timestamp) ? timestamp : null;
}

function ttlSortValue(item) {
  if (item.type === "document") {
    return parseSortDate(item.expires_at);
  }
  const action = (item.default_ttl_action || "").toLowerCase();
  const days = Number(item.default_ttl_days);
  if (!["archive", "delete"].includes(action) || !Number.isFinite(days) || days < 1) {
    return null;
  }
  return days * (action === "delete" ? 1 : 2);
}

function sortValueForItem(item, key) {
  if (key === "date") {
    return parseSortDate(item.latest_updated_at);
  }
  if (key === "user") {
    return (item.latest_by || "").toLocaleLowerCase();
  }
  if (key === "size") {
    return Number(item.size_bytes || 0);
  }
  if (key === "ttl") {
    return ttlSortValue(item);
  }
  return (item.name || "").toLocaleLowerCase();
}

function isMissingSortValue(value) {
  return value === null || value === undefined || value === "";
}

function compareSortValues(a, b) {
  if (typeof a === "number" && typeof b === "number") {
    return a - b;
  }
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" });
}

export function compareContentsItems(a, b, sort) {
  const direction = sort.direction === "desc" ? -1 : 1;
  const aValue = sortValueForItem(a, sort.key);
  const bValue = sortValueForItem(b, sort.key);
  const aMissing = isMissingSortValue(aValue);
  const bMissing = isMissingSortValue(bValue);

  if (aMissing || bMissing) {
    if (!aMissing || !bMissing) {
      return aMissing ? 1 : -1;
    }
  }

  const primary = compareSortValues(aValue, bValue);
  if (primary !== 0) {
    return primary * direction;
  }
  const nameCompare = compareSortValues(sortValueForItem(a, "name"), sortValueForItem(b, "name"));
  if (nameCompare !== 0) {
    return nameCompare;
  }
  if (a.type !== b.type) {
    return a.type === "folder" ? -1 : 1;
  }
  return keyForItem(a).localeCompare(keyForItem(b), undefined, { numeric: true });
}
