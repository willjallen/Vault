export function classNames(...args) {
  return args.filter(Boolean).join(" ");
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

export function triggerDownload(url) {
  const iframe = document.createElement("iframe");
  iframe.style.display = "none";
  iframe.src = url;
  document.body.appendChild(iframe);
  setTimeout(() => iframe.remove(), 4000);
}

export function formatDate(iso) {
  if (!iso) {
    return "Not updated yet";
  }
  const d = new Date(iso);
  return d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
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
