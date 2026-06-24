export const SITE_SETTING_DEFAULTS = {
  archivePermanentDeleteAdminOnly: true,
};

export function normalizeSiteSettings(value) {
  const source = value && typeof value === "object" ? value : {};
  return {
    archivePermanentDeleteAdminOnly:
      typeof source.archivePermanentDeleteAdminOnly === "boolean"
        ? source.archivePermanentDeleteAdminOnly
        : SITE_SETTING_DEFAULTS.archivePermanentDeleteAdminOnly,
  };
}

export function canDeleteForeverItem(item, { isAdmin = false, siteSettings = {} } = {}) {
  const settings = normalizeSiteSettings(siteSettings);
  if (!item?.archived) {
    return false;
  }
  if (isAdmin) {
    return true;
  }
  return !settings.archivePermanentDeleteAdminOnly && Boolean(item.access?.write);
}
