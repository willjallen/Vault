import { classNames, isArchivedPath, isArchiveRootPath } from "../../lib/utils.js";
import { favoriteItemKey } from "../../lib/favoriteItems.js";
import { keyForItem } from "../../lib/itemActions.js";
import {
  SIDEBAR_COLLAPSE_THRESHOLD,
  SIDEBAR_COLLAPSED_SECTION_SIZE,
  SIDEBAR_EXPANDED_SECTION_SIZE,
  normalizeSidebarSectionCollapsed,
  normalizeSidebarSectionSizes,
} from "../../lib/theme.js";
import { Icon } from "../common/Icon.js";
import { MyEdits } from "./MyEdits.js";

const h = React.createElement;
const { useEffect, useRef, useState } = React;
const SIDEBAR_RESIZE_HANDLE_SIZE = 10;

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

function sidebarSectionValue(source, key) {
  switch (key) {
    case "folders":
      return source.folders;
    case "favorites":
      return source.favorites;
    case "archive":
      return source.archive;
    case "editing":
      return source.editing;
    default:
      return undefined;
  }
}

function setSidebarSectionValue(target, key, value) {
  switch (key) {
    case "folders":
      target.folders = value;
      break;
    case "favorites":
      target.favorites = value;
      break;
    case "archive":
      target.archive = value;
      break;
    case "editing":
      target.editing = value;
      break;
    default:
      break;
  }
}

function sectionWeight(source, key) {
  const value = Number(sidebarSectionValue(source, key));
  return Number.isFinite(value) && value > 0 ? value : SIDEBAR_EXPANDED_SECTION_SIZE;
}

function sectionPixelValue(source, key) {
  const value = Number(sidebarSectionValue(source, key));
  return Number.isFinite(value) && value >= 0 ? value : 0;
}

function expandedWeightTotal(sizes, collapsed) {
  let total = 0;
  if (!collapsed.folders) {
    total += sectionWeight(sizes, "folders");
  }
  if (!collapsed.favorites) {
    total += sectionWeight(sizes, "favorites");
  }
  if (!collapsed.archive) {
    total += sectionWeight(sizes, "archive");
  }
  if (!collapsed.editing) {
    total += sectionWeight(sizes, "editing");
  }
  return total;
}

function collapsedSectionCount(collapsed) {
  return (
    Number(Boolean(collapsed.folders)) +
    Number(Boolean(collapsed.favorites)) +
    Number(Boolean(collapsed.archive)) +
    Number(Boolean(collapsed.editing))
  );
}

function resolvedSectionPixels(sizes, collapsed, availableHeight) {
  const expandedAvailable = Math.max(
    0,
    availableHeight - collapsedSectionCount(collapsed) * SIDEBAR_COLLAPSED_SECTION_SIZE
  );
  const totalWeight = expandedWeightTotal(sizes, collapsed);
  const scale = totalWeight > 0 ? expandedAvailable / totalWeight : 0;
  return {
    folders: collapsed.folders
      ? SIDEBAR_COLLAPSED_SECTION_SIZE
      : sectionWeight(sizes, "folders") * scale,
    favorites: collapsed.favorites
      ? SIDEBAR_COLLAPSED_SECTION_SIZE
      : sectionWeight(sizes, "favorites") * scale,
    archive: collapsed.archive
      ? SIDEBAR_COLLAPSED_SECTION_SIZE
      : sectionWeight(sizes, "archive") * scale,
    editing: collapsed.editing
      ? SIDEBAR_COLLAPSED_SECTION_SIZE
      : sectionWeight(sizes, "editing") * scale,
  };
}

