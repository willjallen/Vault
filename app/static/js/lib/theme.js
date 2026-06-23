export const THEME_STORAGE_KEY = "vault.themePreference";
export const THEME_OPTIONS = ["system", "light", "dark"];
export const PALETTE_STORAGE_KEY = "vault.palettePreference";
export const PALETTE_OPTIONS = ["cozy", "winui"];
export const OPEN_FOLDERS_ON_CLICK_STORAGE_KEY = "vault.openFoldersOnClick";
export const ALTERNATE_ROWS_STORAGE_KEY = "vault.alternateRows";
export const DOUBLE_CLICK_DOWNLOAD_STORAGE_KEY = "vault.doubleClickDownload";
const { useCallback, useEffect, useState } = React;

export function normalizeThemePreference(value) {
  return THEME_OPTIONS.includes(value) ? value : "system";
}

export function normalizePalettePreference(value) {
  return PALETTE_OPTIONS.includes(value) ? value : "cozy";
}

function normalizeBooleanPreference(value, fallback) {
  if (value === true || value === "true") {
    return true;
  }
  if (value === false || value === "false") {
    return false;
  }
  return fallback;
}

export function systemPrefersDark() {
  return Boolean(window.matchMedia?.("(prefers-color-scheme: dark)").matches);
}

export function resolveThemePreference(preference) {
  const normalized = normalizeThemePreference(preference);
  if (normalized === "system") {
    return systemPrefersDark() ? "dark" : "light";
  }
  return normalized;
}

export function readStoredThemePreference() {
  try {
    return normalizeThemePreference(
      window.localStorage.getItem(THEME_STORAGE_KEY) ||
        document.documentElement.dataset.themePreference
    );
  } catch (_err) {
    return normalizeThemePreference(document.documentElement.dataset.themePreference);
  }
}

export function readStoredPalettePreference() {
  try {
    return normalizePalettePreference(
      window.localStorage.getItem(PALETTE_STORAGE_KEY) ||
        document.documentElement.dataset.palettePreference
    );
  } catch (_err) {
    return normalizePalettePreference(document.documentElement.dataset.palettePreference);
  }
}

function readStoredBooleanPreference(key, datasetValue, fallback) {
  try {
    return normalizeBooleanPreference(window.localStorage.getItem(key) || datasetValue, fallback);
  } catch (_err) {
    return normalizeBooleanPreference(datasetValue, fallback);
  }
}

export function readStoredOpenFoldersOnClickPreference() {
  return readStoredBooleanPreference(
    OPEN_FOLDERS_ON_CLICK_STORAGE_KEY,
    document.documentElement.dataset.openFoldersOnClick,
    true
  );
}

export function readStoredAlternateRowsPreference() {
  return readStoredBooleanPreference(
    ALTERNATE_ROWS_STORAGE_KEY,
    document.documentElement.dataset.alternateRows,
    false
  );
}

export function readStoredDoubleClickDownloadPreference() {
  return readStoredBooleanPreference(
    DOUBLE_CLICK_DOWNLOAD_STORAGE_KEY,
    document.documentElement.dataset.doubleClickDownload,
    false
  );
}

export function applyThemePreference(preference) {
  const normalized = normalizeThemePreference(preference);
  const resolved = resolveThemePreference(normalized);
  document.documentElement.dataset.themePreference = normalized;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  return resolved;
}

export function applyPalettePreference(preference) {
  const normalized = normalizePalettePreference(preference);
  document.documentElement.dataset.palettePreference = normalized;
  document.documentElement.dataset.palette = normalized;
  return normalized;
}

export function applyOpenFoldersOnClickPreference(preference) {
  const normalized = normalizeBooleanPreference(preference, true);
  document.documentElement.dataset.openFoldersOnClick = String(normalized);
  return normalized;
}

export function applyAlternateRowsPreference(preference) {
  const normalized = normalizeBooleanPreference(preference, false);
  document.documentElement.dataset.alternateRows = String(normalized);
  return normalized;
}

export function applyDoubleClickDownloadPreference(preference) {
  const normalized = normalizeBooleanPreference(preference, false);
  document.documentElement.dataset.doubleClickDownload = String(normalized);
  return normalized;
}

