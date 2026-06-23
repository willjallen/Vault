import { classNames, expiryStatusLabel, formatDate } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const { useCallback, useEffect, useMemo, useRef, useState } = React;
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

function locationPartsFor(doc) {
  const isArchived = Boolean(doc?.archived);
  const folderPath = doc?.folder || "";
  const trimmedFolder = isArchived ? folderPath.replace(/^Archive\/?/, "") : folderPath;
  const parts = [{ label: isArchived ? "Archive" : "Vault", path: isArchived ? "Archive" : "" }];
  if (!trimmedFolder) {
    return parts;
  }
  let running = parts[0].path;
  trimmedFolder
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
  const [historyOpen, setHistoryOpen] = useState(true);
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
    setHistoryOpen(true);
  }, [doc?.id]);

  const historyItems = useMemo(() => doc?.versions || [], [doc?.versions]);
  const versionEntries = useMemo(
    () => historyItems.filter((item) => item.type === "version"),
    [historyItems]
  );
  const versionCount = doc?.version_count || Math.max(versionEntries.length || 0, 1);
  const filteredHistoryItems = useMemo(() => filterHistoryItems(historyItems), [historyItems]);
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
                h("span", null, "Updated"),
                h("strong", null, doc.latest_updated_display || "No updates yet"),
              ]),
              h("div", null, [h("span", null, "User"), h("strong", null, doc.latest_by || "-")]),
              h("div", null, [h("span", null, "Size"), h("strong", null, doc.size_display || "-")]),
            ]),
            expiryLabel
              ? h(MetadataLine, null, `${expiryLabel} · ${formatDate(doc.expires_at)}`)
              : null,
          ]),
          h("section", { className: "file-details-card", key: "history" }, [
            h(
              "button",
              {
                className: "history-toggle",
                onClick: () => setHistoryOpen((current) => !current),
                type: "button",
              },
              [
                h("div", { className: "history-label" }, [
                  h("h3", null, "History"),
                  h(
                    "span",
                    { className: "muted tiny history-count" },
                    `· ${filteredHistoryItems.length} ${
                      filteredHistoryItems.length === 1 ? "event" : "events"
                    }`
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
              ? filteredHistoryItems.length
                ? h(
                    "ul",
                    { className: "history-list timeline" },
                    filteredHistoryItems.map((item, idx) => {
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
                      return h("li", { className: "history-row timeline", key: item.id }, [
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
                      ]);
                    })
                  )
                : h("p", { className: "muted tiny" }, "No previous versions yet.")
              : null,
          ]),
        ]),
      ]
    ),
  ]);
}
