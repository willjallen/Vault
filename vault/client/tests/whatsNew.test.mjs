import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/whatsNew.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(
  bundled.outputFiles.at(0).text
).toString("base64")}`;
const { releaseNoteVisual, releaseNotesSince, releaseVersionsBeforeCurrent } = await import(
  moduleUrl
);

const releaseNotes = [
  { version: "2.2.0", entries: [{ kind: "feat", text: "Newest feature." }] },
  { version: "2.1.0", entries: [{ kind: "fix", text: "Middle improvement." }] },
  { version: "2.0.0", entries: [{ kind: "note", text: "Initial release." }] },
];

test("first-time viewers see only the current release", () => {
  /*
   * Models an account with no acknowledgement and verifies the initial experience stays focused
   * on the version it is currently running instead of replaying the entire product history.
   */
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", ""), [releaseNotes[0]]);
});

test("returning viewers see every release newer than their acknowledgement", () => {
  /*
   * Models a user who last opened What's New on v2.0.0 and verifies both skipped releases are
   * returned in newest-first changelog order.
   */
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "2.0.0"), [
    releaseNotes[0],
    releaseNotes[1],
  ]);
});

test("the current release stays hidden once acknowledged", () => {
  /*
   * Covers the normal dismissed state plus a build whose current version has no bundled release
   * notes, ensuring neither case opens an empty modal.
   */
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "2.2.0"), []);
  assert.deepEqual(releaseNotesSince(releaseNotes, "9.0.0", ""), []);
});

test("unknown older acknowledgements fall back to the current release", () => {
  /*
   * Supplies an acknowledgement absent from the bundled changelog and verifies the fallback does
   * not guess at an unbounded historical range.
   */
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "1.0.0"), [releaseNotes[0]]);
});

test("debug version choices include every bundled release before the current version", () => {
  /*
   * Builds the selectable debug starting points for v2.2.0 and verifies the current release is
   * excluded because acknowledging it would suppress the modal entirely.
   */
  assert.deepEqual(releaseVersionsBeforeCurrent(releaseNotes, "2.2.0"), ["2.1.0", "2.0.0"]);
  assert.deepEqual(releaseVersionsBeforeCurrent(releaseNotes, "9.0.0"), []);
});

test("release-note kinds map to quiet visual treatments", () => {
  /*
   * Checks the deliberate feature treatment and the safe fallback used for unknown changelog
   * labels so new labels cannot break rendering.
   */
  assert.deepEqual(releaseNoteVisual("feat"), {
    icon: "wand-magic-sparkles",
    tone: "feature",
  });
  assert.deepEqual(releaseNoteVisual("unknown"), { icon: "star", tone: "technical" });
});
