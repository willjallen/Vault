import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const transferSourceUrl = new URL("../src/lib/transferClient.js", import.meta.url);
const transferBundle = await build({
  bundle: true,
  entryPoints: [transferSourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const transferModuleUrl = `data:text/javascript;base64,${Buffer.from(
  transferBundle.outputFiles[0].text
).toString("base64")}`;
const { uploadFileResumable } = await import(transferModuleUrl);

async function bundledModule(relativePath) {
  const sourceUrl = new URL(relativePath, import.meta.url);
  const bundle = await build({
    bundle: true,
    entryPoints: [sourceUrl.pathname],
    format: "esm",
    platform: "node",
    write: false,
  });
  return import(
    `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString("base64")}`
  );
}

const {
  sha256Blob,
  uploadContentFingerprint,
  uploadPartManifestSha256,
  uploadResumeIdentitySha256,
} = await bundledModule("../src/lib/fileIntegrity.js");
const { uploadSessionKey: canonicalUploadSessionKey } = await bundledModule(
  "../src/lib/uploadSessionStore.js"
);

globalThis.React = { createElement: () => ({}) };
const dockSourceUrl = new URL("../src/components/TransferDock.js", import.meta.url);
const dockBundle = await build({
  bundle: true,
  entryPoints: [dockSourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const dockModuleUrl = `data:text/javascript;base64,${Buffer.from(
  dockBundle.outputFiles[0].text
).toString("base64")}`;
const { transferMeta, transferStageLabel, transferTitle } = await import(dockModuleUrl);

const STORAGE_KEY = "vault.uploadSessions";

async function uploadIdentity(file, overrides = {}) {
  const contentFingerprint = await uploadContentFingerprint(file);
  const key = canonicalUploadSessionKey({
    contentFingerprint,
    documentId: overrides.documentId || null,
    file,
    folder: overrides.folder || "",
    mode: overrides.mode || "create",
    note: overrides.note || "",
    renameToUpload: Boolean(overrides.renameToUpload),
  });
  return { key, resumeIdentitySha256: await uploadResumeIdentitySha256(key) };
}

function installLocalStorage(initial = {}) {
  const store = new Map(Object.entries(initial));
  globalThis.localStorage = {
    getItem: (key) => store.get(key) || null,
    setItem: (key, value) => store.set(key, String(value)),
  };
  return store;
}

function makeFile(size = 8, seed = 0) {
  const bytes = Uint8Array.from({ length: size }, (_, index) => (index + seed) % 256);
  return fileFromBytes(bytes);
}

function fileFromBytes(bytes, name = "file.bin") {
  const blob = new Blob([bytes], { type: "application/octet-stream" });
  return {
    bytes,
    lastModified: 123,
    name,
    size: bytes.byteLength,
    slice: (start, end) => blob.slice(start, end),
    type: "application/octet-stream",
  };
}

function jsonResponse(body, status = 200) {
  return {
    json: async () => body,
    ok: status >= 200 && status < 300,
    status,
    text: async () => JSON.stringify(body),
  };
}

async function waitUntil(predicate, message) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (predicate()) {
      return;
    }
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.fail(message);
}

function installXhrRecorder() {
  const requests = [];
  globalThis.XMLHttpRequest = class FakeXMLHttpRequest {
    constructor() {
      this.headers = {};
      this.responseText = "{}";
      this.status = 204;
      this.upload = {};
    }

    open(method, url) {
      this.method = method;
      this.url = url;
    }

    setRequestHeader(name, value) {
      this.headers[name.toLowerCase()] = value;
    }

    send(chunk) {
      requests.push({ chunk, headers: { ...this.headers }, method: this.method, url: this.url });
      queueMicrotask(() => {
        this.upload.onprogress?.({ loaded: chunk.size });
        this.onload?.();
      });
    }

    abort() {
      this.onabort?.();
    }
  };
  return requests;
}

async function sessionPayload(file, overrides = {}) {
  const { resumeIdentitySha256 } = await uploadIdentity(file);
  return {
    chunk_size: 4,
    expires_at: new Date(Date.now() + 60 * 60 * 1000).toISOString(),
    filename: file.name,
    id: "old-session",
    part_count: 2,
    part_manifest_sha256: null,
    resume_identity_sha256: resumeIdentitySha256,
    size_bytes: file.size,
    status: "active",
    uploaded_bytes: 0,
    uploaded_parts: [],
    upload_token: "token",
    ...overrides,
  };
}

async function uploadManifest(file, session) {
  const partDigests = new Map();
  for (let partNumber = 1; partNumber <= session.part_count; partNumber += 1) {
    const offset = (partNumber - 1) * session.chunk_size;
    const size = Math.min(session.chunk_size, file.size - offset);
    partDigests.set(partNumber, {
      sha256: await sha256Blob(file.slice(offset, offset + size)),
      size,
    });
  }
  return uploadPartManifestSha256({
    chunkSize: session.chunk_size,
    fileSize: file.size,
    partCount: session.part_count,
    partDigests,
  });
}

async function storedSessionRecord(file, sessionId) {
  const now = Date.now();
  const { key } = await uploadIdentity(file);
  return {
    createdAt: now,
    expiresAt: new Date(now + 60 * 60 * 1000).toISOString(),
    key,
    sessionId,
    updatedAt: now,
  };
}

test("upload abandons a stored active session with no committed bytes", async () => {
  const file = makeFile(4);
  const oldSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "old-zero",
    part_count: 1,
    size_bytes: 4,
  });
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "new-session",
    part_count: 1,
    size_bytes: 4,
  });
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-zero")]),
  });
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push({ body: options.body, method: options.method || "GET", url });
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-zero" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-zero" && options.method === "DELETE") {
      return jsonResponse({ ...oldSession, status: "aborted" });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/new-session" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/new-session/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.deepEqual(
    fetches
      .map((request) => `${request.method} ${request.url}`)
      .filter((entry) => entry.includes("old-zero")),
    ["GET /api/uploads/old-zero", "DELETE /api/uploads/old-zero"]
  );
  assert.equal(xhrRequests[0].url, "/api/uploads/new-session/parts/1");
  assert.equal(xhrRequests[0].headers["x-upload-sha256"], await sha256Blob(file.slice(0, 4)));
  const createBody = JSON.parse(fetches.find((request) => request.url === "/api/uploads").body);
  assert.equal(createBody.resume_identity_sha256, newSession.resume_identity_sha256);
  const stored = JSON.parse(store.get(STORAGE_KEY));
  assert.equal(stored.length, 0);
});

