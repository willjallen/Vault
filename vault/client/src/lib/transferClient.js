import * as browserDownload from "./browserDownload.js";
import {
  sha256Blob,
  uploadContentFingerprint,
  uploadFilePartManifestSha256,
  uploadPartManifestSha256,
  uploadResumeIdentitySha256,
} from "./fileIntegrity.js";
import {
  createUploadCancellation,
  currentUploadLoadedBytes,
  reportUploadProgress,
  runUploadWorkers,
  uploadPart,
  uploadParallelismForLatency,
} from "./uploadPartPolicy.js";
import {
  committedUploadBytes,
  forgetUploadSession,
  rememberUploadSession,
  storedUploadSessionId,
  uploadSessionKey,
} from "./uploadSessionStore.js";
import {
  completedUploadSessionManifest,
  completedUploadSessionResult,
  pollUploadVerificationStatus,
  readUploadSessionStatus,
  uploadSessionLayoutMatchesFile,
  validateInMemoryCompletionManifest,
  waitForUploadSessionTransition as pollUploadSessionTransition,
} from "./uploadStatusPolicy.js";
const EXPORT_POLL_MS = 900;
const PROGRESS_TICK_MS = 80;
const SERVER_PROGRESS_RATE_MIN_BYTES = 1024 * 1024;

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
  const detail = parsed.detail || fallback;
  const error = new Error(detail);
  error.detail = parsed.detail || "";
  error.responseText = text || "";
  error.status = responseStatus;
  return error;
}

async function errorFromResponse(response, fallback) {
  const text = await response.text().catch(() => "");
  return errorFromText(text, response.status, fallback);
}

function progressFromValues(loaded, total, startedAt, options = {}) {
  const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.01);
  const suppressRate =
    options.stage === "preparing" && loaded > 0 && loaded < SERVER_PROGRESS_RATE_MIN_BYTES;
  const bytesPerSecond = suppressRate ? 0 : loaded / elapsedSeconds;
  const finalizing = options.stage === "finalizing" || options.stage === "server-finalizing";
  const etaSeconds =
    total && bytesPerSecond > 0 && loaded < total && !finalizing && !suppressRate
      ? (total - loaded) / bytesPerSecond
      : null;
  return {
    bytesPerSecond: finalizing ? 0 : bytesPerSecond,
    etaSeconds,
    lengthComputable: Boolean(total),
    loaded,
    percent: total ? Math.min(100, Math.max(0, (loaded / total) * 100)) : null,
    resumedBytes: options.resumedBytes || null,
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
      signal?.removeEventListener("abort", onAbort);
      reject(new TransferCancelledError());
    }
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) {
      onAbort();
    }
  });
}

async function measureUploadControlLatency(signal) {
  throwIfAborted(signal);
  const startedAt = performance.now();
  try {
    const response = await fetch(`/health?upload_probe=${Date.now()}`, {
      cache: "no-store",
      credentials: "include",
      signal,
    });
    await response.text().catch(() => "");
    if (!response.ok) {
      return null;
    }
    return performance.now() - startedAt;
  } catch (error) {
    if (isAbortError(error)) {
      throw new TransferCancelledError();
    }
    return null;
  }
}

async function resolveUploadParallelism(signal) {
  const controlRttMs = await measureUploadControlLatency(signal);
  return uploadParallelismForLatency(controlRttMs);
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
  } catch (error) {
    if ([404, 410].includes(error?.status)) {
      return null;
    }
    throw error;
  }
}

function existingUploadSessionStatus(sessionId, signal) {
  return readUploadSessionStatus({ requestJson, sessionId, signal });
}

async function createUploadSession({
  file,
  folder,
  mode,
  documentId,
  note,
  renameToUpload,
  resumeIdentitySha256,
  uploadParallelism,
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
        resume_identity_sha256: resumeIdentitySha256,
        client_upload_parallelism: uploadParallelism,
        size_bytes: file.size,
      }),
      signal,
    },
    "Could not create upload session"
  );
}

async function completeUploadSession(session, { partManifestSha256, sha256 = null }, signal) {
  return requestJson(
    `/api/uploads/${session.id}/complete`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ part_manifest_sha256: partManifestSha256, sha256 }),
      signal,
    },
    "Could not complete upload"
  );
}

function pollUploadVerification({ sessionId, signal, onProgress, isDone }) {
  const startedAt = performance.now();
  return pollUploadVerificationStatus({
    isDone,
    onVerification: (verification) =>
      reportUploadProgress(
        onProgress,
        progressFromValues(
          verification.processed_bytes || 0,
          verification.total_bytes || null,
          startedAt,
          { stage: "verifying" }
        )
      ),
    readStatus: existingUploadSessionStatus,
    sessionId,
    signal,
    wait: waitFor,
  });
}

