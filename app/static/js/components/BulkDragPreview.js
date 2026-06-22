import { classNames } from "../lib/utils.js";

const h = React.createElement;

export function BulkDragPreview({ drag }) {
  if (!drag || !drag.items || drag.items.length <= 1) {
    return null;
  }
  const lead = drag.items[0];
  const files = drag.items.filter((item) => item.type === "document").length;
  const folders = drag.items.length - files;
  return h(
    "div",
    {
      className: classNames("bulk-drag-preview", drag.phase || "visible"),
      style: { left: drag.x + 14, top: drag.y + 14 },
    },
    [
      h("div", { className: "bulk-drag-stack", key: "stack" }, [
        h("span", { className: "bulk-drag-card one", key: "one" }),
        h("span", { className: "bulk-drag-card two", key: "two" }),
        h("span", { className: "bulk-drag-card three", key: "three" }),
      ]),
      h("div", { className: "bulk-drag-copy", key: "copy" }, [
        h("strong", null, `${drag.items.length} items`),
        h(
          "span",
          null,
          `${lead.name || "Selection"}${files ? ` · ${files} files` : ""}${
            folders ? ` · ${folders} folders` : ""
          }`
        ),
      ]),
    ]
  );
}