test("upload completes an empty file with a canonical empty-part manifest", async () => {
  const file = makeFile(0);
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "empty-session",
    part_count: 0,
    size_bytes: 0,
  });
  installLocalStorage();
  const xhrRequests = installXhrRecorder();
  let completionBody = null;
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/empty-session" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/empty-session/complete" && options.method === "POST") {
      completionBody = JSON.parse(options.body);
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(result.size, 0);
  assert.equal(xhrRequests.length, 0);
  assert.equal(completionBody.sha256, null);
  assert.match(completionBody.part_manifest_sha256, /^[a-f0-9]{64}$/);
});

test("upload strictly aborts a malformed newly-created session before sending parts", async () => {
  const file = makeFile(4);
  const malformed = await sessionPayload(file, {
    chunk_size: 4,
    id: "malformed-session",
    part_count: 1,
    resume_identity_sha256: "00".repeat(32),
    size_bytes: 4,
  });
  const store = installLocalStorage();
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(malformed);
    }
    if (url === "/api/uploads/malformed-session" && options.method === "DELETE") {
      return jsonResponse({ ...malformed, status: "aborted" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /layout does not match/
  );

  assert.equal(xhrRequests.length, 0);
  assert.ok(fetches.includes("DELETE /api/uploads/malformed-session"));
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("upload accepts the server-normalized basename in a new session", async () => {
  const file = makeFile(4);
  file.name = " folder\\ report.bin ";
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    filename: "report.bin",
    id: "normalized-name",
    part_count: 1,
    size_bytes: 4,
  });
  installLocalStorage();
  const xhrRequests = installXhrRecorder();
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/normalized-name" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/normalized-name/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(xhrRequests.length, 1);
});

test("upload aborts sibling XHRs and waits for them while preserving the first error", async () => {
  const file = makeFile(32);
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "worker-failure",
    part_count: 8,
    size_bytes: file.size,
  });
  const store = installLocalStorage();
  const requests = [];
  const pendingAborts = [];
  globalThis.XMLHttpRequest = class ControlledXMLHttpRequest {
    constructor() {
      this.headers = {};
      this.responseText = "{}";
      this.status = 204;
      this.upload = {};
    }

    open(method, url) {
      this.method = method;
      this.url = url;
    }

    setRequestHeader(name, value) {
      this.headers[name.toLowerCase()] = value;
    }

    send(chunk) {
      this.chunk = chunk;
      requests.push(this);
      if (requests.length === 4) {
        queueMicrotask(() => {
          requests[0].status = 409;
          requests[0].responseText = JSON.stringify({ detail: "first part rejected" });
          requests[0].onload?.();
        });
      }
    }

    abort() {
      pendingAborts.push(this);
    }
  };
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  let settled = false;
  const outcome = uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  })
    .then(
      (value) => ({ value }),
      (error) => ({ error })
    )
    .finally(() => {
      settled = true;
    });

  await waitUntil(() => pendingAborts.length === 3, "sibling XHRs were not aborted");
  assert.equal(settled, false);
  assert.equal(requests.length, 4);
  assert.deepEqual(
    requests.map((request) => request.url).sort(),
    [1, 2, 3, 4].map((part) => `/api/uploads/worker-failure/parts/${part}`)
  );

  pendingAborts.forEach((request) => request.onabort?.());
  const { error } = await outcome;
  assert.equal(error.message, "first part rejected");
  assert.equal(error.status, 409);
  assert.equal(
    fetches.some((entry) => entry.startsWith("DELETE ")),
    false
  );
  assert.equal(JSON.parse(store.get(STORAGE_KEY)).length, 1);
});

