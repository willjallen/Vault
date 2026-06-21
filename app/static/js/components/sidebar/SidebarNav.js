import { classNames, isArchivePath } from "../../lib/utils.js";

const h = React.createElement;

function folderName(path, fallback) {
  return path.split("/").filter(Boolean).slice(-1)[0] || fallback;
}

function directChildren(folderChildren, parentPath, predicate) {
  const children = Object.prototype.hasOwnProperty.call(folderChildren, parentPath)
    ? // eslint-disable-next-line security/detect-object-injection
      folderChildren[parentPath]
    : [];
  return children
    .filter(predicate)
    .map((path) => ({
      child: true,
      name: folderName(path, parentPath ? "Archive" : "Vault"),
      path,
    }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

function isActiveShortcut(path, currentFolder) {
  if (!path) {
    return !currentFolder;
  }
  if (path === "Archive") {
    return currentFolder === "Archive";
  }
  return currentFolder === path || currentFolder.startsWith(`${path}/`);
}

function SidebarFolderShortcut({
  item,
  currentFolder,
  dropHint,
  onSelect,
  onDropOnFolder,
  onClearDropHint,
}) {
  const isActive = isActiveShortcut(item.path, currentFolder || "");
  const isArchived = isArchivePath(item.path);
  const isDropTarget = dropHint === item.path || (!item.path && dropHint === "");

  return h(
    "button",
    {
      className: classNames(
        "folder-node",
        item.child ? "child-shortcut" : "",
        isActive ? "active" : "",
        isDropTarget ? "drop-target" : "",
        isArchived ? "archived" : ""
      ),
      type: "button",
      title: item.path || item.name,
      onClick: () => onSelect(item.path),
      onDragEnter: (e) => onDropOnFolder(item.path, e, true),
      onDragOver: (e) => e.preventDefault(),
      onDrop: (e) => onDropOnFolder(item.path, e, false),
      onDragLeave: (e) => {
        if (!e.currentTarget.contains(e.relatedTarget)) {
          onClearDropHint();
        }
      },
    },
    [
      h("span", { className: "folder-glyph" }, isArchived ? "🗂" : "📁"),
      h("span", { className: "folder-name" }, item.name),
    ]
  );
}

function SidebarFolderList({
  title,
  items,
  currentFolder,
  dropHint,
  onSelect,
  onDropOnFolder,
  onClearDropHint,
}) {
  return h("div", { className: "sidebar-section" }, [
    h("p", { className: "eyebrow tiny" }, title),
    h(
      "div",
      { className: "tree" },
      items.map((item) =>
        h(SidebarFolderShortcut, {
          key: item.path || item.name,
          item,
          currentFolder,
          dropHint,
          onSelect,
          onDropOnFolder,
          onClearDropHint,
        })
      )
    ),
  ]);
}

export function SidebarNav({
  currentFolder,
  folderChildren,
  dropHint,
  onSelect,
  onDropOnFolder,
  onClearDropHint,
}) {
  const vaultItems = [
    { name: "Vault", path: "" },
    ...directChildren(folderChildren || {}, "", (child) => !isArchivePath(child)),
  ];
  const archiveItems = [
    { name: "Archive", path: "Archive" },
    ...directChildren(folderChildren || {}, "Archive", (child) => isArchivePath(child)),
  ];

  return h("div", null, [
    h(SidebarFolderList, {
      title: "Folders",
      items: vaultItems,
      currentFolder: currentFolder || "",
      dropHint,
      onSelect,
      onDropOnFolder,
      onClearDropHint,
    }),
    h(SidebarFolderList, {
      title: "Archive",
      items: archiveItems,
      currentFolder: currentFolder || "",
      dropHint,
      onSelect,
      onDropOnFolder,
      onClearDropHint,
    }),
  ]);
}
