import { classNames, isArchivePath } from "../../lib/utils.js";

const { useEffect, useMemo, useState } = React;
const h = React.createElement;

function buildTree(childrenMap, nodePath, predicate) {
  const children = Object.prototype.hasOwnProperty.call(childrenMap, nodePath)
    ? // eslint-disable-next-line security/detect-object-injection
      childrenMap[nodePath].filter(predicate)
    : [];
  return children.map((childPath) => ({
    name: childPath.split("/").filter(Boolean).slice(-1)[0] || "Vault",
    path: childPath,
    children: buildTree(childrenMap, childPath, predicate),
  }));
}

function FolderTreeNode({
  node,
  depth,
  activePath,
  dropHint,
  onSelect,
  onDropOnFolder,
  onClearDropHint,
}) {
  const hasChildren = (node.children || []).length > 0;
  const normalizedActive = activePath || "";
  const [expanded, setExpanded] = useState(() => {
    if (!hasChildren) {
      return false;
    }
    if (!node.path) {
      return true;
    }
    return normalizedActive === node.path || normalizedActive.startsWith(`${node.path}/`);
  });

  useEffect(() => {
    if (!hasChildren) {
      return;
    }
    const shouldOpen =
      normalizedActive === node.path || normalizedActive.startsWith(`${node.path}/`);
    if (shouldOpen) {
      setExpanded(true);
    }
  }, [hasChildren, normalizedActive, node.path]);

  const isActive = (node.path || "") === normalizedActive;
  const isDropTarget = dropHint === node.path || (!node.path && dropHint === "");
  const isArchived = isArchivePath(node.path);
  const hasExpandableChildren = hasChildren;
  const glyph = isArchived ? "🗂" : "📁";

  return h(
    "div",
    { className: "tree-node" },
    h(
      "button",
      {
        className: classNames(
          "folder-node",
          isActive ? "active" : "",
          isDropTarget ? "drop-target" : "",
          isArchived ? "archived" : ""
        ),
        type: "button",
        style: { paddingLeft: `${12 + depth * 12}px` },
        onClick: () => onSelect(node.path),
        onDragEnter: (e) => onDropOnFolder(node.path, e, true),
        onDragOver: (e) => e.preventDefault(),
        onDrop: (e) => onDropOnFolder(node.path, e, false),
        onDragLeave: (e) => {
          if (!e.currentTarget.contains(e.relatedTarget)) {
            onClearDropHint();
          }
        },
      },
      [
        hasExpandableChildren
          ? h(
              "span",
              {
                className: classNames("tree-caret", expanded ? "open" : ""),
                onClick: (e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  setExpanded((prev) => !prev);
                },
              },
              "›"
            )
          : h("span", { className: "tree-caret placeholder" }, ""),
        h("span", { className: "folder-glyph" }, glyph),
        h("span", { className: "folder-name" }, node.name || "Folder"),
      ]
    ),
    hasChildren && expanded
      ? h(
          "div",
          { className: "tree-children" },
          node.children.map((child) =>
            h(FolderTreeNode, {
              key: child.path || child.name,
              node: child,
              depth: depth + 1,
              activePath,
              dropHint,
              onSelect,
              onDropOnFolder,
              onClearDropHint,
            })
          )
        )
      : null
  );
}

export function SidebarNav({
  currentFolder,
  folderChildren,
  dropHint,
  onSelect,
  onDropOnFolder,
  onClearDropHint,
}) {
  const vaultTree = useMemo(
    () => buildTree(folderChildren || {}, "", (child) => !isArchivePath(child)),
    [folderChildren]
  );
  const archiveTree = useMemo(
    () => buildTree(folderChildren || {}, "Archive", (child) => isArchivePath(child)),
    [folderChildren]
  );

  return h("div", null, [
    h("div", { className: "sidebar-section" }, [
      h("p", { className: "eyebrow tiny" }, "Folders"),
      h(FolderTreeNode, {
        node: { name: "Vault", path: "", children: vaultTree },
        depth: 0,
        activePath: currentFolder || "",
        dropHint,
        onSelect,
        onDropOnFolder,
        onClearDropHint,
      }),
    ]),
    h("div", { className: "sidebar-section" }, [
      h("p", { className: "eyebrow tiny" }, "Archive"),
      h(FolderTreeNode, {
        node: { name: "Archive", path: "Archive", children: archiveTree },
        depth: 0,
        activePath: currentFolder || "",
        dropHint,
        onSelect,
        onDropOnFolder,
        onClearDropHint,
      }),
      h("p", { className: "muted tiny quiet-text" }, "Restore from here or delete forever."),
    ]),
  ]);
}