test("synchronous XHR setup failure settles once and retains the resumable session", async () => {
  const file = makeFile(4);
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "xhr-setup-failure",
    part_count: 1,
    size_bytes: file.size,
  });
  const store = installLocalStorage();
  let abortCalls = 0;
  globalThis.XMLHttpRequest = class ThrowingXMLHttpRequest {
    constructor() {
      this.upload = {};
    }

    open() {
      const error = new Error("XHR setup failed");
      error.status = 409;
      throw error;
    }

    abort() {
      abortCalls += 1;
    }
  };
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /XHR setup failed/
  );

  assert.equal(abortCalls, 1);
  assert.equal(JSON.parse(store.get(STORAGE_KEY)).length, 1);
});

test("throwing upload progress callbacks cannot fail or strand an upload", async () => {
  const file = makeFile(4);
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "throwing-progress",
    part_count: 1,
    size_bytes: file.size,
  });
  installLocalStorage();
  installXhrRecorder();
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/throwing-progress" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/throwing-progress/complete" && options.method === "POST") {
      return jsonResponse({ id: 1, path: file.name, version: "version-progress" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {
      throw new Error("render failed");
    },
    signal: new AbortController().signal,
  });

  assert.equal(result.body.version, "version-progress");
});

test("upload resumes a stored session only after committed parts exist", async () => {
  const file = makeFile(8);
  const firstPartSha256 = await sha256Blob(file.slice(0, 4));
  const oldSession = await sessionPayload(file, {
    uploaded_bytes: 4,
    uploaded_parts: [{ offset: 0, part_number: 1, sha256: firstPartSha256, size_bytes: 4 }],
  });
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-session")]),
  });
  const xhrRequests = installXhrRecorder();
  const progress = [];
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push({ body: options.body, method: options.method || "GET", url });
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-session" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-session/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: (nextProgress) => progress.push(nextProgress),
    signal: new AbortController().signal,
  });

  assert.equal(
    fetches.some((request) => request.url === "/api/uploads" && request.method === "POST"),
    false
  );
  assert.equal(xhrRequests.length, 1);
  assert.equal(xhrRequests[0].url, "/api/uploads/old-session/parts/2");
  assert.equal(xhrRequests[0].headers["x-upload-sha256"], await sha256Blob(file.slice(4, 8)));
  const completionBody = JSON.parse(
    fetches.find((request) => request.url.endsWith("/complete")).body
  );
  assert.equal(completionBody.sha256, null);
  assert.match(completionBody.part_manifest_sha256, /^[a-f0-9]{64}$/);
  assert.ok(progress.some((nextProgress) => nextProgress.stage === "resuming"));
  assert.ok(progress.some((nextProgress) => nextProgress.resumedBytes === 4));
});

