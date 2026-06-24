import { normalizeFolderName } from "./utils.js";

export const LOCAL_PREFERENCES_STORAGE_KEY = "vault.localPreferences";
const LOCAL_PREFERENCE_DEFAULTS = {
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
    lastFolder: normalizeFolderName(stored.lastFolder || ""),
  };
}

export function readLocalPreference(key, fallback = "") {
  const preferences = readLocalPreferences();
  if (key === "lastFolder") {
    return preferences.lastFolder;
  }
  return fallback;
}

export function writeLocalPreference(key, value) {
  const current = readLocalPreferences();
  const next = { ...current };
  if (key === "lastFolder") {
    next.lastFolder = normalizeFolderName(value || "");
  }
  writeLocalPreferencesObject(next);
  return next;
}
