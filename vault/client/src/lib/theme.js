import { normalizeContentsViewByFolder, setContentsViewForFolder } from "./contentsView.js";
import {
  normalizeContentsColumnOrderByFolder,
  setContentsColumnOrderForFolder,
} from "./contentsColumns.js";

export const THEME_OPTIONS = ["system", "light", "dark"];
export const PALETTE_OPTIONS = ["cozy", "winui"];

const { useCallback, useEffect, useRef, useState } = React;
const USER_PREFERENCE_DEFAULTS = {
  themePreference: "system",
  palettePreference: "cozy",
  openFoldersOnClick: true,
  alternateRows: false,
  doubleClickDownload: false,
  downloadLocationGuidanceDismissed: false,
  whatsNewAcknowledgedVersion: "",
  contentsViewByFolder: {},
  contentsColumnOrderByFolder: {},
  favoriteItems: [],
  sidebarSectionSizes: {
    folders: 180,
    favorites: 95,
    editing: 90,
    archive: 115,
  },
  sidebarSectionCollapsed: {
    folders: false,
    favorites: false,
    editing: false,
    archive: true,
  },
};
export const SIDEBAR_SECTION_KEYS = ["folders", "favorites", "editing", "archive"];
export const MIN_SIDEBAR_SECTION_SIZE = 32;
export const MAX_SIDEBAR_SECTION_SIZE = 4000;
export const SIDEBAR_COLLAPSED_SECTION_SIZE = 32;
export const SIDEBAR_EXPANDED_SECTION_SIZE = 90;
export const SIDEBAR_COLLAPSE_THRESHOLD = 40;
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
  if (value === true) {
    return true;
  }
  if (value === false) {
    return false;
  }
  return fallback;
}

export function normalizeAcknowledgedVersion(value) {
  if (typeof value !== "string" || value.length > 64) {
    return "";
  }
  if (value === "") {
    return value;
  }
  return /^[0-9][0-9A-Za-z.+-]*$/u.test(value) ? value : "";
}

function hasControlCharacters(value) {
  return Array.from(value).some((char) => char.charCodeAt(0) < 32);
}

function normalizeOptionalFavoriteString(value) {
  if (typeof value !== "string") {
    return "";
  }
  const cleaned = value.replace(/\\/g, "/").trim();
  return cleaned.length <= 512 && !hasControlCharacters(cleaned) ? cleaned : "";
}

function normalizeFavoriteItem(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const id = Number(value.id);
  if (!Number.isInteger(id) || id < 1) {
    return null;
  }
  if (value.type === "folder") {
    return {
      type: "folder",
      id,
      name: normalizeOptionalFavoriteString(value.name),
      path: normalizeOptionalFavoriteString(value.path),
      color: normalizeOptionalFavoriteString(value.color),
      icon: normalizeOptionalFavoriteString(value.icon),
      default_ttl_action: normalizeOptionalFavoriteString(value.default_ttl_action),
      default_ttl_days: Number.isInteger(value.default_ttl_days) ? value.default_ttl_days : null,
      effective_ttl_action: normalizeOptionalFavoriteString(value.effective_ttl_action),
      effective_ttl_days: Number.isInteger(value.effective_ttl_days)
        ? value.effective_ttl_days
        : null,
      effective_ttl_inherited: Boolean(value.effective_ttl_inherited),
      effective_ttl_source_id: Number.isInteger(value.effective_ttl_source_id)
        ? value.effective_ttl_source_id
        : null,
      access: value.access && typeof value.access === "object" ? value.access : {},
      archived: Boolean(value.archived),
      modified_at: normalizeOptionalFavoriteString(value.modified_at) || null,
      modified_display: normalizeOptionalFavoriteString(value.modified_display),
      latest_by: normalizeOptionalFavoriteString(value.latest_by),
      size_bytes: Number.isFinite(value.size_bytes) ? value.size_bytes : 0,
      size_display: normalizeOptionalFavoriteString(value.size_display),
    };
  }
  if (value.type === "document") {
    return {
      type: "document",
      id,
      name: normalizeOptionalFavoriteString(value.name),
      folder: normalizeOptionalFavoriteString(value.folder),
      path: normalizeOptionalFavoriteString(value.path),
    };
  }
  return null;
}

function favoriteItemKey(item) {
  return item.type === "document" ? `document:${item.id}` : `folder:${item.id}`;
}

