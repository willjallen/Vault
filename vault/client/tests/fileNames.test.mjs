import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/fileNames.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { fileRenamePrefixSelectionEnd, selectFileRenamePrefix } = await import(moduleUrl);

test("file rename selection excludes the final extension", () => {
  assert.equal(fileRenamePrefixSelectionEnd("report.pdf"), "report".length);
  assert.equal(fileRenamePrefixSelectionEnd("archive.tar.gz"), "archive.tar".length);
  assert.equal(fileRenamePrefixSelectionEnd(".env.local"), ".env".length);
});

test("file rename selection includes names without a real extension", () => {
  assert.equal(fileRenamePrefixSelectionEnd("README"), "README".length);
  assert.equal(fileRenamePrefixSelectionEnd(".env"), ".env".length);
  assert.equal(fileRenamePrefixSelectionEnd("notes."), "notes.".length);
});

test("file rename selection is applied to the input range", () => {
  const calls = [];
  const input = {
    value: "clip.mov",
    setSelectionRange: (start, end) => calls.push([start, end]),
  };

  selectFileRenamePrefix(input);

  assert.deepEqual(calls, [[0, "clip".length]]);
});
