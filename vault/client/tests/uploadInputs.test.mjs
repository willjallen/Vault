import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  Fragment: "fragment",
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
};

const sourceUrl = new URL("../src/components/browser/UploadInputs.js", import.meta.url);
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
const { UploadInputs } = await import(moduleUrl);

function uploadInputs(overrides = {}) {
  return UploadInputs({
    fileInputRef: { current: null },
    folderInputRef: { current: null },
    onFiles: () => {},
    onFolder: () => {},
    ...overrides,
  }).children.flat();
}

test("native upload inputs keep file and folder selection modes separate", () => {
  const inputs = uploadInputs();
  const fileInput = inputs.find((input) => !("webkitdirectory" in input.props));
  const folderInput = inputs.find((input) => "webkitdirectory" in input.props);

  assert.equal(inputs.length, 2);
  assert.equal(fileInput.props.multiple, true);
  assert.equal(folderInput.props.multiple, true);
  assert.equal(folderInput.props.directory, "");
  assert.equal(folderInput.props.webkitdirectory, "");
});

test("native upload inputs route their selections to the matching handler", () => {
  const selections = [];
  const inputs = uploadInputs({
    onFiles: (files) => selections.push(["files", files]),
    onFolder: (files) => selections.push(["folder", files]),
  });
  const fileInput = inputs.find((input) => !("webkitdirectory" in input.props));
  const folderInput = inputs.find((input) => "webkitdirectory" in input.props);
  const files = ["one.txt", "two.txt"];
  const folderFiles = ["Bundle/one.txt"];

  fileInput.props.onChange({ currentTarget: { files } });
  folderInput.props.onChange({ currentTarget: { files: folderFiles } });

  assert.deepEqual(selections, [
    ["files", files],
    ["folder", folderFiles],
  ]);
});
