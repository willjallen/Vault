export const THEME_OPTIONS = ["system", "light", "dark"];
export const PALETTE_OPTIONS = ["cozy", "winui"];

const { useCallback, useEffect, useState } = React;
const USER_PREFERENCE_DEFAULTS = {
  themePreference: "system",
  palettePreference: "cozy",
  openFoldersOnClick: true,
  alternateRows: false,
  doubleClickDownload: false,
};
export function normalizeThemePreference(value) {
  return THEME_OPTIONS.includes(value) ? value : "system";
}

export function normalizePalettePreference(value) {
  return PALETTE_OPTIONS.includes(value) ? value : "cozy";
}

function readHostThemeOverride() {
  const value = document.documentElement.dataset.themeOverride;
  return THEME_OPTIONS.includes(value) ? value : "";
}

function readHostPaletteOverride() {
  const value = document.documentElement.dataset.paletteOverride;
  return PALETTE_OPTIONS.includes(value) ? value : "";
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

export function normalizeUserPreferences(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    themePreference: normalizeThemePreference(source.themePreference),
    palettePreference: normalizePalettePreference(source.palettePreference),
    openFoldersOnClick: normalizeBooleanPreference(
      source.openFoldersOnClick,
      USER_PREFERENCE_DEFAULTS.openFoldersOnClick
    ),
    alternateRows: normalizeBooleanPreference(
      source.alternateRows,
      USER_PREFERENCE_DEFAULTS.alternateRows
    ),
    doubleClickDownload: normalizeBooleanPreference(
      source.doubleClickDownload,
      USER_PREFERENCE_DEFAULTS.doubleClickDownload
    ),
  };
}

function readDomUserPreferences() {
  return normalizeUserPreferences({
    alternateRows: document.documentElement.dataset.alternateRows,
    doubleClickDownload: document.documentElement.dataset.doubleClickDownload,
    openFoldersOnClick: document.documentElement.dataset.openFoldersOnClick,
    palettePreference: document.documentElement.dataset.palettePreference,
    themePreference: document.documentElement.dataset.themePreference,
  });
}

function resolveInitialUserPreferences(initialPreferences) {
  return normalizeUserPreferences(initialPreferences || readDomUserPreferences());
}

async function patchUserPreferences(apiFetch, patch) {
  if (!apiFetch) {
    return null;
  }
  const res = await apiFetch("/api/preferences", {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ preferences: patch }),
  });
  if (!res.ok) {
    const detail = await res.json().catch(() => ({}));
    throw new Error(detail.detail || "Could not save preferences");
  }
  const payload = await res.json();
  return normalizeUserPreferences(payload.preferences);
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

export function applyThemePreference(preference) {
  const normalized = normalizeThemePreference(preference);
  const resolved = resolveThemePreference(readHostThemeOverride() || normalized);
  document.documentElement.dataset.themePreference = normalized;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  return resolved;
}

export function applyPalettePreference(preference) {
  const normalized = normalizePalettePreference(preference);
  document.documentElement.dataset.palettePreference = normalized;
  document.documentElement.dataset.palette = readHostPaletteOverride() || normalized;
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

export function applyUserPreferences(preferences) {
  const normalized = normalizeUserPreferences(preferences);
  applyThemePreference(normalized.themePreference);
  applyPalettePreference(normalized.palettePreference);
  applyOpenFoldersOnClickPreference(normalized.openFoldersOnClick);
  applyAlternateRowsPreference(normalized.alternateRows);
  applyDoubleClickDownloadPreference(normalized.doubleClickDownload);
  return normalized;
}

export function useAppearancePreferences({ apiFetch, initialPreferences } = {}) {
  const [userPreferences, setUserPreferences] = useState(() =>
    resolveInitialUserPreferences(initialPreferences)
  );

  useEffect(() => {
    applyUserPreferences(userPreferences);
  }, [userPreferences]);

  useEffect(() => {
    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!media) {
      return undefined;
    }
    function handleSystemThemeChange() {
      if (userPreferences.themePreference === "system" || readHostThemeOverride() === "system") {
        applyThemePreference(userPreferences.themePreference);
      }
    }
    media.addEventListener("change", handleSystemThemeChange);
    return () => media.removeEventListener("change", handleSystemThemeChange);
  }, [userPreferences.themePreference]);

  const updatePreference = useCallback(
    (patch) => {
      const nextPreferences = normalizeUserPreferences({ ...userPreferences, ...patch });
      setUserPreferences(nextPreferences);
      patchUserPreferences(apiFetch, patch)
        .then((savedPreferences) => {
          if (savedPreferences) {
            setUserPreferences(savedPreferences);
          }
        })
        .catch(() => {});
    },
    [apiFetch, userPreferences]
  );

  const handleThemePreferenceChange = useCallback(
    (preference) => updatePreference({ themePreference: preference }),
    [updatePreference]
  );

  const handlePalettePreferenceChange = useCallback(
    (preference) => updatePreference({ palettePreference: preference }),
    [updatePreference]
  );

  const handleOpenFoldersOnClickChange = useCallback(
    (preference) => updatePreference({ openFoldersOnClick: preference }),
    [updatePreference]
  );

  const handleAlternateRowsChange = useCallback(
    (preference) => updatePreference({ alternateRows: preference }),
    [updatePreference]
  );

  const handleDoubleClickDownloadChange = useCallback(
    (preference) => updatePreference({ doubleClickDownload: preference }),
    [updatePreference]
  );

  return {
    alternateRows: userPreferences.alternateRows,
    doubleClickDownload: userPreferences.doubleClickDownload,
    handleAlternateRowsChange,
    handleDoubleClickDownloadChange,
    handleOpenFoldersOnClickChange,
    handlePalettePreferenceChange,
    handleThemePreferenceChange,
    openFoldersOnClick: userPreferences.openFoldersOnClick,
    palettePreference: userPreferences.palettePreference,
    themePreference: userPreferences.themePreference,
    userPreferences,
  };
}
