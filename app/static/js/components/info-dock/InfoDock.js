import { classNames } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { LockGlyph } from "../common/LockGlyph.js";
import { StatusBadge } from "../common/StatusBadge.js";

const { useEffect, useMemo, useState } = React;
const h = React.createElement;

function formatTimestamp(timestamp) {
  if (!timestamp) {
    return "";
  }
  const dt = new Date(timestamp);
  if (Number.isNaN(dt.getTime())) {
    return timestamp;
  }
  const date = dt.toLocaleDateString(undefined, {
    month: "short",
    day: "2-digit",
    year: "numeric",
  });
  const time = dt.toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  });
  return `${date} at ${time}`;
}

function describeEvent(entry, versionNumber) {
  if (!entry) {
    return "";
  }

  if (entry.type === "version") {
    const via = entry.created_via;
    if (via === "upload") {
      return versionNumber === 1 ? "Initial upload" : "Uploaded file";
    }
    if (via === "checkin") {
      return "Checked in new version";
    }
    if (via === "archive") {
      return "Archived";
    }
    if (via === "unarchive") {
      return "Restored to Vault";
    }
    if (via === "move") {
      return "Moved";
    }
    if (via === "system") {
      return "System update";
    }
    return versionNumber === 1 ? "Initial upload" : "Checked in new version";
  }

  switch (entry.type) {
    case "checkout":
      return "Checked out for editing";
    case "release":
      return "Lock released";
    case "archive":
      return "Moved to Archive";
    case "unarchive":
      return "Restored to Vault";
    case "upload":
      return "Uploaded file";
    case "download":
      return entry.note || "Downloaded";
    case "checkin":
      return "Checked in new version";
    case "move":
      return "Moved";
    default:
      if (entry.note) {
        return entry.note;
      }
      return entry.type ? entry.type.replace(/_/g, " ") : "Activity";
  }
}

function joinMetaPieces(pieces) {
  return pieces.reduce((acc, piece, idx) => {
    if (!piece) {
      return acc;
    }
    if (acc.length) {
      acc.push(h("span", { className: "meta-divider", key: `meta-sep-${idx}` }, "·"));
    }
    acc.push(typeof piece === "string" ? h("span", { key: `meta-${idx}` }, piece) : piece);
    return acc;
  }, []);
}

function filterHistoryItems(historyItems) {
  if (!historyItems || !historyItems.length) {
    return [];
  }

  const versionTimestamps = new Set(
    historyItems
      .filter((item) => item.type === "version" && item.timestamp)
      .map((item) => item.timestamp)
  );

  const redundantTypes = new Set(["upload", "checkin", "archive", "unarchive", "move"]);

  return historyItems.filter((item) => {
    if (item.type === "version" || !item.timestamp) {
      return true;
    }
    if (versionTimestamps.has(item.timestamp) && redundantTypes.has(item.type)) {
      return false;
    }
    return true;
  });
}

function isRootFolder(item) {
  return item.type === "folder" && (!item.path || item.path === "Archive");
}

