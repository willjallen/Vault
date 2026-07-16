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

const { sha256Blob, uploadContentFingerprint, uploadResumeIdentitySha256 } = await bundledModule(
  "../src/lib/fileIntegrity.js"
);
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
    resume_identity_sha256: resumeIdentitySha256,
    size_bytes: file.size,
    status: "active",
    uploaded_bytes: 0,
    uploaded_parts: [],
    upload_token: "token",
    ...overrides,
  };
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
