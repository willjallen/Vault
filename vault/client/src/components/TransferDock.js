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

function errorTransferTitle(transfer) {
  if (transfer.grouped && transfer.succeededItems > 0) {
    return transfer.kind === "upload" ? "Upload incomplete" : "Download incomplete";
  }
  return transfer.kind === "upload" ? "Upload failed" : "Download failed";
}

export function transferTitle(transfer) {
  if (transfer.status === "browser-managed") {
    return "Download started";
  }
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
    return errorTransferTitle(transfer);
  }
  if (transfer.kind === "upload" && transfer.grouped) {
    return "Uploading";
  }
  if (transfer.kind === "upload" && transfer.stage === "verifying") {
    return "Verifying upload";
  }
  if (transfer.kind === "upload" && transfer.resumedBytes > 0) {
    return "Resuming upload";
  }
  if (transfer.kind === "download" && transfer.serverStatus === "queued") {
    return "Waiting to prepare download";
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

function groupedUploadStageLabel(transfer) {
  if (transfer.currentItem) {
    return transfer.currentItem;
  }
  return transfer.stage === "creating-folders" ? "Creating folders" : "Upload";
}

export function transferStageLabel(transfer) {
  if (transfer.status === "browser-managed") {
    return "Browser download";
  }
  if (transfer.kind === "upload" && transfer.grouped) {
    return groupedUploadStageLabel(transfer);
  }
  if (transfer.kind === "upload" && transfer.stage === "verifying") {
    return "Server verification";
  }
  if (transfer.kind === "upload" && transfer.stage === "resuming") {
    return "Previous upload found";
  }
  if (transfer.kind === "download" && transfer.serverStatus === "queued") {
    return "Export queued";
  }
  if (
    transfer.kind === "download" &&
    transfer.serverStatus === "running" &&
    Number.isFinite(transfer.totalItems) &&
    transfer.totalItems > 0
  ) {
    const processedItems = Number.isFinite(transfer.processedItems)
      ? Math.max(0, transfer.processedItems)
      : 0;
    const itemNumber = Math.min(transfer.totalItems, processedItems + 1);
    return `Packaging item ${itemNumber} of ${transfer.totalItems}`;
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

function resumePercent(transfer) {
  if (!transfer.total || !transfer.resumedBytes) {
    return null;
  }
  return Math.min(100, Math.max(0, (transfer.resumedBytes / transfer.total) * 100));
}

function uploadResumeMeta(transfer) {
  const resumedPercent = resumePercent(transfer);
  if (transfer.kind === "upload" && transfer.stage === "resuming" && resumedPercent !== null) {
    return `Resuming previous upload from ${formatPercent(resumedPercent)}`;
  }
  return "";
}

function uploadResumeSuffix(transfer) {
  const resumedPercent = resumePercent(transfer);
  if (transfer.kind === "upload" && resumedPercent !== null && transfer.stage !== "verifying") {
    return `resumed from ${formatPercent(resumedPercent)}`;
  }
  return "";
}

function noProgressMeta(transfer) {
  const seconds = Math.floor(Number(transfer.noProgressSeconds));
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return "";
  }
  return transfer.serverStatus === "queued"
    ? `Waiting for worker for ${seconds}s`
    : `No progress reported for ${seconds}s`;
}

function hasKnownTotal(transfer) {
  if (transfer.total === null || transfer.total === undefined) {
    return false;
  }
  const total = Number(transfer.total);
  return Number.isFinite(total) && total >= 0;
}

function transferItemCount(transfer) {
  const count = Number(transfer.totalItems);
  return Number.isFinite(count) && count > 0 ? Math.floor(count) : null;
}

function transferItemLabel(count) {
  return `${count} ${count === 1 ? "item" : "items"}`;
}

function groupedItemProgress(transfer) {
  const totalItems = transferItemCount(transfer);
  if (!totalItems) {
    return "";
  }
  if (transfer.status === "complete") {
    return transferItemLabel(totalItems);
  }
  const processed = Number.isFinite(Number(transfer.processedItems))
    ? Math.max(0, Math.min(totalItems, Math.floor(Number(transfer.processedItems))))
    : 0;
  return `${processed} of ${transferItemLabel(totalItems)}`;
}

function groupedTransferMeta(transfer) {
  const itemProgress = groupedItemProgress(transfer);
  if (transfer.status === "complete") {
    const size = hasKnownTotal(transfer) ? `${formatBytes(transfer.total)} complete` : "Complete";
    return [itemProgress, size].filter(Boolean).join(" - ");
  }
  const active = activeTransferMeta(transfer);
  return (
    [itemProgress, active === "Starting" ? "" : active].filter(Boolean).join(" - ") || "Starting"
  );
}

function serverFinalizingMeta(transfer) {
  const noProgress = noProgressMeta(transfer);
  const pieces = noProgress ? [noProgress] : [];
  pieces.push(
    hasKnownTotal(transfer)
      ? `${formatBytes(transfer.total, { emptyForZero: false })} packaged`
      : "Finalizing"
  );
  return pieces.join(" - ");
}

function activeTransferMeta(transfer) {
  const noProgress = noProgressMeta(transfer);
  const pieces = noProgress ? [noProgress] : [];
  if (transfer.percent !== null && transfer.percent !== undefined) {
    pieces.push(formatPercent(transfer.percent));
  }
  if (hasKnownTotal(transfer)) {
    pieces.push(
      `${formatBytes(transfer.loaded || 0, { emptyForZero: false })} of ${formatBytes(
        transfer.total,
        { emptyForZero: false }
      )}`
    );
  } else if (transfer.loaded) {
    pieces.push(formatBytes(transfer.loaded));
  }
  if (!noProgress && transfer.bytesPerSecond > 0) {
    pieces.push(`${formatBytes(transfer.bytesPerSecond, { emptyForZero: false })}/s`);
  }
  const resumeSuffix = uploadResumeSuffix(transfer);
  if (resumeSuffix) {
    pieces.push(resumeSuffix);
  }
  const eta = noProgress ? "" : formatEta(transfer.etaSeconds);
  if (eta) {
    pieces.push(eta);
  }
  return pieces.join(" - ") || "Starting";
}

export function transferMeta(transfer) {
  if (transfer.status === "browser-managed") {
    return "Your browser controls the download location and progress";
  }
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
    return transfer.grouped
      ? groupedTransferMeta(transfer)
      : transfer.total
        ? `${formatBytes(transfer.total)} complete`
        : "Complete";
  }
  if (transfer.kind === "download" && transfer.stage === "finalizing") {
    return transfer.total ? `${formatBytes(transfer.total)} received` : "Finalizing";
  }
  if (transfer.kind === "download" && transfer.stage === "server-finalizing") {
    return serverFinalizingMeta(transfer);
  }
  const resumeMeta = uploadResumeMeta(transfer);
  if (resumeMeta) {
    return resumeMeta;
  }

  if (transfer.grouped) {
    return groupedTransferMeta(transfer);
  }

  return activeTransferMeta(transfer);
}

function TransferIcon({ kind }) {
  return h(
    "span",
    { className: classNames("transfer-icon", kind === "upload" ? "uploading" : "downloading") },
    h(Icon, { icon: kind === "upload" ? "upload" : "download", size: 15 })
  );
}

export function transferCanCancel(transfer) {
  return transfer.status === "active" && (transfer.grouped || transfer.stage !== "verifying");
}

function TransferRow({ onCancel, transfer }) {
  const percent =
    transfer.percent !== null && transfer.percent !== undefined ? `${transfer.percent}%` : "100%";
  const phase = transfer.phase || "visible";
  const canCancel = transferCanCancel(transfer);
  return h(
    "div",
    {
      className: classNames(
        "transfer-row",
        transfer.kind,
        transfer.status,
        transfer.grouped ? "grouped" : "",
        `phase-${phase}`
      ),
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
