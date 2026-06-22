import { classNames, isArchivePath } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const h = React.createElement;

export function FolderNode({
  node,
  depth,
  activePath,
  onSelect,
  onDrop,
  dropHint,
  onContextMenu,
  onFolderDragStart,
  onFolderDragEnd,
  draggingFolderPath,
}) {
  const offset = Math.min(12 + depth * 10, 78);
  const isActive = activePath === node.path;
  const isArchived = isArchivePath(node.path);
  const icon = node.icon || (isArchived ? "box-archive" : "folder");
  return h(
    React.Fragment,
    null,
    h(
      "button",
      {
        className: classNames(
          "folder-node",
          isArchived ? "archived" : "",
          isActive ? "active" : "",
          dropHint === node.path ? "drop-target" : "",
          draggingFolderPath === node.path ? "dragging" : ""
        ),
        style: { paddingLeft: `${offset}px` },
        title: node.path || node.name || "Folder",
        onClick: () => onSelect(node.path),
        onContextMenu: (e) => onContextMenu && onContextMenu(e, node),
        draggable: true,
        onDragStart: (e) => onFolderDragStart && onFolderDragStart(e, node.path),
        onDragEnd: onFolderDragEnd,
        onDragEnter: (e) => {
          e.preventDefault();
          onDrop(node.path, e, true);
        },
        onDragLeave: (e) => {
          if (!e.currentTarget.contains(e.relatedTarget)) {
            onDrop(null, e, false, true);
          }
        },
        onDragOver: (e) => {
          e.preventDefault();
          e.dataTransfer.dropEffect = "move";
        },
        onDrop: (e) => onDrop(node.path, e, false),
      },
      h(
        "span",
        {
          className: classNames(
            "folder-glyph",
            node.color ? `folder-color-${node.color}` : "",
            isArchived ? "archived-text" : ""
          ),
        },
        h(Icon, { icon, size: 15 })
      ),
      h(
        "span",
        { className: classNames("folder-name", isArchived ? "archived-text" : "") },
        node.name || "Folder"
      )
    ),
    node.children && node.children.length
      ? node.children.map((child) =>
          h(FolderNode, {
            key: child.path,
            node: child,
            depth: depth + 1,
            activePath: activePath,
            onSelect,
            onDrop,
            dropHint,
            onContextMenu,
            onFolderDragStart,
            onFolderDragEnd,
            draggingFolderPath,
          })
        )
      : null
  );
}
