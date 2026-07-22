import { normalizeFolderName } from "./utils.js";

export const CONTENTS_VIEW_VERSION = 1;
export const CONTENTS_VIEW_MODES = Object.freeze({
  DETAILS: "details",
  ICONS: "icons",
  LIST: "list",
});

export const MIN_CONTENTS_ICON_SIZE = 56;
export const MAX_CONTENTS_ICON_SIZE = 176;
export const DEFAULT_CONTENTS_ICON_SIZE = 80;
export const CONTENTS_ICON_PRESETS = Object.freeze([
  { label: "XL Icons", name: "XL", size: 160 },
  { label: "L Icons", name: "L", size: 112 },
  { label: "M Icons", name: "M", size: DEFAULT_CONTENTS_ICON_SIZE },
]);

export const DEFAULT_CONTENTS_VIEW = Object.freeze({
  iconSize: DEFAULT_CONTENTS_ICON_SIZE,
  mode: CONTENTS_VIEW_MODES.DETAILS,
  version: CONTENTS_VIEW_VERSION,
});

export const CONTENTS_VIEW_SLIDER = Object.freeze({
  details: 0,
  iconStart: 28,
  list: 16,
  maximum: 100,
});

export const VIEW_WHEEL_HYSTERESIS = 32;
const WHEEL_ICON_SCALE = 0.35;

export function clampContentsIconSize(value) {
  const parsed = Number(value);
  if (!Number.isFinite(parsed)) {
    return DEFAULT_CONTENTS_ICON_SIZE;
  }
  return Math.round(Math.min(MAX_CONTENTS_ICON_SIZE, Math.max(MIN_CONTENTS_ICON_SIZE, parsed)));
}

export function normalizeContentsView(value) {
  const candidate = value && typeof value === "object" ? value : {};
  const mode = Object.values(CONTENTS_VIEW_MODES).includes(candidate.mode)
    ? candidate.mode
    : DEFAULT_CONTENTS_VIEW.mode;
  return {
    iconSize: clampContentsIconSize(candidate.iconSize),
    mode,
    version: CONTENTS_VIEW_VERSION,
  };
}

export function normalizeContentsViewByFolder(value) {
  const source = value && typeof value === "object" && !Array.isArray(value) ? value : {};
  return Object.fromEntries(
    Object.entries(source)
      .filter(([, view]) => view && typeof view === "object" && !Array.isArray(view))
      .map(([folder, view]) => [normalizeFolderName(folder), normalizeContentsView(view)])
  );
}

export function contentsViewForFolder(value, folder) {
  const views = normalizeContentsViewByFolder(value);
  const path = normalizeFolderName(folder);
  return Object.prototype.hasOwnProperty.call(views, path)
    ? // eslint-disable-next-line security/detect-object-injection
      views[path]
    : DEFAULT_CONTENTS_VIEW;
}

export function setContentsViewForFolder(value, folder, view) {
  return {
    ...normalizeContentsViewByFolder(value),
    [normalizeFolderName(folder)]: normalizeContentsView(view),
  };
}

export function contentsViewSliderValue(value) {
  const view = normalizeContentsView(value);
  if (view.mode === CONTENTS_VIEW_MODES.DETAILS) {
    return CONTENTS_VIEW_SLIDER.details;
  }
  if (view.mode === CONTENTS_VIEW_MODES.LIST) {
    return CONTENTS_VIEW_SLIDER.list;
  }
  const iconProgress =
    (view.iconSize - MIN_CONTENTS_ICON_SIZE) / (MAX_CONTENTS_ICON_SIZE - MIN_CONTENTS_ICON_SIZE);
  return Math.round(
    CONTENTS_VIEW_SLIDER.iconStart +
      iconProgress * (CONTENTS_VIEW_SLIDER.maximum - CONTENTS_VIEW_SLIDER.iconStart)
  );
}

export function contentsViewFromSlider(value, previous = DEFAULT_CONTENTS_VIEW) {
  const sliderValue = Math.min(
    CONTENTS_VIEW_SLIDER.maximum,
    Math.max(CONTENTS_VIEW_SLIDER.details, Number(value) || 0)
  );
  if (sliderValue < (CONTENTS_VIEW_SLIDER.details + CONTENTS_VIEW_SLIDER.list) / 2) {
    return normalizeContentsView({ ...previous, mode: CONTENTS_VIEW_MODES.DETAILS });
  }
  if (sliderValue < (CONTENTS_VIEW_SLIDER.list + CONTENTS_VIEW_SLIDER.iconStart) / 2) {
    return normalizeContentsView({ ...previous, mode: CONTENTS_VIEW_MODES.LIST });
  }
  const iconProgress =
    (sliderValue - CONTENTS_VIEW_SLIDER.iconStart) /
    (CONTENTS_VIEW_SLIDER.maximum - CONTENTS_VIEW_SLIDER.iconStart);
  return normalizeContentsView({
    ...previous,
    iconSize:
      MIN_CONTENTS_ICON_SIZE +
      Math.max(0, iconProgress) * (MAX_CONTENTS_ICON_SIZE - MIN_CONTENTS_ICON_SIZE),
    mode: CONTENTS_VIEW_MODES.ICONS,
  });
}

