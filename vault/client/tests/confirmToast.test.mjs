import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: () => ({}),
  useCallback: (callback) => callback,
  useEffect: () => {},
  useRef: (value) => ({ current: value }),
  useState: (value) => [value, () => {}],
};

const sourceUrl = new URL("../src/components/ConfirmToast.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { confirmationResolution } = await import(moduleUrl);

test("ordinary confirmations retain boolean results", () => {
  assert.equal(confirmationResolution({}, true, true), true);
  assert.equal(confirmationResolution({}, false, true), false);
});

test("remember confirmations return an explicit checked result", () => {
  const request = { rememberLabel: "Do not show again" };

  assert.deepEqual(confirmationResolution(request, true, true), {
    confirmed: true,
    remember: true,
  });
  assert.deepEqual(confirmationResolution(request, true, false), {
    confirmed: true,
    remember: false,
  });
  assert.deepEqual(confirmationResolution(request, false, true), {
    confirmed: false,
    remember: false,
  });
});
