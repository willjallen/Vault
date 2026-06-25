import {
  classNames,
  formatDate,
  isArchivePath,
  retentionPolicyLabel,
  retentionPolicyStatusLabels,
} from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { RowSelectionIcon } from "./RowSelectionIcon.js";
import { TtlStatusLabel } from "./TtlStatusLabel.js";

const { useEffect, useRef } = React;
const h = React.createElement;

function folderDropAttributes({ editing, folder, isDraft, isDropTarget }) {
  if (editing || isDraft) {
    return {};
  }
  return {
    "data-vault-drop-kind": "folder",
    "data-drop-folder": folder.path || "",
    "data-drop-label": "Move here",
    "data-drop-active": isDropTarget ? "true" : undefined,
  };
}

function folderRetentionStatus(folder) {
  const hasEffectiveRetention =
    folder.effective_ttl_action && folder.effective_ttl_action !== "none";
  const action = hasEffectiveRetention ? folder.effective_ttl_action : folder.default_ttl_action;
  const days = hasEffectiveRetention ? folder.effective_ttl_days : folder.default_ttl_days;
  const label = retentionPolicyLabel(action, days);
  return {
    labels: retentionPolicyStatusLabels(action, days),
    title: label && folder.effective_ttl_inherited ? `${label} · inherited` : label,
  };
}

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
  const committingRef = useRef(false);
  const isArchived = isArchivePath(folder.path || "");
  const retention = folderRetentionStatus(folder);

  useEffect(() => {
    if (!editing || !inputRef.current) {
      return;
    }
    inputRef.current.focus();
    inputRef.current.select();
  }, [editing]);

  function commitEdit() {
    if (!onEditCommit || committingRef.current) {
      return;
    }
    committingRef.current = true;
    const value = inputRef.current ? inputRef.current.value : editValue;
    try {
      const result = onEditCommit(value);
      Promise.resolve(result).finally(() => {
        committingRef.current = false;
      });
    } catch (err) {
      committingRef.current = false;
      throw err;
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
      ...folderDropAttributes({ editing, folder, isDraft, isDropTarget }),
      draggable: !editing && !isDraft && selected,
      tabIndex: editing ? undefined : 0,
      onClick: editing ? undefined : onSelect,
      onDoubleClick: editing || isDraft ? undefined : onOpen,
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
        h("span", { className: "muted tiny" }, formatDate(folder.modified_at))
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
        h("span", {
          "aria-hidden": "true",
          className: "status-empty status-lock",
          key: "lock",
        }),
        retention.labels
          ? h(TtlStatusLabel, {
              className: "policy status-ttl folder-status-ttl",
              labels: retention.labels,
              title: retention.title,
            })
          : h("span", {
              "aria-hidden": "true",
              className: "status-empty status-ttl",
              key: "ttl",
            }),
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
