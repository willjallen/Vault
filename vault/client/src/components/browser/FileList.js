import { CONTENTS_VIEW_MODES, contentsVisualSize } from "../../lib/contentsView.js";
import { browserScopeClasses, canvasDropAttributes } from "../../lib/dropHandlers.js";
import { writeLocalPreference } from "../../lib/localPreferences.js";
import { classNames, formatBytes, isArchivedPath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import {
  COLUMN_WIDTH_STORAGE_KEY,
  columnWidthsForResize,
  contentColumnStyle,
  measuredColumnWidths,
  readStoredColumnWidths,
} from "./contentColumns.js";
import { FolderRow } from "./FolderRow.js";
import { FileRow } from "./FileRow.js";
import { EmptyState } from "./EmptyState.js";
import { ContentsTableHeader } from "./ContentsTableHeader.js";
import { ContentsViewToolbarControls, ViewModeControl } from "./ContentsViewControl.js";
import {
  clientPointToContent,
  clientRectToContent,
  combineMarqueeSelection,
  contentRectToVisibleClientRect,
  rectFromPoints,
  rectsIntersect,
} from "./marqueeSelection.js";
import { useContentsView } from "./useContentsView.js";

const { useCallback, useEffect, useRef, useState } = React;
const h = React.createElement;
const MARQUEE_MIN_DISTANCE = 4;
const MARQUEE_AUTO_SCROLL_EDGE = 48;
const MARQUEE_AUTO_SCROLL_MAX = 18;

function itemSelectionKey(item) {
  if (item.type === "document") {
    return `document:${item.id}`;
  }
  return item.id ? `folder:${item.id}` : `folder:${item.path || ""}`;
}

function rovingSelectionKey(orderedKeys, focusedKey) {
  if (orderedKeys.includes(focusedKey)) {
    return focusedKey;
  }
  return orderedKeys[0] || "";
}

function isActiveFolderDropTarget(dropHint, activeDropTarget, path) {
  const folderPath = path || "";
  return (
    dropHint === path ||
    (activeDropTarget?.kind === "folder" && activeDropTarget.folder === folderPath)
  );
}

function marqueeModeFromEvent(evt) {
  if (evt.ctrlKey || evt.metaKey) {
    return "toggle";
  }
  if (evt.shiftKey) {
    return "add";
  }
  return "replace";
}

function shouldIgnoreMarqueeTarget(target) {
  return Boolean(
    target.closest &&
    target.closest(
      ".contents-table-head, .contents-toolbar, .contents-statusbar, button, input, textarea, select, a, [role='button'], [contenteditable='true']"
    )
  );
}

function clearNativeSelection() {
  const selection = window.getSelection?.();
  if (selection && selection.rangeCount) {
    selection.removeAllRanges();
  }
}

function ContentsLoadMore({ hasMore, loading, onLoadMore }) {
  if (!hasMore && !loading) {
    return null;
  }
  return h(
    "div",
    {
      "aria-live": "polite",
      className: "contents-load-more",
      onClick: (e) => e.stopPropagation(),
      onMouseDown: (e) => e.stopPropagation(),
    },
    h(
      "button",
      {
        "aria-busy": loading ? "true" : undefined,
        "aria-label": loading ? "Loading more contents" : "Load more contents",
        className: "btn secondary contents-load-more-button",
        disabled: loading,
        onClick: onLoadMore,
        type: "button",
      },
      loading ? "Loading more…" : "Load more"
    )
  );
}

function fileListState({
  contentsHasMore,
  contentsPending,
  contentsPendingEmptySearch,
  files,
  folder,
  inlineFolderDraft,
  orderedItems,
  recursiveSearch,
  searchQuery,
  selectedKeys,
  subfolders,
}) {
  const inArchive = isArchivedPath(folder);
  const draftInFolder = inlineFolderDraft && inlineFolderDraft.parent === (folder || "");
  const createDraft = draftInFolder && inlineFolderDraft.mode === "create";
  const hasRows = files.length > 0 || subfolders.length > 0 || createDraft;
  const searchActive = Boolean(searchQuery || recursiveSearch);
  const emptyState =
    !hasRows && !contentsHasMore && (!contentsPending || contentsPendingEmptySearch);
  const selectedSet = new Set(selectedKeys);
  const orderedKeys = orderedItems.map(itemSelectionKey);
  const selectedItems = orderedItems.filter((item) => selectedSet.has(itemSelectionKey(item)));
  const selectedFiles = selectedItems.filter((item) => item.type === "document");
  const selectedFolders = selectedItems.filter((item) => item.type === "folder");
  const selectedSizeDisplay = formatBytes(
    selectedItems.reduce((sum, item) => sum + (item.size_bytes || 0), 0),
    { emptyForZero: false }
  );
  const visibleKeys = orderedKeys;
  const allVisibleSelected =
    visibleKeys.length > 0 && visibleKeys.every((key) => selectedSet.has(key));
  return {
    allVisibleSelected,
    createDraft,
    draftInFolder,
    emptyState,
    inArchive,
    orderedKeys,
    searchActive,
    selectedFiles,
    selectedFolders,
    selectedItems,
    selectedSet,
    selectedSizeDisplay,
    visibleKeys,
  };
}

export function VaultFileList({
  folder,
  contentsViewByFolder,
  subfolders,
  files,
  currentUser,
  doubleClickDownload = false,
  actions = {},
  selectedKeys = [],
  orderedItems = [],
  sort,
  searchQuery = "",
  recursiveSearch = false,
  contentsPending = false,
  contentsPendingEmptySearch = false,
  contentsHasMore,
  contentsLoadingMore,
  draggingId,
  draggingFolderPath,
  dropHint,
  uploadHover,
  activeDropTarget,
  dragActive = false,
  onSelectFolder,
  onSelectItem,
  onSearchQueryChange,
  onRecursiveSearchChange,
  onSortChange,
  onContentsViewChange,
  onBackgroundClick,
  onMarqueeSelectionChange,
  loadMoreContents,
  onOpenFile,
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
  onUploadClick,
}) {
  const headerRef = useRef(null);
  const fileListRef = useRef(null);
  const marqueeDragRef = useRef(null);
  const marqueeFrameRef = useRef(null);
  const resizeDragRef = useRef(null);
  const suppressClickRef = useRef(false);
  const [columnWidths, setColumnWidths] = useState(readStoredColumnWidths);
  const [focusedSelectionKey, setFocusedSelectionKey] = useState("");
  const [marquee, setMarquee] = useState(null);
  const { contentsView, requestContentsView } = useContentsView({
    contentsViewByFolder,
    folder,
    interactionRef: resizeDragRef,
    listRef: fileListRef,
    locked: Boolean(inlineFolderDraft || dragActive),
    onContentsViewChange,
  });
  const {
    allVisibleSelected,
    createDraft,
    draftInFolder,
    emptyState,
    inArchive,
    orderedKeys,
    searchActive,
    selectedFiles,
    selectedFolders,
    selectedItems,
    selectedSet,
    selectedSizeDisplay,
    visibleKeys,
  } = fileListState({
    contentsHasMore,
    contentsPending,
    contentsPendingEmptySearch,
    files,
    folder,
    inlineFolderDraft,
    orderedItems,
    recursiveSearch,
    searchQuery,
    selectedKeys,
    subfolders,
  });
  const rovingFocusKey = rovingSelectionKey(orderedKeys, focusedSelectionKey);
  const visualSize = contentsVisualSize(contentsView);

  const updateMarqueeSelection = useCallback(() => {
    marqueeFrameRef.current = null;
    const drag = marqueeDragRef.current;
    const list = fileListRef.current;
    if (!drag || !list) {
      return;
    }

    const distance = Math.hypot(drag.currentX - drag.startX, drag.currentY - drag.startY);
    if (!drag.active && distance < MARQUEE_MIN_DISTANCE) {
      return;
    }
    drag.active = true;

    const listRect = list.getBoundingClientRect();
    const edgeTop = listRect.top + MARQUEE_AUTO_SCROLL_EDGE;
    const edgeBottom = listRect.bottom - MARQUEE_AUTO_SCROLL_EDGE;
    let scrollDelta = 0;
    if (drag.currentY < edgeTop) {
      scrollDelta =
        -MARQUEE_AUTO_SCROLL_MAX *
        Math.min(1, (edgeTop - drag.currentY) / MARQUEE_AUTO_SCROLL_EDGE);
    } else if (drag.currentY > edgeBottom) {
      scrollDelta =
        MARQUEE_AUTO_SCROLL_MAX *
        Math.min(1, (drag.currentY - edgeBottom) / MARQUEE_AUTO_SCROLL_EDGE);
    }
    if (scrollDelta) {
      list.scrollTop += scrollDelta;
    }

    const currentContentPoint = clientPointToContent({
      clientX: drag.currentX,
      clientY: drag.currentY,
      listRect,
      scrollLeft: list.scrollLeft,
      scrollTop: list.scrollTop,
    });
    const marqueeRect = rectFromPoints(
      { x: drag.startContentX, y: drag.startContentY },
      currentContentPoint
    );
    const visibleMarqueeRect = contentRectToVisibleClientRect(
      marqueeRect,
      listRect,
      list.scrollLeft,
      list.scrollTop
    );
    setMarquee({
      height: visibleMarqueeRect.height,
      left: visibleMarqueeRect.left,
      top: visibleMarqueeRect.top,
      width: visibleMarqueeRect.width,
    });

    const hitKeys = Array.from(list.querySelectorAll("[data-selection-key]"))
      .filter((row) =>
        rectsIntersect(
          marqueeRect,
          clientRectToContent(
            row.getBoundingClientRect(),
            listRect,
            list.scrollLeft,
            list.scrollTop
          )
        )
      )
      .map((row) => row.dataset.selectionKey)
      .filter(Boolean);
    const nextKeys = combineMarqueeSelection({
      baseSelection: drag.baseSelection,
      hitKeys,
      mode: drag.mode,
      orderedKeys: drag.orderedKeys,
    });
    if (onMarqueeSelectionChange) {
      onMarqueeSelectionChange(nextKeys, nextKeys[nextKeys.length - 1] || "");
    }

    if (marqueeDragRef.current && scrollDelta) {
      marqueeFrameRef.current = window.requestAnimationFrame(updateMarqueeSelection);
    }
  }, [onMarqueeSelectionChange]);

  function scheduleMarqueeUpdate() {
    if (!marqueeFrameRef.current) {
      marqueeFrameRef.current = window.requestAnimationFrame(updateMarqueeSelection);
    }
  }

  function cancelMarqueeFrame() {
    if (marqueeFrameRef.current) {
      window.cancelAnimationFrame(marqueeFrameRef.current);
      marqueeFrameRef.current = null;
    }
  }

  function createMarqueeDrag(evt, extra = {}) {
    const list = fileListRef.current || evt.currentTarget;
    const listRect = list.getBoundingClientRect();
    const startContentPoint = clientPointToContent({
      clientX: evt.clientX,
      clientY: evt.clientY,
      listRect,
      scrollLeft: list.scrollLeft,
      scrollTop: list.scrollTop,
    });
    return {
      active: false,
      baseSelection: selectedKeys.slice(),
      captured: false,
      currentX: evt.clientX,
      currentY: evt.clientY,
      mode: marqueeModeFromEvent(evt),
      orderedKeys: orderedKeys.slice(),
      pointerId: evt.pointerId,
      startContentX: startContentPoint.x,
      startContentY: startContentPoint.y,
      startX: evt.clientX,
      startY: evt.clientY,
      ...extra,
    };
  }

  function suppressNextClick() {
    suppressClickRef.current = true;
    window.setTimeout(() => {
      suppressClickRef.current = false;
    }, 0);
  }

  function beginColumnResize(handle, e) {
    if (!handle || e.button !== 0) {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    resizeDragRef.current = {
      ...handle,
      nextWidths: null,
      pointerId: e.pointerId,
      startColumnWidths: columnWidths,
      startWidths: measuredColumnWidths(headerRef.current),
      startX: e.clientX,
    };
    e.currentTarget.setPointerCapture?.(e.pointerId);
    document.body.classList.add("contents-column-resizing");
  }

  function moveColumnResize(e) {
    const drag = resizeDragRef.current;
    if (!drag || drag.pointerId !== e.pointerId) {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    const nextWidths = columnWidthsForResize(drag, e.clientX);
    drag.nextWidths = nextWidths;
    setColumnWidths(nextWidths);
  }

  function endColumnResize(e) {
    const drag = resizeDragRef.current;
    if (!drag || drag.pointerId !== e.pointerId) {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    if (drag.nextWidths) {
      writeLocalPreference(COLUMN_WIDTH_STORAGE_KEY, drag.nextWidths);
    }
    e.currentTarget.releasePointerCapture?.(e.pointerId);
    resizeDragRef.current = null;
    document.body.classList.remove("contents-column-resizing");
  }

  function commitInlineEditBeforePointer(e) {
    if (
      !inlineFolderDraft ||
      !onCommitInlineFolder ||
      (e.target.closest && e.target.closest(".inline-name-editor"))
    ) {
      return false;
    }
    const editor = fileListRef.current?.querySelector(".inline-name-editor");
    onCommitInlineFolder(editor ? editor.value : inlineFolderDraft.value || "");
    suppressNextClick();
    e.preventDefault();
    e.stopPropagation();
    return true;
  }

  function handleMarqueePointerDown(e) {
    if (commitInlineEditBeforePointer(e)) {
      return;
    }
    if (e.button !== 0 || e.pointerType === "touch" || shouldIgnoreMarqueeTarget(e.target)) {
      return;
    }
    const row = e.target.closest?.(".file-row[data-selection-key]");
    const modifierSelection = e.ctrlKey || e.metaKey || e.shiftKey;
    if (row && selectedSet.has(row.dataset.selectionKey) && !modifierSelection) {
      return;
    }
    const rowOrigin = Boolean(row);
    if (!rowOrigin) {
      clearNativeSelection();
    }
    marqueeDragRef.current = createMarqueeDrag(e, { rowOrigin });
    setMarquee(null);
    if (!rowOrigin) {
      marqueeDragRef.current.captured = true;
      e.currentTarget.setPointerCapture?.(e.pointerId);
      e.preventDefault();
    }
  }

  function handleMarqueePointerMove(e) {
    const drag = marqueeDragRef.current;
    if (!drag || drag.pointerId !== e.pointerId) {
      return;
    }
    drag.currentX = e.clientX;
    drag.currentY = e.clientY;
    const distance = Math.hypot(drag.currentX - drag.startX, drag.currentY - drag.startY);
    if (drag.active || distance >= MARQUEE_MIN_DISTANCE) {
      if (!drag.captured) {
        drag.captured = true;
        e.currentTarget.setPointerCapture?.(e.pointerId);
      }
      clearNativeSelection();
      e.preventDefault();
    }
    scheduleMarqueeUpdate();
  }

  function finishMarquee(e) {
    const drag = marqueeDragRef.current;
    if (!drag || drag.pointerId !== e.pointerId) {
      return;
    }
    drag.currentX = e.clientX;
    drag.currentY = e.clientY;
    cancelMarqueeFrame();
    updateMarqueeSelection();
    if (drag.active) {
      clearNativeSelection();
      suppressNextClick();
      e.preventDefault();
    }
    if (drag.captured) {
      e.currentTarget.releasePointerCapture?.(e.pointerId);
    }
    marqueeDragRef.current = null;
    setMarquee(null);
  }

  function handleMarqueeClickCapture(e) {
    if (!suppressClickRef.current) {
      return;
    }
    suppressClickRef.current = false;
    e.preventDefault();
    e.stopPropagation();
  }

  function handleMarqueeDragStartCapture(e) {
    if (marqueeDragRef.current) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  useEffect(
    () => () => {
      cancelMarqueeFrame();
      document.body.classList.remove("contents-column-resizing");
    },
    []
  );

  function dragItemsFor(item, type) {
    const key = itemSelectionKey({ ...item, type });
    if (selectedSet.has(key)) {
      return orderedItems.filter((orderedItem) => selectedSet.has(itemSelectionKey(orderedItem)));
    }
    return [
      type === "document"
        ? {
            archived: Boolean(item.archived),
            directly_archived: Boolean(item.directly_archived),
            archived_from_folder: item.archived_from_folder || "",
            archived_original_name: item.archived_original_name || "",
            archived_original_path: item.archived_original_path || "",
            folder: item.folder || "",
            id: item.id,
            lock: item.lock || {},
            name: item.name,
            path: item.path || (item.folder ? `${item.folder}/${item.name}` : item.name),
            size_bytes: item.size_bytes || 0,
            type: "document",
          }
        : {
            archived: Boolean(item.archived) || isArchivedPath(item.path || ""),
            directly_archived: Boolean(item.archived_at || item.directly_archived),
            archived_origin_path: item.archived_origin_path || "",
            id: item.id || null,
            name: item.name,
            path: item.path || "",
            size_bytes: item.size_bytes || 0,
            type: "folder",
          },
    ];
  }

  function handleBackgroundClick(e) {
    if (suppressClickRef.current) {
      suppressClickRef.current = false;
      e.preventDefault();
      e.stopPropagation();
      return;
    }
    if (
      e.target.closest &&
      e.target.closest(".file-row, .contents-table-head, .contents-toolbar, .contents-statusbar")
    ) {
      return;
    }
    if (onBackgroundClick) {
      onBackgroundClick();
    }
  }

  function selectionControlEvent(e) {
    return {
      ctrlKey: !e.shiftKey,
      metaKey: false,
      shiftKey: e.shiftKey,
    };
  }

  function handleToggleSelect(item, type, e) {
    if (onSelectItem) {
      onSelectItem(item, type, selectionControlEvent(e), orderedItems);
    }
  }

  function handleSelectAllChange(e) {
    e.stopPropagation();
    const nextKeys = allVisibleSelected ? [] : visibleKeys.slice();
    if (onMarqueeSelectionChange) {
      onMarqueeSelectionChange(nextKeys, nextKeys[nextKeys.length - 1] || "");
    }
  }

  function openContextMenuForItem(e, item, options = {}) {
    e.preventDefault();
    e.stopPropagation();
    if (!item) {
      return;
    }
    if (item.type === "folder") {
      if (onFolderContextMenu) {
        onFolderContextMenu(e, item, options);
      }
      return;
    }
    if (onFileContextMenu) {
      onFileContextMenu(e, item, options);
    }
  }

  function handleCollectionKeyDown(e) {
    if (e.target.closest?.("button, input, textarea, select, [contenteditable='true']")) {
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLocaleLowerCase() === "a") {
      e.preventDefault();
      handleSelectAllChange(e);
      return;
    }
    const row = e.target.closest?.("[data-selection-key]");
    if (!row) {
      return;
    }
    if (e.key === " ") {
      const item = orderedItems.find(
        (candidate) => itemSelectionKey(candidate) === row.dataset.selectionKey
      );
      if (item) {
        e.preventDefault();
        handleToggleSelect(item, item.type, e);
      }
      return;
    }
    if (!["ArrowDown", "ArrowLeft", "ArrowRight", "ArrowUp", "End", "Home"].includes(e.key)) {
      return;
    }
    const rows = Array.from(fileListRef.current?.querySelectorAll("[data-selection-key]") || []);
    const currentIndex = rows.indexOf(row);
    if (currentIndex < 0) {
      return;
    }
    const firstTop = rows[0]?.getBoundingClientRect().top;
    const columns =
      contentsView.mode === CONTENTS_VIEW_MODES.ICONS
        ? Math.max(
            1,
            rows.filter(
              (candidate) => Math.abs(candidate.getBoundingClientRect().top - firstTop) < 2
            ).length
          )
        : 1;
    const offsets = {
      ArrowDown: columns,
      ArrowLeft: -1,
      ArrowRight: 1,
      ArrowUp: -columns,
    };
    let nextIndex = currentIndex;
    if (e.key === "Home") {
      nextIndex = 0;
    } else if (e.key === "End") {
      nextIndex = rows.length - 1;
    } else {
      nextIndex = currentIndex + offsets[e.key];
    }
    const nextRow = rows[Math.max(0, Math.min(rows.length - 1, nextIndex))];
    if (nextRow && nextRow !== row) {
      e.preventDefault();
      setFocusedSelectionKey(nextRow.dataset.selectionKey || "");
      nextRow.focus({ preventScroll: true });
      nextRow.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }

  function renderFolderRow(folderItem) {
    const selectionKey = itemSelectionKey(folderItem);
    const folderDropActive = isActiveFolderDropTarget(dropHint, activeDropTarget, folderItem.path);
    return h(FolderRow, {
      key: selectionKey,
      folder: folderItem,
      editing:
        draftInFolder &&
        inlineFolderDraft.mode === "rename" &&
        inlineFolderDraft.path === folderItem.path,
      editValue: inlineFolderDraft?.value || "",
      isDropTarget: folderDropActive,
      isDragging: draggingFolderPath === folderItem.path,
      selectionKey,
      selected: selectedSet.has(selectionKey),
      tabIndex: selectionKey === rovingFocusKey ? 0 : -1,
      visualSize,
      onToggleSelect: (e) => handleToggleSelect(folderItem, "folder", e),
      onMore: (e) => openContextMenuForItem(e, folderItem, { select: false }),
      onSelect: (e) => onSelectItem && onSelectItem(folderItem, "folder", e, orderedItems),
      onOpen: () => onSelectFolder(folderItem.path),
      onDropEnter: (e) => onDropOnFolder(folderItem.path, e, true),
      onDrop: (e) => onDropOnFolder(folderItem.path, e, false),
      onDropLeave: onClearDropHint,
      onDragStart: (e) =>
        onFolderDragStart &&
        onFolderDragStart(e, folderItem.path, dragItemsFor(folderItem, "folder")),
      onDragEnd: (e) => {
        if (onFolderDragEnd) {
          onFolderDragEnd(e);
        }
      },
      onContextMenu: (e) => onFolderContextMenu && onFolderContextMenu(e, folderItem),
      onEditChange: onInlineFolderNameChange,
      onEditCommit: onCommitInlineFolder,
      onEditCancel: onCancelInlineFolder,
      onFocus: () => setFocusedSelectionKey(selectionKey),
    });
  }

  function renderFileRow(doc) {
    const editing =
      draftInFolder &&
      inlineFolderDraft.mode === "renameFile" &&
      inlineFolderDraft.docId === doc.id;
    const selectionKey = `document:${doc.id}`;
    const lockedByMe = doc.lock?.by === currentUser.id;
    return h(FileRow, {
      key: selectionKey,
      doc,
      currentUser,
      editing,
      editValue: editing ? inlineFolderDraft.value || "" : "",
      searchQuery,
      showSearchPath: Boolean(searchQuery && recursiveSearch),
      selectionKey,
      selected: selectedSet.has(selectionKey),
      tabIndex: selectionKey === rovingFocusKey ? 0 : -1,
      visualSize,
      draggingId,
      doubleClickDownload,
      busy: actions.busy,
      onToggleSelect: (e) => handleToggleSelect(doc, "document", e),
      onDownload: () => actions.handleView?.(doc),
      onUpload: () =>
        actions.handleVersionUploadClick?.(doc, { renameToUploadedName: !lockedByMe }),
      onCheckout: () => actions.handleStartEdit?.(doc),
      onLock: () => (lockedByMe ? actions.handleRelease?.(doc.id) : actions.handleLock?.(doc)),
      onMore: (e) => openContextMenuForItem(e, doc, { select: false }),
      onOpenDetails: actions.openFileDetails,
      onSelect: (e) => onSelectItem && onSelectItem(doc, "document", e, orderedItems),
      onOpen: onOpenFile,
      onDragStart: (e) => onFileDragStart(e, doc.id, dragItemsFor(doc, "document")),
      onDragEnd: (e) => {
        if (onFileDragEnd) {
          onFileDragEnd(e);
        }
      },
      onContextMenu: (e) => onFileContextMenu && onFileContextMenu(e, doc),
      onEditChange: onInlineFolderNameChange,
      onEditCommit: onCommitInlineFolder,
      onEditCancel: onCancelInlineFolder,
      onFocus: () => setFocusedSelectionKey(selectionKey),
    });
  }

  const browserDropActive = isActiveFolderDropTarget(dropHint, activeDropTarget, folder);

  return h(
    "section",
    {
      className: classNames(
        "finder-browser",
        `finder-view-${contentsView.mode}`,
        ...browserScopeClasses({ browserDropActive, dragActive, inArchive, uploadHover })
      ),
      ...canvasDropAttributes({
        browserDropActive,
        folder,
        inArchive,
        onCanvasDragLeave,
        onCanvasDragOver,
        onCanvasDrop,
      }),
      onClick: handleBackgroundClick,
      style: contentColumnStyle(columnWidths),
    },
    [
      h("div", { className: "browser-head" }, [
        h("div", { className: "contents-heading" }, [
          h(
            "p",
            { className: classNames("eyebrow", "tiny", inArchive ? "archived-text" : "") },
            "Contents"
          ),
          h(
            "p",
            {
              className: classNames(
                "muted",
                "tiny",
                "quiet-text",
                inArchive ? "archived-text" : ""
              ),
            },
            `Folders: ${subfolders.length} · Files: ${files.length}`
          ),
        ]),
        h("div", { className: "contents-toolbar" }, [
          h(ContentsViewToolbarControls, {
            allVisibleSelected,
            key: "view-controls",
            mode: contentsView.mode,
            onSelectAllChange: handleSelectAllChange,
            onSortChange,
            sort,
            visibleCount: visibleKeys.length,
          }),
          h(
            "div",
            {
              className: "contents-search",
              key: "search",
              onClick: (e) => e.stopPropagation(),
              onMouseDown: (e) => e.stopPropagation(),
            },
            [
              h(
                "span",
                { className: "contents-search-icon" },
                h(Icon, { icon: "search", size: 15 })
              ),
              h("input", {
                "aria-label": "Search contents",
                onChange: (e) => onSearchQueryChange && onSearchQueryChange(e.target.value),
                placeholder: recursiveSearch ? "Search assets in folders..." : "Search assets...",
                type: "search",
                value: searchQuery,
              }),
              h(
                "button",
                {
                  "aria-label": recursiveSearch
                    ? "Disable recursive search"
                    : "Enable recursive search",
                  "aria-pressed": recursiveSearch,
                  className: classNames("recursive-search-button", recursiveSearch ? "active" : ""),
                  onClick: () =>
                    onRecursiveSearchChange && onRecursiveSearchChange(!recursiveSearch),
                  title: recursiveSearch ? "Searching subfolders" : "Search subfolders",
                  type: "button",
                },
                h(Icon, { icon: "folder-tree", size: 15 })
              ),
            ]
          ),
        ]),
      ]),
      h(ContentsTableHeader, {
        allVisibleSelected,
        headerRef,
        key: "table-header",
        mode: contentsView.mode,
        onSelectAllChange: handleSelectAllChange,
        onSortChange,
        resizeHandlers: {
          end: endColumnResize,
          move: moveColumnResize,
          start: beginColumnResize,
        },
        sort,
        visibleCount: visibleKeys.length,
      }),
      h(
        "div",
        {
          "aria-label": "Folder contents",
          "aria-multiselectable": "true",
          className: classNames(
            "file-list",
            `contents-view-${contentsView.mode}`,
            marquee ? "selecting" : ""
          ),
          onClickCapture: handleMarqueeClickCapture,
          onDragStartCapture: handleMarqueeDragStartCapture,
          onKeyDown: handleCollectionKeyDown,
          onPointerCancel: finishMarquee,
          onPointerDown: handleMarqueePointerDown,
          onPointerMove: handleMarqueePointerMove,
          onPointerUp: finishMarquee,
          ref: fileListRef,
          role: "grid",
        },
        [
          createDraft
            ? h(FolderRow, {
                key: "inline-new-folder",
                folder: {
                  path: "",
                  name: inlineFolderDraft.value,
                },
                editing: true,
                editValue: inlineFolderDraft.value,
                isDraft: true,
                visualSize,
                onOpen: () => {},
                onDropEnter: () => {},
                onDrop: () => {},
                onDropLeave: () => {},
                onDragStart: () => {},
                onDragEnd: () => {},
                onContextMenu: () => {},
                onEditChange: onInlineFolderNameChange,
                onEditCommit: onCommitInlineFolder,
                onEditCancel: onCancelInlineFolder,
              })
            : null,
          ...orderedItems.map((item) =>
            item.type === "folder" ? renderFolderRow(item) : renderFileRow(item)
          ),
          h(ContentsLoadMore, {
            hasMore: contentsHasMore,
            key: "contents-load-more",
            loading: contentsLoadingMore,
            onLoadMore: loadMoreContents,
          }),
          emptyState ? h(EmptyState, { onUpload: onUploadClick, search: searchActive }) : null,
        ]
      ),
      marquee
        ? h("div", {
            className: "selection-marquee",
            style: {
              height: marquee.height,
              left: marquee.left,
              top: marquee.top,
              width: marquee.width,
            },
          })
        : null,
      h(
        "div",
        {
          className: "contents-statusbar",
          onClick: (e) => e.stopPropagation(),
          onMouseDown: (e) => e.stopPropagation(),
        },
        [
          h("div", { className: "contents-selection-readout", key: "selection" }, [
            h(
              "span",
              { className: "selection-count", key: "count" },
              `${selectedItems.length} selected`
            ),
            h(
              "span",
              { key: "meta" },
              `${selectedFiles.length} files · ${selectedFolders.length} folders · ${selectedSizeDisplay}`
            ),
          ]),
          h(ViewModeControl, {
            disabled: Boolean(inlineFolderDraft || dragActive),
            key: "view-mode",
            onChange: requestContentsView,
            view: contentsView,
          }),
        ]
      ),
    ]
  );
}
