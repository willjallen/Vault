import { classNames } from "../lib/utils.js";
import { Icon } from "./common/Icon.js";

const h = React.createElement;

function noticeIcon(kind) {
  if (kind === "busy") {
    return "refresh";
  }
  if (kind === "success") {
    return "check";
  }
  if (kind === "info") {
    return "info";
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
  if (notice.kind === "success") {
    return "Success";
  }
  if (notice.kind === "info") {
    return "Notice";
  }
  return "Error";
}

function NotificationRow({ notice, onDismiss }) {
  const detail = String(notice.detail || "").trim();
  const hasDetail = Boolean(detail);
  const dismissible = Boolean(onDismiss) && notice.dismissible !== false;
  const showTimebar = Boolean(notice.duration && hasDetail);
  const durationStyle = showTimebar ? { "--notification-duration": `${notice.duration}ms` } : {};
  return h(
    "div",
    {
      className: classNames(
        "notification-row",
        notice.kind || "error",
        hasDetail ? "has-detail" : "no-detail",
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
        hasDetail ? h("div", { className: "notification-detail", key: "detail" }, detail) : null,
        showTimebar
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
