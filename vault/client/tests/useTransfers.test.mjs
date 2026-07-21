import { Buffer, File } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const hookState = [];
const hookRefs = [];
let nextRef = 0;
let nextState = 0;

globalThis.window = {};
globalThis.React = {
  useCallback: (callback) => callback,
  useEffect: () => {},
  useRef(initialValue) {
    const index = nextRef;
    nextRef += 1;
    hookRefs[index] ||= { current: initialValue };
    return hookRefs[index];
  },
  useState(initialValue) {
    const index = nextState;
    nextState += 1;
    if (!(index in hookState)) {
      hookState[index] = typeof initialValue === "function" ? initialValue() : initialValue;
    }
    return [
      hookState[index],
      (nextValue) => {
        hookState[index] =
          typeof nextValue === "function" ? nextValue(hookState[index]) : nextValue;
      },
    ];
  },
};

const sourceUrl = new URL("../src/lib/useTransfers.js", import.meta.url);
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
const { useTransfers } = await import(moduleUrl);

test("independent actions retain separate popups and cancellation targets one operation", () => {
  const transfers = useTransfers();
  const firstFiles = [
    new File(["one"], "one.txt"),
    new File(["two"], "two.txt"),
    new File(["three"], "three.txt"),
  ];
  const first = transfers.beginUploadOperation({ files: firstFiles });
  const second = transfers.beginUploadOperation({ files: [new File(["four"], "four.txt")] });

  assert.equal(hookState[0].length, 2);
  assert.deepEqual(
    hookState[0].map((transfer) => ({
      grouped: transfer.grouped,
      id: transfer.id,
      name: transfer.name,
    })),
    [
      { grouped: true, id: 1, name: "3 files" },
      { grouped: false, id: 2, name: "four.txt" },
    ]
  );

  transfers.cancelTransfer(1);

  assert.equal(first.signal.aborted, true);
  assert.equal(second.signal.aborted, false);
  assert.equal(hookState[0].find((transfer) => transfer.id === 1).status, "cancelling");
  assert.equal(hookState[0].find((transfer) => transfer.id === 2).status, "active");
});