test("upload recovers a stored completed session without creating or sending data", async () => {
  const file = makeFile(4);
  const completedResult = { id: 41, path: file.name, version: "version-complete" };
  const completedSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "stored-complete",
    part_count: 1,
    result: completedResult,
    size_bytes: file.size,
    status: "complete",
    upload_token: null,
  });
  completedSession.part_manifest_sha256 = await uploadManifest(file, completedSession);
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completedSession.id)]),
  });
  globalThis.XMLHttpRequest = class UnexpectedXMLHttpRequest {
    constructor() {
      throw new Error("completed upload must not create an XHR");
    }
  };
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/stored-complete" && !options.method) {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.deepEqual(result.body, completedResult);
  assert.equal(
    fetches.some((entry) => entry === "POST /api/uploads"),
    false
  );
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("malformed completed session result is rejected without losing its idempotency mapping", async () => {
  const file = makeFile(4);
  const completedSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "malformed-complete",
    part_count: 1,
    result: null,
    size_bytes: file.size,
    status: "complete",
    upload_token: null,
  });
  completedSession.part_manifest_sha256 = await uploadManifest(file, completedSession);
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completedSession.id)]),
  });
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/malformed-complete" && !options.method) {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /missing its result/
  );

  assert.equal(
    fetches.some((entry) => entry.startsWith("POST ")),
    false
  );
  assert.equal(
    fetches.some((entry) => entry.startsWith("DELETE ")),
    false
  );
  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, completedSession.id);
});

test("legacy completed session without a server manifest is retained but not trusted", async () => {
  const file = makeFile(4);
  const completedSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "manifestless-complete",
    part_count: 1,
    result: { id: 50, path: file.name, version: "version-manifestless" },
    size_bytes: file.size,
    status: "complete",
    upload_token: null,
  });
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completedSession.id)]),
  });
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/manifestless-complete" && !options.method) {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /missing its integrity manifest/
  );

  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, completedSession.id);
});

test("sample-colliding file cannot claim a stored completed upload result", async () => {
  const chunkSize = 128 * 1024;
  const oldBytes = new Uint8Array(1024 * 1024);
  const selectedBytes = oldBytes.slice();
  selectedBytes[100_000] = 1;
  const oldFile = fileFromBytes(oldBytes);
  const file = fileFromBytes(selectedBytes);
  assert.deepEqual(await uploadContentFingerprint(oldFile), await uploadContentFingerprint(file));
  const completedSession = await sessionPayload(file, {
    chunk_size: chunkSize,
    id: "collision-complete",
    part_count: 8,
    result: { id: 51, path: file.name, version: "version-collision" },
    size_bytes: file.size,
    status: "complete",
    upload_token: null,
  });
  completedSession.part_manifest_sha256 = await uploadManifest(oldFile, completedSession);
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completedSession.id)]),
  });
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/collision-complete" && !options.method) {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /Selected file does not match/
  );

  assert.equal(
    fetches.some((entry) => entry.startsWith("POST ")),
    false
  );
  assert.equal(
    fetches.some((entry) => entry.startsWith("DELETE ")),
    false
  );
  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, completedSession.id);
});

