export const UPLOAD_VERIFICATION_POLL_INITIAL_MS = 500;
const UPLOAD_VERIFICATION_POLL_MAX_MS = 2000;

function isAbortError(error) {
  return error?.name === "AbortError" || error?.cancelled;
}

export function nextUploadVerificationPollDelay(currentDelay, advanced) {
  if (advanced) {
    return UPLOAD_VERIFICATION_POLL_INITIAL_MS;
  }
  const boundedCurrent = Math.max(
    UPLOAD_VERIFICATION_POLL_INITIAL_MS,
    Number.isFinite(currentDelay) ? currentDelay : UPLOAD_VERIFICATION_POLL_INITIAL_MS
  );
  return Math.min(UPLOAD_VERIFICATION_POLL_MAX_MS, boundedCurrent * 2);
}

export async function readUploadSessionStatus({ requestJson, sessionId, signal }) {
  try {
    return await requestJson(
      `/api/uploads/${sessionId}/status`,
      { cache: "no-store", signal },
      "Upload session status not found"
    );
  } catch (error) {
    if ([404, 410].includes(error?.status)) {
      return null;
    }
    throw error;
  }
}

export async function pollUploadVerificationStatus({
  isDone,
  onVerification,
  readStatus,
  sessionId,
  signal,
  wait,
}) {
  let delay = UPLOAD_VERIFICATION_POLL_INITIAL_MS;
  let immediate = true;
  let lastProcessed = -1;
  while (!isDone()) {
    let waited = false;
    if (immediate) {
      immediate = false;
    } else {
      await wait(delay, signal);
      waited = true;
    }
    if (isDone()) {
      break;
    }
    let current;
    try {
      current = await readStatus(sessionId, signal);
    } catch (error) {
      if (isAbortError(error)) {
        throw error;
      }
      if (waited) {
        delay = nextUploadVerificationPollDelay(delay, false);
      }
      continue;
    }
    const verification = current?.verification;
    const processed = Number(verification?.processed_bytes);
    const advanced = Number.isFinite(processed) && processed > lastProcessed;
    if (waited) {
      delay = nextUploadVerificationPollDelay(delay, advanced);
    }
    if (!verification) {
      continue;
    }
    if (Number.isFinite(processed)) {
      lastProcessed = Math.max(lastProcessed, processed);
    }
    onVerification(verification);
  }
}

export async function waitForUploadSessionTransition({
  readSession,
  readStatus,
  session,
  signal,
  wait,
}) {
  let current = session;
  let delay = UPLOAD_VERIFICATION_POLL_INITIAL_MS;
  let immediate = true;
  let lastProcessed = -1;
  while (current?.status === "completing") {
    let waited = false;
    if (immediate) {
      immediate = false;
    } else {
      await wait(delay, signal);
      waited = true;
    }
    const statusPayload = await readStatus(current.id, signal);
    if (!statusPayload) {
      return null;
    }
    const processed = Number(statusPayload.verification?.processed_bytes);
    const advanced = Number.isFinite(processed) && processed > lastProcessed;
    if (waited) {
      delay = nextUploadVerificationPollDelay(delay, advanced);
    }
    if (Number.isFinite(processed)) {
      lastProcessed = Math.max(lastProcessed, processed);
    }
    current =
      statusPayload.status === "active"
        ? await readSession(current.id, signal)
        : { ...current, ...statusPayload };
  }
  return current;
}

function normalizedUploadFilename(filename) {
  return String(filename || "")
    .replaceAll("\\", "/")
    .split("/")
    .at(-1)
    .trim();
}

export function uploadSessionLayoutMatchesFile(session, file, resumeIdentitySha256) {
  const chunkSize = Number(session?.chunk_size);
  const partCount = Number(session?.part_count);
  const sizeBytes = Number(session?.size_bytes);
  if (
    typeof session?.id !== "string" ||
    !session.id ||
    !Number.isSafeInteger(chunkSize) ||
    chunkSize <= 0 ||
    !Number.isSafeInteger(partCount) ||
    !Number.isSafeInteger(sizeBytes)
  ) {
    return false;
  }
  const expectedPartCount = file.size > 0 ? Math.ceil(file.size / chunkSize) : 0;
  return (
    partCount === expectedPartCount &&
    sizeBytes === file.size &&
    session.filename === normalizedUploadFilename(file.name) &&
    session.resume_identity_sha256 === resumeIdentitySha256
  );
}

export function completedUploadSessionResult(session, file, resumeIdentitySha256) {
  if (!uploadSessionLayoutMatchesFile(session, file, resumeIdentitySha256)) {
    throw new Error("Completed upload session layout does not match the selected file.");
  }
  const result = session?.result;
  if (
    session.status !== "complete" ||
    !Number.isSafeInteger(result?.id) ||
    result.id <= 0 ||
    typeof result.version !== "string" ||
    !result.version ||
    typeof result.path !== "string" ||
    !result.path
  ) {
    throw new Error("Completed upload session is missing its result.");
  }
  return result;
}

export function completedUploadSessionManifest(session) {
  const manifest = session?.part_manifest_sha256;
  if (typeof manifest !== "string" || !/^[a-f0-9]{64}$/.test(manifest)) {
    throw new Error("Completed upload session is missing its integrity manifest.");
  }
  return manifest;
}

export function validateInMemoryCompletionManifest(session, partManifestSha256) {
  if (completedUploadSessionManifest(session) !== partManifestSha256) {
    throw new Error("Completed upload session integrity manifest does not match this upload.");
  }
}
