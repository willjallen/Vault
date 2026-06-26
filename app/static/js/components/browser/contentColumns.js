import { readLocalPreference } from "../../lib/localPreferences.js";

export const COLUMN_WIDTH_STORAGE_KEY = "contentsColumnWidths";
export const COLUMN_RESIZE_HANDLES = {
  modified: { left: "modified", right: "user" },
  name: { left: "name", right: "modified" },
  size: { left: "size", right: "status" },
  status: { left: "status", right: "actions" },
  user: { left: "user", right: "size" },
};

const DEFAULT_COLUMN_WIDTHS = {
  actions: 162,
  modified: 210,
  size: 96,
  status: 236,
  user: 126,
};
const COLUMN_MIN_WIDTHS = {
  actions: 138,
  modified: 128,
  name: 196,
  size: 64,
  status: 152,
  user: 76,
};

export function readStoredColumnWidths() {
  const stored = readLocalPreference(COLUMN_WIDTH_STORAGE_KEY, null);
  if (!stored || typeof stored !== "object" || Array.isArray(stored)) {
    return { ...DEFAULT_COLUMN_WIDTHS };
  }
  return {
    actions: normalizedColumnWidth(
      stored.actions,
      DEFAULT_COLUMN_WIDTHS.actions,
      COLUMN_MIN_WIDTHS.actions
    ),
    modified: normalizedColumnWidth(
      stored.modified,
      DEFAULT_COLUMN_WIDTHS.modified,
      COLUMN_MIN_WIDTHS.modified
    ),
    size: normalizedColumnWidth(stored.size, DEFAULT_COLUMN_WIDTHS.size, COLUMN_MIN_WIDTHS.size),
    status: normalizedColumnWidth(
      stored.status,
      DEFAULT_COLUMN_WIDTHS.status,
      COLUMN_MIN_WIDTHS.status
    ),
    user: normalizedColumnWidth(stored.user, DEFAULT_COLUMN_WIDTHS.user, COLUMN_MIN_WIDTHS.user),
  };
}

export function contentColumnStyle(widths) {
  return {
    "--contents-actions-width": `${widths.actions}px`,
    "--contents-modified-width": `${widths.modified}px`,
    "--contents-size-width": `${widths.size}px`,
    "--contents-status-width": `${widths.status}px`,
    "--contents-user-width": `${widths.user}px`,
  };
}

export function measuredColumnWidths(header) {
  if (!header) {
    return {};
  }
  const widths = {};
  header.querySelectorAll("[data-column-key]").forEach((element) => {
    setMeasuredColumnWidth(
      widths,
      element.dataset.columnKey,
      element.getBoundingClientRect().width
    );
  });
  return widths;
}

export function columnWidthsForResize(drag, clientX) {
  const leftStart = resizeStartWidth(drag, drag.left);
  const rightStart = resizeStartWidth(drag, drag.right);
  const rawDelta = clientX - drag.startX;
  const minDelta = minimumColumnWidth(drag.left) - leftStart;
  const maxDelta = rightStart - minimumColumnWidth(drag.right);
  const delta = Math.max(minDelta, Math.min(maxDelta, rawDelta));
  const next = { ...drag.startColumnWidths };
  if (drag.left !== "name") {
    setStoredColumnWidth(next, drag.left, Math.round(leftStart + delta));
  }
  if (drag.right !== "name") {
    setStoredColumnWidth(next, drag.right, Math.round(rightStart - delta));
  }
  return next;
}

function normalizedColumnWidth(value, fallback, minWidth) {
  const width = Number(value);
  return Number.isFinite(width) ? Math.max(minWidth, Math.round(width)) : fallback;
}

function setMeasuredColumnWidth(widths, key, width) {
  if (key === "actions") {
    widths.actions = width;
  } else if (key === "modified") {
    widths.modified = width;
  } else if (key === "name") {
    widths.name = width;
  } else if (key === "size") {
    widths.size = width;
  } else if (key === "status") {
    widths.status = width;
  } else if (key === "user") {
    widths.user = width;
  }
}

function storedColumnWidth(widths, key) {
  if (key === "actions") {
    return widths.actions;
  }
  if (key === "modified") {
    return widths.modified;
  }
  if (key === "size") {
    return widths.size;
  }
  if (key === "status") {
    return widths.status;
  }
  if (key === "user") {
    return widths.user;
  }
  return 0;
}

function measuredColumnWidth(widths, key) {
  if (key === "actions") {
    return widths.actions;
  }
  if (key === "modified") {
    return widths.modified;
  }
  if (key === "name") {
    return widths.name;
  }
  if (key === "size") {
    return widths.size;
  }
  if (key === "status") {
    return widths.status;
  }
  if (key === "user") {
    return widths.user;
  }
  return 0;
}

function minimumColumnWidth(key) {
  if (key === "actions") {
    return COLUMN_MIN_WIDTHS.actions;
  }
  if (key === "modified") {
    return COLUMN_MIN_WIDTHS.modified;
  }
  if (key === "name") {
    return COLUMN_MIN_WIDTHS.name;
  }
  if (key === "size") {
    return COLUMN_MIN_WIDTHS.size;
  }
  if (key === "status") {
    return COLUMN_MIN_WIDTHS.status;
  }
  if (key === "user") {
    return COLUMN_MIN_WIDTHS.user;
  }
  return 0;
}

function defaultColumnWidth(key) {
  if (key === "actions") {
    return DEFAULT_COLUMN_WIDTHS.actions;
  }
  if (key === "modified") {
    return DEFAULT_COLUMN_WIDTHS.modified;
  }
  if (key === "size") {
    return DEFAULT_COLUMN_WIDTHS.size;
  }
  if (key === "status") {
    return DEFAULT_COLUMN_WIDTHS.status;
  }
  if (key === "user") {
    return DEFAULT_COLUMN_WIDTHS.user;
  }
  return minimumColumnWidth(key);
}

function resizeStartWidth(drag, key) {
  return (
    measuredColumnWidth(drag.startWidths, key) ||
    storedColumnWidth(drag.startColumnWidths, key) ||
    defaultColumnWidth(key)
  );
}

function setStoredColumnWidth(widths, key, width) {
  if (key === "actions") {
    widths.actions = width;
  } else if (key === "modified") {
    widths.modified = width;
  } else if (key === "size") {
    widths.size = width;
  } else if (key === "status") {
    widths.status = width;
  } else if (key === "user") {
    widths.user = width;
  }
}