test("upload polls a stored completing session and recovers its committed result", async () => {
  const file = makeFile(4);
  const completedResult = { id: 42, path: file.name, version: "version-polled" };
  const completingSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "stored-completing",
    part_count: 1,
    size_bytes: file.size,
    status: "completing",
  });
  const completedSession = {
    ...completingSession,
    part_manifest_sha256: await uploadManifest(file, completingSession),
    result: completedResult,
    status: "complete",
    upload_token: null,
  };
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completingSession.id)]),
  });
  let statusReads = 0;
  globalThis.XMLHttpRequest = class UnexpectedXMLHttpRequest {
    constructor() {
      throw new Error("completing upload must not create an XHR");
    }
  };
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/stored-completing" && !options.method) {
      statusReads += 1;
      return jsonResponse(statusReads === 1 ? completingSession : completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.deepEqual(result.body, completedResult);
  assert.equal(statusReads, 2);
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("upload resumes when a stored completing session returns to active", async () => {
  const file = makeFile(8);
  const firstPartSha256 = await sha256Blob(file.slice(0, 4));
  const activeSession = await sessionPayload(file, {
    id: "completing-reset",
    uploaded_bytes: 4,
    uploaded_parts: [{ offset: 0, part_number: 1, sha256: firstPartSha256, size_bytes: 4 }],
  });
  const completingSession = { ...activeSession, status: "completing" };
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, activeSession.id)]),
  });
  const xhrRequests = installXhrRecorder();
  let statusReads = 0;
  let created = false;
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      created = true;
      throw new Error("stored active session must be resumed");
    }
    if (url === "/api/uploads/completing-reset" && !options.method) {
      statusReads += 1;
      return jsonResponse(statusReads === 1 ? completingSession : activeSession);
    }
    if (url === "/api/uploads/completing-reset/complete" && options.method === "POST") {
      return jsonResponse({ id: 43, path: file.name, version: "version-reset" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(result.body.version, "version-reset");
  assert.equal(created, false);
  assert.ok(statusReads >= 2);
  assert.deepEqual(
    xhrRequests.map((request) => request.url),
    ["/api/uploads/completing-reset/parts/2"]
  );
});

test("upload replaces a stored session when committed bytes differ", async () => {
  const file = makeFile(8);
  const different = makeFile(4, 99);
  const oldSession = await sessionPayload(file, {
    uploaded_bytes: 4,
    uploaded_parts: [
      {
        offset: 0,
        part_number: 1,
        sha256: await sha256Blob(different.slice(0, 4)),
        size_bytes: 4,
      },
    ],
  });
  const newSession = await sessionPayload(file, { id: "replacement-session" });
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-session")]),
  });
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-session" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-session" && options.method === "DELETE") {
      return jsonResponse({ ...oldSession, status: "aborted" });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/replacement-session" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/replacement-session/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.deepEqual(fetches.slice(1, 4), [
    "GET /api/uploads/old-session",
    "DELETE /api/uploads/old-session",
    "POST /api/uploads",
  ]);
  assert.deepEqual(xhrRequests.map((request) => request.url).sort(), [
    "/api/uploads/replacement-session/parts/1",
    "/api/uploads/replacement-session/parts/2",
  ]);
});

test("sample-colliding files cannot splice committed content during resume", async () => {
  const chunkSize = 128 * 1024;
  const oldBytes = new Uint8Array(1024 * 1024);
  const selectedBytes = oldBytes.slice();
  selectedBytes[100_000] = 1;
  const oldFile = fileFromBytes(oldBytes);
  const file = fileFromBytes(selectedBytes);
  assert.deepEqual(await uploadContentFingerprint(oldFile), await uploadContentFingerprint(file));
  const oldSession = await sessionPayload(file, {
    chunk_size: chunkSize,
    part_count: 8,
    size_bytes: file.size,
    uploaded_bytes: chunkSize,
    uploaded_parts: [
      {
        offset: 0,
        part_number: 1,
        sha256: await sha256Blob(oldFile.slice(0, chunkSize)),
        size_bytes: chunkSize,
      },
    ],
  });
  const replacement = await sessionPayload(file, {
    chunk_size: chunkSize,
    id: "collision-replacement",
    part_count: 8,
    size_bytes: file.size,
  });
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-session")]),
  });
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-session" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-session" && options.method === "DELETE") {
      return jsonResponse({ ...oldSession, status: "aborted" });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(replacement);
    }
    if (url === "/api/uploads/collision-replacement" && !options.method) {
      return jsonResponse(replacement);
    }
    if (url === "/api/uploads/collision-replacement/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.ok(
    fetches.indexOf("DELETE /api/uploads/old-session") < fetches.indexOf("POST /api/uploads")
  );
  assert.equal(xhrRequests.length, 8);
  assert.ok(
    xhrRequests.every((request) =>
      request.url.startsWith("/api/uploads/collision-replacement/parts/")
    )
  );
});

test("upload does not create a replacement when incompatible-session cleanup fails", async () => {
  const file = makeFile(8);
  const different = makeFile(4, 99);
  const oldSession = await sessionPayload(file, {
    uploaded_bytes: 4,
    uploaded_parts: [
      {
        offset: 0,
        part_number: 1,
        sha256: await sha256Blob(different.slice(0, 4)),
        size_bytes: 4,
      },
    ],
  });
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-session")]),
  });
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-session" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-session" && options.method === "DELETE") {
      return jsonResponse({ detail: "cleanup failed" }, 500);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /cleanup failed/
  );

  assert.equal(fetches.includes("POST /api/uploads"), false);
  assert.equal(JSON.parse(store.get(STORAGE_KEY)).length, 1);
});

