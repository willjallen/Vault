const h = React.createElement;

export function EmptyState({ onUpload }) {
  return h(
    "div",
    { className: "empty-state" },
    h("div", { className: "empty-icon" }, "📂"),
    h("h3", null, "This folder is empty"),
    h("p", { className: "muted tiny" }, "Drop a file here or use the upload button to add one."),
    h(
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
