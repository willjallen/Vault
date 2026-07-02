const UPLOAD_LOW_LATENCY_CONCURRENCY = 4;
const UPLOAD_MAX_CONCURRENCY = 8;
const UPLOAD_GLOBAL_MAX_CONCURRENCY = 8;
const UPLOAD_LOW_LATENCY_RTT_MS = 25;
const UPLOAD_PART_TIMEOUT_MIN_MS = 10 * 60 * 1000;
const UPLOAD_PART_TIMEOUT_MIN_BYTES_PER_SECOND = 64 * 1024;

let activeUploadPartRequests = 0;
const uploadPartWaiters = [];

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
  });
}

export function uploadParallelismForLatency(rttMs) {
  if (Number.isFinite(rttMs) && rttMs >= 0 && rttMs <= UPLOAD_LOW_LATENCY_RTT_MS) {
    return UPLOAD_LOW_LATENCY_CONCURRENCY;
  }
  return UPLOAD_MAX_CONCURRENCY;
}

export function uploadPartTimeoutMs(chunkSize) {
  return Math.max(
    UPLOAD_PART_TIMEOUT_MIN_MS,
    Math.ceil((chunkSize / UPLOAD_PART_TIMEOUT_MIN_BYTES_PER_SECOND) * 1000)
  );
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