// eslint-disable-next-line complexity
export function InfoDock({
  doc,
  selectionItems = [],
  currentUserId,
  onDownload,
  onDownloadSelection,
  onDownloadVersion,
  onLock,
  onLockSelection,
  onRename,
  onStartEdit,
  onRelease,
  onReleaseSelection,
  onSave,
  onArchive,
  onArchiveSelection,
  onUnarchive,
  onRestoreSelection,
  onPermanentDelete,
  onDeleteSelection,
  onOpenFolder,
  onMove,
  onMoveSelection,
  isAdmin,
  busy,
}) {
  const selectionCount = selectionItems.length;
  const lockedByMe = doc && doc.lock && doc.lock.by === currentUserId;
  const lockedByOther = doc && doc.lock && doc.lock.by && doc.lock.by !== currentUserId;
  const isArchived = doc?.archived;
  const folderPath = doc?.folder || "";
  const trimmedFolder = isArchived ? folderPath.replace(/^Archive\/?/, "") : folderPath;

  const historyItems = useMemo(() => doc?.versions || [], [doc?.versions]);
  const versionEntries = useMemo(
    () => historyItems.filter((item) => item.type === "version"),
    [historyItems]
  );
  const versionCount = doc?.version_count || Math.max(versionEntries.length || 0, 1);
  const filteredHistoryItems = useMemo(() => filterHistoryItems(historyItems), [historyItems]);
  const historyCount = filteredHistoryItems.length;
  const versionPositions = useMemo(() => {
    const map = new Map();
    versionEntries.forEach((item, idx) => {
      const number = item.version_number || versionCount - idx;
      map.set(item.id, number);
    });
    return map;
  }, [versionEntries, versionCount]);

  const [historyOpen, setHistoryOpen] = useState(historyCount === 0);
  useEffect(() => {
    setHistoryOpen(historyCount === 0);
  }, [doc?.id, historyCount]);

  const locationParts = useMemo(() => {
    const parts = [];
    const baseLabel = isArchived ? "Archive" : "Vault";
    const basePath = isArchived ? "Archive" : "";
    parts.push({ label: baseLabel, path: basePath });
    if (trimmedFolder) {
      let running = basePath;
      trimmedFolder
        .split("/")
        .filter(Boolean)
        .forEach((segment) => {
          running = running ? `${running}/${segment}` : segment;
          parts.push({ label: segment, path: running });
        });
    }
    return parts;
  }, [isArchived, trimmedFolder]);

  if (selectionCount && !(selectionCount === 1 && selectionItems[0].type === "document" && doc)) {
    const files = selectionItems.filter((item) => item.type === "document");
    const folders = selectionItems.filter((item) => item.type === "folder");
    const allArchived = selectionItems.every((item) => item.archived);
    const noneArchived = selectionItems.every((item) => !item.archived);
    const noRoots = selectionItems.every((item) => !isRootFolder(item));
    const sameLocationScope = allArchived || noneArchived;
    const allFiles = files.length === selectionItems.length;
    const lockable =
      allFiles && files.every((item) => !item.archived && !(item.lock && item.lock.by));
    const unlockable =
      allFiles &&
      files.every((item) => item.lock?.by && (item.lock.by === currentUserId || isAdmin));
    const totalSize = selectionItems.reduce((sum, item) => sum + (item.size_bytes || 0), 0);
    return h("div", { className: "info-dock" }, [
      h("p", { className: "eyebrow tiny" }, "Selection"),
      h("h3", null, `${selectionCount} selected`),
      h(
        "p",
        { className: "muted tiny" },
        `${files.length} files · ${folders.length} folders · ${totalSize ? `${totalSize} bytes` : "Size unknown"}`
      ),
      h("div", { className: "actions row wrap compact subtle-actions" }, [
        h(
          "button",
          {
            className: "btn secondary compact",
            type: "button",
            onClick: () => onDownloadSelection && onDownloadSelection(selectionItems),
            disabled: busy || !noRoots,
          },
          "Download"
        ),
        h(
          "button",
          {
            className: "btn secondary compact",
            type: "button",
            onClick: () => onMoveSelection && onMoveSelection(selectionItems),
            disabled: busy || !noRoots || !sameLocationScope,
          },
          "Move..."
        ),
        noneArchived && noRoots
          ? h(
              "button",
              {
                className: "btn secondary compact",
                type: "button",
                onClick: () => onArchiveSelection && onArchiveSelection(selectionItems),
                disabled: busy,
              },
              "Move to Archive"
            )
          : null,
        allArchived && noRoots
          ? h(
              "button",
              {
                className: "btn secondary compact",
                type: "button",
                onClick: () => onRestoreSelection && onRestoreSelection(selectionItems),
                disabled: busy,
              },
              "Restore"
            )
          : null,
        lockable
          ? h(
              "button",
              {
                className: "btn secondary compact",
                type: "button",
                onClick: () => onLockSelection && onLockSelection(selectionItems),
                disabled: busy,
              },
              "Lock"
            )
          : null,
        unlockable
          ? h(
              "button",
              {
                className: "btn secondary compact",
                type: "button",
                onClick: () => onReleaseSelection && onReleaseSelection(selectionItems),
                disabled: busy,
              },
              "Unlock"
            )
          : null,
        allArchived && noRoots && isAdmin
          ? h(
              "button",
              {
                className: "btn danger compact",
                type: "button",
                onClick: () => onDeleteSelection && onDeleteSelection(selectionItems),
                disabled: busy,
              },
              "Delete forever"
            )
          : null,
      ]),
    ]);
  }

  if (!doc) {
    return h("div", { className: "info-dock empty" }, [
      h("p", { className: "eyebrow tiny" }, "Details"),
      h("h3", null, "Select a file or folder"),
      h(
        "p",
        { className: "muted tiny" },
        "Choose something from the list to see more info and actions."
      ),
    ]);
  }

  function handleSave(e) {
    e.preventDefault();
    const file = e.target.elements.file.files[0];
    const note = e.target.elements.note.value;
    if (!file) {
      return;
    }
    onSave(doc.id, file, note);
    e.target.reset();
  }

  const statusText = lockedByMe
    ? "Checked out by you"
    : lockedByOther
      ? `Checked out by ${doc.lock.name || doc.lock.by}`
      : isArchived
        ? "Archived"
        : "Ready to edit";

  const helperText = lockedByMe
    ? "While it's checked out, others can't upload new versions."
    : lockedByOther
      ? `${doc.lock.name || doc.lock.by} is editing this file. You can still open and download it.`
      : isArchived
        ? "Restore this file from Archive before editing."
        : "Reserves this file so only you can change it until you check it back in.";

  const archiveAction = isArchived
    ? h(
        "button",
        {
          className: "btn secondary compact",
          type: "button",
          onClick: () => onUnarchive(doc.id),
          disabled: busy,
        },
        "Restore to Vault"
      )
    : h(
        "button",
        {
          className: "btn secondary compact",
          type: "button",
          onClick: () => onArchive(doc.id),
          disabled: busy || lockedByOther,
        },
        "Move to Archive"
      );

  return h(
    "div",
    { className: classNames("info-dock", isArchived ? "archived-scope" : "") },
    h(
      "div",
      { className: "summary-block" },
      h("div", { className: "summary-top" }, [
        h("div", { className: "summary-title" }, [
          h("p", { className: "eyebrow tiny" }, "Details"),
          h("div", { className: "summary-heading" }, [
            h("h3", null, doc.name),
            h(StatusBadge, { doc, currentUserId, labelOverride: statusText }),
          ]),
          h("div", { className: "muted tiny location-line" }, [
            "Location: ",
            locationParts
              .map((part, idx) =>
                h(
                  "button",
                  {
                    key: part.path || `loc-${idx}`,
                    className: classNames(
                      "linkish",
                      "crumb-link",
                      isArchived ? "archived-text" : ""
                    ),
                    type: "button",
                    onClick: () => onOpenFolder && onOpenFolder(part.path),
                  },
                  part.label
                )
              )
              .reduce((acc, el, idx) => acc.concat(idx === 0 ? [el] : [" / ", el]), []),
          ]),
          h("div", { className: "muted tiny" }, [
            "Current version: ",
            `v${versionCount}`,
            doc.latest_updated_display ? ` · Last updated ${doc.latest_updated_display}` : "",
            doc.latest_by ? ` · ${doc.latest_by}` : "",
          ]),
        ]),
      ])
    ),
    h("div", { className: "info-block editing-block" }, [
      h("div", { className: "block-heading" }, [
        h("p", { className: "eyebrow tiny" }, "Editing"),
        lockedByOther ? h("span", { className: "muted tiny" }, statusText) : null,
        h(
          "button",
          {
            "aria-label": lockedByMe ? "Unlock file" : "Lock file",
            className: classNames("lock-toggle", lockedByMe ? "active" : ""),
            type: "button",
            title: lockedByMe ? "Unlock file" : "Lock file for editing",
            onClick: () => (lockedByMe ? onRelease(doc.id) : onLock && onLock(doc)),
            disabled: busy || lockedByOther || isArchived,
          },
          h(LockGlyph)
        ),
      ]),
      lockedByMe && !isArchived
        ? h(
            "form",
            {
              className: "dock-form checkin-form",
              onSubmit: handleSave,
            },
            [
              h("div", { className: "field-grid" }, [
                h("label", null, [
                  h("span", { className: "muted tiny" }, "Choose edited file"),
                  h("input", { type: "file", name: "file", required: true }),
                ]),
                h("label", null, [
                  h("span", { className: "muted tiny" }, "What changed? (optional)"),
                  h("input", {
                    type: "text",
                    name: "note",
                    placeholder: "Short note",
                  }),
                ]),
              ]),
              h("div", { className: "actions row wrap compact" }, [
                h(
                  "button",
                  { className: "btn primary", type: "submit", disabled: busy },
                  busy ? "Saving..." : "Check in new version"
                ),
                h(
                  "button",
                  {
                    className: "btn ghost",
                    type: "button",
                    onClick: () => onRelease(doc.id),
                    disabled: busy,
                  },
                  "Release without changes"
                ),
              ]),
              h("p", { className: "muted tiny helper-text" }, helperText),
            ]
          )
        : h("div", { className: "editing-actions" }, [
            h(
              "button",
              {
                className: "btn primary",
                type: "button",
                disabled: lockedByOther || isArchived,
                onClick: () => onStartEdit && onStartEdit(doc),
              },
              lockedByOther || isArchived ? "Check out to edit" : "Check out to edit"
            ),
            h("p", { className: "muted tiny helper-text" }, helperText),
          ]),
      h("div", { className: "actions row wrap compact subtle-actions" }, [
        h(
          "button",
          { className: "btn secondary compact", type: "button", onClick: () => onDownload(doc) },
          "Download"
        ),
        h(
          "button",
          {
            className: "btn secondary compact",
            type: "button",
            onClick: () => onRename && onRename(doc),
            disabled: busy || lockedByOther,
          },
          "Rename"
        ),
        h(
          "button",
          {
            className: "btn secondary compact",
            type: "button",
            onClick: () => onMove && onMove(doc),
            disabled: busy || lockedByOther,
          },
          "Move…"
        ),
        archiveAction,
        isAdmin && isArchived
          ? h(
              "button",
              {
                className: "btn danger",
                type: "button",
                onClick: () => onPermanentDelete(doc.id),
                disabled: busy,
              },
              "Delete forever"
            )
          : null,
      ]),
      h(
        "p",
        { className: "muted tiny helper-text" },
        isArchived
          ? "Move this file back into the main vault."
          : "Hide this file from the main vault but keep a backup."
      ),
    ]),
    h("div", { className: "info-block history-block" }, [
      h(
        "button",
        {
          className: "history-toggle",
          type: "button",
          onClick: () => setHistoryOpen((prevOpen) => !prevOpen),
        },
        [
          h("div", { className: "history-label" }, [
            h("span", { className: "eyebrow tiny" }, "History"),
            h(
              "span",
              { className: "muted tiny history-count" },
              `· ${historyCount} ${historyCount === 1 ? "event" : "events"}`
            ),
          ]),
          h(
            "span",
            { className: classNames("chevron", historyOpen ? "open" : "") },
            h(Icon, { icon: "chevron-right", size: 12 })
          ),
        ]
      ),
      historyOpen
        ? filteredHistoryItems && filteredHistoryItems.length
          ? h(
              "ul",
              { className: "history-list timeline" },
              filteredHistoryItems.map((item, idx) => {
                const versionNumber =
                  versionPositions.get(item.id) || (item.type === "version" ? versionCount : null);
                const actionText = describeEvent(item, versionNumber);
                const timestampLabel = formatTimestamp(item.timestamp || item.display);
                const detailText = item.note && item.note !== actionText ? item.note : null;
                const metaPieces = [];
                if (item.type === "version" && item.original_filename) {
                  metaPieces.push(item.original_filename);
                }
                if (item.type === "version" && versionNumber) {
                  metaPieces.push(
                    h(
                      "span",
                      { className: "version-pill tiny", key: `${item.id}-v` },
                      `v${versionNumber}`
                    )
                  );
                }
                if (item.by) {
                  metaPieces.push(item.by);
                }
                if (timestampLabel) {
                  metaPieces.push(timestampLabel);
                }
                if (detailText) {
                  metaPieces.push(detailText);
                }
                return h("li", { key: item.id, className: "history-row timeline" }, [
                  h("div", { className: "history-marker" }, [
                    h("span", { className: "history-dot" }),
                    idx < filteredHistoryItems.length - 1
                      ? h("span", { className: "history-stem" })
                      : null,
                  ]),
                  h("div", { className: "history-body" }, [
                    h("div", { className: "history-top-line" }, [
                      h("span", { className: "history-action" }, actionText),
                      item.download_url
                        ? h(
                            "button",
                            {
                              className: "history-download",
                              onClick: () => onDownloadVersion && onDownloadVersion(item),
                              title: "Download this version",
                              type: "button",
                            },
                            "Download"
                          )
                        : null,
                    ]),
                    h("div", { className: "history-meta-line" }, joinMetaPieces(metaPieces)),
                  ]),
                ]);
              })
            )
          : h("p", { className: "muted tiny" }, "No previous versions yet.")
        : null,
    ])
  );
}
