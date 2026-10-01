import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const refs = [];
let refIndex = 0;
let preferences;
globalThis.React = {
  useCallback: (callback) => callback,
  useEffect: () => {},
  useRef(initial) {
    refs[refIndex] ||= { current: initial };
    return refs[refIndex++];
  },
  useState(initial) {
    preferences ??= typeof initial === "function" ? initial() : initial;
    return [
      preferences,
      (next) => {
        preferences = typeof next === "function" ? next(preferences) : next;
      },
    ];
  },
};
const bundle = await build({
  bundle: true,
  entryPoints: [new URL("../src/lib/theme.js", import.meta.url).pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const { useAppearancePreferences } = await import(
  `data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString("base64")}`
);

test("rapid folder reorders survive earlier save responses and preference refreshes", async () => {
  const requests = [];
  const saved = { contentsColumnOrderByFolder: {}, contentsViewByFolder: {} };
  const apiFetch = (url, options) => {
    if (!options) {
      return Promise.resolve({
        ok: true,
        json: async () => ({ preferences: structuredClone(saved) }),
      });
    }
    return new Promise((resolve) =>
      requests.push({ patch: JSON.parse(options.body).preferences, resolve })
    );
  };
  function render() {
    refIndex = 0;
    return useAppearancePreferences({ apiFetch, initialPreferences: saved });
  }
  async function completeNext() {
    while (!requests.length) {
      await new Promise((resolve) => setImmediate(resolve));
    }
    const request = requests.shift();
    for (const [field, entries] of Object.entries(request.patch)) {
      saved[field] = { ...saved[field], ...entries };
    }
    request.resolve({ ok: true, json: async () => ({ preferences: structuredClone(saved) }) });
  }
  const first = ["size", "name", "modified", "user"];
  const nested = ["user", "name", "modified", "size"];
  const latest = ["modified", "size", "name", "user"];
  const hook = render();
  const saves = [
    hook.contentsColumns.onOrderChange("", first),
    hook.contentsColumns.onOrderChange("Projects/Alpha", nested),
    hook.contentsColumns.onOrderChange("", latest),
    hook.handleContentsViewChange("Projects/Alpha", { mode: "icons", iconSize: 112 }),
  ];
  await hook.refreshUserPreferences();
  assert.deepEqual(render().contentsColumns.orderByFolder, {
    "": latest,
    "Projects/Alpha": nested,
  });
  for (const save of saves) {
    await completeNext();
    await save;
    assert.deepEqual(render().contentsColumns.orderByFolder, {
      "": latest,
      "Projects/Alpha": nested,
    });
  }
  assert.deepEqual(saved.contentsColumnOrderByFolder, { "": latest, "Projects/Alpha": nested });
  assert.equal(render().contentsViewByFolder["Projects/Alpha"].mode, "icons");
});
