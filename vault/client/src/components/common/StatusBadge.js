import { classNames } from "../../lib/utils.js";
import { Icon } from "./Icon.js";

const h = React.createElement;

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
  const icon =
    statusClass === "locked" || statusClass === "locked-self"
      ? h(Icon, { className: "status-icon", icon: "lock", size: 14 })
      : null;
  return h(
    "span",
    {
      className: classNames("status", "pill", statusClass, icon ? "with-icon" : ""),
      title: label,
    },
    [icon, label]
  );
}
