import { classNames, isArchivePath } from "../../lib/utils.js";
import { favoriteItemKey } from "../../lib/favoriteItems.js";
import { keyForItem } from "../../lib/itemActions.js";
import { MIN_SIDEBAR_SECTION_SIZE, normalizeSidebarSectionSizes } from "../../lib/theme.js";
import { Icon } from "../common/Icon.js";
import { MyEdits } from "./MyEdits.js";

const h = React.createElement;
const { useEffect, useRef, useState } = React;

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

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

function isFolderDropTarget({ activeDropTarget, dropFolder, dropHint, path }) {
  return (
    dropHint === path ||
    (!path && dropHint === "") ||
    (activeDropTarget?.kind === "folder" && activeDropTarget.folder === dropFolder)
  );
}

function folderDropAttributes({ disabled, dropFolder, isDropTarget }) {
  if (disabled) {
    return {};
  }
  return {
    "data-vault-drop-kind": "folder",
    "data-drop-folder": dropFolder,
    "data-drop-label": "Move here",
    "data-drop-active": isDropTarget ? "true" : undefined,
  };
}

function folderDropHandlers({ disabled, dropFolder, onClearDropHint, onDropOnFolder }) {
  if (disabled) {
    return {};
  }
  return {
    onDragEnter: (e) => {
      e.stopPropagation();
      onDropOnFolder(dropFolder, e, true);
    },
    onDragOver: (e) => {
      e.preventDefault();
      e.stopPropagation();
    },
    onDrop: (e) => {
      e.stopPropagation();
      onDropOnFolder(dropFolder, e, false);
    },
    onDragLeave: (e) => {
      if (!e.currentTarget.contains(e.relatedTarget)) {
        onClearDropHint();
      }
    },
  };
}

