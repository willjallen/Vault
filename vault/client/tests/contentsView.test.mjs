import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

const sourceUrl = new URL("../src/lib/contentsView.js", import.meta.url);
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
const {
  CONTENTS_VIEW_MODES,
  CONTENTS_VIEW_VERSION,
  MAX_CONTENTS_ICON_SIZE,
  MIN_CONTENTS_ICON_SIZE,
  VIEW_WHEEL_HYSTERESIS,
  contentsViewForFolder,
  contentsViewFromSlider,
  contentsViewSliderValue,
  normalizeContentsViewByFolder,
  normalizeContentsView,
  normalizeWheelDelta,
  setContentsViewForFolder,
  stepContentsViewWithWheel,
} = await import(moduleUrl);

test("contents view preferences normalize invalid and out-of-range values", () => {
  assert.deepEqual(normalizeContentsView({ iconSize: 999, mode: "unknown", version: 99 }), {
    iconSize: MAX_CONTENTS_ICON_SIZE,
    mode: CONTENTS_VIEW_MODES.DETAILS,
    version: CONTENTS_VIEW_VERSION,
  });
  assert.equal(
    normalizeContentsView({ iconSize: -1, mode: CONTENTS_VIEW_MODES.ICONS }).iconSize,
    MIN_CONTENTS_ICON_SIZE
  );
});

test("slider round trips continuous icon sizes and preserves discrete stops", () => {
  const iconView = normalizeContentsView({ iconSize: 124, mode: CONTENTS_VIEW_MODES.ICONS });
  const roundTrip = contentsViewFromSlider(contentsViewSliderValue(iconView), iconView);
  assert.equal(roundTrip.mode, CONTENTS_VIEW_MODES.ICONS);
  assert.ok(Math.abs(roundTrip.iconSize - iconView.iconSize) <= 1);
  assert.equal(contentsViewFromSlider(0, iconView).mode, CONTENTS_VIEW_MODES.DETAILS);
  assert.equal(contentsViewFromSlider(16, iconView).mode, CONTENTS_VIEW_MODES.LIST);
});

test("wheel movement uses hysteresis at list and minimum-icon boundaries", () => {
  const listView = normalizeContentsView({ mode: CONTENTS_VIEW_MODES.LIST });
  const partial = stepContentsViewWithWheel(listView, -(VIEW_WHEEL_HYSTERESIS - 1));
  assert.equal(partial.view.mode, CONTENTS_VIEW_MODES.LIST);
  assert.equal(partial.boundaryDelta, -(VIEW_WHEEL_HYSTERESIS - 1));

  const icons = stepContentsViewWithWheel(partial.view, -1, partial.boundaryDelta);
  assert.equal(icons.view.mode, CONTENTS_VIEW_MODES.ICONS);
  assert.equal(icons.view.iconSize, MIN_CONTENTS_ICON_SIZE);

  const stillIcons = stepContentsViewWithWheel(icons.view, 20);
  assert.equal(stillIcons.view.mode, CONTENTS_VIEW_MODES.ICONS);
  const listAgain = stepContentsViewWithWheel(stillIcons.view, 100, stillIcons.boundaryDelta);
  assert.equal(listAgain.view.mode, CONTENTS_VIEW_MODES.LIST);
});

test("wheel delta normalization handles line and page modes", () => {
  assert.equal(normalizeWheelDelta(2, 0), 2);
  assert.equal(normalizeWheelDelta(2, 1), 32);
  assert.equal(normalizeWheelDelta(2, 2, 500), 1000);
});

test("contents views are normalized and isolated by folder path", () => {
  const views = setContentsViewForFolder({}, "Projects/Alpha", {
    iconSize: 91,
    mode: CONTENTS_VIEW_MODES.ICONS,
  });

  assert.deepEqual(contentsViewForFolder(views, "Projects/Alpha"), {
    iconSize: 91,
    mode: CONTENTS_VIEW_MODES.ICONS,
    version: CONTENTS_VIEW_VERSION,
  });
  assert.deepEqual(contentsViewForFolder(views, "Projects/Beta"), {
    iconSize: 80,
    mode: CONTENTS_VIEW_MODES.DETAILS,
    version: CONTENTS_VIEW_VERSION,
  });
});

test("contents view maps normalize malformed entries and canonical folder keys", () => {
  assert.deepEqual(
    normalizeContentsViewByFolder({
      "/Projects/Alpha/": { iconSize: 999, mode: CONTENTS_VIEW_MODES.ICONS },
      ignored: null,
    }),
    {
      "Projects/Alpha": {
        iconSize: MAX_CONTENTS_ICON_SIZE,
        mode: CONTENTS_VIEW_MODES.ICONS,
        version: CONTENTS_VIEW_VERSION,
      },
    }
  );
});
