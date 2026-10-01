import { normalizeFolderName } from "./utils.js";

export const DEFAULT_CONTENTS_COLUMN_ORDER = Object.freeze(["name", "modified", "user", "size"]);

export function isContentsColumnOrder(value) {
  return (
    Array.isArray(value) &&
    value.length === DEFAULT_CONTENTS_COLUMN_ORDER.length &&
    new Set(value).size === value.length &&
    value.every((key) => DEFAULT_CONTENTS_COLUMN_ORDER.includes(key))
  );
}

export function normalizeContentsColumnOrder(value) {
  return [...(isContentsColumnOrder(value) ? value : DEFAULT_CONTENTS_COLUMN_ORDER)];
}

export function normalizeContentsColumnOrderByFolder(value) {
  const source = value && typeof value === "object" && !Array.isArray(value) ? value : {};
  return Object.fromEntries(
    Object.entries(source)
      .filter(([, order]) => isContentsColumnOrder(order))
      .map(([folder, order]) => [normalizeFolderName(folder), [...order]])
  );
}

export function contentsColumnOrderForFolder(value, folder) {
  const orders = new Map(Object.entries(normalizeContentsColumnOrderByFolder(value)));
  return normalizeContentsColumnOrder(orders.get(normalizeFolderName(folder)));
}

export function setContentsColumnOrderForFolder(value, folder, order) {
  return {
    ...normalizeContentsColumnOrderByFolder(value),
    [normalizeFolderName(folder)]: normalizeContentsColumnOrder(order),
  };
}

export function moveContentsColumn(value, source, target, after = false) {
  const order = normalizeContentsColumnOrder(value);
  if (source === target || !order.includes(source) || !order.includes(target)) {
    return order;
  }
  const remaining = order.filter((key) => key !== source);
  remaining.splice(remaining.indexOf(target) + (after ? 1 : 0), 0, source);
  return remaining;
}
