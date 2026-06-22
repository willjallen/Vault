import { classNames } from "../../lib/utils.js";
import { FileIcon } from "../common/FileIcon.js";
import { LockGlyph } from "../common/LockGlyph.js";

const h = React.createElement;

// eslint-disable-next-line complexity
export function FileRow({
  doc,
  currentUser,
  selected,
  draggingId,
  onSelect,
  onOpen,
  onDragStart,
  onDragEnd,
  onContextMenu,
}) {
  const lock = doc.lock || {};
  const locked = Boolean(lock && lock.by);
  const isArchived = doc.archived;
  const folderPath = doc.folder || "";
  const relativeFolder = isArchived ? folderPath.replace(/^Archive\/?/, "") : folderPath;
  const locationLabel = isArchived
    ? `Archive${relativeFolder ? ` / ${relativeFolder}` : ""}`
    : relativeFolder
      ? `Vault / ${relativeFolder}`
      : "Vault";
  const metaLabel = isArchived ? `Archived · ${locationLabel}` : locationLabel;
  const versionCount =
    doc.version_count ||
    Math.max((doc.versions || []).filter((item) => item.type === "version").length || 0, 1);
  const lockHolderName = locked
    ? lock.name || (lock.by === currentUser.id ? currentUser.name : lock.by)
    : "";
  return h(
    "div",
    {
      className: classNames(
        "file-row",
        "file",
        isArchived ? "archived" : "",
        selected ? "selected" : "",
        draggingId === doc.id ? "dragging" : ""
      ),
      draggable: true,
      onClick: onSelect,
      onDoubleClick: () => onOpen(doc),
      onDragStart: (e) => onDragStart(e, doc.id),
      onDragEnd: onDragEnd,
      onContextMenu: (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (onContextMenu) {
          onContextMenu(e);
        }
      },
    },
    [
      h("div", { className: "file-cell icon" }, h(FileIcon, { kind: "file" })),
      h("div", { className: "file-cell main" }, [
        h("div", { className: "file-name-line" }, [
          h("div", { className: classNames("name", isArchived ? "archived-text" : "") }, doc.name),
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
        h(
          "div",
          {
            className: classNames("muted", "tiny", "quiet-text", isArchived ? "archived-text" : ""),
          },
          metaLabel
        ),
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
            className: classNames("version-chip", selected ? "visible" : ""),
            title: `Current version: v${versionCount}`,
          },
          `v${versionCount}`
        ),
      ]),
    ]
  );
}
