import { classNames, formatBytes, isArchivePath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { FolderRow } from "./FolderRow.js";
import { FileRow } from "./FileRow.js";
import { EmptyState } from "./EmptyState.js";

const { useCallback, useEffect, useRef, useState } = React;
const h = React.createElement;
const NAME_COLUMN = { key: "name", label: "Name", className: "name", defaultDirection: "asc" };
const DETAIL_SORT_COLUMNS = [
  { key: "date", label: "Date", className: "date", defaultDirection: "desc" },
  { key: "user", label: "User", className: "user", defaultDirection: "asc" },
  { key: "size", label: "Size", className: "size", defaultDirection: "desc" },
  { key: "ttl", label: "Status", className: "status", defaultDirection: "asc" },
];
const MARQUEE_MIN_DISTANCE = 4;
const MARQUEE_AUTO_SCROLL_EDGE = 48;
const MARQUEE_AUTO_SCROLL_MAX = 18;
const ROW_DRAG_HOLD_MS = 180;

function itemSelectionKey(item) {
  return item.type === "document" ? `document:${item.id}` : `folder:${item.path || ""}`;
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
        ".contents-table-head, .contents-toolbar, .contents-selection-readout, button, input, textarea, select, a, [role='button'], [contenteditable='true']"
      )
  );
}

function rectsIntersect(a, b) {
  return a.left <= b.right && a.right >= b.left && a.top <= b.bottom && a.bottom >= b.top;
}

function clearNativeSelection() {
  const selection = window.getSelection?.();
  if (selection && selection.rangeCount) {
    selection.removeAllRanges();
  }
}

function combineMarqueeSelection({ baseSelection, hitKeys, mode, orderedKeys }) {
  if (mode === "replace") {
    return hitKeys;
  }
  const baseSet = new Set(baseSelection);
  const hitSet = new Set(hitKeys);
  return orderedKeys.filter((key) => {
    if (mode === "toggle") {
      return hitSet.has(key) ? !baseSet.has(key) : baseSet.has(key);
    }
    return baseSet.has(key) || hitSet.has(key);
  });
}

function ContentsSortButton({ column, sort, onSortChange }) {
  const active = sort?.key === column.key;
  const direction = active ? sort.direction : column.defaultDirection;
  return h(
    "button",
    {
      type: "button",
      className: classNames(
        "contents-sort-button",
        `contents-sort-${column.className}`,
        active ? "active" : ""
      ),
      "aria-sort": active ? (direction === "desc" ? "descending" : "ascending") : "none",
      onClick: (e) => {
        e.stopPropagation();
        if (onSortChange) {
          onSortChange(column.key);
        }
      },
    },
    [
      h("span", { key: "label" }, column.label),
      h(Icon, {
        className: classNames("contents-sort-arrow", active ? "active" : "preview"),
        icon: direction === "desc" ? "arrow-down" : "arrow-up",
        key: "icon",
        size: 10,
      }),
    ]
  );
}

