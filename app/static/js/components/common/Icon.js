import { icon as renderIcon } from "@fortawesome/fontawesome-svg-core";

import { findIconDefinition } from "../../lib/IconLibrary.js";
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
  const definition = findIconDefinition(icon);
  const rendered = renderIcon(definition, {
    classes: classNames("app-icon", fixedWidth ? "fixed" : ""),
    styles: color ? { color } : {},
  });
  const attrs = {
    "aria-hidden": label ? undefined : "true",
    "aria-label": label || undefined,
    className: classNames("app-icon-wrap", className),
    dangerouslySetInnerHTML: { __html: rendered.html.join("") },
    role: label ? "img" : undefined,
    style: {
      "--icon-size": `${size}px`,
    },
  };
  return h("span", attrs);
}
