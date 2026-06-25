import { classNames } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const h = React.createElement;

export function TtlStatusLabel({ className = "", labels, title }) {
  if (!labels || !labels.compact) {
    return null;
  }
  return h("span", { className: classNames("ttl-chip", className), title: title || labels.full }, [
    h(Icon, { icon: "clock", key: "icon", size: 11 }),
    h("span", { className: "ttl-label ttl-label-full", key: "full" }, labels.full),
    h("span", { className: "ttl-label ttl-label-medium", key: "medium" }, labels.medium),
    h("span", { className: "ttl-label ttl-label-compact", key: "compact" }, labels.compact),
  ]);
}