export function VaultFileList({
  folder,
  subfolders,
  files,
  currentUser,
  actions = {},
  selectedKeys = [],
  orderedItems = [],
  sort,
  searchQuery = "",
  recursiveSearch = false,
  contentsPending = false,
  contentsPendingEmptySearch = false,
  draggingId,
  draggingFolderPath,
  dropHint,
  uploadHover,
  onSelectFolder,
  onSelectItem,
  onSearchQueryChange,
  onRecursiveSearchChange,
  onSortChange,
  onBackgroundClick,
  onMarqueeSelectionChange,
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
  const fileListRef = useRef(null);
  const marqueeDragRef = useRef(null);
  const marqueeFrameRef = useRef(null);
  const rowGestureRef = useRef(null);
  const suppressClickRef = useRef(false);
  const [marquee, setMarquee] = useState(null);
  const inArchive = isArchivePath(folder);
  const draftInFolder = inlineFolderDraft && inlineFolderDraft.parent === (folder || "");
  const createDraft = draftInFolder && inlineFolderDraft.mode === "create";
  const hasRows = files.length > 0 || subfolders.length > 0 || createDraft;
  const searchActive = Boolean(searchQuery || recursiveSearch);
  const emptyState = !hasRows && (!contentsPending || contentsPendingEmptySearch);
  const selectedSet = new Set(selectedKeys);
  const orderedKeys = orderedItems.map(itemSelectionKey);
  const selectedItems = orderedItems.filter((item) => selectedSet.has(itemSelectionKey(item)));
  const selectedFiles = selectedItems.filter((item) => item.type === "document");
  const selectedFolders = selectedItems.filter((item) => item.type === "folder");
  const selectedSizeDisplay = formatBytes(
    selectedItems.reduce((sum, item) => sum + (item.size_bytes || 0), 0),
    { emptyForZero: false }
  );

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

    const marqueeLeft = Math.max(Math.min(drag.startX, drag.currentX), listRect.left);
    const marqueeRight = Math.min(Math.max(drag.startX, drag.currentX), listRect.right);
    const marqueeTop = Math.max(Math.min(drag.startY, drag.currentY), listRect.top);
    const marqueeBottom = Math.min(Math.max(drag.startY, drag.currentY), listRect.bottom);
    const marqueeRect = {
      bottom: marqueeBottom,
      left: marqueeLeft,
      right: marqueeRight,
      top: marqueeTop,
    };
    setMarquee({
      height: Math.max(0, marqueeBottom - marqueeTop),
      left: marqueeLeft,
      top: marqueeTop,
      width: Math.max(0, marqueeRight - marqueeLeft),
    });

    const hitKeys = Array.from(list.querySelectorAll("[data-selection-key]"))
      .filter((row) => rectsIntersect(marqueeRect, row.getBoundingClientRect()))
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
    return {
      active: false,
      baseSelection: selectedKeys.slice(),
      currentX: evt.clientX,
      currentY: evt.clientY,
      mode: marqueeModeFromEvent(evt),
      orderedKeys: orderedKeys.slice(),
      pointerId: evt.pointerId,
      startX: evt.clientX,
      startY: evt.clientY,
      ...extra,
    };
  }

  function rowGestureShouldMoveSingle(selectionKey) {
    const gesture = rowGestureRef.current;
    return (
      gesture &&
      gesture.selectionKey === selectionKey &&
      performance.now() - gesture.startedAt >= ROW_DRAG_HOLD_MS
    );
  }

  function singleDragItemFor(item, type) {
    return [
      type === "document"
        ? {
            archived: Boolean(item.archived),
            folder: item.folder || "",
            id: item.id,
            lock: item.lock || {},
            name: item.name,
            path: item.path || (item.folder ? `${item.folder}/${item.name}` : item.name),
            size_bytes: item.size_bytes || 0,
            type: "document",
          }
        : {
            archived: isArchivePath(item.path || ""),
            name: item.name,
            path: item.path || "",
            size_bytes: item.size_bytes || 0,
            type: "folder",
          },
    ];
  }

  function handleMarqueePointerDown(e) {
    if (e.button !== 0 || e.pointerType === "touch" || shouldIgnoreMarqueeTarget(e.target)) {
      return;
    }
    clearNativeSelection();
    const row = e.target.closest?.(".file-row[data-selection-key]");
    if (row) {
      if (selectedKeys.length) {
        return;
      }
      rowGestureRef.current = {
        ...createMarqueeDrag(e),
        selectionKey: row.dataset.selectionKey,
        startedAt: performance.now(),
      };
      return;
    }
    marqueeDragRef.current = createMarqueeDrag(e);
    setMarquee(null);
    e.currentTarget.setPointerCapture?.(e.pointerId);
    e.preventDefault();
  }

  function handleMarqueePointerMove(e) {
    let drag = marqueeDragRef.current;
    const rowGesture = rowGestureRef.current;
    if (!drag && rowGesture && rowGesture.pointerId === e.pointerId) {
      const distance = Math.hypot(e.clientX - rowGesture.startX, e.clientY - rowGesture.startY);
      if (distance < MARQUEE_MIN_DISTANCE) {
        return;
      }
      if (performance.now() - rowGesture.startedAt >= ROW_DRAG_HOLD_MS) {
        return;
      }
      drag = {
        ...rowGesture,
        currentX: e.clientX,
        currentY: e.clientY,
      };
      marqueeDragRef.current = drag;
      rowGestureRef.current = null;
      setMarquee(null);
      e.currentTarget.setPointerCapture?.(e.pointerId);
      clearNativeSelection();
      e.preventDefault();
    }
    if (!drag || drag.pointerId !== e.pointerId) {
      return;
    }
    drag.currentX = e.clientX;
    drag.currentY = e.clientY;
    if (drag.active) {
      clearNativeSelection();
      e.preventDefault();
    }
    scheduleMarqueeUpdate();
  }

  function finishMarquee(e) {
    const drag = marqueeDragRef.current;
    if (!drag || drag.pointerId !== e.pointerId) {
      if (rowGestureRef.current?.pointerId === e.pointerId) {
        rowGestureRef.current = null;
      }
      return;
    }
    drag.currentX = e.clientX;
    drag.currentY = e.clientY;
    cancelMarqueeFrame();
    updateMarqueeSelection();
    if (drag.active) {
      clearNativeSelection();
      suppressClickRef.current = true;
      window.setTimeout(() => {
        suppressClickRef.current = false;
      }, 0);
      e.preventDefault();
    }
    e.currentTarget.releasePointerCapture?.(e.pointerId);
    marqueeDragRef.current = null;
    rowGestureRef.current = null;
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
    const gesture = rowGestureRef.current;
    if (!marqueeDragRef.current && gesture) {
      if (performance.now() - gesture.startedAt >= ROW_DRAG_HOLD_MS) {
        return;
      }
    }
    if (marqueeDragRef.current || gesture) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  useEffect(
    () => () => {
      cancelMarqueeFrame();
    },
    []
  );

  function dragItemsFor(item, type) {
    const key = type === "document" ? `document:${item.id}` : `folder:${item.path || ""}`;
    if (selectedSet.has(key)) {
      return orderedItems.filter((orderedItem) =>
        selectedSet.has(
          orderedItem.type === "document"
            ? `document:${orderedItem.id}`
            : `folder:${orderedItem.path || ""}`
        )
      );
    }
    return [
      type === "document"
        ? {
            archived: Boolean(item.archived),
            folder: item.folder || "",
            id: item.id,
            lock: item.lock || {},
            name: item.name,
            path: item.path || (item.folder ? `${item.folder}/${item.name}` : item.name),
            size_bytes: item.size_bytes || 0,
            type: "document",
          }
        : {
            archived: isArchivePath(item.path || ""),
            name: item.name,
            path: item.path || "",
            size_bytes: item.size_bytes || 0,
            type: "folder",
          },
    ];
  }

  function dragItemsForRow(item, type, selectionKey) {
    if (rowGestureShouldMoveSingle(selectionKey)) {
      return singleDragItemFor(item, type);
    }
    return dragItemsFor(item, type);
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
      e.target.closest(
        ".file-row, .contents-table-head, .contents-toolbar, .contents-selection-readout"
      )
    ) {
      return;
    }
    if (onBackgroundClick) {
      onBackgroundClick();
    }
  }

  function checkboxSelectionEvent(e) {
    return {
      ctrlKey: !e.shiftKey,
      metaKey: false,
      shiftKey: e.shiftKey,
    };
  }

  function handleToggleSelect(item, type, e) {
    if (onSelectItem) {
      onSelectItem(item, type, checkboxSelectionEvent(e), orderedItems);
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

  function renderFolderRow(folderItem) {
    const selectionKey = `folder:${folderItem.path || ""}`;
    return h(FolderRow, {
      key: selectionKey,
      folder: folderItem,
      editing:
        draftInFolder &&
        inlineFolderDraft.mode === "rename" &&
        inlineFolderDraft.path === folderItem.path,
      editValue: inlineFolderDraft?.value || "",
      isDropTarget: dropHint === folderItem.path,
      isDragging: draggingFolderPath === folderItem.path,
      selectionKey,
      selected: selectedSet.has(selectionKey),
      onToggleSelect: (e) => handleToggleSelect(folderItem, "folder", e),
      onMore: (e) => openContextMenuForItem(e, folderItem, { select: false }),
      onSelect: (e) => onSelectItem && onSelectItem(folderItem, "folder", e, orderedItems),
      onOpen: () => onSelectFolder(folderItem.path),
      onDropEnter: (e) => onDropOnFolder(folderItem.path, e, true),
      onDrop: (e) => onDropOnFolder(folderItem.path, e, false),
      onDropLeave: onClearDropHint,
      onDragStart: (e) =>
        onFolderDragStart &&
        onFolderDragStart(e, folderItem.path, dragItemsForRow(folderItem, "folder", selectionKey)),
      onDragEnd: (e) => {
        rowGestureRef.current = null;
        if (onFolderDragEnd) {
          onFolderDragEnd(e);
        }
      },
      onContextMenu: (e) => onFolderContextMenu && onFolderContextMenu(e, folderItem),
      onEditChange: onInlineFolderNameChange,
      onEditCommit: onCommitInlineFolder,
      onEditCancel: onCancelInlineFolder,
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
      selectionKey,
      selected: selectedSet.has(selectionKey),
      draggingId,
      busy: actions.busy,
      onToggleSelect: (e) => handleToggleSelect(doc, "document", e),
      onDownload: () => actions.handleView?.(doc),
      onUpload: () =>
        actions.handleVersionUploadClick?.(doc, { renameToUploadedName: !lockedByMe }),
      onCheckout: () => actions.handleStartEdit?.(doc),
      onLock: () => (lockedByMe ? actions.handleRelease?.(doc.id) : actions.handleLock?.(doc)),
      onMore: (e) => openContextMenuForItem(e, doc, { select: false }),
      onSelect: (e) => onSelectItem && onSelectItem(doc, "document", e, orderedItems),
      onOpen: onOpenFile,
      onDragStart: (e) =>
        onFileDragStart(e, doc.id, dragItemsForRow(doc, "document", selectionKey)),
      onDragEnd: (e) => {
        rowGestureRef.current = null;
        if (onFileDragEnd) {
          onFileDragEnd(e);
        }
      },
      onContextMenu: (e) => onFileContextMenu && onFileContextMenu(e, doc),
      onEditChange: onInlineFolderNameChange,
      onEditCommit: onCommitInlineFolder,
      onEditCancel: onCancelInlineFolder,
    });
  }

  return h(
    "section",
    {
      className: classNames(
        "finder-browser",
        inArchive ? "archived-scope" : "",
        uploadHover ? "upload-hover" : "",
        dropHint === folder ? "drop-target" : ""
      ),
      onDragOver: onCanvasDragOver,
      onDragLeave: onCanvasDragLeave,
      onDrop: onCanvasDrop,
      onClick: handleBackgroundClick,
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
          h(
            "div",
            {
              className: "contents-search",
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
      h(
        "div",
        {
          className: "contents-table-head",
          onClick: (e) => e.stopPropagation(),
          onMouseDown: (e) => e.stopPropagation(),
        },
        [
          h("span", { className: "contents-sort-spacer select", key: "select-spacer" }),
          h("span", { className: "contents-sort-spacer icon", key: "icon-spacer" }),
          h(ContentsSortButton, {
            column: NAME_COLUMN,
            key: NAME_COLUMN.key,
            onSortChange,
            sort,
          }),
          ...DETAIL_SORT_COLUMNS.map((column) =>
            h(ContentsSortButton, {
              column,
              key: column.key,
              onSortChange,
              sort,
            })
          ),
        ]
      ),
      h(
        "div",
        {
          className: classNames("file-list", marquee ? "selecting" : ""),
          onClickCapture: handleMarqueeClickCapture,
          onDragStartCapture: handleMarqueeDragStartCapture,
          onPointerCancel: finishMarquee,
          onPointerDown: handleMarqueePointerDown,
          onPointerMove: handleMarqueePointerMove,
          onPointerUp: finishMarquee,
          ref: fileListRef,
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
          className: "contents-selection-readout",
          onClick: (e) => e.stopPropagation(),
          onMouseDown: (e) => e.stopPropagation(),
        },
        [
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
        ]
      ),
    ]
  );
}
