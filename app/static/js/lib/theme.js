export const THEME_STORAGE_KEY = "vault.themePreference";
export const THEME_OPTIONS = ["system", "light", "dark"];

export function normalizeThemePreference(value) {
  return THEME_OPTIONS.includes(value) ? value : "system";
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

export function applyThemePreference(preference) {
  const normalized = normalizeThemePreference(preference);
  const resolved = resolveThemePreference(normalized);
  document.documentElement.dataset.themePreference = normalized;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
  return resolved;
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
