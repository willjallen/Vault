import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

globalThis.React = {
  useCallback: () => {},
  useEffect: () => {},
  useState: () => {},
};

const sourceUrl = new URL("../src/lib/theme.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { normalizeUserPreferences } = await import(moduleUrl);

test("boolean preferences accept only real booleans", () => {
  const normalized = normalizeUserPreferences({
    openFoldersOnClick: false,
    alternateRows: true,
    doubleClickDownload: true,
  });

  assert.equal(normalized.openFoldersOnClick, false);
  assert.equal(normalized.alternateRows, true);
  assert.equal(normalized.doubleClickDownload, true);
});

test("boolean preference strings fall back to defaults", () => {
  const normalized = normalizeUserPreferences({
    openFoldersOnClick: "false",
    alternateRows: "true",
    doubleClickDownload: "true",
  });

  assert.equal(normalized.openFoldersOnClick, true);
  assert.equal(normalized.alternateRows, false);
  assert.equal(normalized.doubleClickDownload, false);
});
