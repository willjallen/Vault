import { classNames } from "../../lib/utils.js";
import { FolderNode } from "./FolderNode.js";

const h = React.createElement;

export function SidebarTree({
  tree,
  currentFolder,
  onSelect,
  dropHint,
  onDrop,
  onContextMenu,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
}) {
  return h(
    "div",
    { className: "sidebar-section" },
    h("div", { className: "sidebar-title" }, h("p", { className: "eyebrow tiny" }, "Folders")),
    h(
      "div",
      { className: "tree" },
      h(
        "button",
        {
          className: classNames(
            "folder-node",
            currentFolder === "" ? "active" : "",
            dropHint === "" ? "drop-target" : ""
          ),
          onClick: () => onSelect(""),
          onContextMenu: (e) => onContextMenu && onContextMenu(e, { path: "", name: "Vault" }),
          draggable: false,
          onDragEnter: (e) => {
            e.preventDefault();
            onDrop("", e, true);
          },
          onDragOver: (e) => {
            e.preventDefault();
            e.dataTransfer.dropEffect = "move";
          },
          onDragLeave: (e) => {
            if (!e.currentTarget.contains(e.relatedTarget)) {
              onDrop(null, e, false, true);
            }
          },
          onDrop: (e) => onDrop("", e, false),
        },
        h("span", { className: "folder-glyph" }, "📁"),
        h("span", { className: "folder-name" }, "Vault")
      ),
      tree.map((node) =>
        h(FolderNode, {
          key: node.path,
          node,
          depth: 0,
          activePath: currentFolder,
          dropHint,
          onSelect,
          onDrop,
          onContextMenu,
          onFolderDragStart,
          onFolderDragEnd,
          draggingFolderPath,
        })
      )
    )
  );
}
