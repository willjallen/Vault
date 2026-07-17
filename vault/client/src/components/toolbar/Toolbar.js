import { Breadcrumbs } from "../common/Breadcrumbs.js";
import { Icon } from "../common/Icon.js";

const h = React.createElement;

function logoutControl(logoutUrl) {
  const icon = h(Icon, { className: "logout-icon", icon: "logout", size: 18 });
  const commonProps = {
    "aria-label": "Log out",
    className: "btn logout icon-button",
    title: "Log out",
  };
  if (typeof logoutUrl === "string" && logoutUrl.startsWith("/") && !logoutUrl.startsWith("//")) {
    return h(
      "form",
      { action: logoutUrl, className: "logout-form", key: "logout", method: "post" },
      h("button", { ...commonProps, type: "submit" }, icon)
    );
  }
  return h("a", { ...commonProps, href: logoutUrl, key: "logout" }, icon);
}

export function Toolbar({
  folder,
  breadcrumbs,
  canGoBack,
  canGoForward,
  canGoUp,
  onNavigateBack,
  onNavigateForward,
  onNavigateUp,
  logoutUrl,
  onOpenSettings,
  settingsButtonRef,
  onSelectFolder,
  onDropOnFolder,
  onClearDrop,
}) {
  return h(
    "div",
    { className: "finder-toolbar" },
    h("div", { className: "toolbar-navigation" }, [
      h(
        "button",
        {
          "aria-label": "Back",
          className: "btn ghost nav-button",
          disabled: !canGoBack,
          onClick: onNavigateBack,
          title: "Back",
          type: "button",
        },
        h(Icon, { icon: "arrow-left", size: 14 })
      ),
      h(
        "button",
        {
          "aria-label": "Forward",
          className: "btn ghost nav-button",
          disabled: !canGoForward,
          onClick: onNavigateForward,
          title: "Forward",
          type: "button",
        },
        h(Icon, { icon: "arrow-right", size: 14 })
      ),
      h(
        "button",
        {
          "aria-label": "Up",
          className: "btn ghost nav-button",
          disabled: !canGoUp,
          onClick: onNavigateUp,
          title: "Up",
          type: "button",
        },
        h(Icon, { icon: "arrow-up", size: 14 })
      ),
      h(Breadcrumbs, {
        breadcrumbs,
        activePath: folder,
        onSelect: onSelectFolder,
        onDropOnFolder,
        onClearDrop,
      }),
    ]),
    h("div", { className: "toolbar-actions" }, [
      h(
        "button",
        {
          "aria-label": "Open settings",
          className: "btn settings-button icon-button",
          onClick: onOpenSettings,
          ref: settingsButtonRef,
          title: "Settings",
          type: "button",
          key: "settings",
        },
        h(Icon, { className: "settings-icon", icon: "gear", size: 18 })
      ),
      logoutControl(logoutUrl),
    ])
  );
}
