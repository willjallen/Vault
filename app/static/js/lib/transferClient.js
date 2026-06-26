const UPLOAD_SESSION_STORAGE_KEY = "vault.uploadSessions";
const UPLOAD_CONCURRENCY = 8;
const UPLOAD_RETRY_LIMIT = 3;
const EXPORT_POLL_MS = 900;
const PROGRESS_TICK_MS = 120;
const VERIFICATION_POLL_MS = 240;

export class TransferCancelledError extends Error {
  constructor(message = "Transfer cancelled") {
    super(message);
    this.cancelled = true;
    this.name = "TransferCancelledError";
  }
}

function parseJson(value) {
  if (!value) {
    return {};
  }
  try {
    return JSON.parse(value);
  } catch {
    return {};
  }
}

function errorFromText(text, responseStatus, fallback) {
  const parsed = parseJson(text);
  const error = new Error(parsed.detail || fallback);
  error.status = responseStatus;
  return error;
}

async function errorFromResponse(response, fallback) {
  const text = await response.text().catch(() => "");
  return errorFromText(text, response.status, fallback);
}

function progressFromValues(loaded, total, startedAt, options = {}) {
  const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.01);
  const bytesPerSecond = loaded / elapsedSeconds;
  const etaSeconds =
    total && bytesPerSecond > 0 && loaded < total ? (total - loaded) / bytesPerSecond : null;
  return {
    bytesPerSecond,
    etaSeconds,
    lengthComputable: Boolean(total),
    loaded,
    percent: total ? Math.min(100, Math.max(0, (loaded / total) * 100)) : null,
    stage: options.stage || "transfer",
    total,
  };
}

function isAbortError(error) {
  return error?.name === "AbortError" || error?.cancelled;
}

function throwIfAborted(signal) {
  if (signal?.aborted) {
    throw new TransferCancelledError();
  }
}

function waitFor(delay, signal) {
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, delay);
    function onAbort() {
      clearTimeout(timer);
      reject(new TransferCancelledError());
    }
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

function readStoredUploadSessions() {
  try {
    const parsed = JSON.parse(localStorage.getItem(UPLOAD_SESSION_STORAGE_KEY) || "[]");
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function writeStoredUploadSessions(records) {
  localStorage.setItem(UPLOAD_SESSION_STORAGE_KEY, JSON.stringify(records));
}

function uploadSessionKey({ file, folder, mode, documentId, note, renameToUpload }) {
  return [
    mode || "create",
    documentId || "",
    folder || "",
    file.name,
    file.size,
    file.lastModified,
    note || "",
    renameToUpload ? "rename" : "",
  ].join("|");
}

function rememberUploadSession(key, sessionId) {
  const sessions = readStoredUploadSessions().filter((record) => record.key !== key);
  sessions.push({ key, sessionId });
  writeStoredUploadSessions(sessions);
}

function forgetUploadSession(key) {
  writeStoredUploadSessions(readStoredUploadSessions().filter((record) => record.key !== key));
}

function storedUploadSessionId(key) {
  return readStoredUploadSessions().find((record) => record.key === key)?.sessionId || null;
}

async function requestJson(url, options = {}, fallback = "Request failed") {
  let response;
  try {
    response = await fetch(url, { credentials: "include", ...options });
  } catch (error) {
    if (isAbortError(error)) {
      throw new TransferCancelledError();
    }
    throw error;
  }
  if (!response.ok) {
    throw await errorFromResponse(response, fallback);
  }
  return response.json();
}

async function existingUploadSession(sessionId, signal) {
  try {
    return await requestJson(`/api/uploads/${sessionId}`, { signal }, "Upload session not found");
  } catch {
    return null;
  }
}

async function createUploadSession({
  file,
  folder,
  mode,
  documentId,
  note,
  renameToUpload,
  signal,
}) {
  return requestJson(
    "/api/uploads",
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        document_id: documentId || null,
        filename: file.name,
        folder: folder || "",
        mime_type: file.type || "application/octet-stream",
        mode: mode || "create",
        note: note || "",
        rename_to_upload: Boolean(renameToUpload),
        size_bytes: file.size,
      }),
      signal,
    },
    "Could not create upload session"
  );
}

async function uploadPart({ session, partNumber, chunk, offset, signal }) {
  for (let attempt = 1; attempt <= UPLOAD_RETRY_LIMIT; attempt += 1) {
    throwIfAborted(signal);
    const response = await fetch(`/api/uploads/${session.id}/parts/${partNumber}`, {
      method: "PUT",
      credentials: "include",
      headers: {
        "Content-Type": "application/octet-stream",
        "X-Upload-Offset": String(offset),
        "X-Upload-Size": String(chunk.size),
      },
      body: chunk,
      signal,
    }).catch((error) => {
      if (isAbortError(error)) {
        throw new TransferCancelledError();
      }
      return { ok: false, networkError: error, status: 0 };
    });
    if (response.ok) {
      return response.json();
    }
    if (response.networkError && attempt < UPLOAD_RETRY_LIMIT) {
      await waitFor(attempt * 700, signal);
      continue;
    }
    if (response.networkError) {
      throw new Error("Network error during upload");
    }
    throw await errorFromResponse(response, "Upload part failed");
  }
  throw new Error("Upload part failed");
}

