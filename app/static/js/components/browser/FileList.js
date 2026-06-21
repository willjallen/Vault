import { classNames, isArchivePath } from "../../lib/utils.js";
import { FolderRow } from "./FolderRow.js";
import { FileRow } from "./FileRow.js";
import { EmptyState } from "./EmptyState.js";

const h = React.createElement;

export function VaultFileList({
  folder,
  subfolders,
  files,
  currentUser,
  selectedId,
  draggingId,
  draggingFolderPath,
  dropHint,
  uploadHover,
  onSelectFolder,
  onSelectFile,
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
  const emptyState = files.length === 0 && subfolders.length === 0 && !createDraft;
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
            onOpen: () => onSelectFolder(folderItem.path),
            onDropEnter: (e) => onDropOnFolder(folderItem.path, e, true),
            onDrop: (e) => onDropOnFolder(folderItem.path, e, false),
            onDropLeave: onClearDropHint,
            onDragStart: (e) => onFolderDragStart && onFolderDragStart(e, folderItem.path),
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
            selectedId,
            draggingId,
            onSelect: onSelectFile,
            onOpen: onOpenFile,
            onDragStart: (e) => onFileDragStart(e, doc.id),
            onDragEnd: onFileDragEnd,
            onContextMenu: (e) => onFileContextMenu && onFileContextMenu(e, doc),
          })
        ),
        files.length === 0 && subfolders.length === 0
          ? h(EmptyState, { onUpload: onUploadClick })
          : null,
      ]),
    ]
  );
}