test("stored session refresh auth, server, and network errors preserve the session", async () => {
  const file = makeFile(4);
  const scenarios = [
    { message: "authentication required", status: 401 },
    { message: "status unavailable", status: 503 },
    { message: "status network failed", status: null },
  ];
  for (const [index, scenario] of scenarios.entries()) {
    const sessionId = `status-unavailable-${index}`;
    const store = installLocalStorage({
      [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, sessionId)]),
    });
    const fetches = [];
    globalThis.fetch = async (url, options = {}) => {
      fetches.push(`${options.method || "GET"} ${url}`);
      if (String(url).startsWith("/health?")) {
        return jsonResponse({ ok: true });
      }
      if (url === `/api/uploads/${sessionId}` && !options.method) {
        if (scenario.status === null) {
          throw new Error(scenario.message);
        }
        return jsonResponse({ detail: scenario.message }, scenario.status);
      }
      throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
    };

    await assert.rejects(
      uploadFileResumable({
        file,
        onProgress: () => {},
        signal: new AbortController().signal,
      }),
      new RegExp(scenario.message)
    );

    assert.equal(
      fetches.some((entry) => entry === "POST /api/uploads"),
      false
    );
    assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, sessionId);
  }
});

test("gone stored sessions are forgotten and replaced", async () => {
  const file = makeFile(4);
  const replacement = await sessionPayload(file, {
    chunk_size: 4,
    id: "gone-replacement",
    part_count: 1,
    size_bytes: file.size,
  });
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "gone-session")]),
  });
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/gone-session" && !options.method) {
      return jsonResponse({ detail: "Upload session expired" }, 410);
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(replacement);
    }
    if (url === "/api/uploads/gone-replacement" && !options.method) {
      return jsonResponse(replacement);
    }
    if (url === "/api/uploads/gone-replacement/complete" && options.method === "POST") {
      return jsonResponse({ id: 44, path: file.name, version: "version-replacement" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(result.body.version, "version-replacement");
  assert.ok(fetches.includes("POST /api/uploads"));
  assert.equal(xhrRequests[0].url, "/api/uploads/gone-replacement/parts/1");
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("upload aborts and forgets a session after server manifest rejection", async () => {
  const file = makeFile(4);
  const newSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "manifest-mismatch",
    part_count: 1,
    size_bytes: 4,
  });
  const store = installLocalStorage();
  installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/manifest-mismatch" && !options.method) {
      return jsonResponse(newSession);
    }
    if (url === "/api/uploads/manifest-mismatch/complete" && options.method === "POST") {
      return jsonResponse({ detail: "Upload part manifest mismatch" }, 400);
    }
    if (url === "/api/uploads/manifest-mismatch" && options.method === "DELETE") {
      return jsonResponse({ ...newSession, status: "aborted" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /Upload part manifest mismatch/
  );

  assert.ok(fetches.includes("DELETE /api/uploads/manifest-mismatch"));
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("upload reconciles a lost completion response through completing to complete", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "ambiguous-completion",
    part_count: 1,
    size_bytes: file.size,
  });
  const completedResult = { id: 45, path: file.name, version: "version-recovered" };
  const completingSession = { ...activeSession, status: "completing" };
  const completedSession = {
    ...activeSession,
    part_manifest_sha256: await uploadManifest(file, activeSession),
    result: completedResult,
    status: "complete",
    upload_token: null,
  };
  const store = installLocalStorage();
  installXhrRecorder();
  let completionAttempted = false;
  let recoveryReads = 0;
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/ambiguous-completion/complete" && options.method === "POST") {
      completionAttempted = true;
      throw new Error("completion response lost");
    }
    if (url === "/api/uploads/ambiguous-completion" && !options.method) {
      if (!completionAttempted) {
        return jsonResponse(activeSession);
      }
      recoveryReads += 1;
      return jsonResponse(recoveryReads === 1 ? completingSession : completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.deepEqual(result.body, completedResult);
  assert.ok(recoveryReads >= 2);
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("completion reconciliation refresh failure preserves the original error and mapping", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "completion-refresh-failure",
    part_count: 1,
    size_bytes: file.size,
  });
  const store = installLocalStorage();
  installXhrRecorder();
  let completionAttempted = false;
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/completion-refresh-failure/complete" && options.method === "POST") {
      completionAttempted = true;
      throw new Error("original completion failure");
    }
    if (url === "/api/uploads/completion-refresh-failure" && !options.method) {
      return completionAttempted
        ? jsonResponse({ detail: "refresh unavailable" }, 503)
        : jsonResponse(activeSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: new AbortController().signal,
    }),
    /original completion failure/
  );

  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, activeSession.id);
});

