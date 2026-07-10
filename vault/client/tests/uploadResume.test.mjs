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

function uploadSessionKey(file) {
  return ["create", "", "", file.name, file.size, file.lastModified, "", ""].join("|");
}

function installLocalStorage(initial = {}) {
  const store = new Map(Object.entries(initial));
  globalThis.localStorage = {
    getItem: (key) => store.get(key) || null,
    setItem: (key, value) => store.set(key, String(value)),
  };
  return store;
}

function makeFile(size = 8) {
  return {
    lastModified: 123,
    name: "file.bin",
    size,
    slice: (start, end) => ({ size: Math.max(0, end - start) }),
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
      requests.push({ chunk, method: this.method, url: this.url });
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

function sessionPayload(overrides = {}) {
  return {
    chunk_size: 4,
    expires_at: new Date(Date.now() + 60 * 60 * 1000).toISOString(),
    filename: "file.bin",
    id: "old-session",
    part_count: 2,
    size_bytes: 8,
    status: "active",
    uploaded_bytes: 0,
    uploaded_parts: [],
    upload_token: "token",
    ...overrides,
  };
}

function storedSessionRecord(file, sessionId) {
  const now = Date.now();
  return {
    createdAt: now,
    expiresAt: new Date(now + 60 * 60 * 1000).toISOString(),
    key: uploadSessionKey(file),
    sessionId,
    updatedAt: now,
  };
}

test("upload abandons a stored active session with no committed bytes", async () => {
  const file = makeFile(4);
  const oldSession = sessionPayload({
    chunk_size: 4,
    id: "old-zero",
    part_count: 1,
    size_bytes: 4,
  });
  const newSession = sessionPayload({
    chunk_size: 4,
    id: "new-session",
    part_count: 1,
    size_bytes: 4,
  });
  const store = installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([storedSessionRecord(file, "old-zero")]),
  });
  const xhrRequests = installXhrRecorder();
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push({ method: options.method || "GET", url });
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
  const stored = JSON.parse(store.get(STORAGE_KEY));
  assert.equal(stored.length, 0);
});

test("upload resumes a stored session only after committed parts exist", async () => {
  const file = makeFile(8);
  const oldSession = sessionPayload({
    uploaded_bytes: 4,
    uploaded_parts: [{ offset: 0, part_number: 1, size_bytes: 4 }],
  });
  installLocalStorage({
    [STORAGE_KEY]: JSON.stringify([storedSessionRecord(file, "old-session")]),
  });
  const xhrRequests = installXhrRecorder();
  const progress = [];
  const fetches = [];
  globalThis.fetch = async (url, options = {}) => {
    fetches.push({ method: options.method || "GET", url });
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
  assert.ok(progress.some((nextProgress) => nextProgress.stage === "resuming"));
  assert.ok(progress.some((nextProgress) => nextProgress.resumedBytes === 4));
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