async function abortUploadSession(sessionId) {
  try {
    const session = await requestJson(
      `/api/uploads/${sessionId}`,
      { method: "DELETE" },
      "Could not cancel upload"
    );
    return { missing: false, session };
  } catch (error) {
    if ([404, 410].includes(error?.status)) {
      return { missing: true, session: null };
    }
    // Cancellation cleanup is best-effort after the client has already aborted in-flight work.
    return { missing: false, session: null };
  }
}

async function abortUploadSessionStrict(sessionId, signal) {
  const session = await requestJson(
    `/api/uploads/${sessionId}`,
    { method: "DELETE", signal },
    "Could not replace an incompatible upload session"
  );
  if (!["aborted", "expired", "failed"].includes(session?.status)) {
    throw new Error("The incompatible upload session could not be terminated.");
  }
  return session;
}

function expectedUploadPart(fileSize, chunkSize, partNumber) {
  const offset = (partNumber - 1) * chunkSize;
  return {
    offset,
    size: Math.max(0, Math.min(chunkSize, fileSize - offset)),
  };
}

function activeUploadSessionMatchesFile(session, file, resumeIdentitySha256) {
  return (
    session?.status === "active" &&
    typeof session.upload_token === "string" &&
    Boolean(session.upload_token) &&
    uploadSessionLayoutMatchesFile(session, file, resumeIdentitySha256)
  );
}

async function validateStoredCompletionManifest(file, session, signal) {
  const expected = completedUploadSessionManifest(session);
  const actual = await uploadFilePartManifestSha256(
    file,
    { chunkSize: session.chunk_size, partCount: session.part_count },
    signal
  );
  if (actual !== expected) {
    throw new Error("Selected file does not match the completed upload session.");
  }
}

function waitForUploadSessionTransition(session, signal) {
  return pollUploadSessionTransition({
    readSession: existingUploadSession,
    readStatus: existingUploadSessionStatus,
    session,
    signal,
    wait: waitFor,
  });
}

async function verifiedCommittedPartDigests(file, session, resumeIdentitySha256, signal) {
  if (!activeUploadSessionMatchesFile(session, file, resumeIdentitySha256)) {
    return null;
  }
  const verified = new Map();
  for (const part of session.uploaded_parts || []) {
    throwIfAborted(signal);
    const partNumber = Number(part?.part_number);
    if (
      !Number.isSafeInteger(partNumber) ||
      partNumber < 1 ||
      partNumber > session.part_count ||
      verified.has(partNumber) ||
      (part?.sha256 != null && !/^[a-f0-9]{64}$/.test(part.sha256))
    ) {
      return null;
    }
    const expected = expectedUploadPart(file.size, session.chunk_size, partNumber);
    if (Number(part.offset) !== expected.offset || Number(part.size_bytes) !== expected.size) {
      return null;
    }
    const chunk = file.slice(expected.offset, expected.offset + expected.size);
    const sha256 = await sha256Blob(chunk, signal);
    if (part.sha256 != null && sha256 !== part.sha256) {
      return null;
    }
    verified.set(partNumber, { sha256, size: expected.size });
  }
  return verified;
}

async function resolveUploadSession({
  documentId,
  file,
  folder,
  key,
  mode,
  note,
  renameToUpload,
  resumeIdentitySha256,
  signal,
  uploadParallelism,
}) {
  let session = null;
  let resumedSession = false;
  let partDigests = new Map();
  const storedSessionId = storedUploadSessionId(key);
  if (storedSessionId) {
    session = await existingUploadSession(storedSessionId, signal);
    if (
      ["complete", "completing"].includes(session?.status) &&
      !uploadSessionLayoutMatchesFile(session, file, resumeIdentitySha256)
    ) {
      throw new Error("Stored upload session layout does not match the selected file.");
    }
    session = await waitForUploadSessionTransition(session, signal);
    if (session?.status === "complete") {
      const completedResult = completedUploadSessionResult(session, file, resumeIdentitySha256);
      await validateStoredCompletionManifest(file, session, signal);
      forgetUploadSession(key);
      return { completedResult, partDigests, resumedSession, session };
    }
    if (!session || ["aborted", "expired", "failed"].includes(session.status)) {
      forgetUploadSession(key);
      session = null;
    } else if (session.status !== "active") {
      throw new Error(`Upload session has unsupported status ${session.status}.`);
    } else if (committedUploadBytes(session) <= 0) {
      await abortUploadSessionStrict(session.id, signal);
      forgetUploadSession(key);
      session = null;
    } else {
      const verifiedParts = await verifiedCommittedPartDigests(
        file,
        session,
        resumeIdentitySha256,
        signal
      );
      const verifiedBytes = verifiedParts
        ? [...verifiedParts.values()].reduce((total, part) => total + part.size, 0)
        : 0;
      if (!verifiedParts || verifiedBytes !== committedUploadBytes(session)) {
        await abortUploadSessionStrict(session.id, signal);
        forgetUploadSession(key);
        session = null;
      } else {
        partDigests = verifiedParts;
        resumedSession = true;
        rememberUploadSession(key, session);
      }
    }
  }
  if (!session) {
    session = await createUploadSession({
      documentId,
      file,
      folder,
      mode,
      note,
      renameToUpload,
      resumeIdentitySha256,
      uploadParallelism,
      signal,
    });
    if (!activeUploadSessionMatchesFile(session, file, resumeIdentitySha256)) {
      const createdSessionId = typeof session?.id === "string" && session.id ? session.id : null;
      if (createdSessionId) {
        await abortUploadSessionStrict(createdSessionId, signal);
      }
      forgetUploadSession(key);
      throw new Error("Upload session layout does not match the selected file.");
    }
    rememberUploadSession(key, session);
  }
  return { partDigests, resumedSession, session };
}

