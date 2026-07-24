import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/uploadRecovery.js", import.meta.url);
const bundle = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(
  bundle.outputFiles.at(0).text
).toString("base64")}`;
const { discoverRecoverableUploads, recoverableUploadNotice } = await import(moduleUrl);

const STORAGE_KEY = "vault.uploadSessions";

function sessionKey(name, folder = "") {
  return JSON.stringify({
    file: {
      fingerprint: {
        digest: "ab".repeat(32),
        scheme: "sha256-sampled-v1",
      },
      lastModified: 123,
      name,
      size: 100,
    },
    target: {
      documentId: null,
      folder,
      mode: "create",
      note: "",
      renameToUpload: false,
    },
    version: 2,
  });
}

function storedRecord(key, sessionId, updatedAt = Date.now()) {
  return {
    createdAt: updatedAt,
    expiresAt: new Date(updatedAt + 60 * 60 * 1000).toISOString(),
    key,
    sessionId,
    updatedAt,
  };
}

function response(body, status = 200) {
  return {
    json: async () => body,
    ok: status >= 200 && status < 300,
    status,
  };
}

test("startup discovery exposes recoverable sessions and prunes known-dead mappings", async () => {
  const now = Date.now();
  const activeKey = sessionKey("sphinx_master.blend", "Models");
  const failedKey = sessionKey("failed.bin");
  const missingKey = sessionKey("missing.bin");
  const unreachableKey = sessionKey("offline.bin");
  const values = new Map([
    [
      STORAGE_KEY,
      JSON.stringify([
        storedRecord(activeKey, "active", now - 1),
        storedRecord(failedKey, "failed", now - 2),
        storedRecord(missingKey, "missing", now - 3),
        storedRecord(unreachableKey, "unreachable", now - 4),
      ]),
    ],
  ]);
  globalThis.localStorage = {
    getItem: (key) => values.get(key) || null,
    setItem: (key, value) => values.set(key, String(value)),
  };

  const recoveries = await discoverRecoverableUploads({
    apiFetch: async (url) => {
      if (url.endsWith("/active")) {
        return response({
          expires_at: new Date(Date.now() + 60 * 60 * 1000).toISOString(),
          part_count: 4,
          size_bytes: 100,
          status: "active",
          uploaded_bytes: 75,
          uploaded_parts: [{}, {}, {}],
        });
      }
      if (url.endsWith("/failed")) {
        return response({ status: "failed" });
      }
      if (url.endsWith("/missing")) {
        return response({}, 404);
      }
      if (url.endsWith("/unreachable")) {
        throw new TypeError("network unavailable");
      }
      assert.fail(`unexpected URL ${url}`);
    },
    signal: new AbortController().signal,
  });

  assert.equal(recoveries.length, 1);
  assert.deepEqual(
    {
      committedBytes: recoveries[0].committedBytes,
      committedParts: recoveries[0].committedParts,
      fileName: recoveries[0].fileName,
      folder: recoveries[0].target.folder,
      totalParts: recoveries[0].totalParts,
    },
    {
      committedBytes: 75,
      committedParts: 3,
      fileName: "sphinx_master.blend",
      folder: "Models",
      totalParts: 4,
    }
  );
  assert.deepEqual(
    JSON.parse(values.get(STORAGE_KEY))
      .map((record) => record.sessionId)
      .sort(),
    ["active", "unreachable"]
  );
});

test("recovery discovery produces persistent, explicit re-selection guidance", () => {
  const notice = recoverableUploadNotice([
    {
      committedBytes: 75,
      committedParts: 3,
      fileName: "sphinx_master.blend",
      totalBytes: 100,
      totalParts: 4,
    },
    {
      committedBytes: 20,
      committedParts: 1,
      fileName: "rocket.blend",
      totalBytes: 100,
      totalParts: 5,
    },
  ]);

  assert.equal(notice.duration, null);
  assert.equal(notice.progress, false);
  assert.equal(notice.title, "Interrupted uploads available");
  assert.equal(
    notice.detail,
    "sphinx_master.blend has 3 of 4 parts secured. 1 other interrupted upload is also available. Select the same file again from its original upload or check-in action to continue."
  );
});
