import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

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
  UPLOAD_FINGERPRINT_SAMPLE_BYTES,
} = await bundledModule("../src/lib/fileIntegrity.js");
const { storedUploadSessionId, uploadSessionKey } = await bundledModule(
  "../src/lib/uploadSessionStore.js"
);

const ABCD_SHA256 = "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589";
const EFGH_SHA256 = "e5e088a0b66163a0a26a5e053d2a4496dc16ab6e0e3dd1adf2d16aa84a078c9d";

test("upload hashes match canonical SHA-256 and manifest vectors", async () => {
  assert.equal(
    await sha256Blob(new Blob(["abc"])),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
  );
  const manifest = await uploadPartManifestSha256({
    chunkSize: 4,
    fileSize: 8,
    partCount: 2,
    partDigests: new Map([
      [1, { sha256: ABCD_SHA256, size: 4 }],
      [2, { sha256: EFGH_SHA256, size: 4 }],
    ]),
  });
  assert.equal(manifest, "7cc9fd2e08c97a13a7d7aa4129de08ae9067cc0b1e93b99680ad1f111d113839");
});

test("content fingerprint sampling stays bounded for a five GiB file", async () => {
  const slices = [];
  const file = {
    size: 5 * 1024 * 1024 * 1024,
    slice(start, end) {
      slices.push({ end, start });
      return new Blob([new Uint8Array(end - start)]);
    },
  };
  const fingerprint = await uploadContentFingerprint(file);
  assert.equal(fingerprint.scheme, "sha256-sampled-v1");
  assert.match(fingerprint.digest, /^[a-f0-9]{64}$/);
  assert.equal(slices.length, 3);
  assert.ok(slices.every(({ end, start }) => end - start <= UPLOAD_FINGERPRINT_SAMPLE_BYTES));
});

test("sample collisions remain possible and are only a lookup hint", async () => {
  const left = new Uint8Array(1024 * 1024);
  const right = left.slice();
  right[100_000] = 1;
  const leftFile = new Blob([left]);
  const rightFile = new Blob([right]);
  assert.deepEqual(
    await uploadContentFingerprint(leftFile),
    await uploadContentFingerprint(rightFile)
  );
  assert.notEqual(
    await sha256Blob(leftFile.slice(65_536, 131_072)),
    await sha256Blob(rightFile.slice(65_536, 131_072))
  );
});

test("structured upload keys separate delimiter collisions and bind resume identity", async () => {
  const fileA = { lastModified: 7, name: "z", size: 4 };
  const fileB = { lastModified: 7, name: "y|z", size: 4 };
  const contentFingerprint = { digest: ABCD_SHA256, scheme: "sha256-sampled-v1" };
  const keyA = uploadSessionKey({ contentFingerprint, file: fileA, folder: "x|y" });
  const keyB = uploadSessionKey({ contentFingerprint, file: fileB, folder: "x" });
  assert.notEqual(keyA, keyB);
  assert.notEqual(await uploadResumeIdentitySha256(keyA), await uploadResumeIdentitySha256(keyB));
});

test("legacy delimiter keys are pruned instead of resumed", () => {
  const legacyKey = "create|||file.bin|8|123||";
  const now = Date.now();
  const values = new Map([
    [
      "vault.uploadSessions",
      JSON.stringify([{ createdAt: now, key: legacyKey, sessionId: "legacy", updatedAt: now }]),
    ],
  ]);
  globalThis.localStorage = {
    getItem: (key) => values.get(key) || null,
    setItem: (key, value) => values.set(key, String(value)),
  };
  assert.equal(storedUploadSessionId(legacyKey), null);
  assert.deepEqual(JSON.parse(values.get("vault.uploadSessions")), []);
});
