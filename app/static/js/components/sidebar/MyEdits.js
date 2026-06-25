import { classNames, isArchivePath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const h = React.createElement;

export function MyEdits({
  edits,
  selectedId,
  onSelect,
  onContextMenu,
  className = "",
  style,
  collapsed = false,
  onToggleCollapsed,
}) {
  return h(
    "div",
    { className: classNames("sidebar-section", className, collapsed ? "collapsed" : ""), style },
    h(
      "button",
      {
        className: classNames("sidebar-section-header", collapsed ? "collapsed" : ""),
        onClick: onToggleCollapsed,
        title: collapsed ? "Expand You're editing" : "Collapse You're editing",
        type: "button",
      },
      [
        h("span", { className: "sidebar-section-title eyebrow tiny" }, "You're editing"),
        h(Icon, {
          className: "sidebar-section-chevron",
          icon: collapsed ? "chevron-right" : "chevron-down",
          size: 11,
        }),
      ]
    ),
    collapsed
      ? null
      : h(
          "div",
          { className: "my-edits-list" },
          edits.length
            ? edits.map((doc) => {
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
                    onContextMenu: (e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      onSelect(doc);
                      if (onContextMenu) {
                        onContextMenu(e, doc);
                      }
                    },
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
            : h("div", { className: "sidebar-empty" }, "No checked-out files")
        )
  );
}
