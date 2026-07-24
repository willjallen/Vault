import { classNames, isArchivedPath } from "../../lib/utils.js";

const h = React.createElement;

export function Breadcrumbs({ breadcrumbs, activePath, onSelect, onDropOnFolder, onClearDrop }) {
  return h(
    "div",
    { className: "crumbs-list" },
    breadcrumbs.map((crumb, idx) => {
      const archived = isArchivedPath(crumb.path);
      return h(
        React.Fragment,
        { key: crumb.path + idx },
        h(
          "button",
          {
            className: classNames(
              "crumb",
              crumb.path === activePath ? "active" : "",
              archived ? "archived" : ""
            ),
            ...(archived
              ? {}
              : {
                  "data-vault-drop-kind": "folder",
                  "data-drop-folder": crumb.path || "",
                  "data-drop-label": "Move here",
                  onDragEnter: (e) => onDropOnFolder(crumb.path, e, true),
                  onDragOver: (e) => e.preventDefault(),
                  onDrop: (e) => onDropOnFolder(crumb.path, e, false),
                  onDragLeave: (e) => {
                    if (!e.currentTarget.contains(e.relatedTarget)) {
                      onClearDrop();
                    }
                  },
                }),
            onClick: () => onSelect(crumb.path),
          },
          crumb.name
        ),
        idx < breadcrumbs.length - 1 ? h("span", { className: "slash" }, "/") : null
      );
    })
  );
}