function SidebarFolderShortcut({
  item,
  currentFolder,
  dropHint,
  activeDropTarget,
  selected,
  disableFolderDrops = false,
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
  const dropFolder = item.path || "";
  const isDropTarget = isFolderDropTarget({
    activeDropTarget,
    dropFolder,
    dropHint,
    path: item.path,
  });
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
        isArchived ? "archived" : "",
        item.favorite ? "favorite" : ""
      ),
      type: "button",
      draggable: Boolean(item.path && item.path !== "Archive"),
      title: item.path || item.name,
      ...folderDropAttributes({
        disabled: disableFolderDrops,
        dropFolder,
        isDropTarget,
      }),
      onClick: (e) => onSelectItem && onSelectItem(item, e),
      onDoubleClick: () => onSelect(item.path),
      onContextMenu: (e) =>
        onContextMenu && onContextMenu(e, { ...item, sourcePane: item.sourcePane || "folders" }),
      onDragStart: (e) => onFolderDragStart && onFolderDragStart(e, item.path),
      onDragEnd: onFolderDragEnd,
      onKeyDown: (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          onSelect(item.path);
        }
      },
      ...folderDropHandlers({
        disabled: disableFolderDrops,
        dropFolder,
        onClearDropHint,
        onDropOnFolder,
      }),
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
  className = "",
  style,
  emptyContent,
  sectionDragProps = {},
  dropActive = false,
  dropAvailable = false,
  disableFolderDrops = false,
  currentFolder,
  dropHint,
  activeDropTarget,
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
  return h("div", { className: classNames("sidebar-section", className), style }, [
    h("p", { className: "eyebrow tiny" }, title),
    h(
      "div",
      {
        className: classNames(
          "sidebar-section-body",
          dropAvailable ? "drop-available" : "",
          dropActive ? "drop-active" : ""
        ),
        ...sectionDragProps,
      },
      h(
        "div",
        { className: "tree" },
        items.length
          ? items.map((item) =>
              h(SidebarFolderShortcut, {
                key: item.path || item.name,
                item,
                currentFolder,
                dropHint,
                activeDropTarget,
                selected: selectedSet.has(keyForItem(item)),
                disableFolderDrops,
                onSelect,
                onSelectItem: (selectedItem, e) =>
                  onSelectItem && onSelectItem(selectedItem, e, paneItems),
                onContextMenu,
                onFolderDragStart: (e, path) => {
                  const key = keyForItem(item);
                  const dragItems = selectedSet.has(key)
                    ? paneItems.filter((folderItem) => selectedSet.has(keyForItem(folderItem)))
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
          : h("div", { className: "sidebar-empty" }, emptyContent || "Empty")
      )
    ),
  ]);
}

function SidebarFavoriteShortcut({
  item,
  selected,
  active,
  insertAfter = false,
  insertBefore = false,
  onSelect,
  onSelectItem,
  onSelectFavoriteDocument,
  onContextMenu,
  onFavoriteFileContextMenu,
  onFileDragStart,
  onFileDragEnd,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
}) {
  const isDocument = item.type === "document";
  const isArchived = isArchivePath(item.path || item.folder || "");
  const title = item.path || item.name;
  const icon = isDocument ? "file" : folderShortcutIcon(item, isArchived);

  return h(
    "button",
    {
      className: classNames(
        "folder-node",
        "favorite",
        isDocument ? "favorite-file" : "",
        insertBefore ? "favorite-insert-before" : "",
        insertAfter ? "favorite-insert-after" : "",
        selected ? "selected" : "",
        active ? "active" : "",
        draggingFolderPath === item.path ? "dragging" : "",
        isArchived ? "archived" : ""
      ),
      "data-favorite-key": favoriteItemKey(item),
      type: "button",
      draggable: Boolean(isDocument ? item.id : item.path && item.path !== "Archive"),
      title,
      onClick: (e) => {
        if (isDocument) {
          onSelectFavoriteDocument?.(item, e);
        } else if (onSelectItem) {
          onSelectItem(item, e);
        }
      },
      onDoubleClick: () => {
        if (!isDocument) {
          onSelect(item.path || "");
        }
      },
      onContextMenu: (e) => {
        if (isDocument) {
          onFavoriteFileContextMenu?.(e, { ...item, sourcePane: "favorites" });
          return;
        }
        onContextMenu?.(e, { ...item, sourcePane: "favorites" });
      },
      onDragStart: (e) => {
        if (isDocument) {
          onFileDragStart?.(e, item.id, [item]);
          return;
        }
        onFolderDragStart?.(e, item.path, [item]);
      },
      onDragEnd: isDocument ? onFileDragEnd : onFolderDragEnd,
      onKeyDown: (e) => {
        if (e.key !== "Enter") {
          return;
        }
        e.preventDefault();
        if (isDocument) {
          onSelectFavoriteDocument?.(item, e);
        } else {
          onSelect(item.path || "");
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

function SidebarFavoriteList({
  title,
  items,
  className = "",
  style,
  sectionDragProps = {},
  dropActive = false,
  dropAvailable = false,
  currentFolder,
  activeDropTarget,
  selectedKeys,
  selectedId,
  onSelect,
  onSelectItem,
  onSelectFavoriteDocument,
  onContextMenu,
  onFavoriteFileContextMenu,
  onFileDragStart,
  onFileDragEnd,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
}) {
  const selectedSet = new Set(selectedKeys || []);
  const beforeKey =
    activeDropTarget?.kind === "favorites" ? activeDropTarget.beforeKey || "" : null;
  const rows = [];
  items.forEach((item, index) => {
    const key = favoriteItemKey(item);
    rows.push(
      h(SidebarFavoriteShortcut, {
        key,
        item,
        insertAfter: beforeKey === "" && index === items.length - 1,
        insertBefore: beforeKey === key,
        selected:
          item.type === "folder" ? selectedSet.has(keyForItem(item)) : selectedId === item.id,
        active:
          item.type === "folder"
            ? isActiveShortcut(item.path, currentFolder || "")
            : selectedId === item.id,
        onSelect,
        onSelectItem: (selectedItem, e) => onSelectItem?.(selectedItem, e, items),
        onSelectFavoriteDocument,
        onContextMenu,
        onFavoriteFileContextMenu,
        onFileDragStart,
        onFileDragEnd,
        onFolderDragStart,
        onFolderDragEnd,
        draggingFolderPath,
      })
    );
  });

  return h("div", { className: classNames("sidebar-section", className), style }, [
    h("p", { className: "eyebrow tiny" }, title),
    h(
      "div",
      {
        className: classNames(
          "sidebar-section-body",
          dropAvailable ? "drop-available" : "",
          dropActive ? "drop-active" : ""
        ),
        ...sectionDragProps,
      },
      h(
        "div",
        { className: "tree" },
        rows.length ? rows : h("div", { className: "sidebar-empty" }, "No favorites")
      )
    ),
  ]);
}

function SidebarResizeHandle({ before, after, onPointerDown }) {
  return h("div", {
    className: "sidebar-resize-handle",
    role: "separator",
    "aria-orientation": "horizontal",
    "aria-label": "Resize sidebar sections",
    onPointerDown: (e) => onPointerDown(e, before, after),
  });
}

export function SidebarNav({
  currentFolder,
  folderChildren,
  folderItems,
  favoriteItems = [],
  selectedKeys = [],
  dropHint,
  activeDropTarget,
  dragActive = false,
  favoriteDropAvailable = false,
  sidebarSectionSizes,
  myEdits = [],
  selectedId,
  onSelect,
  onSelectItem,
  onSelectFavoriteDocument,
  onContextMenu,
  onSidebarSectionSizesChange,
  onFavoriteFileContextMenu,
  onSelectMyEdit,
  onMyEditContextMenu,
  onFileDragStart,
  onFileDragEnd,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
  onDropOnFolder,
  onClearDropHint,
}) {
  const [draftSizes, setDraftSizes] = useState(() =>
    normalizeSidebarSectionSizes(sidebarSectionSizes)
  );
  const draftSizesRef = useRef(draftSizes);

  useEffect(() => {
    const normalized = normalizeSidebarSectionSizes(sidebarSectionSizes);
    draftSizesRef.current = normalized;
    setDraftSizes(normalized);
  }, [sidebarSectionSizes]);

  function updateDraftSizes(nextSizes) {
    draftSizesRef.current = nextSizes;
    setDraftSizes(nextSizes);
  }

  function sectionStyle(key) {
    // eslint-disable-next-line security/detect-object-injection
    return { "--sidebar-section-size": draftSizes[key] };
  }

  function handleResizePointerDown(pointerEvent, before, after) {
    pointerEvent.preventDefault();
    const startY = pointerEvent.clientY;
    const startSizes = draftSizesRef.current;
    // eslint-disable-next-line security/detect-object-injection
    const startBefore = startSizes[before];
    // eslint-disable-next-line security/detect-object-injection
    const startAfter = startSizes[after];
    const pairSize = startBefore + startAfter;

    function handlePointerMove(moveEvent) {
      const maxBefore = Math.max(MIN_SIDEBAR_SECTION_SIZE, pairSize - MIN_SIDEBAR_SECTION_SIZE);
      const nextBefore = clamp(
        startBefore + moveEvent.clientY - startY,
        MIN_SIDEBAR_SECTION_SIZE,
        maxBefore
      );
      const nextAfter = clamp(pairSize - nextBefore, MIN_SIDEBAR_SECTION_SIZE, pairSize);
      updateDraftSizes({
        ...startSizes,
        [before]: nextBefore,
        [after]: nextAfter,
      });
    }

    function handlePointerUp() {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
      document.body.classList.remove("sidebar-resizing");
      if (onSidebarSectionSizesChange) {
        onSidebarSectionSizesChange(draftSizesRef.current);
      }
    }

    document.body.classList.add("sidebar-resizing");
    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp, { once: true });
  }

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
  const favoriteShortcutItems = favoriteItems.map((item) => ({
    ...item,
    favorite: true,
    sourcePane: "favorites",
  }));

  return [
    h(SidebarFolderList, {
      key: "folders",
      title: "Folders",
      items: vaultItems,
      orderedItems: allItems,
      className: "resizable-sidebar-section",
      style: sectionStyle("folders"),
      currentFolder: currentFolder || "",
      dropHint,
      activeDropTarget,
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
    h(SidebarResizeHandle, {
      key: "folders-favorites",
      before: "folders",
      after: "favorites",
      onPointerDown: handleResizePointerDown,
    }),
    h(SidebarFavoriteList, {
      key: "favorites",
      title: "Favorites",
      items: favoriteShortcutItems,
      className: "resizable-sidebar-section",
      style: sectionStyle("favorites"),
      sectionDragProps: {
        "data-vault-drop-kind": "favorites",
        "data-drop-active": activeDropTarget?.kind === "favorites" ? "true" : undefined,
      },
      dropActive: activeDropTarget?.kind === "favorites",
      dropAvailable: dragActive && favoriteDropAvailable,
      currentFolder: currentFolder || "",
      activeDropTarget,
      selectedKeys,
      selectedId,
      onSelect,
      onSelectItem,
      onSelectFavoriteDocument,
      onContextMenu,
      onFavoriteFileContextMenu,
      onFileDragStart,
      onFileDragEnd,
      onFolderDragStart,
      onFolderDragEnd,
      draggingFolderPath,
    }),
    h(SidebarResizeHandle, {
      key: "favorites-archive",
      before: "favorites",
      after: "archive",
      onPointerDown: handleResizePointerDown,
    }),
    h(SidebarFolderList, {
      key: "archive",
      title: "Archive",
      items: archiveItems,
      orderedItems: allItems,
      className: "resizable-sidebar-section",
      style: sectionStyle("archive"),
      currentFolder: currentFolder || "",
      dropHint,
      activeDropTarget,
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
    h(SidebarResizeHandle, {
      key: "archive-editing",
      before: "archive",
      after: "editing",
      onPointerDown: handleResizePointerDown,
    }),
    h(MyEdits, {
      key: "my-edits",
      className: "resizable-sidebar-section",
      style: sectionStyle("editing"),
      edits: myEdits,
      selectedId,
      onSelect: onSelectMyEdit,
      onContextMenu: onMyEditContextMenu,
    }),
  ];
}