test("a stalled verification refresh is aborted and cannot hang successful completion", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "stalled-verification",
    part_count: 1,
    size_bytes: file.size,
  });
  installLocalStorage();
  installXhrRecorder();
  let verificationAborted = false;
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/stalled-verification" && !options.method) {
      return new Promise((resolve, reject) => {
        options.signal.addEventListener(
          "abort",
          () => {
            verificationAborted = true;
            const error = new Error("verification cancelled");
            error.name = "AbortError";
            reject(error);
          },
          { once: true }
        );
      });
    }
    if (url === "/api/uploads/stalled-verification/complete" && options.method === "POST") {
      return jsonResponse({ id: 46, path: file.name, version: "version-stalled" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(result.body.version, "version-stalled");
  assert.equal(verificationAborted, true);
});

test("caller cancellation returns success when completion won the server race", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "cancel-complete-race",
    part_count: 1,
    size_bytes: file.size,
  });
  const completedResult = { id: 47, path: file.name, version: "version-cancel-race" };
  const completedSession = {
    ...activeSession,
    part_manifest_sha256: await uploadManifest(file, activeSession),
    result: completedResult,
    status: "complete",
    upload_token: null,
  };
  const store = installLocalStorage();
  installXhrRecorder();
  const controller = new AbortController();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push(`${options.method || "GET"} ${url}`);
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-complete-race" && !options.method) {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-complete-race/complete" && options.method === "POST") {
      controller.abort();
      const error = new Error("completion response cancelled");
      error.name = "AbortError";
      throw error;
    }
    if (url === "/api/uploads/cancel-complete-race" && options.method === "DELETE") {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  const result = await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: controller.signal,
  });

  assert.deepEqual(result.body, completedResult);
  assert.ok(fetches.includes("DELETE /api/uploads/cancel-complete-race"));
  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("early cancellation retains a completed mapping until its identity can be validated", async () => {
  const file = makeFile(4);
  const completedSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "early-cancel-complete",
    part_count: 1,
    result: { id: 49, path: file.name, version: "version-early-cancel" },
    size_bytes: file.size,
    status: "complete",
    upload_token: null,
  });
  completedSession.part_manifest_sha256 = await uploadManifest(file, completedSession);
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, completedSession.id)]),
  });
  const controller = new AbortController();
  const subtle = globalThis.crypto.subtle;
  const originalDigest = subtle.digest;
  let digestCalls = 0;
  subtle.digest = async function (...args) {
    const digest = await originalDigest.call(this, ...args);
    digestCalls += 1;
    if (digestCalls === 2) {
      controller.abort();
    }
    return digest;
  };
  globalThis.fetch = async (url, options = {}) => {
    if (url === "/api/uploads/early-cancel-complete" && options.method === "DELETE") {
      return jsonResponse(completedSession);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  try {
    await assert.rejects(
      uploadFileResumable({
        file,
        onProgress: () => {},
        signal: controller.signal,
      }),
      (error) => error?.cancelled === true
    );
  } finally {
    subtle.digest = originalDigest;
  }

  assert.equal(digestCalls, 2);
  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, completedSession.id);
});

test("unobservable cancellation cleanup retains the resumable session mapping", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "cancel-cleanup-lost",
    part_count: 1,
    size_bytes: file.size,
  });
  const store = installLocalStorage();
  installXhrRecorder();
  const controller = new AbortController();
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-cleanup-lost" && !options.method) {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-cleanup-lost/complete" && options.method === "POST") {
      controller.abort();
      const error = new Error("completion response cancelled");
      error.name = "AbortError";
      throw error;
    }
    if (url === "/api/uploads/cancel-cleanup-lost" && options.method === "DELETE") {
      throw new Error("abort response lost");
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: controller.signal,
    }),
    (error) => error?.cancelled === true
  );

  assert.equal(JSON.parse(store.get(STORAGE_KEY))[0].sessionId, activeSession.id);
});

