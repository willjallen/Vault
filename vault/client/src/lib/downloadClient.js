import * as browserDownload from "./browserDownload.js";
import { createExportProgressTracker, trackExportJobProgress } from "./exportStatusPolicy.js";
import {
  TransferCancelledError,
  byteLength,
  errorFromResponse,
  isAbortError,
  progressFromValues,
  requestJson,
  throwIfAborted,
  waitFor,
} from "./transferCore.js";

const EXPORT_POLL_MS = 900;
const PROGRESS_TICK_MS = 80;

function filenameFromDisposition(disposition) {
  if (!disposition) {
    return "";
  }

  const utfMatch = disposition.match(/filename\*=UTF-8''([^;]+)/i);
  if (utfMatch) {
    try {
      return decodeURIComponent(utfMatch[1].replace(/"/g, "").trim());
    } catch {
      return utfMatch[1].replace(/"/g, "").trim();
    }
  }

  const quotedMatch = disposition.match(/filename="([^"]+)"/i);
  if (quotedMatch) {
    return quotedMatch[1].trim();
  }

  const plainMatch = disposition.match(/filename=([^;]+)/i);
  return plainMatch ? plainMatch[1].replace(/"/g, "").trim() : "";
}

function browserManagedDownload({
  fallbackName,
  fallbackTotal,
  onProgress,
  signal,
  startedAt,
  url,
}) {
  onProgress(progressFromValues(0, fallbackTotal, startedAt, { stage: "browser-handoff" }));
  browserDownload.startBrowserDownload(url, fallbackName, signal);
  return {
    browserManaged: true,
    filename: browserDownload.cleanDownloadName(fallbackName),
    size: fallbackTotal || 0,
    status: 202,
  };
}

function totalFromDownloadResponse(response, fallbackTotal) {
  const headerLength = Number(response.headers.get("Content-Length") || 0);
  return Number.isFinite(headerLength) && headerLength > 0 ? headerLength : fallbackTotal;
}

function sameOriginDownloadUrl(url) {
  if (typeof url !== "string" || !url) {
    throw new Error("Download preparation did not return a URL.");
  }
  const resolvedUrl = new URL(url, window.location.href);
  if (resolvedUrl.origin !== window.location.origin) {
    throw new Error("Download URL must use the same origin as Vault.");
  }
  return url;
}

async function prepareDownloadUrl({ prepare, signal, url }) {
  throwIfAborted(signal);
  const preparedUrl = prepare ? await prepare(signal) : url;
  throwIfAborted(signal);
  return sameOriginDownloadUrl(preparedUrl);
}

async function cancelResponseBody(response) {
  if (!response?.body || typeof response.body.cancel !== "function") {
    return;
  }
  await response.body.cancel().catch(() => {});
}

async function streamResponseToFile({ response, writer, total, onProgress, signal, startedAt }) {
  if (!response.body?.pipeThrough || typeof TransformStream !== "function") {
    throw new Error("Streaming downloads are not supported by this browser.");
  }
  let loaded = 0;
  let lastProgressEmittedAt = 0;

  function emitDownloadProgress(stage = "downloading", options = {}) {
    const now = performance.now();
    if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
      return;
    }
    lastProgressEmittedAt = now;
    onProgress(progressFromValues(loaded, total, startedAt, { stage }));
  }

  const progressStream = new TransformStream({
    transform(chunk, controller) {
      throwIfAborted(signal);
      loaded += byteLength(chunk);
      emitDownloadProgress();
      controller.enqueue(chunk);
    },
  });
  onProgress(progressFromValues(0, total, startedAt, { stage: "downloading" }));
  await response.body.pipeThrough(progressStream).pipeTo(writer, { signal });
  emitDownloadProgress("finalizing", { force: true });
  return loaded;
}

export async function downloadUrl({
  url,
  customDownloadsEnabled = false,
  fallbackName = "download",
  onProgress,
  fallbackTotal = null,
  signal,
  writer: existingWriter = null,
  prepare = null,
}) {
  const startedAt = performance.now();
  let response = null;
  let writer = existingWriter;
  try {
    throwIfAborted(signal);
    onProgress(progressFromValues(0, fallbackTotal, startedAt, { stage: "starting" }));
    const useBrowserDownload =
      !browserDownload.canUseFileSystemDownloadWriter(customDownloadsEnabled);
    if (!writer && useBrowserDownload) {
      const browserPreparedUrl = await prepareDownloadUrl({ prepare, signal, url });
      return browserManagedDownload({
        fallbackName,
        fallbackTotal,
        onProgress,
        signal,
        startedAt,
        url: browserPreparedUrl,
      });
    }
    if (!writer) {
      writer = await browserDownload.openFileSystemDownloadWriter(fallbackName, signal);
    }
    const preparedUrl = await prepareDownloadUrl({ prepare, signal, url });
    response = await fetch(preparedUrl, { credentials: "include", signal });
    if (!response.ok) {
      throw await errorFromResponse(response, "Download failed");
    }
    const total = totalFromDownloadResponse(response, fallbackTotal);
    const filename =
      filenameFromDisposition(response.headers.get("Content-Disposition")) ||
      fallbackName ||
      "download";
    const size = await streamResponseToFile({
      onProgress,
      response,
      signal,
      startedAt,
      total,
      writer,
    });
    writer = null;
    return { filename, size: size || total || 0, status: response.status };
  } catch (error) {
    await cancelResponseBody(response);
    if (writer) {
      await writer.abort().catch(() => {});
    }
    if (signal?.aborted || isAbortError(error)) {
      throw new TransferCancelledError();
    }
    throw error;
  }
}

async function cancelExportJob(jobId) {
  try {
    await requestJson(`/api/exports/${jobId}`, { method: "DELETE" }, "Could not cancel export");
  } catch {
    // Cancellation cleanup is best-effort after the client has already aborted polling.
  }
}

export async function exportAndDownload({
  customDownloadsEnabled = false,
  payload,
  onProgress,
  signal,
  suggestedName = "vault-download.zip",
}) {
  const startedAt = performance.now();
  let job = null;
  let writer = null;
  try {
    onProgress(progressFromValues(0, null, startedAt, { stage: "starting" }));
    if (browserDownload.canUseFileSystemDownloadWriter(customDownloadsEnabled)) {
      writer = await browserDownload.openFileSystemDownloadWriter(suggestedName, signal);
    }
    job = await requestJson(
      "/api/exports",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
        signal,
      },
      "Could not start export"
    );
    const exportStartedAt = performance.now();
    const progressTracker = createExportProgressTracker(exportStartedAt);
    let current = job;
    while (!["complete", "failed", "cancelled"].includes(current.status)) {
      throwIfAborted(signal);
      const serverProgress = trackExportJobProgress(current, progressTracker);
      onProgress(
        progressFromValues(serverProgress.loaded, serverProgress.total, exportStartedAt, {
          ...serverProgress,
          stage:
            current.status === "queued"
              ? "queued"
              : current.status === "finalizing"
                ? "server-finalizing"
                : "preparing",
        })
      );
      await waitFor(EXPORT_POLL_MS, signal);
      current = await requestJson(`/api/exports/${job.id}`, { signal }, "Could not refresh export");
    }
    if (current.status === "cancelled") {
      throw new TransferCancelledError("Export cancelled");
    }
    if (current.status !== "complete" || !current.download_url) {
      throw new Error(current.error || `Export ${current.status}`);
    }
    const downloadWriter = writer;
    writer = null;
    return downloadUrl({
      customDownloadsEnabled,
      fallbackName: current.filename || suggestedName || "vault-download.zip",
      fallbackTotal: current.size_bytes || current.total_bytes || null,
      onProgress,
      signal,
      url: current.download_url,
      writer: downloadWriter,
    });
  } catch (error) {
    if (writer) {
      await writer.abort().catch(() => {});
    }
    if (isAbortError(error)) {
      if (job?.id) {
        await cancelExportJob(job.id);
      }
      throw new TransferCancelledError();
    }
    throw error;
  }
}