async function reconcileAmbiguousUploadCompletion({
  error,
  file,
  resumeIdentitySha256,
  partManifestSha256,
  session,
  signal,
}) {
  try {
    const statusPayload = await existingUploadSessionStatus(session.id, signal);
    let current = statusPayload ? { ...session, ...statusPayload } : null;
    if (
      current?.status === "completing" &&
      !uploadSessionLayoutMatchesFile(current, file, resumeIdentitySha256)
    ) {
      throw error;
    }
    current = await waitForUploadSessionTransition(current, signal);
    if (current?.status === "complete") {
      validateInMemoryCompletionManifest(current, partManifestSha256);
      return completedUploadSessionResult(current, file, resumeIdentitySha256);
    }
  } catch (recoveryError) {
    if (isAbortError(recoveryError)) {
      throw recoveryError;
    }
    throw error;
  }
  throw error;
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
  const cancellation = createUploadCancellation(signal);
  const uploadSignal = cancellation.signal;
  let key = null;
  let partManifestSha256 = null;
  let session = null;
  let resumeIdentitySha256 = null;

  async function handleFailure(error) {
    if (cancellation.callerCancelled()) {
      const cancellationSessionId = session?.id || (key ? storedUploadSessionId(key) : null);
      const cleanupResult = cancellationSessionId
        ? await abortUploadSession(cancellationSessionId)
        : { missing: false, session: null };
      if (cleanupResult.session?.status === "complete") {
        if (!resumeIdentitySha256 || !partManifestSha256) {
          throw new TransferCancelledError();
        }
        validateInMemoryCompletionManifest(cleanupResult.session, partManifestSha256);
        const result = completedUploadSessionResult(
          cleanupResult.session,
          file,
          resumeIdentitySha256
        );
        forgetUploadSession(key);
        return { body: result, size: file.size, status: 200 };
      }
      if (
        cleanupResult.missing ||
        ["aborted", "expired", "failed"].includes(cleanupResult.session?.status)
      ) {
        forgetUploadSession(key);
      }
      throw new TransferCancelledError();
    }
    if (isAbortError(error)) {
      const unexpectedAbort = new Error("Upload stopped before all parts completed.");
      unexpectedAbort.cause = error;
      throw unexpectedAbort;
    }
    if (error?.detail === "Upload part manifest mismatch" && session?.id) {
      await abortUploadSessionStrict(session.id, uploadSignal);
      forgetUploadSession(key);
    }
    throw error;
  }

  try {
    const contentFingerprint = await uploadContentFingerprint(file, uploadSignal);
    key = uploadSessionKey({
      contentFingerprint,
      documentId,
      file,
      folder,
      mode,
      note,
      renameToUpload,
    });
    resumeIdentitySha256 = await uploadResumeIdentitySha256(key, uploadSignal);
    // Upload session sizing is path-sensitive. Low-latency clients should not
    // pay for high request fanout, but stream-limited clients need enough active
    // PUTs to fill their uplink. The server uses this hint to choose chunk size.
    const uploadParallelism = await resolveUploadParallelism(uploadSignal);
    const resolved = await resolveUploadSession({
      documentId,
      file,
      folder,
      key,
      mode,
      note,
      renameToUpload,
      resumeIdentitySha256,
      signal: uploadSignal,
      uploadParallelism,
    });
    session = resolved.session;
    if (resolved.completedResult) {
      return { body: resolved.completedResult, size: file.size, status: 200 };
    }
    const { partDigests, resumedSession } = resolved;

    const startedAt = performance.now();
    const uploadedParts = new Map(
      (session.uploaded_parts || []).map((part) => [part.part_number, part])
    );
    let completedBytes = [...uploadedParts.values()].reduce(
      (total, part) => total + part.size_bytes,
      0
    );
    const resumedBytes = resumedSession ? completedBytes : 0;
    const activeParts = new Map();
    let lastProgressEmittedAt = 0;

    function emitUploadProgress(options = {}) {
      const now = performance.now();
      if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
        return;
      }
      lastProgressEmittedAt = now;
      reportUploadProgress(
        onProgress,
        progressFromValues(
          currentUploadLoadedBytes({
            activeParts,
            completedBytes,
            fileSize: file.size,
          }),
          file.size,
          startedAt,
          {
            resumedBytes,
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
    if (resumedBytes > 0) {
      reportUploadProgress(
        onProgress,
        progressFromValues(completedBytes, file.size, startedAt, {
          resumedBytes,
          stage: "resuming",
        })
      );
    }

    let nextPartNumber = 1;
    async function uploadWorker() {
      while (nextPartNumber <= session.part_count) {
        throwIfAborted(uploadSignal);
        const partNumber = nextPartNumber;
        nextPartNumber += 1;
        const offset = (partNumber - 1) * session.chunk_size;
        const end = Math.min(offset + session.chunk_size, file.size);
        const chunk = file.slice(offset, end);
        const existing = uploadedParts.get(partNumber);
        if (existing) {
          continue;
        }
        const sha256 = await sha256Blob(chunk, uploadSignal);
        activeParts.set(partNumber, { loaded: 0, size: chunk.size });
        emitUploadProgress({ force: true });
        try {
          await uploadPart({
            chunk,
            onAttemptStart: () => updateActivePartProgress(partNumber, 0, { reset: true }),
            onProgress: (loaded) => updateActivePartProgress(partNumber, loaded),
            offset,
            partNumber,
            session,
            sha256,
            signal: uploadSignal,
          });
        } finally {
          activeParts.delete(partNumber);
        }
        partDigests.set(partNumber, { sha256, size: chunk.size });
        completedBytes += chunk.size;
        emitUploadProgress({ force: true });
      }
    }
    await runUploadWorkers(
      Math.min(uploadParallelism, session.part_count),
      uploadWorker,
      cancellation.abortSiblings
    );

    partManifestSha256 = await uploadPartManifestSha256(
      {
        chunkSize: session.chunk_size,
        fileSize: file.size,
        partCount: session.part_count,
        partDigests,
      },
      uploadSignal
    );

    const verificationStartedAt = performance.now();
    reportUploadProgress(
      onProgress,
      progressFromValues(0, file.size, verificationStartedAt, { stage: "verifying" })
    );
    let verificationDone = false;
    const verificationController = new AbortController();
    const abortVerification = () => verificationController.abort();
    if (uploadSignal.aborted) {
      abortVerification();
    } else {
      uploadSignal.addEventListener("abort", abortVerification, { once: true });
    }
    const verificationPoll = pollUploadVerification({
      isDone: () => verificationDone,
      onProgress,
      sessionId: session.id,
      signal: verificationController.signal,
    }).catch(() => {});
    let result;
    try {
      result = await completeUploadSession(session, { partManifestSha256 }, uploadSignal);
    } catch (error) {
      if (isAbortError(error)) {
        throw error;
      }
      result = await reconcileAmbiguousUploadCompletion({
        error,
        file,
        partManifestSha256,
        resumeIdentitySha256,
        session,
        signal: uploadSignal,
      });
    } finally {
      verificationDone = true;
      verificationController.abort();
      uploadSignal.removeEventListener("abort", abortVerification);
      await verificationPoll;
    }
    forgetUploadSession(key);
    reportUploadProgress(
      onProgress,
      progressFromValues(file.size, file.size, verificationStartedAt, { stage: "verifying" })
    );
    return { body: result, size: file.size, status: 200 };
  } catch (error) {
    return await handleFailure(error);
  } finally {
    cancellation.dispose();
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
      return browserManagedDownload({
        fallbackName,
        fallbackTotal,
        onProgress,
        signal,
        startedAt,
        url,
      });
    }
    if (!writer) {
      writer = await browserDownload.openFileSystemDownloadWriter(fallbackName, signal);
    }
    response = await fetch(url, { credentials: "include", signal });
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
    let current = job;
    while (!["complete", "failed", "cancelled"].includes(current.status)) {
      throwIfAborted(signal);
      onProgress(
        progressFromValues(
          current.processed_bytes || 0,
          current.total_bytes || null,
          exportStartedAt,
          {
            stage: current.status === "finalizing" ? "server-finalizing" : "preparing",
          }
        )
      );
      await waitFor(EXPORT_POLL_MS, signal);
      current = await requestJson(`/api/exports/${job.id}`, { signal }, "Could not refresh export");
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
