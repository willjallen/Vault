import { classNames, formatBytes } from "../lib/utils.js";
import { Icon } from "./common/Icon.js";

const h = React.createElement;

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
  if (transfer.status === "cancelled") {
    return transfer.kind === "upload" ? "Upload cancelled" : "Download cancelled";
  }
  if (transfer.status === "cancelling") {
    return "Cancelling";
  }
  if (transfer.status === "error") {
    return transfer.kind === "upload" ? "Upload failed" : "Download failed";
  }
  if (transfer.kind === "upload" && transfer.stage === "verifying") {
    return "Verifying upload";
  }
  if (transfer.kind === "download" && transfer.stage === "preparing") {
    return "Preparing download";
  }
  if (transfer.kind === "download" && transfer.stage === "server-finalizing") {
    return "Finalizing export";
  }
  if (transfer.kind === "download" && transfer.stage === "starting") {
    return "Starting download";
  }
  if (transfer.kind === "download" && transfer.stage === "finalizing") {
    return "Saving download";
  }
  return transfer.kind === "upload" ? "Uploading" : "Downloading";
}

function transferStageLabel(transfer) {
  if (transfer.kind === "upload" && transfer.stage === "verifying") {
    return "Server verification";
  }
  if (transfer.kind === "download" && transfer.stage === "preparing") {
    return "Server export";
  }
  if (transfer.kind === "download" && transfer.stage === "server-finalizing") {
    return "Server finalization";
  }
  if (transfer.kind === "download" && transfer.stage === "starting") {
    return "Browser handoff";
  }
  if (transfer.kind === "download" && transfer.stage === "finalizing") {
    return "File save";
  }
  return transfer.kind === "upload" ? "File upload" : "Download";
}

function formatPercent(percent) {
  if (percent === null || percent === undefined) {
    return "";
  }
  if (percent > 0 && percent < 1) {
    return "<1%";
  }
  if (percent < 10 && percent % 1 !== 0) {
    return `${percent.toFixed(1)}%`;
  }
  return `${Math.floor(percent)}%`;
}

function transferMeta(transfer) {
  if (transfer.status === "cancelled") {
    return "Cancelled";
  }
  if (transfer.status === "cancelling") {
    return "Stopping transfer";
  }
  if (transfer.status === "error") {
    return transfer.error || "Transfer failed";
  }
  if (transfer.status === "complete") {
    return transfer.total ? `${formatBytes(transfer.total)} complete` : "Complete";
  }
  if (transfer.kind === "download" && transfer.stage === "finalizing") {
    return transfer.total ? `${formatBytes(transfer.total)} received` : "Finalizing";
  }
  if (transfer.kind === "download" && transfer.stage === "server-finalizing") {
    return transfer.total ? `${formatBytes(transfer.total)} packaged` : "Finalizing";
  }

  const pieces = [];
  if (transfer.percent !== null && transfer.percent !== undefined) {
    pieces.push(formatPercent(transfer.percent));
  }
  if (transfer.loaded && transfer.total) {
    pieces.push(`${formatBytes(transfer.loaded)} of ${formatBytes(transfer.total)}`);
  } else if (transfer.loaded) {
    pieces.push(formatBytes(transfer.loaded));
  }
  if (transfer.bytesPerSecond > 0) {
    pieces.push(`${formatBytes(transfer.bytesPerSecond, { emptyForZero: false })}/s`);
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
    h(Icon, { icon: kind === "upload" ? "upload" : "download", size: 15 })
  );
}

function TransferRow({ onCancel, transfer }) {
  const percent =
    transfer.percent !== null && transfer.percent !== undefined ? `${transfer.percent}%` : "100%";
  const phase = transfer.phase || "visible";
  const canCancel = transfer.status === "active" && transfer.stage !== "verifying";
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
        h("div", { className: "transfer-stage", key: "stage" }, transferStageLabel(transfer)),
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
      canCancel
        ? h(
            "button",
            {
              "aria-label": `Cancel ${transfer.kind}`,
              className: "transfer-cancel-button",
              key: "cancel",
              onClick: () => onCancel?.(transfer.id),
              title: `Cancel ${transfer.kind}`,
              type: "button",
            },
            h(Icon, { icon: "close", size: 12 })
          )
        : null,
    ]
  );
}

export function TransferDock({ onCancelTransfer, transfers }) {
  if (!transfers.length) {
    return null;
  }

  return h(
    "div",
    { "aria-live": "polite", className: "transfer-dock" },
    transfers.map((transfer) =>
      h(TransferRow, { key: transfer.id, onCancel: onCancelTransfer, transfer })
    )
  );
}
