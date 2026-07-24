import {
  committedUploadBytes,
  forgetUploadSession,
  storedUploadSessionRecords,
} from "./uploadSessionStore.js";

const RECOVERABLE_UPLOAD_STATUSES = new Set(["active", "completing"]);
const TERMINAL_UPLOAD_STATUSES = new Set(["aborted", "expired", "failed"]);

async function inspectStoredUpload(record, apiFetch, signal) {
  let response;
  try {
    response = await apiFetch(`/api/uploads/${encodeURIComponent(record.sessionId)}`, {
      cache: "no-store",
      signal,
    });
  } catch (error) {
    if (signal?.aborted || error?.name === "AbortError") {
      throw error;
    }
    // Discovery is advisory. A temporarily unreachable Vault must not erase a
    // session that can still be reconciled when the user resumes it.
    return null;
  }

  if ([404, 410].includes(response.status)) {
    forgetUploadSession(record.key);
    return null;
  }
  if (!response.ok) {
    return null;
  }

  const session = await response.json().catch(() => null);
  if (!session || typeof session !== "object") {
    return null;
  }
  if (TERMINAL_UPLOAD_STATUSES.has(session.status)) {
    forgetUploadSession(record.key);
    return null;
  }

  const committedBytes = committedUploadBytes(session);
  if (!RECOVERABLE_UPLOAD_STATUSES.has(session.status) || committedBytes <= 0) {
    return null;
  }
  const committedParts = Array.isArray(session.uploaded_parts) ? session.uploaded_parts.length : 0;
  const totalBytes = Math.max(0, Number(session.size_bytes) || record.file.size);
  const totalParts = Math.max(0, Number(session.part_count) || 0);
  return {
    committedBytes: Math.min(totalBytes || committedBytes, committedBytes),
    committedParts,
    expiresAt: session.expires_at || record.expiresAt,
    fileName: record.file.name || session.filename || "File",
    key: record.key,
    sessionId: record.sessionId,
    status: session.status,
    target: record.target,
    totalBytes,
    totalParts,
    updatedAt: record.updatedAt,
  };
}

export async function discoverRecoverableUploads({ apiFetch, signal }) {
  const records = storedUploadSessionRecords();
  const inspected = await Promise.all(
    records.map((record) => inspectStoredUpload(record, apiFetch, signal))
  );
  return inspected
    .filter(Boolean)
    .sort((left, right) => Number(right.updatedAt) - Number(left.updatedAt));
}

function securedDescription(recovery) {
  if (recovery.totalParts > 0 && recovery.committedParts > 0) {
    return `${recovery.committedParts} of ${recovery.totalParts} parts secured`;
  }
  if (recovery.totalBytes > 0) {
    const percent = Math.min(
      100,
      Math.max(0, (recovery.committedBytes / recovery.totalBytes) * 100)
    );
    return `${percent.toFixed(1)}% secured`;
  }
  return "partially secured";
}

export function recoverableUploadNotice(recoveries) {
  if (!recoveries.length) {
    return null;
  }
  const first = recoveries[0];
  const others = recoveries.length - 1;
  const otherCopy = others
    ? ` ${others} other interrupted ${others === 1 ? "upload is" : "uploads are"} also available.`
    : "";
  return {
    detail: `${first.fileName} has ${securedDescription(first)}.${otherCopy} Select the same file again from its original upload or check-in action to continue.`,
    duration: null,
    kind: "info",
    progress: false,
    title:
      recoveries.length === 1 ? "Interrupted upload available" : "Interrupted uploads available",
  };
}
