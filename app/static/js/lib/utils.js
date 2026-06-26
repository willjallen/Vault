export function classNames(...args) {
  return args.filter(Boolean).join(" ");
}

export function formatBytes(bytes, options = {}) {
  const value = Number(bytes);
  const emptyForZero = options.emptyForZero ?? true;

  if (!Number.isFinite(value) || value < 0 || (emptyForZero && value === 0)) {
    return "";
  }

  let size = value;
  let unitIndex = 0;

  while (size >= 1024 && unitIndex < 4) {
    size /= 1024;
    unitIndex += 1;
  }

  let unit = "B";
  if (unitIndex === 1) {
    unit = "KB";
  } else if (unitIndex === 2) {
    unit = "MB";
  } else if (unitIndex === 3) {
    unit = "GB";
  } else if (unitIndex === 4) {
    unit = "TB";
  }

  return unitIndex === 0 ? `${size} ${unit}` : `${size.toFixed(1)} ${unit}`;
}

export function toBreadcrumbs(folder) {
  const safeFolder = folder || "";
  if (isArchivedPath(safeFolder)) {
    return [{ name: "Archive", path: "Archive" }];
  }

  const vaultCrumbs = [{ name: "Vault", path: "" }];
  if (!safeFolder) {
    return vaultCrumbs;
  }
  const vaultParts = safeFolder.split("/").filter(Boolean);
  vaultParts.forEach((part, idx) => {
    vaultCrumbs.push({ name: part, path: vaultParts.slice(0, idx + 1).join("/") });
  });
  return vaultCrumbs;
}

export function buildTree(childrenMap, nodePath = "") {
  const children = Object.prototype.hasOwnProperty.call(childrenMap, nodePath)
    ? // eslint-disable-next-line security/detect-object-injection
      childrenMap[nodePath]
    : [];
  return children.map((childPath) => ({
    name: childPath.split("/").filter(Boolean).slice(-1)[0] || "Vault",
    path: childPath,
    children: buildTree(childrenMap, childPath),
  }));
}

function sameLocalDay(a, b) {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

function formatClock(date) {
  return date
    .toLocaleTimeString(undefined, {
      hour: "numeric",
      minute: "2-digit",
    })
    .toLowerCase();
}

function formatSemanticDate(date, now) {
  if (sameLocalDay(date, now)) {
    return "Today";
  }
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  yesterday.setDate(yesterday.getDate() - 1);
  if (sameLocalDay(date, yesterday)) {
    return "Yesterday";
  }
  const options = {
    day: "numeric",
    month: "short",
  };
  if (date.getFullYear() !== now.getFullYear()) {
    options.year = "numeric";
  }
  return date.toLocaleDateString(undefined, options);
}

function ttlActionVerb(action) {
  const normalized = (action || "").toLowerCase();
  if (normalized === "delete") {
    return "Delete";
  }
  if (normalized === "archive") {
    return "Archive";
  }
  return "";
}

function formatDurationLong(value, unit) {
  return `${value} ${unit}${value === 1 ? "" : "s"}`;
}

function formatDurationCompact(value, unit) {
  const suffix = unit === "minute" ? "m" : unit === "hour" ? "h" : "d";
  return `${value}${suffix}`;
}

function ttlDisplayLabels(verb, relation, value, unit) {
  const compact = formatDurationCompact(value, unit);
  return {
    compact,
    full: `${verb} ${relation} ${formatDurationLong(value, unit)}`,
    medium: `${verb} ${compact}`,
  };
}

export function formatDate(iso, fallback = "Not updated yet", now = new Date()) {
  if (!iso) {
    return fallback;
  }
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) {
    return fallback;
  }
  return `${formatSemanticDate(d, now)} at ${formatClock(d)}`;
}

export function retentionPolicyLabel(action, days) {
  const verb = ttlActionVerb(action);
  const ttlDays = Number(days);
  if (!verb || !Number.isFinite(ttlDays) || ttlDays < 1) {
    return "";
  }
  return `${verb} after ${formatDurationLong(ttlDays, "day")}`;
}

export function retentionPolicyStatusLabels(action, days) {
  const verb = ttlActionVerb(action);
  const ttlDays = Number(days);
  if (!verb || !Number.isFinite(ttlDays) || ttlDays < 1) {
    return null;
  }
  return ttlDisplayLabels(verb, "after", ttlDays, "day");
}

export function expiryStatusLabel(expiresAt, action) {
  const verb = ttlActionVerb(action);
  const expires = new Date(expiresAt || "");
  if (!expiresAt || Number.isNaN(expires.getTime()) || !verb) {
    return "";
  }
  const remainingMs = expires.getTime() - Date.now();
  if (remainingMs <= 0) {
    return `${verb} due`;
  }
  const minutes = Math.ceil(remainingMs / 60000);
  if (minutes < 60) {
    return `${verb} in ${formatDurationLong(minutes, "minute")}`;
  }
  const hours = Math.ceil(remainingMs / 3600000);
  if (hours < 48) {
    return `${verb} in ${formatDurationLong(hours, "hour")}`;
  }
  return `${verb} in ${formatDurationLong(Math.ceil(remainingMs / 86400000), "day")}`;
}

export function expiryStatusLabels(expiresAt, action) {
  const verb = ttlActionVerb(action);
  const expires = new Date(expiresAt || "");
  if (!expiresAt || Number.isNaN(expires.getTime()) || !verb) {
    return null;
  }
  const remainingMs = expires.getTime() - Date.now();
  if (remainingMs <= 0) {
    return {
      compact: "due",
      full: `${verb} due`,
      medium: `${verb} due`,
    };
  }
  const minutes = Math.ceil(remainingMs / 60000);
  if (minutes < 60) {
    return ttlDisplayLabels(verb, "in", minutes, "minute");
  }
  const hours = Math.ceil(remainingMs / 3600000);
  if (hours < 48) {
    return ttlDisplayLabels(verb, "in", hours, "hour");
  }
  return ttlDisplayLabels(verb, "in", Math.ceil(remainingMs / 86400000), "day");
}

const ARCHIVE_ROOT_PATH = "Archive";

export function isArchiveRootPath(path) {
  return path === ARCHIVE_ROOT_PATH;
}

export function isArchivedPath(path) {
  return (
    typeof path === "string" &&
    (isArchiveRootPath(path) || path.startsWith(`${ARCHIVE_ROOT_PATH}/`))
  );
}

export function folderNameFromPath(path) {
  return (path || "").split("/").filter(Boolean).slice(-1)[0] || "";
}

export function folderParts(path) {
  return (path || "").split("/").filter(Boolean);
}

export function folderParent(path) {
  return folderParts(path).slice(0, -1).join("/");
}

export function folderBaseName(path, fallback = "New Folder") {
  return folderParts(path).slice(-1)[0] || fallback;
}

export function normalizeFolderName(value) {
  return (value || "").trim().replace(/^\/+|\/+$/g, "");
}

export function folderPathForName(parentPath, folderName) {
  return parentPath ? `${parentPath}/${folderName}` : folderName;
}
