import { classNames, expiryStatusLabel, expiryStatusLabels, formatDate } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { RowSelectionIcon } from "./RowSelectionIcon.js";
import { TtlStatusLabel } from "./TtlStatusLabel.js";

const { useEffect, useRef } = React;
const h = React.createElement;

function highlightedFileName(fileNameValue, query) {
  const fileName = String(fileNameValue || "");
  const needle = String(query || "").trim();
  if (!needle) {
    return fileName;
  }

  const lowerName = fileName.toLocaleLowerCase();
  const lowerNeedle = needle.toLocaleLowerCase();
  const parts = [];
  let cursor = 0;
  let matchIndex = lowerName.indexOf(lowerNeedle);
  while (matchIndex !== -1) {
    if (matchIndex > cursor) {
      parts.push(fileName.slice(cursor, matchIndex));
    }
    const matchEnd = matchIndex + needle.length;
    parts.push(
      h(
        "span",
        { className: "file-name-match", key: `match-${matchIndex}` },
        fileName.slice(matchIndex, matchEnd)
      )
    );
    cursor = matchEnd;
    matchIndex = lowerName.indexOf(lowerNeedle, cursor);
  }

  if (!parts.length) {
    return fileName;
  }
  if (cursor < fileName.length) {
    parts.push(fileName.slice(cursor));
  }
  return parts;
}

// eslint-disable-next-line complexity
export function FileRow({
  doc,
  currentUser,
  doubleClickDownload = false,
  busy,
  editing,
  editValue,
  searchQuery = "",
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
  const committingRef = useRef(false);
  const lock = doc.lock || {};
  const locked = Boolean(lock && lock.by);
  const lockedByMe = locked && lock.by === currentUser.id;
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
  const expiryLabels = expiryStatusLabels(doc.expires_at, doc.expiry_action);
  const expiryDateLabel = doc.expires_at ? formatDate(doc.expires_at) : "";
  const expiryTitle =
    expiryLabel && expiryDateLabel ? `${expiryLabel} · ${expiryDateLabel}` : expiryDateLabel;

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
        "file",
        isArchived ? "archived" : "",
        selected ? "selected" : "",
        draggingId === doc.id ? "dragging" : "",
        editing ? "editing" : ""
      ),
      "data-selection-key": selectionKey || undefined,
      draggable: !editing && selected,
      onClick: editing ? undefined : onSelect,
      onDoubleClick: editing || !doubleClickDownload ? undefined : () => onOpen(doc),
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
        { className: "file-cell icon" },
        h(RowSelectionIcon, {
          disabled: editing,
          fileName: doc.name,
          kind: "file",
          label: selected ? `Deselect ${doc.name}` : `Select ${doc.name}`,
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
          : h("div", { className: "file-name-line" }, [
              h(
                "div",
                { className: classNames("name", isArchived ? "archived-text" : "") },
                highlightedFileName(doc.name, searchQuery)
              ),
            ]),
      ]),
      h("div", { className: "file-cell meta" }, [
        h("div", { className: "muted tiny" }, formatDate(doc.modified_at, "No modifications yet")),
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
        expiryLabels
          ? h(TtlStatusLabel, {
              className: "applied status-ttl",
              labels: expiryLabels,
              title: expiryTitle,
            })
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
            className: classNames("row-action-button", lockedByMe ? "checked-out-upload" : ""),
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
