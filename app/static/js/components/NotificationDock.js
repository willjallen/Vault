import { classNames } from "../lib/utils.js";
import { Icon } from "./common/Icon.js";

const h = React.createElement;

function noticeIcon(kind) {
  if (kind === "busy") {
    return "refresh";
  }
  return "warning";
}

function noticeTitle(notice) {
  if (notice.title) {
    return notice.title;
  }
  if (notice.kind === "busy") {
    return "Working";
  }
  return "Error";
}

function NotificationRow({ notice, onDismiss }) {
  const dismissible = Boolean(onDismiss) && notice.dismissible !== false;
  const durationStyle = notice.duration
    ? { "--notification-duration": `${notice.duration}ms` }
    : {};
  return h(
    "div",
    {
      className: classNames(
        "notification-row",
        notice.kind || "error",
        `phase-${notice.phase || "visible"}`
      ),
      role: notice.kind === "error" ? "alert" : "status",
      style: durationStyle,
    },
    [
      h(
        "span",
        { className: "notification-icon", key: "icon" },
        h(Icon, { icon: noticeIcon(notice.kind), size: 15 })
      ),
      h("div", { className: "notification-copy", key: "copy" }, [
        h("div", { className: "notification-title", key: "title" }, noticeTitle(notice)),
        notice.message
          ? h("div", { className: "notification-message", key: "message" }, notice.message)
          : null,
        notice.duration
          ? h("div", { className: "notification-timebar", key: "timebar" }, h("span", null))
          : null,
      ]),
      dismissible
        ? h(
            "button",
            {
              "aria-label": "Dismiss notification",
              className: "notification-dismiss",
              key: "dismiss",
              onClick: () => onDismiss(notice.id),
              type: "button",
            },
            h(Icon, { icon: "close", size: 13 })
          )
        : null,
    ]
  );
}

export function NotificationDock({ notices, onDismiss }) {
  if (!notices.length) {
    return null;
  }

  return h(
    "div",
    { "aria-live": "polite", className: "notification-dock" },
    notices.map((notice) => h(NotificationRow, { key: notice.id, notice, onDismiss }))
  );
}