export function normalizeFavoriteItems(value) {
  if (!Array.isArray(value)) {
    return [];
  }
  const seen = new Set();
  const items = [];
  value.forEach((rawItem) => {
    const item = normalizeFavoriteItem(rawItem);
    if (item) {
      const key = favoriteItemKey(item);
      if (!seen.has(key)) {
        seen.add(key);
        items.push(item);
      }
    }
  });
  return items;
}

export function normalizeSidebarSectionSizes(value) {
  const source = value && typeof value === "object" && !Array.isArray(value) ? value : {};
  return SIDEBAR_SECTION_KEYS.reduce((sizes, key) => {
    // eslint-disable-next-line security/detect-object-injection
    const rawSize = source[key];
    // eslint-disable-next-line security/detect-object-injection
    const defaultSize = USER_PREFERENCE_DEFAULTS.sidebarSectionSizes[key];
    const numericSize =
      typeof rawSize === "number" && Number.isFinite(rawSize) ? rawSize : defaultSize;
    // eslint-disable-next-line security/detect-object-injection
    sizes[key] = Math.max(
      MIN_SIDEBAR_SECTION_SIZE,
      Math.min(MAX_SIDEBAR_SECTION_SIZE, Math.round(numericSize))
    );
    return sizes;
  }, {});
}

