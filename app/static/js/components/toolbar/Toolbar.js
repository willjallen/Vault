import { Breadcrumbs } from "../common/Breadcrumbs.js";
import { Icon } from "../common/Icon.js";

const h = React.createElement;

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
      h(
        "a",
        {
          "aria-label": "Log out",
          className: "btn logout icon-button",
          href: logoutUrl,
          title: "Log out",
          key: "logout",
        },
        h(Icon, { className: "logout-icon", icon: "logout", size: 18 })
      ),
    ])
  );
}
