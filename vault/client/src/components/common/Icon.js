import { icon as renderIcon } from "@fortawesome/fontawesome-svg-core";

import { findIconEntry } from "../../lib/IconLibrary.js";
import { classNames } from "../../lib/utils.js";

const h = React.createElement;

export function Icon({
  className = "",
  color = "",
  fixedWidth = false,
  icon,
  label = "",
  size = 16,
}) {
  const entry = findIconEntry(icon);
  const baseAttrs = {
    "aria-hidden": label ? undefined : "true",
    "aria-label": label || undefined,
    className: classNames("app-icon-wrap", entry.kind === "image" ? "image" : "", className),
    role: label ? "img" : undefined,
    style: {
      "--icon-size": `${size}px`,
    },
  };
  if (entry.kind === "image") {
    return h(
      "span",
      baseAttrs,
      h("img", {
        alt: "",
        className: "app-icon-img",
        draggable: "false",
        src: entry.src,
      })
    );
  }

  const definition = entry.icon;
  const rendered = renderIcon(definition, {
    classes: classNames("app-icon", fixedWidth ? "fixed" : ""),
    styles: color ? { color } : {},
  });
  const attrs = {
    ...baseAttrs,
    dangerouslySetInnerHTML: { __html: rendered.html.join("") },
  };
  return h("span", attrs);
}