export function normalizeSidebarSectionCollapsed(value) {
  const source = value && typeof value === "object" && !Array.isArray(value) ? value : {};
  return SIDEBAR_SECTION_KEYS.reduce((collapsed, key) => {
    // eslint-disable-next-line security/detect-object-injection
    const defaultValue = USER_PREFERENCE_DEFAULTS.sidebarSectionCollapsed[key];
    // eslint-disable-next-line security/detect-object-injection
    collapsed[key] = typeof source[key] === "boolean" ? source[key] : defaultValue;
    return collapsed;
  }, {});
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
    downloadLocationGuidanceDismissed: normalizeBooleanPreference(
      source.downloadLocationGuidanceDismissed,
      USER_PREFERENCE_DEFAULTS.downloadLocationGuidanceDismissed
    ),
    whatsNewAcknowledgedVersion: normalizeAcknowledgedVersion(source.whatsNewAcknowledgedVersion),
    contentsViewByFolder: normalizeContentsViewByFolder(source.contentsViewByFolder),
    contentsColumnOrderByFolder: normalizeContentsColumnOrderByFolder(
      source.contentsColumnOrderByFolder
    ),
    favoriteItems: normalizeFavoriteItems(source.favoriteItems),
    sidebarSectionSizes: normalizeSidebarSectionSizes(source.sidebarSectionSizes),
    sidebarSectionCollapsed: normalizeSidebarSectionCollapsed(source.sidebarSectionCollapsed),
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

async function fetchUserPreferences(apiFetch) {
  if (!apiFetch) {
    return null;
  }
  const res = await apiFetch("/api/preferences");
  if (!res.ok) {
    throw new Error("Could not refresh preferences");
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

function mergeFolderPreferenceEntries(preferences, field, entries) {
  if (field === "contentsViewByFolder") {
    return normalizeUserPreferences({
      ...preferences,
      contentsViewByFolder: { ...preferences.contentsViewByFolder, ...entries },
    });
  }
  return normalizeUserPreferences({
    ...preferences,
    contentsColumnOrderByFolder: { ...preferences.contentsColumnOrderByFolder, ...entries },
  });
}

export function useAppearancePreferences({
  apiFetch,
  initialPreferences,
  onPreferenceSaveError,
} = {}) {
  const [userPreferences, setUserPreferences] = useState(() =>
    resolveInitialUserPreferences(initialPreferences)
  );
  const folderPreferencePendingRef = useRef(new Map());
  const folderPreferenceRevisionRef = useRef(0);
  const folderPreferenceSaveChainRef = useRef(Promise.resolve(null));

  const reconcileSavedPreferences = useCallback((savedPreferences) => {
    let reconciled = savedPreferences;
    folderPreferencePendingRef.current.forEach((pending) => {
      reconciled = mergeFolderPreferenceEntries(reconciled, pending.field, pending.entries);
    });
    return normalizeUserPreferences(reconciled);
  }, []);

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
      setUserPreferences((current) => normalizeUserPreferences({ ...current, ...patch }));
      patchUserPreferences(apiFetch, patch)
        .then((savedPreferences) => {
          if (savedPreferences) {
            setUserPreferences(reconcileSavedPreferences(savedPreferences));
          }
        })
        .catch(() => {});
    },
    [apiFetch, reconcileSavedPreferences]
  );

  const refreshUserPreferences = useCallback(() => {
    return fetchUserPreferences(apiFetch).then((savedPreferences) => {
      if (savedPreferences) {
        setUserPreferences(reconcileSavedPreferences(savedPreferences));
      }
      return savedPreferences;
    });
  }, [apiFetch, reconcileSavedPreferences]);

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

  const handleWhatsNewAcknowledgedVersionChange = useCallback(
    (version) => updatePreference({ whatsNewAcknowledgedVersion: version }),
    [updatePreference]
  );

  const handleFavoriteItemsChange = useCallback(
    (preference) => updatePreference({ favoriteItems: preference }),
    [updatePreference]
  );

  const saveFolderPreference = useCallback(
    (field, entries) => {
      const [folderPath] = Object.keys(entries);
      const pendingKey = JSON.stringify([field, folderPath]);
      folderPreferenceRevisionRef.current += 1;
      const revision = folderPreferenceRevisionRef.current;
      folderPreferencePendingRef.current.set(pendingKey, { entries, field, revision });
      setUserPreferences((current) => mergeFolderPreferenceEntries(current, field, entries));
      const save = folderPreferenceSaveChainRef.current
        .catch(() => null)
        .then(() => patchUserPreferences(apiFetch, { [field]: entries }));
      folderPreferenceSaveChainRef.current = save;
      return save
        .then((savedPreferences) => {
          if (folderPreferencePendingRef.current.get(pendingKey)?.revision === revision) {
            folderPreferencePendingRef.current.delete(pendingKey);
          }
          if (savedPreferences) {
            setUserPreferences(reconcileSavedPreferences(savedPreferences));
          }
          return savedPreferences;
        })
        .catch(() => {
          if (folderPreferencePendingRef.current.get(pendingKey)?.revision === revision) {
            folderPreferencePendingRef.current.delete(pendingKey);
          }
          if (field === "contentsColumnOrderByFolder") {
            onPreferenceSaveError?.({
              kind: "error",
              title: "Could not save column order",
              detail: "Try moving the column again to save it across your devices.",
            });
          }
          return null;
        });
    },
    [apiFetch, onPreferenceSaveError, reconcileSavedPreferences]
  );

  const handleContentsViewChange = useCallback(
    (folder, view) =>
      saveFolderPreference("contentsViewByFolder", setContentsViewForFolder({}, folder, view)),
    [saveFolderPreference]
  );

  const handleContentsColumnOrderChange = useCallback(
    (folder, order) =>
      saveFolderPreference(
        "contentsColumnOrderByFolder",
        setContentsColumnOrderForFolder({}, folder, order)
      ),
    [saveFolderPreference]
  );

  const handleSidebarSectionSizesChange = useCallback(
    (preference) => updatePreference({ sidebarSectionSizes: preference }),
    [updatePreference]
  );

  const handleSidebarLayoutChange = useCallback(
    ({ sizes, collapsed }) =>
      updatePreference({
        sidebarSectionSizes: sizes,
        sidebarSectionCollapsed: collapsed,
      }),
    [updatePreference]
  );

  return {
    alternateRows: userPreferences.alternateRows,
    contentsViewByFolder: userPreferences.contentsViewByFolder,
    contentsColumns: {
      orderByFolder: userPreferences.contentsColumnOrderByFolder,
      onOrderChange: handleContentsColumnOrderChange,
    },
    doubleClickDownload: userPreferences.doubleClickDownload,
    favoriteItems: userPreferences.favoriteItems,
    handleAlternateRowsChange,
    handleContentsViewChange,
    handleDoubleClickDownloadChange,
    handleFavoriteItemsChange,
    handleOpenFoldersOnClickChange,
    handlePalettePreferenceChange,
    handleSidebarLayoutChange,
    handleSidebarSectionSizesChange,
    handleThemePreferenceChange,
    handleWhatsNewAcknowledgedVersionChange,
    openFoldersOnClick: userPreferences.openFoldersOnClick,
    palettePreference: userPreferences.palettePreference,
    refreshUserPreferences,
    sidebarSectionCollapsed: userPreferences.sidebarSectionCollapsed,
    sidebarSectionSizes: userPreferences.sidebarSectionSizes,
    themePreference: userPreferences.themePreference,
    whatsNewAcknowledgedVersion: userPreferences.whatsNewAcknowledgedVersion,
    userPreferences,
  };
}