function sizesFromPixels(pixels, collapsed, previousSizes) {
  return {
    folders: collapsed.folders
      ? sectionWeight(previousSizes, "folders")
      : Math.max(1, sectionPixelValue(pixels, "folders")),
    favorites: collapsed.favorites
      ? sectionWeight(previousSizes, "favorites")
      : Math.max(1, sectionPixelValue(pixels, "favorites")),
    archive: collapsed.archive
      ? sectionWeight(previousSizes, "archive")
      : Math.max(1, sectionPixelValue(pixels, "archive")),
    editing: collapsed.editing
      ? sectionWeight(previousSizes, "editing")
      : Math.max(1, sectionPixelValue(pixels, "editing")),
  };
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

function SidebarSectionHeader({ title, collapsed = false, onToggleCollapsed }) {
  return h(
    "button",
    {
      className: classNames("sidebar-section-header", collapsed ? "collapsed" : ""),
      type: "button",
      onClick: onToggleCollapsed,
      title: collapsed ? `Expand ${title}` : `Collapse ${title}`,
    },
    [
      h("span", { className: "sidebar-section-title eyebrow tiny" }, title),
      h(Icon, {
        className: "sidebar-section-chevron",
        icon: collapsed ? "chevron-right" : "chevron-down",
        size: 11,
      }),
    ]
  );
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
  const isArchived = isArchiveRootPath(item.path);
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
  collapsed = false,
  onToggleCollapsed,
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
  return h(
    "div",
    {
      className: classNames("sidebar-section", className, collapsed ? "collapsed" : ""),
      style,
      ...sectionDragProps,
    },
    h(SidebarSectionHeader, { collapsed, onToggleCollapsed, title }),
    collapsed
      ? null
      : h(
          "div",
          {
            className: classNames(
              "sidebar-section-body",
              dropAvailable ? "drop-available" : "",
              dropActive ? "drop-active" : ""
            ),
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
        )
  );
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
  const isArchived = isArchivedPath(item.path || item.folder || "");
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
  collapsed = false,
  onToggleCollapsed,
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

  return h(
    "div",
    {
      className: classNames("sidebar-section", className, collapsed ? "collapsed" : ""),
      style,
      ...sectionDragProps,
    },
    h(SidebarSectionHeader, { collapsed, onToggleCollapsed, title }),
    collapsed
      ? null
      : h(
          "div",
          {
            className: classNames(
              "sidebar-section-body",
              dropAvailable ? "drop-available" : "",
              dropActive ? "drop-active" : ""
            ),
          },
          h(
            "div",
            { className: "tree" },
            rows.length ? rows : h("div", { className: "sidebar-empty" }, "No favorites")
          )
        )
  );
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
  sidebarSectionCollapsed,
  sidebarSectionSizes,
  myEdits = [],
  selectedId,
  onSelect,
  onSelectItem,
  onSelectFavoriteDocument,
  onContextMenu,
  onSidebarLayoutChange,
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
  const layoutRef = useRef(null);
  const [draftSizes, setDraftSizes] = useState(() =>
    normalizeSidebarSectionSizes(sidebarSectionSizes)
  );
  const [draftCollapsed, setDraftCollapsed] = useState(() =>
    normalizeSidebarSectionCollapsed(sidebarSectionCollapsed)
  );
  const [layoutHeight, setLayoutHeight] = useState(0);
  const draftSizesRef = useRef(draftSizes);
  const draftCollapsedRef = useRef(draftCollapsed);

  useEffect(() => {
    const normalized = normalizeSidebarSectionSizes(sidebarSectionSizes);
    draftSizesRef.current = normalized;
    setDraftSizes(normalized);
  }, [sidebarSectionSizes]);

  useEffect(() => {
    const normalized = normalizeSidebarSectionCollapsed(sidebarSectionCollapsed);
    draftCollapsedRef.current = normalized;
    setDraftCollapsed(normalized);
  }, [sidebarSectionCollapsed]);

  useEffect(() => {
    const layout = layoutRef.current;
    if (!layout) {
      return undefined;
    }
    function updateLayoutHeight() {
      setLayoutHeight(layout.getBoundingClientRect().height);
    }
    updateLayoutHeight();
    const observer = new ResizeObserver(updateLayoutHeight);
    observer.observe(layout);
    return () => observer.disconnect();
  }, []);

  function updateDraftLayout(nextSizes, nextCollapsed) {
    draftSizesRef.current = nextSizes;
    draftCollapsedRef.current = nextCollapsed;
    setDraftSizes(nextSizes);
    setDraftCollapsed(nextCollapsed);
  }

  function commitDraftLayout() {
    if (onSidebarLayoutChange) {
      onSidebarLayoutChange({
        collapsed: draftCollapsedRef.current,
        sizes: draftSizesRef.current,
      });
      return;
    }
    if (onSidebarSectionSizesChange) {
      onSidebarSectionSizesChange(draftSizesRef.current);
    }
  }

  function availableSectionHeight() {
    return Math.max(0, layoutHeight - SIDEBAR_RESIZE_HANDLE_SIZE * 3);
  }

  function currentSectionPixels() {
    return resolvedSectionPixels(draftSizes, draftCollapsed, availableSectionHeight());
  }

  function layoutStyle() {
    const pixels = currentSectionPixels();
    return {
      gridTemplateRows: [
        `${Math.max(0, pixels.folders)}px`,
        `${SIDEBAR_RESIZE_HANDLE_SIZE}px`,
        `${Math.max(0, pixels.favorites)}px`,
        `${SIDEBAR_RESIZE_HANDLE_SIZE}px`,
        `${Math.max(0, pixels.editing)}px`,
        `${SIDEBAR_RESIZE_HANDLE_SIZE}px`,
        `${Math.max(0, pixels.archive)}px`,
      ].join(" "),
    };
  }

  function sectionCollapsed(key) {
    return Boolean(sidebarSectionValue(draftCollapsed, key));
  }

  function toggleSectionCollapsed(key) {
    const nextSizes = { ...draftSizesRef.current };
    const nextCollapsed = { ...draftCollapsedRef.current };
    const collapsed = !sidebarSectionValue(nextCollapsed, key);
    setSidebarSectionValue(nextCollapsed, key, collapsed);
    setSidebarSectionValue(nextSizes, key, SIDEBAR_EXPANDED_SECTION_SIZE);
    updateDraftLayout(nextSizes, nextCollapsed);
    commitDraftLayout();
  }

  function handleResizePointerDown(pointerEvent, before, after) {
    pointerEvent.preventDefault();
    const startY = pointerEvent.clientY;
    const startSizes = sizesFromPixels(
      currentSectionPixels(),
      draftCollapsedRef.current,
      draftSizesRef.current
    );
    const startCollapsed = draftCollapsedRef.current;
    const startPixels = resolvedSectionPixels(startSizes, startCollapsed, availableSectionHeight());
    const startBefore = sectionPixelValue(startPixels, before);
    const startAfter = sectionPixelValue(startPixels, after);
    const pairSize = startBefore + startAfter;

    function handlePointerMove(moveEvent) {
      const desiredBefore = startBefore + moveEvent.clientY - startY;
      const desiredAfter = pairSize - desiredBefore;
      const nextPixels = { ...startPixels };
      const nextCollapsed = { ...startCollapsed };
      if (desiredBefore <= SIDEBAR_COLLAPSE_THRESHOLD) {
        setSidebarSectionValue(nextCollapsed, before, true);
        setSidebarSectionValue(nextCollapsed, after, false);
        setSidebarSectionValue(nextPixels, before, SIDEBAR_COLLAPSED_SECTION_SIZE);
        setSidebarSectionValue(
          nextPixels,
          after,
          Math.max(1, pairSize - SIDEBAR_COLLAPSED_SECTION_SIZE)
        );
      } else if (desiredAfter <= SIDEBAR_COLLAPSE_THRESHOLD) {
        setSidebarSectionValue(nextCollapsed, before, false);
        setSidebarSectionValue(nextCollapsed, after, true);
        setSidebarSectionValue(
          nextPixels,
          before,
          Math.max(1, pairSize - SIDEBAR_COLLAPSED_SECTION_SIZE)
        );
        setSidebarSectionValue(nextPixels, after, SIDEBAR_COLLAPSED_SECTION_SIZE);
      } else {
        const nextBefore = clamp(desiredBefore, 1, Math.max(1, pairSize - 1));
        setSidebarSectionValue(nextCollapsed, before, false);
        setSidebarSectionValue(nextCollapsed, after, false);
        setSidebarSectionValue(nextPixels, before, nextBefore);
        setSidebarSectionValue(nextPixels, after, Math.max(1, pairSize - nextBefore));
      }
      const nextSizes = sizesFromPixels(nextPixels, nextCollapsed, startSizes);
      updateDraftLayout(nextSizes, nextCollapsed);
    }

    function handlePointerUp() {
      window.removeEventListener("pointermove", handlePointerMove);
      window.removeEventListener("pointerup", handlePointerUp);
      document.body.classList.remove("sidebar-resizing");
      commitDraftLayout();
    }

    document.body.classList.add("sidebar-resizing");
    window.addEventListener("pointermove", handlePointerMove);
    window.addEventListener("pointerup", handlePointerUp, { once: true });
  }

  const allItems = folderItems || [
    { name: "Vault", path: "" },
    ...directChildren(folderChildren || {}, "", (child) => !isArchiveRootPath(child)),
    { name: "Archive", path: "Archive" },
  ];
  const vaultItems = allItems.filter(
    (item) => item.path !== "Archive" && !isArchiveRootPath(item.path)
  );
  const archiveItems = allItems.filter((item) => item.path === "Archive");
  const favoriteShortcutItems = favoriteItems.map((item) => ({
    ...item,
    favorite: true,
    sourcePane: "favorites",
  }));

  return h(
    "div",
    {
      className: "sidebar-layout",
      ref: layoutRef,
      style: layoutStyle(),
    },
    h(SidebarFolderList, {
      key: "folders",
      title: "Folders",
      items: vaultItems,
      orderedItems: allItems,
      className: "resizable-sidebar-section",
      collapsed: sectionCollapsed("folders"),
      onToggleCollapsed: () => toggleSectionCollapsed("folders"),
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
      collapsed: sectionCollapsed("favorites"),
      onToggleCollapsed: () => toggleSectionCollapsed("favorites"),
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
      key: "favorites-editing",
      before: "favorites",
      after: "editing",
      onPointerDown: handleResizePointerDown,
    }),
    h(MyEdits, {
      key: "my-edits",
      className: "resizable-sidebar-section",
      collapsed: sectionCollapsed("editing"),
      edits: myEdits,
      onToggleCollapsed: () => toggleSectionCollapsed("editing"),
      selectedId,
      onSelect: onSelectMyEdit,
      onContextMenu: onMyEditContextMenu,
    }),
    h(SidebarResizeHandle, {
      key: "editing-archive",
      before: "editing",
      after: "archive",
      onPointerDown: handleResizePointerDown,
    }),
    h(SidebarFolderList, {
      key: "archive",
      title: "Archive",
      items: archiveItems,
      orderedItems: allItems,
      className: "resizable-sidebar-section",
      collapsed: sectionCollapsed("archive"),
      onToggleCollapsed: () => toggleSectionCollapsed("archive"),
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
    })
  );
}
