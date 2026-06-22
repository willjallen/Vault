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
    h(
      "div",
      { className: "toolbar-actions" },
      h(
        "a",
        {
          "aria-label": "Log out",
          className: "btn logout icon-button",
          href: logoutUrl,
          title: "Log out",
        },
        h(LogoutIcon)
      )
    )
  );
}
