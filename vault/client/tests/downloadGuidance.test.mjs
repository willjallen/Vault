import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/downloadGuidance.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { confirmNativeDownload } = await import(moduleUrl);

test("native download guidance repeats until explicitly dismissed", async () => {
  const requests = [];
  const options = {
    dismissed: false,
    onDismiss: async () => {
      throw new Error("ordinary downloads must not dismiss guidance");
    },
    requestConfirm: async (request) => {
      requests.push(request);
      return { confirmed: true, remember: false };
    },
  };

  assert.equal(await confirmNativeDownload(options), true);
  assert.equal(await confirmNativeDownload(options), true);
  assert.equal(requests.length, 2);
  assert.equal(requests[0].rememberLabel, "Do not show again");
  assert.match(requests[0].message, /browser profile, not only Vault/);
});

test("Do not show again persists through the supplied synced preference callback", async () => {
  let dismissed = false;
  let requests = 0;
  const onDismiss = async () => {
    dismissed = true;
  };
  const requestConfirm = async () => {
    requests += 1;
    return { confirmed: true, remember: true };
  };

  assert.equal(await confirmNativeDownload({ dismissed, onDismiss, requestConfirm }), true);
  assert.equal(dismissed, true);
  assert.equal(await confirmNativeDownload({ dismissed, onDismiss, requestConfirm }), true);
  assert.equal(requests, 1);
});

test("cancelled native download guidance does not start a download", async () => {
  assert.equal(
    await confirmNativeDownload({
      dismissed: false,
      onDismiss: async () => {},
      requestConfirm: async () => ({ confirmed: false, remember: false }),
    }),
    false
  );
});
