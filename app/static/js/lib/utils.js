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
  const inArchive = isArchivePath(safeFolder);
  if (!inArchive) {
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

  const archiveCrumbs = [{ name: "Archive", path: "Archive" }];
  const archivePath = safeFolder.replace(/^Archive\/?/, "");
  if (!archivePath) {
    return archiveCrumbs;
  }
  const archiveParts = archivePath.split("/").filter(Boolean);
  let current = "Archive";
  archiveParts.forEach((part) => {
    current = `${current}/${part}`;
    archiveCrumbs.push({ name: part, path: current });
  });
  return archiveCrumbs;
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

export function formatDate(iso, fallback = "Not updated yet") {
  if (!iso) {
    return fallback;
  }
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) {
    return fallback;
  }
  const date = d.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
  });
  const time = d
    .toLocaleTimeString(undefined, {
      hour: "numeric",
      minute: "2-digit",
    })
    .toLowerCase();
  return `${date} at ${time}`;
}

export function retentionPolicyLabel(action, days) {
  const normalized = (action || "").toLowerCase();
  const ttlDays = Number(days);
  if (!Number.isFinite(ttlDays) || ttlDays < 1 || !["archive", "delete"].includes(normalized)) {
    return "";
  }
  const verb = normalized === "delete" ? "Delete" : "Archive";
  return `${verb} after ${ttlDays}d`;
}

export function expiryStatusLabel(expiresAt, action) {
  const normalized = (action || "").toLowerCase();
  const expires = new Date(expiresAt || "");
  if (
    !expiresAt ||
    Number.isNaN(expires.getTime()) ||
    !["archive", "delete"].includes(normalized)
  ) {
    return "";
  }
  const verb = normalized === "delete" ? "Delete" : "Archive";
  const remainingMs = expires.getTime() - Date.now();
  if (remainingMs <= 0) {
    return `${verb} due`;
  }
  const minutes = Math.ceil(remainingMs / 60000);
  if (minutes < 60) {
    return `${verb} in ${minutes}m`;
  }
  const hours = Math.ceil(remainingMs / 3600000);
  if (hours < 48) {
    return `${verb} in ${hours}h`;
  }
  return `${verb} in ${Math.ceil(remainingMs / 86400000)}d`;
}

export function isArchivePath(path) {
  return typeof path === "string" && (path === "Archive" || path.startsWith("Archive/"));
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
