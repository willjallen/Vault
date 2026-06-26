import { Toolbar } from "./toolbar/Toolbar.js";
import { SidebarNav } from "./sidebar/SidebarNav.js";
import { VaultFileList } from "./browser/FileList.js";
import {
  dragCanUseVaultDropZones,
  dragHasFavoriteItems,
  favoriteItemsFromDrag,
} from "../lib/dragPayloads.js";

const h = React.createElement;
const { useCallback, useEffect, useRef, useState } = React;

function sameDropTarget(left, right) {
  return (
    (left?.kind || "") === (right?.kind || "") &&
    (left?.folder || "") === (right?.folder || "") &&
    (left?.beforeKey || "") === (right?.beforeKey || "")
  );
}

function resolveFavoriteBeforeKey(evt, zone) {
  const row = evt.target.closest?.("[data-favorite-key]");
  if (!row || !zone.contains(row)) {
    return "";
  }
  const rect = row.getBoundingClientRect();
  if (evt.clientY < rect.top + rect.height / 2) {
    return row.dataset.favoriteKey || "";
  }
  const rows = Array.from(zone.querySelectorAll("[data-favorite-key]"));
  const index = rows.indexOf(row);
  return rows[index + 1]?.dataset.favoriteKey || "";
}

export function FinderShell({
  folder,
  breadcrumbs,
  myEdits,
  folderChildren,
  favoriteItems,
  subfolders,
  files,
  selectedId,
  contentsItems,
  contentsSort,
  contentsSelection,
  folderItems,
  folderSelection,
  sidebarSectionCollapsed,
  sidebarSectionSizes,
  searchQuery,
  recursiveSearch,
  contentsPending,
  contentsPendingEmptySearch,
  dropHint,
  uploadHover,
  draggingId,
  draggingFolderPath,
  currentUser,
  doubleClickDownload,
  canGoBack,
  canGoForward,
  canGoUp,
  onNavigateBack,
  onNavigateForward,
  onNavigateUp,
  onSelectFolder,
  onSelectDoc,
  onSelectFavoriteDocument,
  onSelectContentItem,
  onSelectFolderItem,
  onAddFavoriteItems,
  onSidebarLayoutChange,
  onSidebarSectionSizesChange,
  onSearchQueryChange,
  onRecursiveSearchChange,
  onContentsSortChange,
  onClearSelection,
  onContentsMarqueeSelectionChange,
  onOpenDoc,
  onDropOnFolder,
  onClearDropHint,
  onCanvasDrop,
  onCanvasDragOver,
  onCanvasDragLeave,
  onFileDragStart,
  onFileDragEnd,
  onFolderDragStart,
  onFolderDragEnd,
  onFileContextMenu,
  onFavoriteFileContextMenu,
  onFolderContextMenu,
  onMyEditContextMenu,
  onPageContextMenu,
  inlineFolderDraft,
  onInlineFolderNameChange,
  onCommitInlineFolder,
  onCancelInlineFolder,
  onTriggerUpload,
  logoutUrl,
  onOpenSettings,
  settingsButtonRef,
  actions,
}) {
  const shellRef = useRef(null);
  const dragFrameRef = useRef(0);
  const pendingDragUiRef = useRef(null);
  const [dragUi, setDragUi] = useState({ active: false, target: null, favoriteDrop: false });

  function resolveDropTarget(evt) {
    const shell = shellRef.current;
    const zone = evt.target.closest?.("[data-vault-drop-kind]");
    if (!shell || !zone || !shell.contains(zone)) {
      return null;
    }
    const kind = zone.dataset.vaultDropKind;
    if (kind === "favorites") {
      return dragHasFavoriteItems(evt)
        ? { kind: "favorites", beforeKey: resolveFavoriteBeforeKey(evt, zone) }
        : null;
    }
    if (kind === "folder") {
      return { kind: "folder", folder: zone.dataset.dropFolder || "" };
    }
    return null;
  }

  function commitDragUi(nextUi) {
    setDragUi((current) =>
      current.active === nextUi.active &&
      current.favoriteDrop === nextUi.favoriteDrop &&
      sameDropTarget(current.target, nextUi.target)
        ? current
        : nextUi
    );
  }

  function scheduleDragUi(nextUi) {
    pendingDragUiRef.current = nextUi;
    if (dragFrameRef.current) {
      return;
    }
    dragFrameRef.current = window.requestAnimationFrame(() => {
      dragFrameRef.current = 0;
      commitDragUi(
        pendingDragUiRef.current || { active: false, target: null, favoriteDrop: false }
      );
    });
  }

  const clearDragUi = useCallback(() => {
    pendingDragUiRef.current = null;
    if (dragFrameRef.current) {
      window.cancelAnimationFrame(dragFrameRef.current);
      dragFrameRef.current = 0;
    }
    commitDragUi({ active: false, target: null, favoriteDrop: false });
    if (onClearDropHint) {
      onClearDropHint();
    }
  }, [onClearDropHint]);

  useEffect(
    () => () => {
      if (dragFrameRef.current) {
        window.cancelAnimationFrame(dragFrameRef.current);
        dragFrameRef.current = 0;
      }
    },
    []
  );

  function handleShellDragOver(evt) {
    if (!dragCanUseVaultDropZones(evt)) {
      return;
    }
    evt.preventDefault();
    evt.stopPropagation();
    const target = resolveDropTarget(evt);
    evt.dataTransfer.dropEffect = target?.kind === "favorites" ? "copy" : target ? "move" : "none";
    scheduleDragUi({
      active: true,
      favoriteDrop: dragHasFavoriteItems(evt),
      target,
    });
  }

  function handleShellDragLeave(evt) {
    if (evt.currentTarget.contains(evt.relatedTarget)) {
      return;
    }
    clearDragUi();
  }

  function handleShellDrop(evt) {
    if (!dragCanUseVaultDropZones(evt)) {
      return;
    }
    evt.preventDefault();
    evt.stopPropagation();
    const target = resolveDropTarget(evt);
    clearDragUi();
    if (!target) {
      return;
    }
    if (target.kind === "favorites") {
      const items = favoriteItemsFromDrag(evt);
      if (items.length && onAddFavoriteItems) {
        onAddFavoriteItems(items, { beforeKey: target.beforeKey || "" });
      }
      return;
    }
    if (target.kind === "folder" && onDropOnFolder) {
      onDropOnFolder(target.folder || "", evt, false);
    }
  }

  return h(
    "div",
    {
      className: `finder-shell${dragUi.active ? " drag-active" : ""}`,
      onContextMenu: onPageContextMenu,
      onDragLeaveCapture: handleShellDragLeave,
      onDragOverCapture: handleShellDragOver,
      onDropCapture: handleShellDrop,
      ref: shellRef,
    },
    h(Toolbar, {
      folder,
      breadcrumbs,
      canGoBack,
      canGoForward,
      canGoUp,
      onNavigateBack,
      onNavigateForward,
      onNavigateUp,
      logoutUrl,
      onOpenSettings,
      settingsButtonRef,
      onSelectFolder,
      onDropOnFolder,
      onClearDrop: onClearDropHint,
    }),
    h(
      "div",
      { className: "finder-frame" },
      h(
        "aside",
        { className: "finder-sidebar" },
        h(SidebarNav, {
          currentFolder: folder,
          folderChildren,
          folderItems,
          favoriteItems,
          selectedKeys: folderSelection,
          dropHint,
          activeDropTarget: dragUi.target,
          dragActive: dragUi.active,
          favoriteDropAvailable: dragUi.favoriteDrop,
          sidebarSectionCollapsed,
          sidebarSectionSizes,
          myEdits,
          selectedId,
          onSelect: onSelectFolder,
          onSelectItem: onSelectFolderItem,
          onSelectFavoriteDocument,
          onSelectMyEdit: (doc) => {
            onSelectFolder(doc.folder || "");
            onSelectDoc(doc.id);
          },
          onContextMenu: onFolderContextMenu,
          onSidebarLayoutChange,
          onSidebarSectionSizesChange,
          onFavoriteFileContextMenu,
          onMyEditContextMenu,
          onFileDragStart,
          onFileDragEnd,
          onFolderDragStart,
          onFolderDragEnd,
          draggingFolderPath,
          onDropOnFolder,
          onClearDropHint,
        })
      ),
      h(VaultFileList, {
        folder,
        subfolders,
        files,
        currentUser,
        doubleClickDownload,
        actions,
        selectedKeys: contentsSelection,
        orderedItems: contentsItems,
        sort: contentsSort,
        searchQuery,
        recursiveSearch,
        contentsPending,
        contentsPendingEmptySearch,
        draggingId,
        draggingFolderPath,
        dropHint,
        uploadHover,
        activeDropTarget: dragUi.target,
        dragActive: dragUi.active,
        onSelectFolder,
        onSelectItem: onSelectContentItem,
        onSearchQueryChange,
        onRecursiveSearchChange,
        onSortChange: onContentsSortChange,
        onBackgroundClick: onClearSelection,
        onMarqueeSelectionChange: onContentsMarqueeSelectionChange,
        onOpenFile: onOpenDoc,
        onFileDragStart,
        onFileDragEnd,
        onFolderDragStart,
        onFolderDragEnd,
        onFileContextMenu,
        onFolderContextMenu,
        inlineFolderDraft,
        onInlineFolderNameChange,
        onCommitInlineFolder,
        onCancelInlineFolder,
        onDropOnFolder,
        onClearDropHint,
        onCanvasDrop,
        onCanvasDragOver,
        onCanvasDragLeave,
        onUploadClick: onTriggerUpload,
      })
    )
  );
}