function smoothLoadedBytes({ activeParts, averageBytesPerSecond, completedBytes, fileSize }) {
  if (!averageBytesPerSecond || !activeParts.size) {
    return completedBytes;
  }
  const now = performance.now();
  const bytesPerActivePart = averageBytesPerSecond / activeParts.size;
  const estimatedActiveBytes = [...activeParts.values()].reduce((total, part) => {
    const elapsedSeconds = Math.max((now - part.startedAt) / 1000, 0);
    return total + Math.min(part.size * 0.94, elapsedSeconds * bytesPerActivePart);
  }, 0);
  return Math.min(fileSize, Math.max(completedBytes, completedBytes + estimatedActiveBytes));
}

function recordThroughputSample(currentAverage, bytes, elapsedMs) {
  if (elapsedMs <= 0) {
    return currentAverage;
  }
  const sample = bytes / Math.max(elapsedMs / 1000, 0.001);
  if (!Number.isFinite(sample) || sample <= 0) {
    return currentAverage;
  }
  if (!currentAverage) {
    return sample;
  }
  return currentAverage * 0.72 + sample * 0.28;
}

async function completeUploadSession(session, sha256, signal) {
  return requestJson(
    `/api/uploads/${session.id}/complete`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ sha256 }),
      signal,
    },
    "Could not complete upload"
  );
}

async function pollUploadVerification({ sessionId, signal, onProgress, isDone }) {
  const startedAt = performance.now();
  while (!isDone()) {
    await waitFor(VERIFICATION_POLL_MS, signal);
    if (isDone()) {
      break;
    }
    const current = await existingUploadSession(sessionId, signal);
    const verification = current?.verification;
    if (!verification) {
      continue;
    }
    onProgress(
      progressFromValues(
        verification.processed_bytes || 0,
        verification.total_bytes || null,
        startedAt,
        { stage: "verifying" }
      )
    );
  }
}

async function abortUploadSession(sessionId) {
  try {
    await requestJson(`/api/uploads/${sessionId}`, { method: "DELETE" }, "Could not cancel upload");
  } catch {
    // Cancellation cleanup is best-effort after the client has already aborted in-flight work.
  }
}

export async function uploadFileResumable({
  file,
  folder = "",
  mode = "create",
  documentId = null,
  note = "",
  renameToUpload = false,
  onProgress,
  signal,
}) {
  const key = uploadSessionKey({ file, folder, mode, documentId, note, renameToUpload });
  const storedSessionId = storedUploadSessionId(key);
  let session = null;
  try {
    session = storedSessionId ? await existingUploadSession(storedSessionId, signal) : null;
    if (!session || session.status !== "active") {
      session = await createUploadSession({
        documentId,
        file,
        folder,
        mode,
        note,
        renameToUpload,
        signal,
      });
      rememberUploadSession(key, session.id);
    }

    const startedAt = performance.now();
    const uploadedParts = new Map(
      (session.uploaded_parts || []).map((part) => [part.part_number, part])
    );
    let completedBytes = [...uploadedParts.values()].reduce(
      (total, part) => total + part.size_bytes,
      0
    );
    const activeParts = new Map();
    let averageBytesPerSecond = 0;
    let progressTimer = null;

    function emitUploadProgress() {
      onProgress(
        progressFromValues(
          smoothLoadedBytes({
            activeParts,
            averageBytesPerSecond,
            completedBytes,
            fileSize: file.size,
          }),
          file.size,
          startedAt,
          {
            stage: "uploading",
          }
        )
      );
    }

    function startProgressTimer() {
      if (progressTimer !== null) {
        return;
      }
      progressTimer = setInterval(() => {
        if (activeParts.size) {
          emitUploadProgress();
        }
      }, PROGRESS_TICK_MS);
    }

    function stopProgressTimer() {
      if (progressTimer === null) {
        return;
      }
      clearInterval(progressTimer);
      progressTimer = null;
    }

    emitUploadProgress();
    startProgressTimer();

    let nextPartNumber = 1;
    async function uploadWorker() {
      while (nextPartNumber <= session.part_count) {
        throwIfAborted(signal);
        const partNumber = nextPartNumber;
        nextPartNumber += 1;
        const offset = (partNumber - 1) * session.chunk_size;
        const end = Math.min(offset + session.chunk_size, file.size);
        const chunk = file.slice(offset, end);
        const existing = uploadedParts.get(partNumber);
        if (existing) {
          continue;
        }
        const partStartedAt = performance.now();
        activeParts.set(partNumber, { size: chunk.size, startedAt: partStartedAt });
        try {
          session = await uploadPart({
            chunk,
            offset,
            partNumber,
            session,
            signal,
          });
        } finally {
          activeParts.delete(partNumber);
        }
        averageBytesPerSecond = recordThroughputSample(
          averageBytesPerSecond,
          chunk.size,
          performance.now() - partStartedAt
        );
        completedBytes += chunk.size;
        emitUploadProgress();
      }
    }
    try {
      await Promise.all(
        Array.from({ length: Math.min(UPLOAD_CONCURRENCY, session.part_count) }, () =>
          uploadWorker()
        )
      );
    } finally {
      stopProgressTimer();
    }

    const verificationStartedAt = performance.now();
    onProgress(progressFromValues(0, file.size, verificationStartedAt, { stage: "verifying" }));
    let verificationDone = false;
    const verificationPoll = pollUploadVerification({
      isDone: () => verificationDone,
      onProgress,
      sessionId: session.id,
      signal,
    }).catch((error) => {
      if (!isAbortError(error)) {
        throw error;
      }
    });
    let result;
    try {
      result = await completeUploadSession(session, null, signal);
    } finally {
      verificationDone = true;
      await verificationPoll;
    }
    forgetUploadSession(key);
    onProgress(
      progressFromValues(file.size, file.size, verificationStartedAt, { stage: "verifying" })
    );
    return { body: result, size: file.size, status: 200 };
  } catch (error) {
    if (isAbortError(error)) {
      if (session?.id) {
        await abortUploadSession(session.id);
      }
      forgetUploadSession(key);
      throw new TransferCancelledError();
    }
    throw error;
  }
}

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

