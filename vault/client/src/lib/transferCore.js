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

export async function errorFromResponse(response, fallback) {
  const text = await response.text().catch(() => "");
  return errorFromText(text, response.status, fallback);
}

export function finiteNonnegative(value) {
  const number = Number(value);
  return Number.isFinite(number) ? Math.max(0, number) : 0;
}

function progressTiming(loaded, total, startedAt, options) {
  const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.01);
  const suppressRate =
    options.noProgressSeconds > 0 ||
    (options.stage === "preparing" && loaded > 0 && loaded < SERVER_PROGRESS_RATE_MIN_BYTES);
  const bytesPerSecond = suppressRate ? 0 : loaded / elapsedSeconds;
  const finalizing = ["finalizing", "server-finalizing", "verifying"].includes(options.stage);
  const etaSeconds =
    total && bytesPerSecond > 0 && loaded < total && !finalizing && !suppressRate
      ? (total - loaded) / bytesPerSecond
      : null;
  return {
    bytesPerSecond: finalizing ? 0 : bytesPerSecond,
    etaSeconds,
  };
}

function normalizedCommittedBytes(total, value) {
  if (value === null || value === undefined) {
    return null;
  }
  const maximum = total || Number.MAX_SAFE_INTEGER;
  return Math.min(maximum, finiteNonnegative(value));
}

function progressPercentage(loaded, total, committedBytes) {
  if (!total) {
    return null;
  }
  const rawPercent = Math.min(100, Math.max(0, (loaded / total) * 100));
  return rawPercent === 100 && committedBytes !== null && committedBytes < total
    ? 99.9
    : rawPercent;
}

function valueOrNull(value) {
  return value || null;
}

export function progressFromValues(loaded, total, startedAt, options = {}) {
  const committedBytes = normalizedCommittedBytes(total, options.committedBytes);
  const timing = progressTiming(loaded, total, startedAt, options);
  return {
    attempt: options.attempt ?? null,
    bytesPerSecond: timing.bytesPerSecond,
    committedBytes,
    createdAt: valueOrNull(options.createdAt),
    etaSeconds: timing.etaSeconds,
    inFlightBytes: finiteNonnegative(options.inFlightBytes),
    lengthComputable: Boolean(total),
    loaded,
    maxAttempts: options.maxAttempts ?? null,
    noProgressSeconds: options.noProgressSeconds || 0,
    percent: progressPercentage(loaded, total, committedBytes),
    processedItems: options.processedItems ?? null,
    resumedBytes: valueOrNull(options.resumedBytes),
    retryDelayMs: valueOrNull(options.retryDelayMs),
    serverStatus: valueOrNull(options.serverStatus),
    stage: options.stage || "transfer",
    total,
    totalItems: options.totalItems ?? null,
    updatedAt: valueOrNull(options.updatedAt),
    waitingForAcknowledgement: Boolean(options.waitingForAcknowledgement),
  };
}

export function byteLength(value) {
  return value?.byteLength || value?.size || value?.length || 0;
}

export function isAbortError(error) {
  return error?.name === "AbortError" || error?.cancelled;
}

export function throwIfAborted(signal) {
  if (signal?.aborted) {
    throw new TransferCancelledError();
  }
}

export function waitFor(delay, signal) {
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

export async function requestJson(url, options = {}, fallback = "Request failed") {
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