export function normalizeWheelDelta(deltaY, deltaMode = 0, pageHeight = 800) {
  if (!Number.isFinite(deltaY)) {
    return 0;
  }
  if (deltaMode === 1) {
    return deltaY * 16;
  }
  if (deltaMode === 2) {
    return deltaY * Math.max(1, pageHeight);
  }
  return deltaY;
}

function boundaryResult(view, boundaryDelta = 0) {
  return { boundaryDelta, view: normalizeContentsView(view) };
}

// Positive wheel movement travels toward smaller views; negative movement travels toward larger
// views. Discrete boundaries deliberately accumulate motion so trackpads do not oscillate while
// resting at the list/icon transition.
export function stepContentsViewWithWheel(value, pixelDelta, boundaryDelta = 0) {
  const view = normalizeContentsView(value);
  const delta = Number.isFinite(pixelDelta) ? pixelDelta : 0;
  if (!delta) {
    return boundaryResult(view);
  }

  if (view.mode === CONTENTS_VIEW_MODES.DETAILS) {
    if (delta > 0) {
      return boundaryResult(view);
    }
    const detailsBoundary = boundaryDelta + -delta;
    return detailsBoundary >= VIEW_WHEEL_HYSTERESIS
      ? boundaryResult({ ...view, mode: CONTENTS_VIEW_MODES.LIST })
      : boundaryResult(view, detailsBoundary);
  }

  if (view.mode === CONTENTS_VIEW_MODES.LIST) {
    const direction = Math.sign(delta);
    const listBoundary = Math.sign(boundaryDelta) === direction ? boundaryDelta + delta : delta;
    if (Math.abs(listBoundary) < VIEW_WHEEL_HYSTERESIS) {
      return boundaryResult(view, listBoundary);
    }
    return direction > 0
      ? boundaryResult({ ...view, mode: CONTENTS_VIEW_MODES.DETAILS })
      : boundaryResult({
          ...view,
          iconSize: MIN_CONTENTS_ICON_SIZE,
          mode: CONTENTS_VIEW_MODES.ICONS,
        });
  }

  const nextSize = view.iconSize - delta * WHEEL_ICON_SCALE;
  if (nextSize > MIN_CONTENTS_ICON_SIZE) {
    return boundaryResult({ ...view, iconSize: nextSize });
  }
  if (delta < 0) {
    return boundaryResult({ ...view, iconSize: nextSize });
  }
  const iconBoundary = boundaryDelta + Math.max(0, MIN_CONTENTS_ICON_SIZE - nextSize);
  return iconBoundary >= VIEW_WHEEL_HYSTERESIS
    ? boundaryResult({ ...view, mode: CONTENTS_VIEW_MODES.LIST })
    : boundaryResult({ ...view, iconSize: MIN_CONTENTS_ICON_SIZE }, iconBoundary);
}

export function contentsViewLabel(value) {
  const view = normalizeContentsView(value);
  if (view.mode === CONTENTS_VIEW_MODES.DETAILS) {
    return "Details";
  }
  if (view.mode === CONTENTS_VIEW_MODES.LIST) {
    return "List";
  }
  const nearestPreset = CONTENTS_ICON_PRESETS.reduce((nearest, preset) =>
    Math.abs(preset.size - view.iconSize) < Math.abs(nearest.size - view.iconSize)
      ? preset
      : nearest
  );
  return Math.abs(nearestPreset.size - view.iconSize) <= 5
    ? `${nearestPreset.name} Icons`
    : "Icons";
}

export function contentsViewAriaValue(value) {
  const view = normalizeContentsView(value);
  return view.mode === CONTENTS_VIEW_MODES.ICONS
    ? `Icons, ${view.iconSize} pixels`
    : `${contentsViewLabel(view)} view`;
}

export function sameContentsView(left, right) {
  return left.mode === right.mode && left.iconSize === right.iconSize;
}

export function contentsVisualSize(value) {
  const view = normalizeContentsView(value);
  if (view.mode === CONTENTS_VIEW_MODES.ICONS) {
    return view.iconSize;
  }
  return view.mode === CONTENTS_VIEW_MODES.LIST ? 28 : 16;
}