function cleanDownloadName(filename) {
  return (filename || "download").trim().replace(/[\\/:*?"<>|]+/g, "_") || "download";
}

async function openDownloadWriter(filename, signal) {
  throwIfAborted(signal);
  if (!window.showSaveFilePicker) {
    throw new Error("Streaming downloads require a browser with file save support.");
  }
  const handle = await window.showSaveFilePicker({
    suggestedName: cleanDownloadName(filename),
  });
  throwIfAborted(signal);
  return handle.createWritable();
}

async function cancelResponseBody(response) {
  if (!response?.body || typeof response.body.cancel !== "function") {
    return;
  }
  await response.body.cancel().catch(() => {});
}

async function streamResponseToFile({ response, writer, total, onProgress, signal, startedAt }) {
  const reader = response.body?.getReader?.();
  if (!reader) {
    await writer.abort().catch(() => {});
    throw new Error("Streaming downloads are not supported by this browser.");
  }
  let loaded = 0;
  try {
    while (true) {
      throwIfAborted(signal);
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      if (!value) {
        continue;
      }
      await writer.write(value);
      loaded += value.byteLength || value.length || 0;
      onProgress(progressFromValues(loaded, total, startedAt, { stage: "downloading" }));
    }
    await writer.close();
    return loaded;
  } catch (error) {
    await reader.cancel().catch(() => {});
    await writer.abort().catch(() => {});
    if (isAbortError(error)) {
      throw new TransferCancelledError();
    }
    throw error;
  }
}

export async function downloadUrl({
  url,
  fallbackName = "download",
  onProgress,
  fallbackTotal = null,
  signal,
}) {
  const startedAt = performance.now();
  let response = null;
  let writer = null;
  try {
    throwIfAborted(signal);
    onProgress(progressFromValues(0, fallbackTotal, startedAt, { stage: "starting" }));
    writer = await openDownloadWriter(fallbackName, signal);
    response = await fetch(url, { credentials: "include", signal });
    if (!response.ok) {
      throw await errorFromResponse(response, "Download failed");
    }
    const headerLength = Number(response.headers.get("Content-Length") || 0);
    const total = Number.isFinite(headerLength) && headerLength > 0 ? headerLength : fallbackTotal;
    const filename =
      filenameFromDisposition(response.headers.get("Content-Disposition")) ||
      fallbackName ||
      "download";
    onProgress(progressFromValues(0, total, startedAt, { stage: "downloading" }));
    const size = await streamResponseToFile({
      onProgress,
      response,
      signal,
      startedAt,
      total,
      writer,
    });
    return { filename, size: size || total || 0, status: response.status };
  } catch (error) {
    await cancelResponseBody(response);
    if (writer) {
      await writer.abort().catch(() => {});
    }
    if (isAbortError(error)) {
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

export async function exportAndDownload({ payload, onProgress, signal }) {
  const startedAt = performance.now();
  let job = null;
  try {
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
    let current = job;
    while (!["complete", "failed", "cancelled"].includes(current.status)) {
      throwIfAborted(signal);
      onProgress(
        progressFromValues(current.processed_bytes || 0, current.total_bytes || null, startedAt, {
          stage: "preparing",
        })
      );
      await waitFor(EXPORT_POLL_MS, signal);
      current = await requestJson(`/api/exports/${job.id}`, { signal }, "Could not refresh export");
    }
    if (current.status !== "complete" || !current.download_url) {
      throw new Error(current.error || `Export ${current.status}`);
    }
    return downloadUrl({
      fallbackName: current.filename || "vault-download.zip",
      fallbackTotal: current.size_bytes || current.total_bytes || null,
      onProgress,
      signal,
      url: current.download_url,
    });
  } catch (error) {
    if (isAbortError(error)) {
      if (job?.id) {
        await cancelExportJob(job.id);
      }
      throw new TransferCancelledError();
    }
    throw error;
  }
}
