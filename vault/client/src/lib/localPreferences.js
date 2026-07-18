import { normalizeFolderName } from "./utils.js";
import { DEFAULT_CONTENTS_VIEW, normalizeContentsView } from "./contentsView.js";

export const LOCAL_PREFERENCES_STORAGE_KEY = "vault.localPreferences";
const LOCAL_PREFERENCE_DEFAULTS = {
  contentsColumnWidths: null,
  contentsView: DEFAULT_CONTENTS_VIEW,
  lastFolder: "",
};

function readLocalPreferencesObject() {
  try {
    const raw = window.localStorage.getItem(LOCAL_PREFERENCES_STORAGE_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch (_err) {
    return {};
  }
}

function writeLocalPreferencesObject(preferences) {
  try {
    window.localStorage.setItem(LOCAL_PREFERENCES_STORAGE_KEY, JSON.stringify(preferences));
  } catch (_err) {
    // Local preferences only improve device-specific continuity.
  }
}

export function readLocalPreferences() {
  const stored = readLocalPreferencesObject();
  return {
    ...LOCAL_PREFERENCE_DEFAULTS,
    ...stored,
    contentsView: normalizeContentsView(stored.contentsView),
    lastFolder: normalizeFolderName(stored.lastFolder || ""),
  };
}

export function readLocalPreference(key, fallback = "") {
  const preferences = readLocalPreferences();
  if (key === "contentsColumnWidths") {
    return preferences.contentsColumnWidths || fallback;
  }
  if (key === "contentsView") {
    return preferences.contentsView || normalizeContentsView(fallback);
  }
  if (key === "lastFolder") {
    return preferences.lastFolder;
  }
  return fallback;
}

export function writeLocalPreference(key, value) {
  const current = readLocalPreferencesObject();
  const next = { ...current };
  if (key === "lastFolder") {
    next.lastFolder = normalizeFolderName(value || "");
  } else if (key === "contentsColumnWidths") {
    next.contentsColumnWidths = value;
  } else if (key === "contentsView") {
    next.contentsView = normalizeContentsView(value);
  } else {
    return readLocalPreferences();
  }
  writeLocalPreferencesObject(next);
  return readLocalPreferences();
}
