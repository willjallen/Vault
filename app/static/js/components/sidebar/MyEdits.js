import { classNames, isArchivePath } from "../../lib/utils.js";

const h = React.createElement;

export function MyEdits({ edits, selectedId, onSelect }) {
  if (!edits.length) {
    return null;
  }
  return h(
    "div",
    { className: "sidebar-section" },
    h("p", { className: "eyebrow tiny" }, "You're editing"),
    edits.map((doc) => {
      const inArchive = isArchivePath(doc.folder || "");
      return h(
        "button",
        {
          key: doc.id,
          className: classNames(
            "my-edit-chip",
            selectedId === doc.id ? "active" : "",
            inArchive ? "archived" : ""
          ),
          onClick: () => onSelect(doc),
        },
        [
          h("span", { className: "chip-dot" }),
          h("span", null, doc.name),
          h(
            "span",
            { className: classNames("muted", "tiny", inArchive ? "archived-text" : "") },
            doc.folder || "Vault"
          ),
        ]
      );
    })
  );
}
