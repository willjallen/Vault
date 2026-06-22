import { classNames, isArchivePath } from "../../lib/utils.js";
import { FolderRow } from "./FolderRow.js";
import { FileRow } from "./FileRow.js";
import { EmptyState } from "./EmptyState.js";

const h = React.createElement;

function SearchIcon() {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      viewBox: "0 0 20 20",
      width: 16,
      height: 16,
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 1.8,
      strokeLinecap: "round",
      strokeLinejoin: "round",
    },
    [h("circle", { cx: 9, cy: 9, r: 5 }), h("path", { d: "m13 13 4 4" })]
  );
}

function RecursiveSearchIcon() {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      viewBox: "0 0 20 20",
      width: 16,
      height: 16,
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 1.8,
      strokeLinecap: "round",
      strokeLinejoin: "round",
    },
    [
      h("path", { d: "M4 5h4a4 4 0 0 1 4 4v6" }),
      h("path", { d: "M4 15h4a4 4 0 0 0 4-4V5" }),
      h("path", { d: "m9 12 3 3 3-3" }),
      h("path", { d: "m9 8 3-3 3 3" }),
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
    if (e.target.closest && e.target.closest(".file-row")) {
      return;
    }
    if (onBackgroundClick) {
      onBackgroundClick();
    }
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
        ...subfolders.map((folderItem) =>
          h(FolderRow, {
            key: folderItem.path || "root",
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
          })
        ),
        ...files.map((doc) =>
          h(FileRow, {
            key: doc.id,
            doc,
            currentUser,
            selected: selectedSet.has(`document:${doc.id}`),
            draggingId,
            onSelect: (e) => onSelectItem && onSelectItem(doc, "document", e, orderedItems),
            onOpen: onOpenFile,
            onDragStart: (e) => onFileDragStart(e, doc.id, dragItemsFor(doc, "document")),
            onDragEnd: onFileDragEnd,
            onContextMenu: (e) => onFileContextMenu && onFileContextMenu(e, doc),
          })
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
          h("span", { className: "contents-search-icon" }, h(SearchIcon)),
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
            h(RecursiveSearchIcon)
          ),
        ]
      ),
    ]
  );
}
