import { classNames } from "../lib/utils.js";

const h = React.createElement;

function formatBytes(bytes) {
  if (!bytes || bytes < 0) {
    return "";
  }
  let value = bytes;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < 4) {
    value /= 1024;
    unitIndex += 1;
  }
  let unit = "B";
  if (unitIndex === 1) {
    unit = "KB";
  } else if (unitIndex === 2) {
    unit = "MB";
  } else if (unitIndex === 3) {
    unit = "GB";
  } else if (unitIndex === 4) {
    unit = "TB";
  }
  return unitIndex === 0 ? `${value} ${unit}` : `${value.toFixed(1)} ${unit}`;
}

function formatEta(seconds) {
  if (!seconds) {
    return "";
  }
  if (seconds < 1) {
    return "Less than 1s left";
  }
  if (seconds < 60) {
    return `${Math.ceil(seconds)}s left`;
  }
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = Math.ceil(seconds % 60);
  return `${minutes}m ${String(remainingSeconds).padStart(2, "0")}s left`;
}

function transferTitle(transfer) {
  if (transfer.status === "complete") {
    return transfer.kind === "upload" ? "Uploaded" : "Downloaded";
  }
  if (transfer.status === "error") {
    return transfer.kind === "upload" ? "Upload failed" : "Download failed";
  }
  return transfer.kind === "upload" ? "Uploading" : "Downloading";
}

function transferMeta(transfer) {
  if (transfer.status === "error") {
    return transfer.error || "Transfer failed";
  }
  if (transfer.status === "complete") {
    return transfer.total ? `${formatBytes(transfer.total)} complete` : "Complete";
  }

  const pieces = [];
  if (transfer.percent !== null && transfer.percent !== undefined) {
    pieces.push(`${transfer.percent}%`);
  }
  if (transfer.loaded && transfer.total) {
    pieces.push(`${formatBytes(transfer.loaded)} of ${formatBytes(transfer.total)}`);
  } else if (transfer.loaded) {
    pieces.push(formatBytes(transfer.loaded));
  }
  const eta = formatEta(transfer.etaSeconds);
  if (eta) {
    pieces.push(eta);
  }
  return pieces.join(" - ") || "Starting";
}

function TransferIcon({ kind }) {
  return h(
    "span",
    { className: classNames("transfer-icon", kind === "upload" ? "uploading" : "downloading") },
    kind === "upload" ? "↑" : "↓"
  );
}

function TransferRow({ transfer }) {
  const percent =
    transfer.percent !== null && transfer.percent !== undefined ? `${transfer.percent}%` : "100%";
  const phase = transfer.phase || "visible";
  return h(
    "div",
    {
      className: classNames("transfer-row", transfer.kind, transfer.status, `phase-${phase}`),
    },
    [
      h(TransferIcon, { kind: transfer.kind, key: "icon" }),
      h("div", { className: "transfer-copy", key: "copy" }, [
        h("div", { className: "transfer-line", key: "line" }, [
          h("span", { className: "transfer-title", key: "title" }, transferTitle(transfer)),
          h("span", { className: "transfer-name", key: "name" }, transfer.name),
        ]),
        h("div", { className: "transfer-meta", key: "meta" }, transferMeta(transfer)),
        h(
          "div",
          {
            className: classNames(
              "transfer-progress",
              transfer.percent === null || transfer.percent === undefined ? "indeterminate" : ""
            ),
            key: "progress",
          },
          h("span", { style: { width: percent } })
        ),
      ]),
    ]
  );
}

export function TransferDock({ transfers }) {
  if (!transfers.length) {
    return null;
  }

  return h(
    "div",
    { "aria-live": "polite", className: "transfer-dock" },
    transfers.map((transfer) => h(TransferRow, { key: transfer.id, transfer }))
  );
}
