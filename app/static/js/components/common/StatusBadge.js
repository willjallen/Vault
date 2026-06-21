import { classNames } from "../../lib/utils.js";

const h = React.createElement;

function LockIcon() {
  return h(
    "svg",
    {
      className: "status-icon",
      viewBox: "0 0 20 20",
      width: 14,
      height: 14,
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 1.6,
    },
    [
      h("rect", { x: 4, y: 8, width: 12, height: 9, rx: 2 }),
      h("path", { d: "M7 8V6a3 3 0 1 1 6 0v2" }),
    ]
  );
}

export function StatusBadge({ doc, currentUserId, showReady = true, labelOverride = null }) {
  if (!doc) {
    return null;
  }
  const lock = doc.lock || {};
  const lockedByMe = lock.by && lock.by === currentUserId;
  const lockedByOther = lock.by && lock.by !== currentUserId;
  const statusClass = doc.archived
    ? "archived"
    : lockedByMe
      ? "locked-self"
      : lockedByOther
        ? "locked"
        : "ready";
  if (statusClass === "ready" && !showReady) {
    return null;
  }
  const label =
    labelOverride ||
    (statusClass === "archived"
      ? "Archived"
      : statusClass === "locked-self"
        ? "Checked out by you"
        : statusClass === "locked"
          ? `Checked out by ${lock.name || lock.by}`
          : "Ready to edit");
  const icon = statusClass === "locked" || statusClass === "locked-self" ? h(LockIcon) : null;
  return h(
    "span",
    {
      className: classNames("status", "pill", statusClass, icon ? "with-icon" : ""),
      title: label,
    },
    [icon, label]
  );
}
