import { classNames, expiryStatusLabel, formatDate } from "../../lib/utils.js";
import { FileIcon } from "../common/FileIcon.js";
import { Icon } from "../common/Icon.js";

const { useEffect, useRef } = React;
const h = React.createElement;

// eslint-disable-next-line complexity
export function FileRow({
  doc,
  currentUser,
  busy,
  editing,
  editValue,
  selectionKey = "",
  selected,
  draggingId,
  onToggleSelect,
  onDownload,
  onUpload,
  onCheckout,
  onLock,
  onMore,
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
  const lockedByOther = locked && lock.by !== currentUser.id;
  const isArchived = doc.archived;
  const versionCount =
    doc.version_count ||
    Math.max((doc.versions || []).filter((item) => item.type === "version").length || 0, 1);
  const lockHolderName = locked
    ? lock.name || (lock.by === currentUser.id ? currentUser.name : lock.by)
    : "";
  const lockButtonTitle = locked
    ? lockedByOther
      ? `Locked by ${lockHolderName}`
      : "Unlock file"
    : "Lock for editing";
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
      h(
        "div",
        { className: "file-cell select" },
        h("input", {
          "aria-label": selected ? `Deselect ${doc.name}` : `Select ${doc.name}`,
          checked: Boolean(selected),
          className: "row-checkbox",
          disabled: editing,
          onChange: () => {},
          onClick: (e) => stopRowAction(e, onToggleSelect),
          type: "checkbox",
        })
      ),
      h(
        "div",
        { className: "file-cell icon" },
        h(FileIcon, { fileName: doc.name, kind: "file", size: 13 })
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
          : h("div", { className: "file-name-line" }, [
              h(
                "div",
                { className: classNames("name", isArchived ? "archived-text" : "") },
                doc.name
              ),
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
      h("div", { className: "file-cell status-col" }, [
        h(
          "span",
          {
            className: "version-chip status-version",
            title: `Current version: v${versionCount}`,
          },
          `v${versionCount}`
        ),
        locked
          ? h(
              "span",
              {
                className: "file-lock-indicator status-lock",
                title: `Checked out by ${lockHolderName}`,
              },
              [
                h(Icon, { icon: "lock", key: "icon", size: 11 }),
                h("span", { key: "label" }, lockHolderName),
              ]
            )
          : h("span", { "aria-hidden": "true", className: "status-empty status-lock" }),
        expiryLabel
          ? h("span", { className: "ttl-chip applied status-ttl", title: expiryTitle }, [
              h(Icon, { icon: "clock", key: "icon", size: 11 }),
              h("span", { key: "label" }, expiryLabel),
            ])
          : h("span", { "aria-hidden": "true", className: "status-empty status-ttl" }),
      ]),
      h("div", { className: "file-cell row-actions" }, [
        h(
          "button",
          {
            "aria-label": `Download ${doc.name}`,
            className: "row-action-button",
            onClick: (e) => stopRowAction(e, onDownload),
            title: "Download",
            type: "button",
          },
          h(Icon, { icon: "download", size: 14 })
        ),
        h(
          "button",
          {
            "aria-label": locked
              ? `Upload checked-out version for ${doc.name}`
              : `Upload replacement for ${doc.name}`,
            className: "row-action-button",
            disabled: busy || isArchived || lockedByOther,
            onClick: (e) => stopRowAction(e, onUpload),
            title: locked ? "Upload checked-out version" : "Upload replacement",
            type: "button",
          },
          h(Icon, { icon: locked ? "file-upload" : "upload", size: 14 })
        ),
        locked
          ? null
          : h(
              "button",
              {
                "aria-label": `Check out ${doc.name}`,
                className: "row-action-button checkout",
                disabled: busy || isArchived,
                onClick: (e) => stopRowAction(e, onCheckout),
                title: "Check out",
                type: "button",
              },
              h(Icon, { icon: "file-download", size: 14 })
            ),
        h(
          "button",
          {
            "aria-label": locked ? lockButtonTitle : `Lock ${doc.name}`,
            className: classNames("row-action-button", "row-lock-button", locked ? "locked" : ""),
            disabled: busy || isArchived || lockedByOther,
            onClick: (e) => stopRowAction(e, onLock),
            title: lockButtonTitle,
            type: "button",
          },
          h(Icon, { icon: "lock", size: 14 })
        ),
        h(
          "button",
          {
            "aria-label": `More actions for ${doc.name}`,
            className: "row-action-button more",
            onClick: (e) => stopRowAction(e, onMore),
            title: "More actions",
            type: "button",
          },
          h(Icon, { icon: "ellipsis", size: 14 })
        ),
      ]),
    ]
  );
}
