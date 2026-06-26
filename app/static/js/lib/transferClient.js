const UPLOAD_SESSION_STORAGE_KEY = "vault.uploadSessions";
const UPLOAD_CONCURRENCY = 8;
const UPLOAD_RETRY_LIMIT = 3;
const DOWNLOAD_CONCURRENCY = 4;
const DOWNLOAD_SEGMENT_BYTES = 64 * 1024 * 1024;
const DOWNLOAD_SEGMENTED_MIN_BYTES = 128 * 1024 * 1024;
const DOWNLOAD_WRITE_BUFFER_BYTES = 32 * 1024 * 1024;
const DOWNLOAD_WRITE_BACKPRESSURE_BYTES = 512 * 1024 * 1024;
const EXPORT_POLL_MS = 900;
const PROGRESS_TICK_MS = 80;
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
  const finalizing = options.stage === "finalizing";
  const etaSeconds =
    total && bytesPerSecond > 0 && loaded < total && !finalizing
      ? (total - loaded) / bytesPerSecond
      : null;
  return {
    bytesPerSecond: finalizing ? 0 : bytesPerSecond,
    etaSeconds,
    lengthComputable: Boolean(total),
    loaded,
    percent: total ? Math.min(100, Math.max(0, (loaded / total) * 100)) : null,
    stage: options.stage || "transfer",
    total,
  };
}

function byteLength(value) {
  return value?.byteLength || value?.size || value?.length || 0;
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

function uploadPartRequest({ session, partNumber, chunk, offset, onProgress, signal }) {
  throwIfAborted(signal);
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    let settled = false;

    function cleanup() {
      signal?.removeEventListener("abort", abortRequest);
    }

    function settle(callback, value) {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      callback(value);
    }

    function abortRequest() {
      xhr.abort();
    }

    xhr.upload.onprogress = (progressEvent) => {
      if (!onProgress) {
        return;
      }
      const loaded = Number.isFinite(progressEvent.loaded) ? progressEvent.loaded : 0;
      onProgress(Math.min(chunk.size, Math.max(0, loaded)));
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        onProgress?.(chunk.size);
        settle(resolve, parseJson(xhr.responseText));
        return;
      }
      settle(reject, errorFromText(xhr.responseText, xhr.status, "Upload part failed"));
    };
    xhr.onerror = () => {
      const error = new Error("Network error during upload");
      error.networkError = true;
      settle(reject, error);
    };
    xhr.onabort = () => {
      settle(reject, new TransferCancelledError());
    };
    xhr.ontimeout = () => {
      const error = new Error("Upload part timed out");
      error.networkError = true;
      settle(reject, error);
    };

    signal?.addEventListener("abort", abortRequest, { once: true });
    xhr.open("PUT", `/api/uploads/${session.id}/parts/${partNumber}`);
    xhr.withCredentials = true;
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.setRequestHeader("X-Upload-Offset", String(offset));
    xhr.setRequestHeader("X-Upload-Size", String(chunk.size));
    xhr.send(chunk);
  });
}

function shouldRetryUploadPart(error) {
  if (isAbortError(error)) {
    return false;
  }
  if (error?.networkError || !error?.status) {
    return true;
  }
  return [408, 429, 500, 502, 503, 504].includes(error.status);
}

async function uploadPart({
  session,
  partNumber,
  chunk,
  offset,
  onAttemptStart,
  onProgress,
  signal,
}) {
  for (let attempt = 1; attempt <= UPLOAD_RETRY_LIMIT; attempt += 1) {
    throwIfAborted(signal);
    onAttemptStart?.();
    try {
      return await uploadPartRequest({
        chunk,
        offset,
        onProgress,
        partNumber,
        session,
        signal,
      });
    } catch (error) {
      if (!shouldRetryUploadPart(error) || attempt >= UPLOAD_RETRY_LIMIT) {
        throw error;
      }
      await waitFor(attempt * 700, signal);
    }
  }
  throw new Error("Upload part failed");
}

