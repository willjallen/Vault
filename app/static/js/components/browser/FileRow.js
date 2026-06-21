import { classNames } from "../../lib/utils.js";
import { StatusBadge } from "../common/StatusBadge.js";
import { FileIcon } from "../common/FileIcon.js";

const h = React.createElement;

// eslint-disable-next-line complexity
export function FileRow({
  doc,
  currentUser,
  selectedId,
  draggingId,
  onSelect,
  onOpen,
  onDragStart,
  onDragEnd,
  onContextMenu,
}) {
  const lock = doc.lock || {};
  const lockedByMe = lock && lock.by === currentUser.id;
  const lockedByOther = lock && lock.by && lock.by !== currentUser.id;
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
  return h(
    "div",
    {
      className: classNames(
        "file-row",
        "file",
        isArchived ? "archived" : "",
        selectedId === doc.id ? "selected" : "",
        draggingId === doc.id ? "dragging" : ""
      ),
      draggable: true,
      onClick: () => onSelect(doc.id),
      onDoubleClick: () => onOpen(doc),
      onDragStart: (e) => onDragStart(e, doc.id),
      onDragEnd: onDragEnd,
      onContextMenu: (e) => {
        if (selectedId !== doc.id) {
          return;
        }
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
        h("div", { className: classNames("name", isArchived ? "archived-text" : "") }, doc.name),
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
          ? h(
              "div",
              { className: "muted tiny" },
              `${doc.latest_updated_display}${doc.latest_by ? ` · ${doc.latest_by}` : ""}`
            )
          : h("div", { className: "muted tiny" }, "No updates yet"),
      ]),
      h("div", { className: "file-cell status-col" }, [
        h(StatusBadge, { doc, currentUserId: currentUser.id, showReady: false }),
        h(
          "span",
          {
            className: classNames("version-chip", selectedId === doc.id ? "visible" : ""),
            title: `Current version: v${versionCount}`,
          },
          `v${versionCount}`
        ),
        lockedByOther
          ? h("span", { className: "muted tiny" }, `Checked out by ${lock.name || lock.by}`)
          : lockedByMe && !doc.archived
            ? h(
                "span",
                { className: "muted tiny linkish", onClick: () => onSelect(doc.id) },
                "Upload edits"
              )
            : null,
      ]),
    ]
  );
}
