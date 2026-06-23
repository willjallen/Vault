import { classNames, formatDate, isArchivePath, retentionPolicyLabel } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { RowSelectionIcon } from "./RowSelectionIcon.js";

const { useEffect, useRef } = React;
const h = React.createElement;

export function FolderRow({
  folder,
  editing,
  editValue,
  isDraft,
  selectionKey = "",
  selected,
  isDropTarget,
  isDragging,
  onToggleSelect,
  onMore,
  onOpen,
  onSelect,
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
  const retentionLabel = retentionPolicyLabel(folder.default_ttl_action, folder.default_ttl_days);

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

  function stopRowAction(e, action) {
    e.preventDefault();
    e.stopPropagation();
    if (action) {
      action(e);
    }
  }

  return h(
    "div",
    {
      className: classNames(
        "file-row",
        "folder",
        isArchived ? "archived" : "",
        selected ? "selected" : "",
        isDropTarget ? "drop-target" : "",
        isDragging ? "dragging" : "",
        editing ? "editing" : ""
      ),
      "data-selection-key": selectionKey || undefined,
      draggable: !editing && !isDraft,
      tabIndex: editing ? undefined : 0,
      onClick: editing ? undefined : onSelect,
      onKeyDown: editing
        ? undefined
        : (e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              onOpen();
            }
          },
      onContextMenu: (e) => {
        if (editing) {
          e.preventDefault();
          e.stopPropagation();
          return;
        }
        e.preventDefault();
        e.stopPropagation();
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
      h(
        "div",
        { className: "file-cell icon" },
        h(RowSelectionIcon, {
          color: folder.color,
          disabled: editing,
          folderIcon: folder.icon,
          interactive: !isDraft,
          kind: "folder",
          label: selected ? `Deselect ${folder.name}` : `Select ${folder.name}`,
          onSelect: onToggleSelect,
          selected,
          size: 12,
        })
      ),
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
      ]),
      h(
        "div",
        { className: "file-cell meta" },
        h("span", { className: "muted tiny" }, formatDate(folder.latest_updated_at))
      ),
      h(
        "div",
        { className: "file-cell user" },
        h("span", { className: "muted tiny" }, folder.latest_by || "-")
      ),
      h(
        "div",
        { className: "file-cell size" },
        h("span", { className: "muted tiny" }, folder.size_display || "0 B")
      ),
      h("div", { className: "file-cell status-col" }, [
        h("span", { className: "status-pill subtle status-version" }, "Folder"),
        h("span", { "aria-hidden": "true", className: "status-empty status-lock" }),
        retentionLabel
          ? h("span", { className: "ttl-chip policy status-ttl", title: retentionLabel }, [
              h(Icon, { icon: "clock", key: "icon", size: 11 }),
              h("span", { key: "label" }, retentionLabel),
            ])
          : h("span", { "aria-hidden": "true", className: "status-empty status-ttl" }),
      ]),
      h(
        "div",
        { className: "file-cell row-actions" },
        isDraft
          ? null
          : h(
              "button",
              {
                "aria-label": `More actions for ${folder.name}`,
                className: "row-action-button more",
                onClick: (e) => stopRowAction(e, onMore),
                title: "More actions",
                type: "button",
              },
              h(Icon, { icon: "ellipsis", size: 14 })
            )
      ),
    ]
  );
}