function currentUploadLoadedBytes({ activeParts, completedBytes, fileSize }) {
  const activeBytes = [...activeParts.values()].reduce(
    (total, part) => total + Math.min(part.size, Math.max(0, part.loaded || 0)),
    0
  );
  return Math.min(fileSize, Math.max(completedBytes, completedBytes + activeBytes));
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
    let lastProgressEmittedAt = 0;

    function emitUploadProgress(options = {}) {
      const now = performance.now();
      if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
        return;
      }
      lastProgressEmittedAt = now;
      onProgress(
        progressFromValues(
          currentUploadLoadedBytes({
            activeParts,
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

    function updateActivePartProgress(partNumber, loaded, options = {}) {
      const current = activeParts.get(partNumber);
      if (!current) {
        return;
      }
      const nextLoaded = Math.min(current.size, Math.max(0, loaded));
      activeParts.set(partNumber, {
        ...current,
        loaded: options.reset ? nextLoaded : Math.max(current.loaded || 0, nextLoaded),
      });
      emitUploadProgress({ force: Boolean(options.reset) });
    }

    emitUploadProgress({ force: true });

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
        activeParts.set(partNumber, { loaded: 0, size: chunk.size });
        emitUploadProgress({ force: true });
        try {
          session = await uploadPart({
            chunk,
            onAttemptStart: () => updateActivePartProgress(partNumber, 0, { reset: true }),
            onProgress: (loaded) => updateActivePartProgress(partNumber, loaded),
            offset,
            partNumber,
            session,
            signal,
          });
        } finally {
          activeParts.delete(partNumber);
        }
        completedBytes += chunk.size;
        emitUploadProgress({ force: true });
      }
    }
    await Promise.all(
      Array.from({ length: Math.min(UPLOAD_CONCURRENCY, session.part_count) }, () => uploadWorker())
    );

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

function totalFromContentRange(contentRange) {
  const match = (contentRange || "").match(/^bytes\s+\d+-\d+\/(\d+)$/i);
  if (!match) {
    return null;
  }
  const total = Number(match[1]);
  return Number.isFinite(total) && total > 0 ? total : null;
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

function createWriteQueue(writer, signal) {
  let chain = Promise.resolve();
  let pendingBytes = 0;
  let queuedError = null;
  let waiters = [];

  function notifyWaiters() {
    const currentWaiters = waiters;
    waiters = [];
    currentWaiters.forEach((resolve) => resolve());
  }

  async function waitForBackpressure() {
    while (pendingBytes > DOWNLOAD_WRITE_BACKPRESSURE_BYTES && !queuedError) {
      await new Promise((resolve) => {
        waiters.push(resolve);
      });
    }
    if (queuedError) {
      throw queuedError;
    }
  }

  function enqueue(payload, size) {
    throwIfAborted(signal);
    if (queuedError) {
      throw queuedError;
    }
    pendingBytes += size;
    const writeTask = chain.then(() => {
      throwIfAborted(signal);
      return writer.write(payload);
    });
    chain = writeTask.catch(() => {});
    writeTask.then(
      () => {
        pendingBytes -= size;
        notifyWaiters();
      },
      (error) => {
        pendingBytes -= size;
        queuedError = error;
        notifyWaiters();
      }
    );
    return waitForBackpressure();
  }

  return {
    async write(data, position = null) {
      const size = byteLength(data);
      const payload = position === null ? data : { type: "write", position, data };
      await enqueue(payload, size);
    },
    async idle() {
      await chain;
      if (queuedError) {
        throw queuedError;
      }
    },
  };
}

async function streamResponseToFile({ response, writer, total, onProgress, signal, startedAt }) {
  const reader = response.body?.getReader?.();
  if (!reader) {
    await writer.abort().catch(() => {});
    throw new Error("Streaming downloads are not supported by this browser.");
  }
  const writeQueue = createWriteQueue(writer, signal);
  let loaded = 0;
  let pendingChunks = [];
  let pendingBytes = 0;
  let lastProgressEmittedAt = 0;

  function emitDownloadProgress(stage = "downloading", options = {}) {
    const now = performance.now();
    if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
      return;
    }
    lastProgressEmittedAt = now;
    onProgress(progressFromValues(loaded, total, startedAt, { stage }));
  }

  async function flushPendingChunks() {
    if (!pendingBytes) {
      return;
    }
    const payload =
      pendingChunks.length === 1
        ? pendingChunks[0]
        : new Blob(pendingChunks, { type: "application/octet-stream" });
    pendingChunks = [];
    pendingBytes = 0;
    await writeQueue.write(payload);
  }

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
      loaded += value.byteLength || value.length || 0;
      pendingChunks.push(value);
      pendingBytes += value.byteLength || value.length || 0;
      if (pendingBytes >= DOWNLOAD_WRITE_BUFFER_BYTES) {
        await flushPendingChunks();
      }
      emitDownloadProgress();
    }
    await flushPendingChunks();
    await writeQueue.idle();
    emitDownloadProgress("finalizing", { force: true });
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

async function downloadRangesToFile({ url, writer, total, onProgress, signal, startedAt }) {
  const writeQueue = createWriteQueue(writer, signal);
  const rangeAbort = new AbortController();
  const workerCount = Math.min(DOWNLOAD_CONCURRENCY, Math.ceil(total / DOWNLOAD_SEGMENT_BYTES));
  let loaded = 0;
  let nextStart = 0;
  let lastProgressEmittedAt = 0;

  function abortRangeRequests() {
    rangeAbort.abort();
  }

  if (signal) {
    signal.addEventListener("abort", abortRangeRequests, { once: true });
  }

  function nextSegment() {
    if (nextStart >= total) {
      return null;
    }
    const start = nextStart;
    const end = Math.min(start + DOWNLOAD_SEGMENT_BYTES - 1, total - 1);
    nextStart = end + 1;
    return { end, start };
  }

  function emitDownloadProgress(options = {}) {
    const now = performance.now();
    if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
      return;
    }
    lastProgressEmittedAt = now;
    onProgress(
      progressFromValues(loaded, total, startedAt, { stage: options.stage || "downloading" })
    );
  }

  async function downloadSegment(segment) {
    let response = null;
    try {
      response = await fetch(url, {
        credentials: "include",
        headers: { Range: `bytes=${segment.start}-${segment.end}` },
        signal: rangeAbort.signal,
      });
      if (response.status !== 206) {
        throw await errorFromResponse(response, "Download range request failed");
      }
      const reader = response.body?.getReader?.();
      if (!reader) {
        throw new Error("Streaming downloads are not supported by this browser.");
      }
      let writePosition = segment.start;
      let pendingChunks = [];
      let pendingBytes = 0;

      async function flushPendingChunks() {
        if (!pendingBytes) {
          return;
        }
        const payload =
          pendingChunks.length === 1
            ? pendingChunks[0]
            : new Blob(pendingChunks, { type: "application/octet-stream" });
        const position = writePosition;
        writePosition += pendingBytes;
        pendingChunks = [];
        pendingBytes = 0;
        await writeQueue.write(payload, position);
      }

      while (true) {
        throwIfAborted(signal);
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        if (!value) {
          continue;
        }
        const size = byteLength(value);
        loaded += size;
        pendingChunks.push(value);
        pendingBytes += size;
        if (pendingBytes >= DOWNLOAD_WRITE_BUFFER_BYTES) {
          await flushPendingChunks();
        }
        emitDownloadProgress();
      }
      await flushPendingChunks();
    } catch (error) {
      abortRangeRequests();
      throw error;
    } finally {
      await cancelResponseBody(response);
    }
  }

  async function downloadWorker() {
    while (true) {
      throwIfAborted(signal);
      const segment = nextSegment();
      if (!segment) {
        return;
      }
      await downloadSegment(segment);
    }
  }

  try {
    onProgress(progressFromValues(0, total, startedAt, { stage: "downloading" }));
    await Promise.all(Array.from({ length: workerCount }, () => downloadWorker()));
    await writeQueue.idle();
    emitDownloadProgress({ force: true, stage: "finalizing" });
    await writer.close();
    return loaded;
  } finally {
    if (signal) {
      signal.removeEventListener("abort", abortRangeRequests);
    }
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
    response = await fetch(url, {
      credentials: "include",
      headers: { Range: "bytes=0-0" },
      signal,
    });
    if (!response.ok) {
      throw await errorFromResponse(response, "Download failed");
    }
    const rangeTotal = totalFromContentRange(response.headers.get("Content-Range"));
    const headerLength = Number(response.headers.get("Content-Length") || 0);
    const total =
      rangeTotal ||
      (Number.isFinite(headerLength) && headerLength > 0 ? headerLength : fallbackTotal);
    const filename =
      filenameFromDisposition(response.headers.get("Content-Disposition")) ||
      fallbackName ||
      "download";
    if (
      response.status === 206 &&
      total >= DOWNLOAD_SEGMENTED_MIN_BYTES &&
      typeof writer.seek === "function"
    ) {
      await cancelResponseBody(response);
      response = null;
      const segmentedSize = await downloadRangesToFile({
        onProgress,
        signal,
        startedAt,
        total,
        url,
        writer,
      });
      writer = null;
      return { filename, size: segmentedSize || total || 0, status: 200 };
    }
    if (response.status === 206) {
      await cancelResponseBody(response);
      response = await fetch(url, { credentials: "include", signal });
      if (!response.ok) {
        throw await errorFromResponse(response, "Download failed");
      }
    }
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
