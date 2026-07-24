const UPLOAD_LOW_LATENCY_CONCURRENCY = 4;
const UPLOAD_CONSTRAINED_CONCURRENCY = 2;
const UPLOAD_GLOBAL_MAX_CONCURRENCY = 4;
const UPLOAD_LOW_LATENCY_RTT_MS = 25;
export const UPLOAD_PART_STALL_NOTICE_MS = 15 * 1000;
export const UPLOAD_PART_IDLE_TIMEOUT_MS = 75 * 1000;
export const UPLOAD_PART_ACK_TIMEOUT_MS = 90 * 1000;
const UPLOAD_RETRY_LIMIT = 3;
const UPLOAD_RETRY_INITIAL_DELAY_MS = 1000;
const UPLOAD_RETRY_MAX_DELAY_MS = 3000;

let activeUploadPartRequests = 0;
const uploadPartWaiters = [];

export function createUploadCancellation(callerSignal) {
  const controller = new AbortController();
  let callerCancelled = Boolean(callerSignal?.aborted);
  const abortFromCaller = () => {
    callerCancelled = true;
    controller.abort();
  };
  if (callerSignal?.aborted) {
    abortFromCaller();
  } else {
    callerSignal?.addEventListener("abort", abortFromCaller, { once: true });
  }
  return {
    abortSiblings: () => controller.abort(),
    callerCancelled: () => callerCancelled,
    dispose: () => callerSignal?.removeEventListener("abort", abortFromCaller),
    signal: controller.signal,
  };
}

export class UploadPartPolicyCancelledError extends Error {
  constructor(message = "Transfer cancelled") {
    super(message);
    this.cancelled = true;
    this.name = "TransferCancelledError";
  }
}

function isAbortError(error) {
  return error?.name === "AbortError" || error?.cancelled;
}

function throwIfUploadPolicyAborted(signal) {
  if (signal?.aborted) {
    throw new UploadPartPolicyCancelledError();
  }
}

function waitForRetry(delay, signal) {
  throwIfUploadPolicyAborted(signal);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, delay);
    function onAbort() {
      clearTimeout(timer);
      signal?.removeEventListener("abort", onAbort);
      reject(new UploadPartPolicyCancelledError());
    }
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) {
      onAbort();
    }
  });
}

function releaseUploadPartSlot() {
  activeUploadPartRequests = Math.max(0, activeUploadPartRequests - 1);
  const next = uploadPartWaiters.shift();
  if (next) {
    next();
  }
}

export function acquireUploadPartSlot(signal) {
  throwIfUploadPolicyAborted(signal);
  if (activeUploadPartRequests < UPLOAD_GLOBAL_MAX_CONCURRENCY) {
    activeUploadPartRequests += 1;
    return Promise.resolve(releaseUploadPartSlot);
  }

  return new Promise((resolve, reject) => {
    let settled = false;

    function cleanup() {
      signal?.removeEventListener("abort", onAbort);
    }

    function grant() {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      activeUploadPartRequests += 1;
      resolve(releaseUploadPartSlot);
    }

    function onAbort() {
      if (settled) {
        return;
      }
      settled = true;
      const index = uploadPartWaiters.indexOf(grant);
      if (index >= 0) {
        uploadPartWaiters.splice(index, 1);
      }
      cleanup();
      reject(new UploadPartPolicyCancelledError());
    }

    uploadPartWaiters.push(grant);
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) {
      onAbort();
    }
  });
}

export function uploadParallelismForLatency(rttMs) {
  if (Number.isFinite(rttMs) && rttMs >= 0 && rttMs <= UPLOAD_LOW_LATENCY_RTT_MS) {
    return UPLOAD_LOW_LATENCY_CONCURRENCY;
  }
  return UPLOAD_CONSTRAINED_CONCURRENCY;
}

