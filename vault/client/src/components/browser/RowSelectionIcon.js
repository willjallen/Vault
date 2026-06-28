import { FileIcon } from "../common/FileIcon.js";
import { classNames } from "../../lib/utils.js";

const h = React.createElement;

export function RowSelectionIcon({
  color = "",
  disabled = false,
  fileName = "",
  folderIcon = "",
  interactive = true,
  kind,
  label,
  onSelect,
  selected = false,
  size,
}) {
  const icon = h(FileIcon, { color, fileName, folderIcon, kind, size });

  if (!interactive) {
    return h(
      "span",
      { className: "row-select-button static", "aria-hidden": "true" },
      h("span", { className: "row-select-icon" }, icon)
    );
  }

  function handleClick(e) {
    e.preventDefault();
    e.stopPropagation();
    if (!disabled && onSelect) {
      onSelect(e);
    }
  }

  return h(
    "button",
    {
      "aria-checked": Boolean(selected),
      "aria-label": label,
      className: classNames("row-select-button", selected ? "selected" : ""),
      disabled,
      draggable: false,
      onClick: handleClick,
      role: "checkbox",
      title: label,
      type: "button",
    },
    [
      h("span", { className: "row-select-icon", key: "icon" }, icon),
      h("span", { "aria-hidden": "true", className: "row-select-check", key: "check" }),
    ]
  );
}
