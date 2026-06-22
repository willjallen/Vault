import { Breadcrumbs } from "../common/Breadcrumbs.js";

const h = React.createElement;

function LogoutIcon() {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      className: "logout-icon",
      fill: "none",
      height: "18",
      stroke: "currentColor",
      strokeLinecap: "round",
      strokeLinejoin: "round",
      strokeWidth: "2",
      viewBox: "0 0 24 24",
      width: "18",
    },
    [
      h("path", { d: "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4", key: "door" }),
      h("path", { d: "M16 17l5-5-5-5", key: "arrow" }),
      h("path", { d: "M21 12H9", key: "line" }),
    ]
  );
}

function SettingsIcon() {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      className: "settings-icon",
      fill: "none",
      height: "18",
      stroke: "currentColor",
      strokeLinecap: "round",
      strokeLinejoin: "round",
      strokeWidth: "2",
      viewBox: "0 0 24 24",
      width: "18",
    },
    [
      h("path", { d: "M12 15.5A3.5 3.5 0 1 0 12 8a3.5 3.5 0 0 0 0 7.5Z", key: "gear-core" }),
      h("path", {
        d: "M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06A1.7 1.7 0 0 0 15 19.37a1.7 1.7 0 0 0-1 .54V20a2 2 0 0 1-4 0v-.09a1.7 1.7 0 0 0-1-.54 1.7 1.7 0 0 0-1.88.34l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.63 15a1.7 1.7 0 0 0-.54-1H4a2 2 0 0 1 0-4h.09a1.7 1.7 0 0 0 .54-1 1.7 1.7 0 0 0-.34-1.88l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.63a1.7 1.7 0 0 0 1-.54V4a2 2 0 0 1 4 0v.09a1.7 1.7 0 0 0 1 .54 1.7 1.7 0 0 0 1.88-.34l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.37 9c.2.34.38.67.54 1H20a2 2 0 0 1 0 4h-.09c-.16.34-.34.67-.51 1Z",
        key: "gear-ring",
      }),
    ]
  );
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
        "‹"
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
        "›"
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
        "↑"
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
        h(SettingsIcon)
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
        h(LogoutIcon)
      ),
    ])
  );
}