test("confirmed missing cancellation cleanup forgets the resumable session mapping", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "cancel-cleanup-missing",
    part_count: 1,
    size_bytes: file.size,
  });
  const store = installLocalStorage();
  installXhrRecorder();
  const controller = new AbortController();
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-cleanup-missing" && !options.method) {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/cancel-cleanup-missing/complete" && options.method === "POST") {
      controller.abort();
      const error = new Error("completion response cancelled");
      error.name = "AbortError";
      throw error;
    }
    if (url === "/api/uploads/cancel-cleanup-missing" && options.method === "DELETE") {
      return jsonResponse({ detail: "Upload session not found" }, 404);
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await assert.rejects(
    uploadFileResumable({
      file,
      onProgress: () => {},
      signal: controller.signal,
    }),
    (error) => error?.cancelled === true
  );

  assert.deepEqual(JSON.parse(store.get(STORAGE_KEY)), []);
});

test("upload removes its caller cancellation relay after settling", async () => {
  const file = makeFile(4);
  const activeSession = await sessionPayload(file, {
    chunk_size: 4,
    id: "listener-cleanup",
    part_count: 1,
    size_bytes: file.size,
  });
  installLocalStorage();
  installXhrRecorder();
  const controller = new AbortController();
  const originalAdd = controller.signal.addEventListener.bind(controller.signal);
  const originalRemove = controller.signal.removeEventListener.bind(controller.signal);
  let abortListenerAdds = 0;
  let abortListenerRemoves = 0;
  controller.signal.addEventListener = (type, listener, options) => {
    if (type === "abort") {
      abortListenerAdds += 1;
    }
    return originalAdd(type, listener, options);
  };
  controller.signal.removeEventListener = (type, listener, options) => {
    if (type === "abort") {
      abortListenerRemoves += 1;
    }
    return originalRemove(type, listener, options);
  };
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads" && options.method === "POST") {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/listener-cleanup" && !options.method) {
      return jsonResponse(activeSession);
    }
    if (url === "/api/uploads/listener-cleanup/complete" && options.method === "POST") {
      return jsonResponse({ id: 48, path: file.name, version: "version-listener" });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: controller.signal,
  });

  assert.equal(abortListenerAdds, 1);
  assert.equal(abortListenerRemoves, 1);
});

test("upload retains recovered parts without sidecar hashes for server manifest verification", async () => {
  const file = makeFile(8);
  const oldSession = await sessionPayload(file, {
    uploaded_bytes: 4,
    uploaded_parts: [{ offset: 0, part_number: 1, sha256: null, size_bytes: 4 }],
  });
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([await storedSessionRecord(file, "old-session")]),
  });
  const xhrRequests = installXhrRecorder();
  globalThis.fetch = async (url, options = {}) => {
    if (String(url).startsWith("/health?")) {
      return jsonResponse({ ok: true });
    }
    if (url === "/api/uploads/old-session" && !options.method) {
      return jsonResponse(oldSession);
    }
    if (url === "/api/uploads/old-session/complete" && options.method === "POST") {
      return jsonResponse({ id: 1 });
    }
    throw new Error(`unexpected fetch ${options.method || "GET"} ${url}`);
  };

  await uploadFileResumable({
    file,
    onProgress: () => {},
    signal: new AbortController().signal,
  });

  assert.equal(xhrRequests.length, 1);
  assert.equal(xhrRequests[0].url, "/api/uploads/old-session/parts/2");
});

test("transfer dock calls out resumed uploads", () => {
  const transfer = {
    bytesPerSecond: 0,
    kind: "upload",
    loaded: 4,
    percent: 50,
    resumedBytes: 4,
    stage: "resuming",
    status: "active",
    total: 8,
  };

  assert.equal(transferTitle(transfer), "Resuming upload");
  assert.equal(transferStageLabel(transfer), "Previous upload found");
  assert.equal(transferMeta(transfer), "Resuming previous upload from 50%");
});

test("transfer dock delegates browser-managed download status", () => {
  const transfer = {
    kind: "download",
    stage: "browser-handoff",
    status: "browser-managed",
  };

  assert.equal(transferTitle(transfer), "Download started");
  assert.equal(transferStageLabel(transfer), "Browser download");
  assert.equal(transferMeta(transfer), "Your browser controls the download location and progress");
});
