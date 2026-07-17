const UPLOAD_FINGERPRINT_SCHEME = "sha256-sampled-v1";
const UPLOAD_FINGERPRINT_SAMPLE_BYTES = 64 * 1024;
const UPLOAD_PART_MANIFEST_DOMAIN = "vault-upload-part-manifest-v1";
const UPLOAD_RESUME_IDENTITY_DOMAIN = "vault-upload-resume-identity-v1";
const MAX_CONCURRENT_DIGESTS = 2;

let activeDigests = 0;
const digestWaiters = [];

function abortError() {
  const error = new Error("Transfer cancelled");
  error.name = "AbortError";
  return error;
}

function throwIfAborted(signal) {
  if (signal?.aborted) {
    throw abortError();
  }
}

function subtleCrypto() {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) {
    throw new Error("This browser cannot verify upload integrity.");
  }
  return subtle;
}

function bytesToHex(bytes) {
  return [...new Uint8Array(bytes)].map((value) => value.toString(16).padStart(2, "0")).join("");
}

function releaseDigestSlot() {
  const next = digestWaiters.shift();
  if (next) {
    next();
    return;
  }
  activeDigests = Math.max(0, activeDigests - 1);
}

function acquireDigestSlot(signal) {
  throwIfAborted(signal);
  if (activeDigests < MAX_CONCURRENT_DIGESTS) {
    activeDigests += 1;
    return Promise.resolve(releaseDigestSlot);
  }
  return new Promise((resolve, reject) => {
    function grant() {
      signal?.removeEventListener("abort", onAbort);
      resolve(releaseDigestSlot);
    }
    function onAbort() {
      const index = digestWaiters.indexOf(grant);
      if (index >= 0) {
        digestWaiters.splice(index, 1);
      }
      reject(abortError());
    }
    digestWaiters.push(grant);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

async function sha256Bytes(bytes, signal) {
  throwIfAborted(signal);
  const digest = await subtleCrypto().digest("SHA-256", bytes);
  throwIfAborted(signal);
  return bytesToHex(digest);
}

export async function sha256Blob(blob, signal) {
  const release = await acquireDigestSlot(signal);
  try {
    throwIfAborted(signal);
    const bytes = await blob.arrayBuffer();
    throwIfAborted(signal);
    return await sha256Bytes(bytes, signal);
  } finally {
    release();
  }
}

function fingerprintOffsets(size, sampleSize) {
  if (size <= 0 || sampleSize <= 0) {
    return [];
  }
  const lastOffset = Math.max(0, size - sampleSize);
  return [...new Set([0, Math.floor(lastOffset / 2), lastOffset])].sort(
    (left, right) => left - right
  );
}

export async function uploadContentFingerprint(file, signal) {
  throwIfAborted(signal);
  const sampleSize = Math.min(UPLOAD_FINGERPRINT_SAMPLE_BYTES, Math.max(0, file.size));
  const offsets = fingerprintOffsets(file.size, sampleSize);
  const samples = [];
  for (const offset of offsets) {
    throwIfAborted(signal);
    samples.push({
      bytes: new Uint8Array(await file.slice(offset, offset + sampleSize).arrayBuffer()),
      offset,
    });
  }
  const descriptor = new TextEncoder().encode(
    `${UPLOAD_FINGERPRINT_SCHEME}\nsize=${file.size}\n${samples
      .map((sample) => `sample=${sample.offset}:${sample.bytes.byteLength}`)
      .join("\n")}\n`
  );
  const totalBytes = samples.reduce(
    (total, sample) => total + sample.bytes.byteLength,
    descriptor.length
  );
  const fingerprintBytes = new Uint8Array(totalBytes);
  fingerprintBytes.set(descriptor);
  let cursor = descriptor.length;
  for (const sample of samples) {
    fingerprintBytes.set(sample.bytes, cursor);
    cursor += sample.bytes.byteLength;
  }
  return {
    digest: await sha256Bytes(fingerprintBytes, signal),
    scheme: UPLOAD_FINGERPRINT_SCHEME,
  };
}

export async function uploadResumeIdentitySha256(uploadSessionKey, signal) {
  return sha256Bytes(
    new TextEncoder().encode(`${UPLOAD_RESUME_IDENTITY_DOMAIN}\n${uploadSessionKey}`),
    signal
  );
}

export async function uploadPartManifestSha256(
  { chunkSize, fileSize, partCount, partDigests },
  signal
) {
  const lines = [
    UPLOAD_PART_MANIFEST_DOMAIN,
    `size=${fileSize}`,
    `chunk=${chunkSize}`,
    `parts=${partCount}`,
  ];
  for (let partNumber = 1; partNumber <= partCount; partNumber += 1) {
    const part = partDigests.get(partNumber);
    if (!part || !/^[a-f0-9]{64}$/.test(part.sha256)) {
      throw new Error("Upload part integrity verification is incomplete.");
    }
    const offset = (partNumber - 1) * chunkSize;
    lines.push(`part=${partNumber}:${offset}:${part.size}:${part.sha256}`);
  }
  return sha256Bytes(new TextEncoder().encode(`${lines.join("\n")}\n`), signal);
}

export async function uploadFilePartManifestSha256(file, { chunkSize, partCount }, signal) {
  const partDigests = new Map();
  for (let partNumber = 1; partNumber <= partCount; partNumber += 1) {
    throwIfAborted(signal);
    const offset = (partNumber - 1) * chunkSize;
    const size = Math.max(0, Math.min(chunkSize, file.size - offset));
    const chunk = file.slice(offset, offset + size);
    partDigests.set(partNumber, {
      sha256: await sha256Blob(chunk, signal),
      size,
    });
  }
  return uploadPartManifestSha256(
    { chunkSize, fileSize: file.size, partCount, partDigests },
    signal
  );
}

export { UPLOAD_FINGERPRINT_SAMPLE_BYTES, UPLOAD_FINGERPRINT_SCHEME };
