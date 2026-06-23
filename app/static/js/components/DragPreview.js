import { classNames } from "../lib/utils.js";

const h = React.createElement;

function pluralize(count, singular, plural = `${singular}s`) {
  return `${count} ${count === 1 ? singular : plural}`;
}

export function DragPreview({ drag }) {
  if (!drag || !drag.items || drag.items.length === 0) {
    return null;
  }
  const lead = drag.items[0];
  const files = drag.items.filter((item) => item.type === "document").length;
  const folders = drag.items.length - files;
  const details = [
    lead.name || "Selection",
    files ? pluralize(files, "file") : "",
    folders ? pluralize(folders, "folder") : "",
  ].filter(Boolean);

  return h(
    "div",
    {
      className: classNames(
        "drag-preview",
        drag.items.length === 1 ? "single" : "multiple",
        drag.phase || "visible"
      ),
      style: { left: drag.x + 14, top: drag.y + 14 },
    },
    [
      h("div", { className: "drag-preview-stack", key: "stack" }, [
        h("span", { className: "drag-preview-card one", key: "one" }),
        h("span", { className: "drag-preview-card two", key: "two" }),
        h("span", { className: "drag-preview-card three", key: "three" }),
      ]),
      h("div", { className: "drag-preview-copy", key: "copy" }, [
        h("strong", null, pluralize(drag.items.length, "item")),
        h("span", null, details.join(" · ")),
      ]),
    ]
  );
}
