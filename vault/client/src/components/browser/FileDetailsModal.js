import { classNames, expiryStatusLabel, formatDate } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const { useCallback, useEffect, useMemo, useRef, useState } = React;
const h = React.createElement;

function formatTimestamp(timestamp) {
  if (!timestamp) {
    return "";
  }
  return formatDate(timestamp, timestamp);
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

function isVersionEntry(item) {
  return item?.type === "version";
}

function isDownloadableVersionEntry(item) {
  return isVersionEntry(item) && Boolean(item.download_url);
}

function countLabel(count, singular, plural) {
  return `${count} ${count === 1 ? singular : plural}`;
}

function locationPartsFor(doc) {
  const isArchived = Boolean(doc?.archived);
  const folderPath = doc?.folder || "";
  const parts = [{ label: isArchived ? "Archive" : "Vault", path: isArchived ? "Archive" : "" }];
  if (isArchived || !folderPath) {
    return parts;
  }
  let running = parts[0].path;
  folderPath
    .split("/")
    .filter(Boolean)
    .forEach((segment) => {
      running = running ? `${running}/${segment}` : segment;
      parts.push({ label: segment, path: running });
    });
  return parts;
}

function MetadataLine({ children }) {
  return h("p", { className: "file-details-meta muted tiny" }, children);
}

export function FileDetailsModal({ actions, doc, onClose }) {
  const [phase, setPhase] = useState("entering");
  const [showFullHistory, setShowFullHistory] = useState(false);
  const closeTimer = useRef(null);
  const closeButton = useRef(null);

  const closeModal = useCallback(() => {
    setPhase("leaving");
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(onClose, 140);
  }, [onClose]);

  useEffect(() => {
    let frame = null;
    frame = window.requestAnimationFrame(() => setPhase("visible"));
    const focusTimer = window.setTimeout(() => closeButton.current?.focus(), 120);

    function handleKeyDown(evt) {
      if (evt.key === "Escape") {
        closeModal();
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(closeTimer.current);
      window.clearTimeout(focusTimer);
      if (frame) {
        window.cancelAnimationFrame(frame);
      }
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [closeModal]);

  useEffect(() => {
    setShowFullHistory(false);
  }, [doc?.id]);

  const historyItems = useMemo(() => doc?.versions || [], [doc?.versions]);
  const versionEntries = useMemo(
    () => historyItems.filter((item) => item.type === "version"),
    [historyItems]
  );
  const versionCount = doc?.version_count || Math.max(versionEntries.length || 0, 1);
  const downloadableVersionItems = useMemo(
    () => historyItems.filter(isDownloadableVersionEntry),
    [historyItems]
  );
  const visibleHistoryItems = showFullHistory ? historyItems : downloadableVersionItems;
  const hiddenHistoryCount = Math.max(historyItems.length - downloadableVersionItems.length, 0);
  const versionPositions = useMemo(() => {
    const map = new Map();
    versionEntries.forEach((item, idx) => {
      const number = item.version_number || versionCount - idx;
      map.set(item.id, number);
    });
    return map;
  }, [versionEntries, versionCount]);
  const locationParts = useMemo(() => locationPartsFor(doc), [doc]);

  if (!doc) {
    return null;
  }

  const isArchived = Boolean(doc.archived);
  const expiryLabel = expiryStatusLabel(doc.expires_at, doc.expiry_action);

  return h("div", { className: classNames("file-details-layer", `phase-${phase}`) }, [
    h("button", {
      "aria-label": "Close file details",
      className: "file-details-backdrop",
      key: "backdrop",
      onClick: closeModal,
      type: "button",
    }),
    h(
      "section",
      {
        "aria-labelledby": "file-details-title",
        "aria-modal": "true",
        className: classNames("file-details-window", isArchived ? "archived-scope" : ""),
        key: "window",
        role: "dialog",
      },
      [
        h("header", { className: "file-details-head", key: "head" }, [
          h("div", { className: "file-details-title", key: "title" }, [
            h("p", { className: "eyebrow tiny", key: "eyebrow" }, "File Details"),
            h("h2", { id: "file-details-title", key: "title" }, doc.name),
            h("div", { className: "file-details-location muted tiny", key: "location" }, [
              "Location: ",
              locationParts
                .map((part, idx) =>
                  h(
                    "button",
                    {
                      className: "linkish crumb-link",
                      key: part.path || `loc-${idx}`,
                      onClick: () => actions.navigateToFolder?.(part.path),
                      type: "button",
                    },
                    part.label
                  )
                )
                .reduce((acc, el, idx) => acc.concat(idx === 0 ? [el] : [" / ", el]), []),
            ]),
          ]),
          h(
            "button",
            {
              "aria-label": "Close",
              className: "settings-close",
              key: "close",
              onClick: closeModal,
              ref: closeButton,
              type: "button",
            },
            h(Icon, { icon: "close", size: 16 })
          ),
        ]),
        h("div", { className: "file-details-body", key: "body" }, [
          h("section", { className: "file-details-card", key: "summary" }, [
            h("h3", null, "Summary"),
            h("div", { className: "file-detail-grid" }, [
              h("div", null, [h("span", null, "Version"), h("strong", null, `v${versionCount}`)]),
              h("div", null, [
                h("span", null, "Modified"),
                h("strong", null, formatDate(doc.modified_at, "No modifications yet")),
              ]),
              h("div", null, [h("span", null, "User"), h("strong", null, doc.latest_by || "-")]),
              h("div", null, [h("span", null, "Size"), h("strong", null, doc.size_display || "-")]),
            ]),
            expiryLabel
              ? h(MetadataLine, null, `${expiryLabel} · ${formatDate(doc.expires_at)}`)
              : null,
          ]),
          h("section", { className: "file-details-card", key: "history" }, [
            h("div", { className: "file-details-card-head history-head" }, [
              h("div", { className: "history-title" }, [
                h("div", { className: "history-label" }, [
                  h("h3", null, "History"),
                  h(
                    "span",
                    { className: "muted tiny history-count" },
                    showFullHistory
                      ? `· ${countLabel(historyItems.length, "event", "events")}`
                      : `· ${countLabel(downloadableVersionItems.length, "version", "versions")}`
                  ),
                ]),
                !showFullHistory && hiddenHistoryCount > 0
                  ? h(
                      "p",
                      { className: "muted tiny history-mode-note" },
                      `${countLabel(hiddenHistoryCount, "activity event", "activity events")} hidden`
                    )
                  : null,
              ]),
              h(
                "button",
                {
                  "aria-pressed": String(showFullHistory),
                  className: classNames("history-mode-toggle", showFullHistory ? "active" : ""),
                  onClick: () => setShowFullHistory((current) => !current),
                  type: "button",
                },
                "Full history"
              ),
            ]),
            visibleHistoryItems.length
              ? h(
                  "ul",
                  { className: "history-list timeline" },
                  visibleHistoryItems.map((item, idx) => {
                    const isVersion = isVersionEntry(item);
                    const versionNumber =
                      versionPositions.get(item.id) ||
                      (item.type === "version" ? versionCount : null);
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
                    return h(
                      "li",
                      {
                        className: classNames(
                          "history-row",
                          "timeline",
                          isVersion ? "version" : "activity"
                        ),
                        key: item.id,
                      },
                      [
                        h("div", { className: "history-marker" }, [
                          h("span", { className: "history-dot" }),
                          idx < visibleHistoryItems.length - 1
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
                                    onClick: () => actions.handleVersionDownload?.(item),
                                    title: "Download this version",
                                    type: "button",
                                  },
                                  "Download"
                                )
                              : null,
                          ]),
                          h("div", { className: "history-meta-line" }, joinMetaPieces(metaPieces)),
                        ]),
                      ]
                    );
                  })
                )
              : h(
                  "p",
                  { className: "muted tiny" },
                  showFullHistory ? "No history yet." : "No downloadable versions yet."
                ),
          ]),
        ]),
      ]
    ),
  ]);
}
