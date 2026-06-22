import { classNames, isArchivePath } from "../../lib/utils.js";

const h = React.createElement;

export function Breadcrumbs({ breadcrumbs, activePath, onSelect, onDropOnFolder, onClearDrop }) {
  return h(
    "div",
    { className: "crumbs-list" },
    breadcrumbs.map((crumb, idx) =>
      h(
        React.Fragment,
        { key: crumb.path + idx },
        h(
          "button",
          {
            className: classNames(
              "crumb",
              crumb.path === activePath ? "active" : "",
              isArchivePath(crumb.path) ? "archived" : ""
            ),
            onClick: () => onSelect(crumb.path),
            onDragEnter: (e) => onDropOnFolder(crumb.path, e, true),
            onDragOver: (e) => e.preventDefault(),
            onDrop: (e) => onDropOnFolder(crumb.path, e, false),
            onDragLeave: (e) => {
              if (!e.currentTarget.contains(e.relatedTarget)) {
                onClearDrop();
              }
            },
          },
          crumb.name
        ),
        idx < breadcrumbs.length - 1 ? h("span", { className: "slash" }, "/") : null
      )
    )
  );
}
