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
const { downloadUrl, exportAndDownload } = await import(transferModuleUrl);

function installBrowser() {
  const downloads = [];
  globalThis.window = {
    location: {
      href: "https://vault.example/app",
      origin: "https://vault.example",
    },
    showSaveFilePicker: async () => {
      throw new Error("custom downloads must remain disabled");
    },
  };
  globalThis.document = {
    body: {
      appendChild: (link) => {
        link.appended = true;
      },
    },
    createElement: (tagName) => {
      assert.equal(tagName, "a");
      return {
        click() {
          assert.equal(this.appended, true);
          downloads.push({ download: this.download, hidden: this.hidden, href: this.href });
        },
        remove() {
          this.appended = false;
        },
      };
    },
  };
  return downloads;
}

test("default downloads hand the URL directly to the browser without a probe", async () => {
  const downloads = installBrowser();
  globalThis.fetch = async () => {
    throw new Error("browser-managed downloads must not fetch through JavaScript");
  };
  const progress = [];

  const result = await downloadUrl({
    fallbackName: "quarterly:report.pdf",
    fallbackTotal: 42,
    onProgress: (nextProgress) => progress.push(nextProgress),
    signal: new AbortController().signal,
    url: "/documents/1/download",
  });

  assert.deepEqual(downloads, [
    {
      download: "quarterly_report.pdf",
      hidden: true,
      href: "https://vault.example/documents/1/download",
    },
  ]);
  assert.equal(progress.at(-1).stage, "browser-handoff");
  assert.deepEqual(result, {
    browserManaged: true,
    filename: "quarterly_report.pdf",
    size: 42,
    status: 202,
  });
});

test("browser-managed downloads reject cross-origin URLs", async () => {
  const downloads = installBrowser();

  await assert.rejects(
    downloadUrl({
      fallbackName: "report.pdf",
      onProgress: () => {},
      signal: new AbortController().signal,
      url: "https://files.example/report.pdf",
    }),
    /same origin/
  );

  assert.deepEqual(downloads, []);
});

test("enabled picker downloads use one bounded browser stream without range requests", async () => {
  installBrowser();
  const written = [];
  const requests = [];
  let suggestedName = "";
  let closed = false;
  window.showSaveFilePicker = async (options) => {
    suggestedName = options.suggestedName;
    return {
      createWritable: async () =>
        new WritableStream({
          close() {
            closed = true;
          },
          write(chunk) {
            written.push(...chunk);
          },
        }),
    };
  };
  globalThis.fetch = async (url, options) => {
    requests.push({ options, url });
    return new Response(new Uint8Array([1, 2, 3, 4]), {
      headers: {
        "Content-Disposition": 'attachment; filename="server-report.pdf"',
        "Content-Length": "4",
      },
      status: 200,
    });
  };
  const progress = [];

  const result = await downloadUrl({
    customDownloadsEnabled: true,
    fallbackName: "report.pdf",
    onProgress: (nextProgress) => progress.push(nextProgress),
    signal: new AbortController().signal,
    url: "/documents/1/download",
  });

  assert.equal(suggestedName, "report.pdf");
  assert.deepEqual(written, [1, 2, 3, 4]);
  assert.equal(closed, true);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "/documents/1/download");
  assert.equal(requests[0].options.credentials, "include");
  assert.equal(requests[0].options.headers, undefined);
  assert.equal(progress.at(-1).stage, "finalizing");
  assert.deepEqual(result, { filename: "server-report.pdf", size: 4, status: 200 });
});

test("completed exports are handed directly to the browser", async () => {
  const downloads = installBrowser();
  const requests = [];
  globalThis.fetch = async (url, options = {}) => {
    requests.push({ method: options.method || "GET", url });
    assert.equal(url, "/api/exports");
    assert.equal(options.method, "POST");
    return {
      json: async () => ({
        download_url: "/api/exports/export-1/download",
        filename: "vault-export.zip",
        id: "export-1",
        size_bytes: 128,
        status: "complete",
      }),
      ok: true,
      status: 200,
    };
  };

  const result = await exportAndDownload({
    onProgress: () => {},
    payload: { document_ids: [1] },
    signal: new AbortController().signal,
  });

  assert.deepEqual(requests, [{ method: "POST", url: "/api/exports" }]);
  assert.deepEqual(downloads, [
    {
      download: "vault-export.zip",
      hidden: true,
      href: "https://vault.example/api/exports/export-1/download",
    },
  ]);
  assert.equal(result.browserManaged, true);
});
