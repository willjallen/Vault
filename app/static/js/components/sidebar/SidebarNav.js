import { classNames, isArchivePath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

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

function folderShortcutIcon(item, archived) {
  if (item.icon) {
    return item.icon;
  }
  if (!item.path) {
    return "house";
  }
  return archived ? "box-archive" : "folder";
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
  selected,
  onSelect,
  onSelectItem,
  onContextMenu,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
  onDropOnFolder,
  onClearDropHint,
}) {
  const isActive = isActiveShortcut(item.path, currentFolder || "");
  const isArchived = isArchivePath(item.path);
  const isDropTarget = dropHint === item.path || (!item.path && dropHint === "");
  const icon = folderShortcutIcon(item, isArchived);

  return h(
    "button",
    {
      className: classNames(
        "folder-node",
        item.child ? "child-shortcut" : "",
        selected ? "selected" : "",
        isActive ? "active" : "",
        isDropTarget ? "drop-target" : "",
        draggingFolderPath === item.path ? "dragging" : "",
        isArchived ? "archived" : ""
      ),
      type: "button",
      draggable: Boolean(item.path && item.path !== "Archive"),
      title: item.path || item.name,
      onClick: (e) => onSelectItem && onSelectItem(item, e),
      onContextMenu: (e) => onContextMenu && onContextMenu(e, { ...item, sourcePane: "folders" }),
      onDragStart: (e) => onFolderDragStart && onFolderDragStart(e, item.path),
      onDragEnd: onFolderDragEnd,
      onKeyDown: (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          onSelect(item.path);
        }
      },
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
      h(
        "span",
        {
          className: classNames(
            "folder-glyph",
            item.color ? `folder-color-${item.color}` : "",
            isArchived ? "archived-text" : ""
          ),
        },
        h(Icon, { icon, size: 15 })
      ),
      h("span", { className: "folder-name" }, item.name),
    ]
  );
}

function SidebarFolderList({
  title,
  items,
  orderedItems,
  currentFolder,
  dropHint,
  selectedKeys,
  onSelect,
  onSelectItem,
  onContextMenu,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
  onDropOnFolder,
  onClearDropHint,
}) {
  const selectedSet = new Set(selectedKeys || []);
  const paneItems = orderedItems || items;
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
          selected: selectedSet.has(`folder:${item.path || ""}`),
          onSelect,
          onSelectItem: (selectedItem, e) =>
            onSelectItem && onSelectItem(selectedItem, e, paneItems),
          onContextMenu,
          onFolderDragStart: (e, path) => {
            const key = `folder:${path || ""}`;
            const dragItems = selectedSet.has(key)
              ? paneItems.filter((folderItem) => selectedSet.has(`folder:${folderItem.path || ""}`))
              : [item];
            if (onFolderDragStart) {
              onFolderDragStart(e, path, dragItems);
            }
          },
          onFolderDragEnd,
          draggingFolderPath,
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
  folderItems,
  selectedKeys = [],
  dropHint,
  onSelect,
  onSelectItem,
  onContextMenu,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
  onDropOnFolder,
  onClearDropHint,
}) {
  const allItems = folderItems || [
    { name: "Vault", path: "" },
    ...directChildren(folderChildren || {}, "", (child) => !isArchivePath(child)),
    { name: "Archive", path: "Archive" },
    ...directChildren(folderChildren || {}, "Archive", (child) => isArchivePath(child)),
  ];
  const vaultItems = allItems.filter(
    (item) => item.path !== "Archive" && !isArchivePath(item.path)
  );
  const archiveItems = allItems.filter(
    (item) => item.path === "Archive" || isArchivePath(item.path)
  );

  return h("div", null, [
    h(SidebarFolderList, {
      title: "Folders",
      items: vaultItems,
      orderedItems: allItems,
      currentFolder: currentFolder || "",
      dropHint,
      selectedKeys,
      onSelect,
      onSelectItem,
      onContextMenu,
      onFolderDragStart,
      onFolderDragEnd,
      draggingFolderPath,
      onDropOnFolder,
      onClearDropHint,
    }),
    h(SidebarFolderList, {
      title: "Archive",
      items: archiveItems,
      orderedItems: allItems,
      currentFolder: currentFolder || "",
      dropHint,
      selectedKeys,
      onSelect,
      onSelectItem,
      onContextMenu,
      onFolderDragStart,
      onFolderDragEnd,
      draggingFolderPath,
      onDropOnFolder,
      onClearDropHint,
    }),
  ]);
}
