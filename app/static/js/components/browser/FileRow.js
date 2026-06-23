import { classNames, expiryStatusLabel, formatDate } from "../../lib/utils.js";
import { FileIcon } from "../common/FileIcon.js";
import { Icon } from "../common/Icon.js";
import { LockGlyph } from "../common/LockGlyph.js";

const { useEffect, useRef } = React;
const h = React.createElement;

// eslint-disable-next-line complexity
export function FileRow({
  doc,
  currentUser,
  editing,
  editValue,
  selectionKey = "",
  selected,
  draggingId,
  onSelect,
  onOpen,
  onDragStart,
  onDragEnd,
  onContextMenu,
  onEditChange,
  onEditCommit,
  onEditCancel,
}) {
  const inputRef = useRef(null);
  const lock = doc.lock || {};
  const locked = Boolean(lock && lock.by);
  const isArchived = doc.archived;
  const versionCount =
    doc.version_count ||
    Math.max((doc.versions || []).filter((item) => item.type === "version").length || 0, 1);
  const lockHolderName = locked
    ? lock.name || (lock.by === currentUser.id ? currentUser.name : lock.by)
    : "";
  const expiryLabel = expiryStatusLabel(doc.expires_at, doc.expiry_action);
  const expiryTitle = doc.expires_at ? formatDate(doc.expires_at) : "";

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
        "file",
        isArchived ? "archived" : "",
        selected ? "selected" : "",
        draggingId === doc.id ? "dragging" : "",
        editing ? "editing" : ""
      ),
      "data-selection-key": selectionKey || undefined,
      draggable: !editing,
      onClick: editing ? undefined : onSelect,
      onDoubleClick: editing ? undefined : () => onOpen(doc),
      onDragStart: editing ? undefined : (e) => onDragStart(e, doc.id),
      onDragEnd: editing ? undefined : onDragEnd,
      onContextMenu: (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (editing) {
          return;
        }
        if (onContextMenu) {
          onContextMenu(e);
        }
      },
    },
    [
      h("div", { className: "file-cell icon" }, h(FileIcon, { fileName: doc.name, kind: "file" })),
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
          : h("div", { className: "file-name-line" }, [
              h(
                "div",
                { className: classNames("name", isArchived ? "archived-text" : "") },
                doc.name
              ),
              locked
                ? h(
                    "span",
                    {
                      className: "file-lock-indicator",
                      title: `Checked out by ${lockHolderName}`,
                    },
                    [h(LockGlyph), h("span", null, lockHolderName)]
                  )
                : null,
            ]),
      ]),
      h("div", { className: "file-cell meta" }, [
        doc.latest_updated_display
          ? h("div", { className: "muted tiny" }, doc.latest_updated_display)
          : h("div", { className: "muted tiny" }, "No updates yet"),
      ]),
      h(
        "div",
        { className: "file-cell user" },
        h("span", { className: "muted tiny" }, doc.latest_by || "-")
      ),
      h(
        "div",
        { className: "file-cell size" },
        h("span", { className: "muted tiny" }, doc.size_display || "-")
      ),
      h(
        "div",
        { className: "file-cell ttl" },
        expiryLabel
          ? h("span", { className: "ttl-chip applied", title: expiryTitle }, [
              h(Icon, { icon: "clock", key: "icon", size: 11 }),
              h("span", { key: "label" }, expiryLabel),
            ])
          : h("span", { className: "muted tiny" }, "-")
      ),
      h("div", { className: "file-cell status-col" }, [
        h(
          "span",
          {
            className: classNames("version-chip", selected ? "visible" : ""),
            title: `Current version: v${versionCount}`,
          },
          `v${versionCount}`
        ),
      ]),
    ]
  );
}
