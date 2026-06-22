export const THEME_STORAGE_KEY = "vault.themePreference";
export const THEME_OPTIONS = ["system", "light", "dark"];
export const PALETTE_STORAGE_KEY = "vault.palettePreference";
export const PALETTE_OPTIONS = ["cozy", "winui"];
const { useCallback, useEffect, useState } = React;

export function normalizeThemePreference(value) {
  return THEME_OPTIONS.includes(value) ? value : "system";
}

export function normalizePalettePreference(value) {
  return PALETTE_OPTIONS.includes(value) ? value : "cozy";
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

export function useAppearancePreferences() {
  const [themePreference, setThemePreference] = useState(readStoredThemePreference);
  const [palettePreference, setPalettePreference] = useState(readStoredPalettePreference);

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

  const handleThemePreferenceChange = useCallback((preference) => {
    storeThemePreference(preference);
    setThemePreference(preference);
  }, []);

  const handlePalettePreferenceChange = useCallback((preference) => {
    storePalettePreference(preference);
    setPalettePreference(preference);
  }, []);

  return {
    handlePalettePreferenceChange,
    handleThemePreferenceChange,
    palettePreference,
    themePreference,
  };
}
