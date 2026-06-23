import { Icon } from "../common/Icon.js";

const h = React.createElement;

export function EmptyState({ onUpload, search = false }) {
  return h(
    "div",
    { className: "empty-state" },
    h(
      "div",
      { className: "empty-icon" },
      h(Icon, { icon: search ? "search" : "folder", size: 24 })
    ),
    h("h3", null, search ? "No results found" : "This folder is empty"),
    h(
      "p",
      { className: "muted tiny" },
      search ? "Try a different search." : "Drop a file here or use the upload button to add one."
    ),
    search
      ? null
      : h(
          "button",
          {
            className: "btn secondary",
            type: "button",
            onClick: onUpload,
          },
          "Upload file"
        )
  );
}
