"""User preference defaults and validation."""

USER_PREFERENCE_DEFAULTS: dict[str, object] = {
    "themePreference": "system",
    "palettePreference": "cozy",
    "openFoldersOnClick": True,
    "alternateRows": False,
    "doubleClickDownload": False,
    "favoriteItems": [],
    "sidebarSectionSizes": {
        "folders": 180,
        "favorites": 95,
        "archive": 115,
        "editing": 90,
    },
}
THEME_PREFERENCES = {"system", "light", "dark"}
PALETTE_PREFERENCES = {"cozy", "winui"}
BOOLEAN_PREFERENCES = {"openFoldersOnClick", "alternateRows", "doubleClickDownload"}
SIDEBAR_SECTION_KEYS = ("folders", "favorites", "archive", "editing")
MIN_SIDEBAR_SECTION_SIZE = 72
MAX_SIDEBAR_SECTION_SIZE = 520


def _clean_favorite_id(value: object, *, label: str, strict: bool) -> int | None:
    if isinstance(value, bool) or not isinstance(value, int):
        if strict:
            raise ValueError(f"{label} id must be an integer")
        return None
    if value < 1:
        if strict:
            raise ValueError(f"{label} id must be positive")
        return None
    return value


def _clean_favorite_item(value: object, *, strict: bool) -> dict[str, object] | None:
    if not isinstance(value, dict):
        if strict:
            raise ValueError("Favorite items must be objects")
        return None
    item_type = value.get("type")
    if item_type == "folder":
        folder_id = _clean_favorite_id(value.get("id"), label="Favorite folder", strict=strict)
        if folder_id is None:
            return None
        return {
            "type": "folder",
            "id": folder_id,
        }
    if item_type == "document":
        document_id = _clean_favorite_id(
            value.get("id"), label="Favorite document", strict=strict
        )
        if document_id is None:
            return None
        return {
            "type": "document",
            "id": document_id,
        }
    if strict:
        raise ValueError("Favorite item type must be folder or document")
    return None


def _favorite_item_key(item: dict[str, object]) -> str:
    return f"{item.get('type')}:{item.get('id')}"


def _clean_favorite_items(value: object, *, strict: bool) -> list[dict[str, object]]:
    if not isinstance(value, list):
        if strict:
            raise ValueError("favoriteItems must be a list")
        return []
    cleaned: list[dict[str, object]] = []
    seen: set[str] = set()
    for raw_item in value:
        item = _clean_favorite_item(raw_item, strict=strict)
        if item is None:
            continue
        key = _favorite_item_key(item)
        if key in seen:
            continue
        cleaned.append(item)
        seen.add(key)
    return cleaned


def _clean_sidebar_section_sizes(value: object, *, strict: bool) -> dict[str, int]:
    raw_defaults = USER_PREFERENCE_DEFAULTS["sidebarSectionSizes"]
    defaults = raw_defaults if isinstance(raw_defaults, dict) else {}
    sizes = {key: int(defaults.get(key, MIN_SIDEBAR_SECTION_SIZE)) for key in SIDEBAR_SECTION_KEYS}
    if not isinstance(value, dict):
        if strict:
            raise ValueError("sidebarSectionSizes must be an object")
        return sizes
    if strict:
        unknown_keys = set(value) - set(SIDEBAR_SECTION_KEYS)
        if unknown_keys:
            raise ValueError(f"Unknown sidebar section: {next(iter(unknown_keys))}")
    for key in SIDEBAR_SECTION_KEYS:
        raw_size = value.get(key)
        if raw_size is None:
            continue
        if isinstance(raw_size, bool) or not isinstance(raw_size, int | float):
            if strict:
                raise ValueError(f"{key} sidebar section size must be numeric")
            continue
        sizes[key] = max(
            MIN_SIDEBAR_SECTION_SIZE,
            min(MAX_SIDEBAR_SECTION_SIZE, int(round(raw_size))),
        )
    return sizes


def normalize_user_preferences(raw: object) -> dict[str, object]:
    """Return a complete, valid user preference object."""
    normalized = dict(USER_PREFERENCE_DEFAULTS)
    normalized["favoriteItems"] = []
    normalized["sidebarSectionSizes"] = _clean_sidebar_section_sizes(None, strict=False)
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
    normalized["favoriteItems"] = _clean_favorite_items(
        raw.get("favoriteItems"),
        strict=False,
    )
    normalized["sidebarSectionSizes"] = _clean_sidebar_section_sizes(
        raw.get("sidebarSectionSizes"),
        strict=False,
    )
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
        elif key == "favoriteItems":
            cleaned[key] = _clean_favorite_items(value, strict=True)
        elif key == "sidebarSectionSizes":
            cleaned[key] = _clean_sidebar_section_sizes(value, strict=True)
    return cleaned


def merge_user_preferences(existing: object, patch: dict[str, object]) -> dict[str, object]:
    return normalize_user_preferences({**normalize_user_preferences(existing), **patch})
