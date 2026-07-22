import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  useCallback: () => {},
  useEffect: () => {},
  useState: () => {},
};

const sourceUrl = new URL("../src/lib/theme.js", import.meta.url);
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
const { normalizeAcknowledgedVersion, normalizeUserPreferences } = await import(moduleUrl);

test("boolean preferences accept only real booleans", () => {
  const normalized = normalizeUserPreferences({
    openFoldersOnClick: false,
    alternateRows: true,
    doubleClickDownload: true,
    downloadLocationGuidanceDismissed: true,
  });

  assert.equal(normalized.openFoldersOnClick, false);
  assert.equal(normalized.alternateRows, true);
  assert.equal(normalized.doubleClickDownload, true);
  assert.equal(normalized.downloadLocationGuidanceDismissed, true);
});

test("boolean preference strings fall back to defaults", () => {
  const normalized = normalizeUserPreferences({
    openFoldersOnClick: "false",
    alternateRows: "true",
    doubleClickDownload: "true",
    downloadLocationGuidanceDismissed: "true",
  });

  assert.equal(normalized.openFoldersOnClick, true);
  assert.equal(normalized.alternateRows, false);
  assert.equal(normalized.doubleClickDownload, false);
  assert.equal(normalized.downloadLocationGuidanceDismissed, false);
});

test("synced contents views are normalized by folder path", () => {
  const normalized = normalizeUserPreferences({
    contentsViewByFolder: {
      "Projects/Alpha": { iconSize: 112, mode: "icons", version: 99 },
    },
  });

  assert.deepEqual(normalized.contentsViewByFolder, {
    "Projects/Alpha": { iconSize: 112, mode: "icons", version: 1 },
  });
});

test("acknowledged What's New versions accept only compact release identifiers", () => {
  assert.equal(normalizeAcknowledgedVersion("2.1.0"), "2.1.0");
  assert.equal(normalizeAcknowledgedVersion("2.1.0-rc.1"), "2.1.0-rc.1");
  assert.equal(normalizeAcknowledgedVersion("not a version"), "");
  assert.equal(normalizeUserPreferences({}).whatsNewAcknowledgedVersion, "");
});
