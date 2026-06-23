import { classNames, isArchivePath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { FolderRow } from "./FolderRow.js";
import { FileRow } from "./FileRow.js";
import { EmptyState } from "./EmptyState.js";

const h = React.createElement;
const NAME_COLUMN = { key: "name", label: "Name", className: "name", defaultDirection: "asc" };
const DETAIL_SORT_COLUMNS = [
  { key: "date", label: "Date", className: "date", defaultDirection: "desc" },
  { key: "user", label: "User", className: "user", defaultDirection: "asc" },
  { key: "size", label: "Size", className: "size", defaultDirection: "desc" },
  { key: "ttl", label: "TTL", className: "ttl", defaultDirection: "asc" },
];

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
  selectedKeys = [],
  orderedItems = [],
  sort,
  searchQuery = "",
  recursiveSearch = false,
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
  const inArchive = isArchivePath(folder);
  const draftInFolder = inlineFolderDraft && inlineFolderDraft.parent === (folder || "");
  const createDraft = draftInFolder && inlineFolderDraft.mode === "create";
  const hasRows = files.length > 0 || subfolders.length > 0 || createDraft;
  const emptyState = !hasRows;
  const selectedSet = new Set(selectedKeys);
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

  function handleBackgroundClick(e) {
    if (e.target.closest && e.target.closest(".file-row, .contents-table-head")) {
      return;
    }
    if (onBackgroundClick) {
      onBackgroundClick();
    }
  }

  function renderFolderRow(folderItem) {
    return h(FolderRow, {
      key: `folder:${folderItem.path || ""}`,
      folder: folderItem,
      editing:
        draftInFolder &&
        inlineFolderDraft.mode === "rename" &&
        inlineFolderDraft.path === folderItem.path,
      editValue: inlineFolderDraft?.value || "",
      isDropTarget: dropHint === folderItem.path,
      isDragging: draggingFolderPath === folderItem.path,
      selected: selectedSet.has(`folder:${folderItem.path || ""}`),
      onSelect: (e) => onSelectItem && onSelectItem(folderItem, "folder", e, orderedItems),
      onOpen: () => onSelectFolder(folderItem.path),
      onDropEnter: (e) => onDropOnFolder(folderItem.path, e, true),
      onDrop: (e) => onDropOnFolder(folderItem.path, e, false),
      onDropLeave: onClearDropHint,
      onDragStart: (e) =>
        onFolderDragStart &&
        onFolderDragStart(e, folderItem.path, dragItemsFor(folderItem, "folder")),
      onDragEnd: onFolderDragEnd,
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
    return h(FileRow, {
      key: `document:${doc.id}`,
      doc,
      currentUser,
      editing,
      editValue: editing ? inlineFolderDraft.value || "" : "",
      selected: selectedSet.has(`document:${doc.id}`),
      draggingId,
      onSelect: (e) => onSelectItem && onSelectItem(doc, "document", e, orderedItems),
      onOpen: onOpenFile,
      onDragStart: (e) => onFileDragStart(e, doc.id, dragItemsFor(doc, "document")),
      onDragEnd: onFileDragEnd,
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
        h("div", null, [
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
        emptyState
          ? h("div", { className: "muted tiny" }, "Drop files here to start this folder.")
          : h("div", { className: "muted tiny quiet-text" }, "Select an item to see details."),
      ]),
      h(
        "div",
        {
          className: "contents-table-head",
          onClick: (e) => e.stopPropagation(),
          onMouseDown: (e) => e.stopPropagation(),
        },
        [
          h("span", { className: "contents-sort-spacer", key: "spacer" }),
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
          h("span", { className: "contents-sort-status", key: "status" }),
        ]
      ),
      h("div", { className: "file-list" }, [
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
        emptyState ? h(EmptyState, { onUpload: onUploadClick }) : null,
      ]),
      h(
        "div",
        {
          className: "contents-search",
          onClick: (e) => e.stopPropagation(),
          onMouseDown: (e) => e.stopPropagation(),
        },
        [
          h("span", { className: "contents-search-icon" }, h(Icon, { icon: "search", size: 15 })),
          h("input", {
            type: "search",
            value: searchQuery,
            placeholder: "Search",
            "aria-label": "Search contents",
            onChange: (e) => onSearchQueryChange && onSearchQueryChange(e.target.value),
          }),
          h(
            "button",
            {
              type: "button",
              className: classNames("recursive-search-button", recursiveSearch ? "active" : ""),
              title: recursiveSearch ? "Searching subfolders" : "Search subfolders",
              "aria-label": recursiveSearch
                ? "Disable recursive search"
                : "Enable recursive search",
              "aria-pressed": recursiveSearch,
              onClick: () => onRecursiveSearchChange && onRecursiveSearchChange(!recursiveSearch),
            },
            h(Icon, { icon: "circle-nodes", size: 15 })
          ),
        ]
      ),
    ]
  );
}
