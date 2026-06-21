import { classNames, isArchivePath } from "../../lib/utils.js";
import { FileIcon } from "../common/FileIcon.js";

const { useEffect, useRef } = React;
const h = React.createElement;

export function FolderRow({
  folder,
  editing,
  editValue,
  isDraft,
  isDropTarget,
  isDragging,
  onOpen,
  onDropEnter,
  onDrop,
  onDropLeave,
  onDragStart,
  onDragEnd,
  onContextMenu,
  onEditChange,
  onEditCommit,
  onEditCancel,
}) {
  const inputRef = useRef(null);
  const isArchived = isArchivePath(folder.path || "");
  const parentPath = (folder.path || "").split("/").slice(0, -1).join("/");
  const parentWithinArchive = parentPath.startsWith("Archive");
  const trimmedParent = parentWithinArchive ? parentPath.replace(/^Archive\/?/, "") : parentPath;
  const parentLabel = trimmedParent
    ? `In ${parentWithinArchive ? `Archive / ${trimmedParent}` : trimmedParent}`
    : parentWithinArchive || isArchived
      ? "In Archive"
      : "In Vault";

  useEffect(() => {
    if (!editing || !inputRef.current) {
      return;
    }
    inputRef.current.focus();
    inputRef.current.select();
  }, [editing]);

  function commitEdit() {
    if (onEditCommit) {
      onEditCommit(editValue);
    }
  }

  function cancelEdit() {
    if (onEditCancel) {
      onEditCancel();
    }
  }

  return h(
    "div",
    {
      className: classNames(
        "file-row",
        "folder",
        isArchived ? "archived" : "",
        isDropTarget ? "drop-target" : "",
        isDragging ? "dragging" : "",
        editing ? "editing" : ""
      ),
      draggable: !editing && !isDraft,
      onClick: editing ? undefined : onOpen,
      onDoubleClick: editing ? undefined : onOpen,
      onContextMenu: (e) => {
        if (editing) {
          e.preventDefault();
          e.stopPropagation();
          return;
        }
        e.preventDefault();
        if (onContextMenu) {
          onContextMenu(e);
        }
      },
      onDragStart,
      onDragEnd,
      onDragEnter: editing ? undefined : onDropEnter,
      onDragOver: (e) => {
        e.preventDefault();
        if (!editing) {
          e.dataTransfer.dropEffect = "move";
        }
      },
      onDragLeave: (e) => {
        if (!editing && !e.currentTarget.contains(e.relatedTarget)) {
          onDropLeave();
        }
      },
      onDrop: editing ? undefined : onDrop,
    },
    [
      h("div", { className: "file-cell icon" }, h(FileIcon, { kind: "folder" })),
      h("div", { className: "file-cell main" }, [
        editing
          ? h("input", {
              ref: inputRef,
              className: "inline-name-editor",
              type: "text",
              value: editValue,
              onClick: (e) => e.stopPropagation(),
              onChange: (e) => onEditChange && onEditChange(e.target.value),
              onBlur: commitEdit,
              onKeyDown: (e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  commitEdit();
                }
                if (e.key === "Escape") {
                  e.preventDefault();
                  cancelEdit();
                }
              },
            })
          : h(
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
