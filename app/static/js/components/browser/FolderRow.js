import { classNames, isArchivePath } from "../../lib/utils.js";
import { FileIcon } from "../common/FileIcon.js";

const h = React.createElement;

export function FolderRow({
  folder,
  isDropTarget,
  isDragging,
  onOpen,
  onDropEnter,
  onDrop,
  onDropLeave,
  onDragStart,
  onDragEnd,
  onContextMenu,
}) {
  const isArchived = isArchivePath(folder.path || "");
  const parentPath = (folder.path || "").split("/").slice(0, -1).join("/");
  const parentWithinArchive = parentPath.startsWith("Archive");
  const trimmedParent = parentWithinArchive ? parentPath.replace(/^Archive\/?/, "") : parentPath;
  const parentLabel = trimmedParent
    ? `In ${parentWithinArchive ? `Archive / ${trimmedParent}` : trimmedParent}`
    : parentWithinArchive || isArchived
      ? "In Archive"
      : "In Vault";
  return h(
    "div",
    {
      className: classNames(
        "file-row",
        "folder",
        isArchived ? "archived" : "",
        isDropTarget ? "drop-target" : "",
        isDragging ? "dragging" : ""
      ),
      draggable: true,
      onClick: onOpen,
      onDoubleClick: onOpen,
      onContextMenu: (e) => {
        e.preventDefault();
        if (onContextMenu) {
          onContextMenu(e);
        }
      },
      onDragStart,
      onDragEnd,
      onDragEnter: onDropEnter,
      onDragOver: (e) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
      },
      onDragLeave: (e) => {
        if (!e.currentTarget.contains(e.relatedTarget)) {
          onDropLeave();
        }
      },
      onDrop,
    },
    [
      h("div", { className: "file-cell icon" }, h(FileIcon, { kind: "folder" })),
      h("div", { className: "file-cell main" }, [
        h(
          "div",
          { className: classNames("name", isArchived ? "archived-text" : "") },
          folder.name || "Folder"
        ),
        h(
          "div",
          {
            className: classNames("muted", "tiny", "quiet-text", isArchived ? "archived-text" : ""),
          },
          parentLabel
        ),
      ]),
      h("div", { className: "file-cell meta" }, h("span", { className: "muted tiny" }, "Folder")),
      h("div", { className: "file-cell status-col" }, h("span", { className: "row-chevron" }, "›")),
    ]
  );
}