export function storeThemePreference(preference) {
  const normalized = normalizeThemePreference(preference);
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, normalized);
  } catch (_err) {
    // localStorage can be disabled; data attributes still keep the current tab correct.
  }
  return applyThemePreference(normalized);
}

export function storePalettePreference(preference) {
  const normalized = normalizePalettePreference(preference);
  try {
    window.localStorage.setItem(PALETTE_STORAGE_KEY, normalized);
  } catch (_err) {
    // localStorage can be disabled; data attributes still keep the current tab correct.
  }
  return applyPalettePreference(normalized);
}

function storeBooleanPreference(key, preference, fallback, applyPreference) {
  const normalized = normalizeBooleanPreference(preference, fallback);
  try {
    window.localStorage.setItem(key, String(normalized));
  } catch (_err) {
    // localStorage can be disabled; data attributes still keep the current tab correct.
  }
  return applyPreference(normalized);
}

export function storeOpenFoldersOnClickPreference(preference) {
  return storeBooleanPreference(
    OPEN_FOLDERS_ON_CLICK_STORAGE_KEY,
    preference,
    true,
    applyOpenFoldersOnClickPreference
  );
}

export function storeAlternateRowsPreference(preference) {
  return storeBooleanPreference(
    ALTERNATE_ROWS_STORAGE_KEY,
    preference,
    false,
    applyAlternateRowsPreference
  );
}

export function storeDoubleClickDownloadPreference(preference) {
  return storeBooleanPreference(
    DOUBLE_CLICK_DOWNLOAD_STORAGE_KEY,
    preference,
    false,
    applyDoubleClickDownloadPreference
  );
}

export function useAppearancePreferences() {
  const [themePreference, setThemePreference] = useState(readStoredThemePreference);
  const [palettePreference, setPalettePreference] = useState(readStoredPalettePreference);
  const [openFoldersOnClick, setOpenFoldersOnClick] = useState(
    readStoredOpenFoldersOnClickPreference
  );
  const [alternateRows, setAlternateRows] = useState(readStoredAlternateRowsPreference);
  const [doubleClickDownload, setDoubleClickDownload] = useState(
    readStoredDoubleClickDownloadPreference
  );

  useEffect(() => {
    applyThemePreference(themePreference);
    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!media) {
      return undefined;
    }
    function handleSystemThemeChange() {
      if (themePreference === "system") {
        applyThemePreference("system");
      }
    }
    media.addEventListener("change", handleSystemThemeChange);
    return () => media.removeEventListener("change", handleSystemThemeChange);
  }, [themePreference]);

  useEffect(() => {
    applyPalettePreference(palettePreference);
  }, [palettePreference]);

  useEffect(() => {
    applyOpenFoldersOnClickPreference(openFoldersOnClick);
  }, [openFoldersOnClick]);

  useEffect(() => {
    applyAlternateRowsPreference(alternateRows);
  }, [alternateRows]);

  useEffect(() => {
    applyDoubleClickDownloadPreference(doubleClickDownload);
  }, [doubleClickDownload]);

  const handleThemePreferenceChange = useCallback((preference) => {
    storeThemePreference(preference);
    setThemePreference(preference);
  }, []);

  const handlePalettePreferenceChange = useCallback((preference) => {
    storePalettePreference(preference);
    setPalettePreference(preference);
  }, []);

  const handleOpenFoldersOnClickChange = useCallback((preference) => {
    storeOpenFoldersOnClickPreference(preference);
    setOpenFoldersOnClick(preference);
  }, []);

  const handleAlternateRowsChange = useCallback((preference) => {
    storeAlternateRowsPreference(preference);
    setAlternateRows(preference);
  }, []);

  const handleDoubleClickDownloadChange = useCallback((preference) => {
    storeDoubleClickDownloadPreference(preference);
    setDoubleClickDownload(preference);
  }, []);

  return {
    alternateRows,
    doubleClickDownload,
    handleAlternateRowsChange,
    handleDoubleClickDownloadChange,
    handleOpenFoldersOnClickChange,
    handlePalettePreferenceChange,
    handleThemePreferenceChange,
    openFoldersOnClick,
    palettePreference,
    themePreference,
  };
}