export function uploadRetryDelayMs(failedAttempt) {
  return failedAttempt <= 1 ? UPLOAD_RETRY_INITIAL_DELAY_MS : UPLOAD_RETRY_MAX_DELAY_MS;
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

function errorFromText(text, responseStatus) {
  const parsed = parseJson(text);
  const error = new Error(parsed.detail || "Upload part failed");
  error.detail = parsed.detail || "";
  error.responseText = text || "";
  error.status = responseStatus;
  return error;
}

export function reportUploadProgress(callback, loaded) {
  try {
    callback?.(loaded);
  } catch {
    // Transfer progress is cosmetic and must never control request settlement.
  }
}

export function reportUploadState(callback, state) {
  try {
    callback?.(state);
  } catch {
    // Transfer state is presentational and must never control request settlement.
  }
}

export function currentUploadInFlightBytes(activeParts) {
  return [...activeParts.values()].reduce(
    (total, part) => total + Math.min(part.size, Math.max(0, part.loaded || 0)),
    0
  );
}

export function currentUploadLoadedBytes({ activeParts, completedBytes, fileSize }) {
  const activeBytes = currentUploadInFlightBytes(activeParts);
  return Math.min(fileSize, Math.max(completedBytes, completedBytes + activeBytes));
}

function uploadPartRequest({
  ackTimeoutMs,
  attempt,
  chunk,
  idleTimeoutMs,
  maxAttempts,
  offset,
  onProgress,
  onState,
  partNumber,
  session,
  sha256,
  signal,
  stallNoticeMs,
}) {
  throwIfUploadPolicyAborted(signal);
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    let lastLoaded = 0;
    let phase = "uploading";
    let requestStarted = false;
    let settled = false;
    let stallTimer = null;
    let timeoutTimer = null;

    function clearWatchdogs() {
      clearTimeout(stallTimer);
      clearTimeout(timeoutTimer);
      stallTimer = null;
      timeoutTimer = null;
    }

    function cleanup() {
      clearWatchdogs();
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

    function emitState(stage, details = {}) {
      if (stage !== "stalled") {
        phase = stage;
      }
      reportUploadState(onState, {
        attempt,
        maxAttempts,
        partNumber,
        stage,
        ...details,
      });
    }

    function abortForWatchdog() {
      const awaitingAcknowledgement = phase === "awaiting-ack";
      const error = new Error(
        awaitingAcknowledgement
          ? "Vault did not acknowledge the upload part"
          : "Upload part stopped making progress"
      );
      error.networkError = true;
      error.timeout = true;
      error.timeoutPhase = phase;
      settle(reject, error);
      try {
        xhr.abort();
      } catch {
        // The watchdog error has already settled the request.
      }
    }

    function armWatchdogs(timeoutMs) {
      clearWatchdogs();
      const boundedTimeout = Math.max(1, Number(timeoutMs) || 1);
      const boundedNotice = Math.max(0, Number(stallNoticeMs) || 0);
      if (boundedNotice > 0 && boundedNotice < boundedTimeout) {
        const armedAt = Date.now();
        const reportStall = () => {
          emitState("stalled", {
            noProgressSeconds: Math.max(1, Math.floor((Date.now() - armedAt) / 1000)),
            waitingForAcknowledgement: phase === "awaiting-ack",
          });
          stallTimer = setTimeout(reportStall, 1000);
        };
        stallTimer = setTimeout(reportStall, boundedNotice);
      }
      timeoutTimer = setTimeout(abortForWatchdog, boundedTimeout);
    }

    function abortRequest() {
      if (!requestStarted) {
        settle(reject, new UploadPartPolicyCancelledError());
      }
      try {
        xhr.abort();
      } catch {
        settle(reject, new UploadPartPolicyCancelledError());
      }
    }

    xhr.upload.onprogress = (progressEvent) => {
      const loaded = Number.isFinite(progressEvent.loaded) ? progressEvent.loaded : 0;
      const boundedLoaded = Math.min(chunk.size, Math.max(0, loaded));
      reportUploadProgress(onProgress, boundedLoaded);
      if (boundedLoaded <= lastLoaded) {
        return;
      }
      lastLoaded = boundedLoaded;
      if (boundedLoaded >= chunk.size) {
        emitState("awaiting-ack", { waitingForAcknowledgement: true });
        armWatchdogs(ackTimeoutMs);
      } else {
        emitState("uploading");
        armWatchdogs(idleTimeoutMs);
      }
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        settle(resolve, parseJson(xhr.responseText));
        reportUploadProgress(onProgress, chunk.size);
        return;
      }
      settle(reject, errorFromText(xhr.responseText, xhr.status));
    };
    xhr.onerror = () => {
      const error = new Error("Network error during upload");
      error.networkError = true;
      settle(reject, error);
    };
    xhr.onabort = () => {
      settle(reject, new UploadPartPolicyCancelledError());
    };
    xhr.ontimeout = () => {
      const error = new Error("Upload part timed out");
      error.networkError = true;
      error.timeout = true;
      error.timeoutPhase = phase;
      settle(reject, error);
    };

    try {
      signal?.addEventListener("abort", abortRequest, { once: true });
      if (signal?.aborted) {
        abortRequest();
        return;
      }
      xhr.open("PUT", `/api/uploads/${session.id}/parts/${partNumber}`);
      // A total XHR timeout penalizes healthy, slow transfers while letting a
      // motionless request wait for the whole allowance. Progress-sensitive
      // watchdogs above own timeout behavior instead.
      xhr.timeout = 0;
      xhr.withCredentials = true;
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.setRequestHeader("X-Upload-Offset", String(offset));
      xhr.setRequestHeader("X-Upload-Size", String(chunk.size));
      xhr.setRequestHeader("X-Upload-Sha256", sha256);
      if (session.upload_token) {
        xhr.setRequestHeader("X-Upload-Token", session.upload_token);
      }
      requestStarted = true;
      xhr.send(chunk);
      // Test doubles and a cached/local transport can settle synchronously.
      // Do not leave a watchdog behind after their load callback has run.
      if (!settled) {
        emitState("uploading");
        armWatchdogs(idleTimeoutMs);
      }
    } catch (error) {
      settle(reject, error);
      try {
        xhr.abort();
      } catch {
        // A synchronous setup failure has already rejected and cleaned up.
      }
    }
  });
}

