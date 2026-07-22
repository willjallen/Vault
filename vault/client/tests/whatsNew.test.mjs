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
const { releaseNoteVisual, releaseNotesSince } = await import(moduleUrl);

const releaseNotes = [
  { version: "2.2.0", entries: [{ kind: "feat", text: "Newest feature." }] },
  { version: "2.1.0", entries: [{ kind: "fix", text: "Middle improvement." }] },
  { version: "2.0.0", entries: [{ kind: "note", text: "Initial release." }] },
];

test("first-time viewers see only the current release", () => {
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", ""), [releaseNotes[0]]);
});

test("returning viewers see every release newer than their acknowledgement", () => {
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "2.0.0"), [
    releaseNotes[0],
    releaseNotes[1],
  ]);
});

test("the current release stays hidden once acknowledged", () => {
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "2.2.0"), []);
  assert.deepEqual(releaseNotesSince(releaseNotes, "9.0.0", ""), []);
});

test("unknown older acknowledgements fall back to the current release", () => {
  assert.deepEqual(releaseNotesSince(releaseNotes, "2.2.0", "1.0.0"), [releaseNotes[0]]);
});

test("release-note kinds map to quiet visual treatments", () => {
  assert.deepEqual(releaseNoteVisual("feat"), {
    icon: "wand-magic-sparkles",
    tone: "feature",
  });
  assert.deepEqual(releaseNoteVisual("unknown"), { icon: "star", tone: "technical" });
});
