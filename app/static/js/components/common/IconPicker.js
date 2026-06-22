import { folderIconOptions, searchIcons } from "../../lib/IconLibrary.js";
import { classNames } from "../../lib/utils.js";
import { Icon } from "./Icon.js";

const { useCallback, useMemo, useState } = React;
const h = React.createElement;

export const iconColorOptions = [
  { id: "", label: "Default", value: "" },
  { id: "blue", label: "Blue", value: "#2563eb" },
  { id: "teal", label: "Teal", value: "#0f766e" },
  { id: "green", label: "Green", value: "#15803d" },
  { id: "amber", label: "Amber", value: "#b45309" },
  { id: "rose", label: "Rose", value: "#be123c" },
  { id: "violet", label: "Violet", value: "#7c3aed" },
  { id: "slate", label: "Slate", value: "#475569" },
];

export function colorValueForToken(token) {
  return iconColorOptions.find((option) => option.id === token)?.value || "";
}

export function IconPicker({ allowDelete = true, color = "", icon = "", onChange, size = 24 }) {
  const [query, setQuery] = useState("");
  const currentIcon = icon || "folder";
  const currentColor = colorValueForToken(color);
  const hasCustomAppearance = Boolean(icon || color);
  const filteredIcons = useMemo(() => searchIcons(query), [query]);
  const selectedIcon = useMemo(
    () => folderIconOptions.find((option) => option.id === currentIcon),
    [currentIcon]
  );
  const selectedColor = useMemo(
    () => iconColorOptions.find((option) => option.id === color) || iconColorOptions[0],
    [color]
  );

  const chooseIcon = useCallback(
    (nextIcon) => {
      onChange?.(nextIcon, color);
    },
    [color, onChange]
  );

  const chooseColor = useCallback(
    (nextColor) => {
      onChange?.(icon, nextColor);
    },
    [icon, onChange]
  );

  const resetAppearance = useCallback(() => {
    onChange?.("", "");
    setQuery("");
  }, [onChange]);

  return h("div", { className: "icon-picker icon-picker-inline" }, [
    h("div", { className: "icon-picker-current", key: "current" }, [
      h(
        "span",
        { className: "icon-picker-current-sample", key: "sample" },
        h(Icon, { color: currentColor, icon: currentIcon, size })
      ),
      h("div", { className: "icon-picker-current-copy", key: "copy" }, [
        h("strong", { key: "name" }, selectedIcon?.label || "Default folder"),
        h("span", { key: "color" }, selectedColor.label),
      ]),
      allowDelete && hasCustomAppearance
        ? h(
            "button",
            {
              className: "icon-picker-remove",
              key: "remove",
              onClick: resetAppearance,
              type: "button",
            },
            "Remove"
          )
        : null,
    ]),
    h("label", { className: "icon-picker-search", key: "search" }, [
      h(Icon, { icon: "search", key: "icon", size: 14 }),
      h("input", {
        "aria-label": "Search icons",
        key: "input",
        onChange: (evt) => setQuery(evt.target.value),
        placeholder: "Search icons",
        type: "search",
        value: query,
      }),
    ]),
    h("div", { className: "icon-picker-grid", key: "grid" }, [
      h(
        "button",
        {
          className: classNames("icon-picker-choice", !icon ? "active" : ""),
          key: "default",
          onClick: () => chooseIcon(""),
          type: "button",
        },
        [
          h(Icon, {
            color: currentColor,
            icon: "folder",
            key: "icon",
            size: 18,
          }),
          h("span", { key: "label" }, "Default"),
        ]
      ),
      ...filteredIcons.map((option) =>
        h(
          "button",
          {
            className: classNames("icon-picker-choice", icon === option.id ? "active" : ""),
            key: option.id,
            onClick: () => chooseIcon(option.id),
            title: option.label,
            type: "button",
          },
          [
            h(Icon, {
              color: currentColor,
              icon: option.id,
              key: "icon",
              size: 18,
            }),
            h("span", { key: "label" }, option.label),
          ]
        )
      ),
      filteredIcons.length
        ? null
        : h("p", { className: "icon-picker-empty", key: "empty" }, "No icons found."),
    ]),
    h("div", { className: "icon-color-row", key: "colors" }, [
      ...iconColorOptions.map((option) =>
        h(
          "button",
          {
            "aria-label": option.label,
            className: classNames("icon-color-choice", color === option.id ? "active" : ""),
            key: option.id || "default",
            onClick: () => chooseColor(option.id),
            title: option.label,
            type: "button",
          },
          [
            h("span", {
              className: "icon-color-swatch",
              key: "swatch",
              style: option.value ? { background: option.value } : {},
            }),
            h("span", { key: "label" }, option.label),
          ]
        )
      ),
    ]),
  ]);
}
