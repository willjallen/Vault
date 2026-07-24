export { downloadUrl, exportAndDownload } from "./downloadClient.js";
export { TransferCancelledError } from "./transferCore.js";

import {
  sha256Blob,
  uploadContentFingerprint,
  uploadFilePartManifestSha256,
  uploadPartManifestSha256,
  uploadResumeIdentitySha256,
} from "./fileIntegrity.js";
import {
  createUploadCancellation,
  currentUploadInFlightBytes,
  currentUploadLoadedBytes,
  reportUploadProgress,
  runUploadWorkers,
  shouldRetryUploadPart,
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
import {
  TransferCancelledError,
  isAbortError,
  progressFromValues,
  requestJson,
  throwIfAborted,
  waitFor,
} from "./transferCore.js";

const PROGRESS_TICK_MS = 80;
const UPLOAD_RECONCILIATION_REQUEST_TIMEOUT_MS = 15 * 1000;

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

async function existingUploadSessionForReconciliation(sessionId, signal) {
  throwIfAborted(signal);
  const controller = new AbortController();
  let timedOut = false;
  const abortFromCaller = () => controller.abort();
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, UPLOAD_RECONCILIATION_REQUEST_TIMEOUT_MS);
  signal?.addEventListener("abort", abortFromCaller, { once: true });
  try {
    return await existingUploadSession(sessionId, controller.signal);
  } catch (error) {
    if (signal?.aborted) {
      throw new TransferCancelledError();
    }
    if (timedOut) {
      const timeoutError = new Error("Vault did not answer the upload status request");
      timeoutError.networkError = true;
      timeoutError.timeout = true;
      throw timeoutError;
    }
    throw error;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", abortFromCaller);
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

function pollUploadVerification({ fileSize, sessionId, signal, onProgress, isDone }) {
  const startedAt = performance.now();
  return pollUploadVerificationStatus({
    isDone,
    onVerification: (verification) =>
      reportUploadProgress(
        onProgress,
        progressFromValues(fileSize, fileSize, startedAt, {
          committedBytes: fileSize,
          inFlightBytes: 0,
          stage: "verifying",
          verificationLoaded: verification.processed_bytes || 0,
          verificationTotal: verification.total_bytes || null,
        })
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

function matchingCommittedUploadPart(current, partNumber, expected, sha256) {
  const part = (current?.uploaded_parts || []).find(
    (candidate) => Number(candidate?.part_number) === partNumber
  );
  if (!part) {
    return null;
  }
  if (Number(part.offset) !== expected.offset || Number(part.size_bytes) !== expected.size) {
    const error = new Error("Vault reported conflicting content for a retried upload part.");
    error.status = 409;
    throw error;
  }
  if (part.sha256 == null) {
    return null;
  }
  if (part.sha256 !== sha256) {
    const error = new Error("Vault reported conflicting content for a retried upload part.");
    error.status = 409;
    throw error;
  }
  return part;
}

async function reconcileUploadPartAfterFailure({
  attempt,
  expected,
  file,
  key,
  maxAttempts,
  onState,
  partNumber,
  resumeIdentitySha256,
  session,
  sha256,
  signal,
}) {
  let retryDelayMs = 1000;
  while (true) {
    throwIfAborted(signal);
    let current;
    try {
      current = await existingUploadSessionForReconciliation(session.id, signal);
    } catch (error) {
      if (isAbortError(error)) {
        throw error;
      }
      if (!shouldRetryUploadPart(error)) {
        throw error;
      }
      onState({
        attempt,
        maxAttempts,
        partNumber,
        retryDelayMs,
        stage: "reconnecting",
      });
      await waitFor(retryDelayMs, signal);
      retryDelayMs = Math.min(30_000, retryDelayMs * 2);
      continue;
    }

    if (!current) {
      const error = new Error("The upload session expired while reconnecting.");
      error.status = 410;
      throw error;
    }
    if (!uploadSessionLayoutMatchesFile(current, file, resumeIdentitySha256)) {
      throw new Error("The upload session changed while reconnecting.");
    }
    const committed = matchingCommittedUploadPart(current, partNumber, expected, sha256);
    if (committed) {
      rememberUploadSession(key, current);
      return committed;
    }
    if (current.status !== "active") {
      const error = new Error(`Upload session is ${current.status}.`);
      error.status = 409;
      throw error;
    }
    rememberUploadSession(key, current);
    return null;
  }
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
    // Upload session sizing is path-sensitive. Use modest fanout on paths that
    // have not demonstrated low latency; the server also uses this hint when it
    // chooses the session's immutable chunk layout.
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
    let committedBytes = [...uploadedParts.values()].reduce(
      (total, part) => total + part.size_bytes,
      0
    );
    const resumedBytes = resumedSession ? committedBytes : 0;
    const activeParts = new Map();
    let lastProgressEmittedAt = 0;

    function activeUploadPresentation() {
      const priorities = {
        "awaiting-ack": 1,
        retrying: 2,
        stalled: 3,
        reconciling: 4,
        reconnecting: 5,
      };
      let selected = null;
      for (const part of activeParts.values()) {
        if (!selected || (priorities[part.stage] || 0) > (priorities[selected.stage] || 0)) {
          selected = part;
        }
      }
      return {
        attempt: selected?.attempt ?? null,
        maxAttempts: selected?.maxAttempts ?? null,
        noProgressSeconds: selected?.noProgressSeconds || 0,
        retryDelayMs: selected?.retryDelayMs || null,
        stage: selected?.stage || "uploading",
        waitingForAcknowledgement: Boolean(selected?.waitingForAcknowledgement),
      };
    }

    function emitUploadProgress(options = {}) {
      const now = performance.now();
      if (!options.force && now - lastProgressEmittedAt < PROGRESS_TICK_MS) {
        return;
      }
      lastProgressEmittedAt = now;
      const inFlightBytes = currentUploadInFlightBytes(activeParts);
      reportUploadProgress(
        onProgress,
        progressFromValues(
          currentUploadLoadedBytes({
            activeParts,
            completedBytes: committedBytes,
            fileSize: file.size,
          }),
          file.size,
          startedAt,
          {
            committedBytes,
            inFlightBytes,
            resumedBytes,
            ...activeUploadPresentation(),
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

    function updateActivePartState(partNumber, state) {
      const current = activeParts.get(partNumber);
      if (!current) {
        return;
      }
      activeParts.set(partNumber, {
        ...current,
        ...state,
      });
      emitUploadProgress({ force: true });
    }

    emitUploadProgress({ force: true });
    if (resumedBytes > 0) {
      reportUploadProgress(
        onProgress,
        progressFromValues(committedBytes, file.size, startedAt, {
          committedBytes,
          inFlightBytes: 0,
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
        const expected = { offset, size: chunk.size };
        activeParts.set(partNumber, {
          attempt: null,
          loaded: 0,
          maxAttempts: null,
          size: chunk.size,
          stage: "uploading",
        });
        emitUploadProgress({ force: true });
        try {
          await uploadPart({
            chunk,
            onAttemptStart: (state) => {
              updateActivePartProgress(partNumber, 0, { reset: true });
              updateActivePartState(partNumber, { ...state, stage: "uploading" });
            },
            onProgress: (loaded) => updateActivePartProgress(partNumber, loaded),
            onState: (state) => updateActivePartState(partNumber, state),
            offset,
            partNumber,
            reconcileAfterFailure: async ({ attempt, maxAttempts }) => {
              // XHR progress describes bytes handed to the transport, not
              // durable server state. Once that request fails, discard its
              // speculative contribution before asking Vault what committed.
              updateActivePartProgress(partNumber, 0, { reset: true });
              updateActivePartState(partNumber, {
                attempt,
                maxAttempts,
                stage: "reconciling",
              });
              const committed = await reconcileUploadPartAfterFailure({
                attempt,
                expected,
                file,
                key,
                maxAttempts,
                onState: (state) => updateActivePartState(partNumber, state),
                partNumber,
                resumeIdentitySha256,
                session,
                sha256,
                signal: uploadSignal,
              });
              if (committed) {
                uploadedParts.set(partNumber, committed);
              }
              return Boolean(committed);
            },
            session,
            sha256,
            signal: uploadSignal,
          });
        } finally {
          activeParts.delete(partNumber);
        }
        partDigests.set(partNumber, { sha256, size: chunk.size });
        committedBytes += chunk.size;
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
      progressFromValues(file.size, file.size, verificationStartedAt, {
        committedBytes: file.size,
        inFlightBytes: 0,
        stage: "verifying",
      })
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
      fileSize: file.size,
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
      progressFromValues(file.size, file.size, verificationStartedAt, {
        committedBytes: file.size,
        inFlightBytes: 0,
        stage: "verifying",
      })
    );
    return { body: result, size: file.size, status: 200 };
  } catch (error) {
    return await handleFailure(error);
  } finally {
    cancellation.dispose();
  }
}
