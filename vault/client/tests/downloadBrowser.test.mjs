/* global AbortController, Response, URL, WritableStream, window */

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
  transferBundle.outputFiles.at(0).text
).toString("base64")}`;
const { TransferCancelledError, downloadUrl, exportAndDownload } = await import(transferModuleUrl);

function installBrowser({ onClick } = {}) {
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
          onClick?.();
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

test("browser-managed checkout prepares before click and uses the pinned URL", async () => {
  const events = [];
  const downloads = installBrowser({ onClick: () => events.push("click") });
  globalThis.fetch = async () => {
    throw new Error("browser-managed downloads must not fetch through JavaScript");
  };

  const result = await downloadUrl({
    fallbackName: "checkout.txt",
    fallbackTotal: 8,
    onProgress: () => {},
    prepare: async (signal) => {
      assert.equal(signal.aborted, false);
      events.push("prepare");
      return "/documents/1/versions/version-1/download";
    },
    signal: new AbortController().signal,
  });

  assert.deepEqual(events, ["prepare", "click"]);
  assert.equal(
    downloads.at(0).href,
    "https://vault.example/documents/1/versions/version-1/download"
  );
  assert.equal(result.browserManaged, true);
});

test("browser-managed checkout rejects a prepared cross-origin URL before handoff", async () => {
  const downloads = installBrowser();

  await assert.rejects(
    downloadUrl({
      fallbackName: "checkout.txt",
      onProgress: () => {},
      prepare: async () => "https://files.example/checkout.txt",
      signal: new AbortController().signal,
    }),
    /same origin/
  );

  assert.deepEqual(downloads, []);
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
  const events = [];
  const written = [];
  const requests = [];
  let suggestedName = "";
  let closed = false;
  window.showSaveFilePicker = async (options) => {
    events.push("picker");
    suggestedName = options.suggestedName;
    return {
      createWritable: async () => {
        events.push("writer");
        return new WritableStream({
          close() {
            closed = true;
          },
          write(chunk) {
            written.push(...chunk);
          },
        });
      },
    };
  };
  globalThis.fetch = async (url, options) => {
    events.push("fetch");
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
    prepare: async () => {
      events.push("prepare");
      return "/documents/1/versions/version-1/download";
    },
    signal: new AbortController().signal,
  });

  assert.deepEqual(events, ["picker", "writer", "prepare", "fetch"]);
  assert.equal(suggestedName, "report.pdf");
  assert.deepEqual(written, [1, 2, 3, 4]);
  assert.equal(closed, true);
  assert.equal(requests.length, 1);
  assert.equal(requests.at(0).url, "/documents/1/versions/version-1/download");
  assert.equal(requests.at(0).options.credentials, "include");
  assert.equal(requests.at(0).options.headers, undefined);
  assert.equal(progress.at(-1).stage, "finalizing");
  assert.deepEqual(result, { filename: "server-report.pdf", size: 4, status: 200 });
});

test("picker cancellation never prepares checkout", async () => {
  installBrowser();
  let prepareCalls = 0;
  window.showSaveFilePicker = async () => {
    const error = new Error("picker cancelled");
    error.name = "AbortError";
    throw error;
  };
  globalThis.fetch = async () => {
    throw new Error("picker cancellation must not fetch");
  };

  await assert.rejects(
    downloadUrl({
      customDownloadsEnabled: true,
      fallbackName: "checkout.txt",
      onProgress: () => {},
      prepare: async () => {
        prepareCalls += 1;
        return "/documents/1/versions/version-1/download";
      },
      signal: new AbortController().signal,
    }),
    (error) => error instanceof TransferCancelledError
  );

  assert.equal(prepareCalls, 0);
});

test("checkout preparation failure aborts the chosen writer without a handoff", async () => {
  const downloads = installBrowser();
  let abortCalls = 0;
  let fetchCalls = 0;
  window.showSaveFilePicker = async () => ({
    createWritable: async () => ({
      abort: async () => {
        abortCalls += 1;
      },
    }),
  });
  globalThis.fetch = async () => {
    fetchCalls += 1;
    throw new Error("preparation failure must not fetch");
  };

  await assert.rejects(
    downloadUrl({
      customDownloadsEnabled: true,
      fallbackName: "checkout.txt",
      onProgress: () => {},
      prepare: async () => {
        throw new Error("Lock failed");
      },
      signal: new AbortController().signal,
    }),
    /Lock failed/
  );

  assert.equal(abortCalls, 1);
  assert.equal(fetchCalls, 0);
  assert.deepEqual(downloads, []);
});

test("checkout cancellation during preparation aborts the writer and maps to cancellation", async () => {
  installBrowser();
  const controller = new AbortController();
  let abortCalls = 0;
  window.showSaveFilePicker = async () => ({
    createWritable: async () => ({
      abort: async () => {
        abortCalls += 1;
      },
    }),
  });

  await assert.rejects(
    downloadUrl({
      customDownloadsEnabled: true,
      fallbackName: "checkout.txt",
      onProgress: () => {},
      prepare: async () => {
        controller.abort();
        throw new Error("wrapped cancellation");
      },
      signal: controller.signal,
    }),
    (error) => error instanceof TransferCancelledError
  );

  assert.equal(abortCalls, 1);
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
