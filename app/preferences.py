"""User preference defaults and validation."""

USER_PREFERENCE_DEFAULTS: dict[str, object] = {
    "themePreference": "system",
    "palettePreference": "cozy",
    "openFoldersOnClick": True,
    "alternateRows": False,
    "doubleClickDownload": False,
}
THEME_PREFERENCES = {"system", "light", "dark"}
PALETTE_PREFERENCES = {"cozy", "winui"}
BOOLEAN_PREFERENCES = {"openFoldersOnClick", "alternateRows", "doubleClickDownload"}


def normalize_user_preferences(raw: object) -> dict[str, object]:
    """Return a complete, valid user preference object."""
    normalized = dict(USER_PREFERENCE_DEFAULTS)
    if not isinstance(raw, dict):
        return normalized
    theme = raw.get("themePreference")
    if isinstance(theme, str) and theme in THEME_PREFERENCES:
        normalized["themePreference"] = theme
    palette = raw.get("palettePreference")
    if isinstance(palette, str) and palette in PALETTE_PREFERENCES:
        normalized["palettePreference"] = palette
    for key in BOOLEAN_PREFERENCES:
        value = raw.get(key)
        if isinstance(value, bool):
            normalized[key] = value
    return normalized


def clean_user_preference_patch(raw: object) -> dict[str, object]:
    """Validate a partial user preference update."""
    if not isinstance(raw, dict):
        raise ValueError("Preferences must be an object")
    cleaned: dict[str, object] = {}
    for key, value in raw.items():
        if key not in USER_PREFERENCE_DEFAULTS:
            raise ValueError(f"Unknown preference: {key}")
        if key == "themePreference":
            if not isinstance(value, str) or value not in THEME_PREFERENCES:
                raise ValueError("Invalid theme preference")
            cleaned[key] = value
        elif key == "palettePreference":
            if not isinstance(value, str) or value not in PALETTE_PREFERENCES:
                raise ValueError("Invalid palette preference")
            cleaned[key] = value
        elif key in BOOLEAN_PREFERENCES:
            if not isinstance(value, bool):
                raise ValueError(f"{key} must be a boolean")
            cleaned[key] = value
    return cleaned


def merge_user_preferences(existing: object, patch: dict[str, object]) -> dict[str, object]:
    return normalize_user_preferences({**normalize_user_preferences(existing), **patch})