export async function uploadPart({
  session,
  partNumber,
  chunk,
  offset,
  onAttemptStart,
  onProgress,
  onState,
  reconcileAfterFailure,
  sha256,
  signal,
  stallNoticeMs = UPLOAD_PART_STALL_NOTICE_MS,
  idleTimeoutMs = UPLOAD_PART_IDLE_TIMEOUT_MS,
  ackTimeoutMs = UPLOAD_PART_ACK_TIMEOUT_MS,
  retryDelayForAttempt = uploadRetryDelayMs,
}) {
  for (let attempt = 1; attempt <= UPLOAD_RETRY_LIMIT; attempt += 1) {
    throwIfUploadPolicyAborted(signal);
    onAttemptStart?.({ attempt, maxAttempts: UPLOAD_RETRY_LIMIT, partNumber });
    try {
      const releaseSlot = await acquireUploadPartSlot(signal);
      try {
        return await uploadPartRequest({
          ackTimeoutMs,
          attempt,
          chunk,
          idleTimeoutMs,
          maxAttempts: UPLOAD_RETRY_LIMIT,
          offset,
          onProgress,
          onState,
          partNumber,
          session,
          sha256,
          signal,
          stallNoticeMs,
        });
      } finally {
        releaseSlot();
      }
    } catch (error) {
      if (!shouldRetryUploadPart(error)) {
        throw error;
      }
      if (
        await reconcileAfterFailure?.({
          attempt,
          error,
          maxAttempts: UPLOAD_RETRY_LIMIT,
          partNumber,
        })
      ) {
        return { reconciled: true };
      }
      if (attempt >= UPLOAD_RETRY_LIMIT) {
        throw error;
      }
      const retryDelayMs = retryDelayForAttempt(attempt);
      reportUploadState(onState, {
        attempt: attempt + 1,
        maxAttempts: UPLOAD_RETRY_LIMIT,
        partNumber,
        retryDelayMs,
        stage: "retrying",
      });
      await waitForRetry(retryDelayMs, signal);
    }
  }
  throw new Error("Upload part failed");
}

export async function runUploadWorkers(workerCount, worker, abortSiblings) {
  let firstError = null;
  const workers = Array.from({ length: workerCount }, () =>
    worker().catch((error) => {
      if (firstError === null) {
        firstError = error;
        abortSiblings();
      }
      throw error;
    })
  );
  await Promise.allSettled(workers);
  if (firstError !== null) {
    throw firstError;
  }
}

export function shouldRetryUploadPart(error) {
  if (isAbortError(error)) {
    return false;
  }
  if (error?.networkError || !error?.status) {
    return true;
  }
  if (
    error.status === 400 &&
    (!error.responseText ||
      error.detail === "Upload failed while reading request body" ||
      error.detail === "Upload part size does not match session")
  ) {
    return true;
  }
  return [408, 429, 500, 502, 503, 504].includes(error.status);
}
