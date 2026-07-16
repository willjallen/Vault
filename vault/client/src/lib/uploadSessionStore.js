const UPLOAD_SESSION_STORAGE_KEY = "vault.uploadSessions";
const UPLOAD_SESSION_STORAGE_TTL_MS = 6 * 60 * 60 * 1000;
const UPLOAD_SESSION_KEY_VERSION = 2;

function isCanonicalUploadSessionKey(key) {
  try {
    const parsed = JSON.parse(key);
    return (
      parsed?.version === UPLOAD_SESSION_KEY_VERSION &&
      parsed.file?.fingerprint?.scheme === "sha256-sampled-v1" &&
      /^[a-f0-9]{64}$/.test(parsed.file?.fingerprint?.digest || "")
    );
  } catch {
    return false;
  }
}

function normalizeStoredUploadSessionRecord(record, nowMs) {
  if (!record || typeof record !== "object") {
    return null;
  }
  const key = typeof record.key === "string" ? record.key : "";
  const sessionId = typeof record.sessionId === "string" ? record.sessionId : "";
  const updatedAt = Number(record.updatedAt);
  const createdAt = Number(record.createdAt);
  if (!key || !isCanonicalUploadSessionKey(key) || !sessionId || !Number.isFinite(updatedAt)) {
    return null;
  }
  if (nowMs - updatedAt > UPLOAD_SESSION_STORAGE_TTL_MS) {
    return null;
  }
  const expiresAt = typeof record.expiresAt === "string" ? record.expiresAt : "";
  if (expiresAt) {
    const expiresAtMs = Date.parse(expiresAt);
    if (!Number.isFinite(expiresAtMs) || expiresAtMs <= nowMs) {
      return null;
    }
  }
  return {
    createdAt: Number.isFinite(createdAt) ? createdAt : updatedAt,
    expiresAt,
    key,
    sessionId,
    updatedAt,
  };
}

function readStoredUploadSessions() {
  try {
    const parsed = JSON.parse(localStorage.getItem(UPLOAD_SESSION_STORAGE_KEY) || "[]");
    if (!Array.isArray(parsed)) {
      return [];
    }
    const records = parsed
      .map((record) => normalizeStoredUploadSessionRecord(record, Date.now()))
      .filter(Boolean);
    if (records.length !== parsed.length) {
      writeStoredUploadSessions(records);
    }
    return records;
  } catch {
    return [];
  }
}

function writeStoredUploadSessions(records) {
  localStorage.setItem(UPLOAD_SESSION_STORAGE_KEY, JSON.stringify(records));
}

export function uploadSessionKey({
  contentFingerprint,
  file,
  folder,
  mode,
  documentId,
  note,
  renameToUpload,
}) {
  if (
    contentFingerprint?.scheme !== "sha256-sampled-v1" ||
    !/^[a-f0-9]{64}$/.test(contentFingerprint?.digest || "")
  ) {
    throw new Error("Upload content fingerprint is invalid.");
  }
  return JSON.stringify({
    version: UPLOAD_SESSION_KEY_VERSION,
    file: {
      fingerprint: contentFingerprint,
      lastModified: Number(file.lastModified) || 0,
      name: String(file.name || ""),
      size: Number(file.size) || 0,
    },
    target: {
      documentId: documentId || null,
      folder: folder || "",
      mode: mode || "create",
      note: note || "",
      renameToUpload: Boolean(renameToUpload),
    },
  });
}

export function rememberUploadSession(key, session) {
  const sessionId = typeof session === "string" ? session : session?.id;
  if (!sessionId) {
    return;
  }
  const nowMs = Date.now();
  const sessions = readStoredUploadSessions();
  const existing = sessions.find((record) => record.key === key);
  const next = {
    createdAt: Number.isFinite(existing?.createdAt) ? existing.createdAt : nowMs,
    expiresAt: typeof session?.expires_at === "string" ? session.expires_at : "",
    key,
    sessionId,
    updatedAt: nowMs,
  };
  const nextSessions = sessions.filter((record) => record.key !== key);
  nextSessions.push(next);
  writeStoredUploadSessions(nextSessions);
}

export function forgetUploadSession(key) {
  writeStoredUploadSessions(readStoredUploadSessions().filter((record) => record.key !== key));
}

export function storedUploadSessionId(key) {
  return readStoredUploadSessions().find((record) => record.key === key)?.sessionId || null;
}

export function committedUploadBytes(session) {
  if (Number.isFinite(session?.uploaded_bytes) && session.uploaded_bytes > 0) {
    return session.uploaded_bytes;
  }
  return (session?.uploaded_parts || []).reduce(
    (total, part) => total + Math.max(0, Number(part.size_bytes) || 0),
    0
  );
}
