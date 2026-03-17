import { classNames, isArchivePath, toBreadcrumbs } from "../../lib/utils.js";

const { useMemo, useState } = React;
const h = React.createElement;

function BreadcrumbLine({ crumbs, onSelect }) {
  return h(
    "div",
    { className: "crumbs-list compact modal-crumbs" },
    crumbs.map((crumb, idx) =>
      h(
        React.Fragment,
        { key: `${crumb.path || "root"}-${idx}` },
        h(
          "button",
          {
            type: "button",
            className: classNames(
              "crumb",
              isArchivePath(crumb.path) ? "archived" : "",
              crumb.active ? "active" : ""
            ),
            onClick: () => onSelect && onSelect(crumb.path),
          },
          crumb.name
        ),
        idx < crumbs.length - 1 ? h("span", { className: "slash" }, "/") : null
      )
    )
  );
}

function DestinationList({ items, onSelect }) {
  return h(
    "div",
    { className: "destination-list" },
    items.map((folderItem) =>
      h(
        "button",
        {
          key: folderItem.path || folderItem.name,
          type: "button",
          className: classNames("destination-row", folderItem.blocked ? "disabled" : ""),
          onClick: () => !folderItem.blocked && onSelect(folderItem.path),
        },
        [
          h("span", { className: "folder-glyph" }, "📁"),
          h("span", { className: "folder-name" }, folderItem.name),
          h(
            "span",
            { className: "muted tiny" },
            folderItem.blocked ? "Can't move a folder into itself" : "Open"
          ),
        ]
      )
    )
  );
}

function NewFolderRow({ value, creating, hasName, onChange, onCreate }) {
  return h("div", { className: "new-folder-inline move-new-folder" }, [
    h("input", {
      type: "text",
      value,
      placeholder: "New folder name",
      onChange: (e) => onChange && onChange(e.target.value),
      disabled: creating,
      onKeyDown: (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          if (onCreate && hasName) {
            onCreate();
          }
        }
      },
    }),
    h(
      "button",
      {
        className: "btn secondary",
        type: "button",
        onClick: () => onCreate && onCreate(),
        disabled: creating || !hasName,
      },
      creating ? "Creating..." : "Create"
    ),
  ]);
}

function ModalFooter({ crumbs, note, onClose, onConfirm, confirmDisabled, cancelDisabled }) {
  return h("div", { className: "modal-actions move-footer" }, [
    h("div", { className: "footer-destination" }, [
      note ? h("p", { className: "muted tiny quiet-text" }, note) : null,
      !note && crumbs.length
        ? h("div", { className: "footer-crumbs" }, [
            h("span", { className: "muted tiny quiet-text" }, "Will move to"),
            h(BreadcrumbLine, {
              crumbs,
            }),
          ])
        : null,
    ]),
    h("div", { className: "modal-actions-buttons" }, [
      h(
        "button",
        {
          className: "btn ghost",
          type: "button",
          onClick: onClose,
          disabled: cancelDisabled,
        },
        "Cancel"
      ),
      h(
        "button",
        {
          className: "btn primary",
          type: "button",
          onClick: onConfirm,
          disabled: confirmDisabled,
        },
        "Move here"
      ),
    ]),
  ]);
}

const EMPTY_TARGET = { type: "doc", name: "", path: "", folder: "", archived: false };

// eslint-disable-next-line complexity
export function MoveDialog({
  target,
  destination,
  folderChildren,
  newFolderName,
  creatingFolder,
  onDestinationChange,
  onClose,
  onConfirm,
  onCreateFolder,
  onNewFolderNameChange,
}) {
  const [showNewFolder, setShowNewFolder] = useState(Boolean(newFolderName));
  const safeTarget = target || EMPTY_TARGET;
  const movingInArchive =
    safeTarget.archived || isArchivePath(safeTarget.path || safeTarget.folder || "");
  const destinationPath = destination || (movingInArchive ? "Archive" : "");
  const targetFolder = safeTarget.type === "folder" ? safeTarget.path : safeTarget.folder || "";
  const targetBaseName =
    safeTarget.type === "folder"
      ? target.name || targetFolder.split("/").filter(Boolean).slice(-1)[0] || "Folder"
      : target?.name || (safeTarget.path || "").split("/").filter(Boolean).slice(-1)[0] || "File";

  const destinationCrumbs = toBreadcrumbs(destinationPath);
  const currentCrumbs = toBreadcrumbs(
    safeTarget.type === "folder"
      ? targetFolder
      : safeTarget.folder || (movingInArchive ? "Archive" : "")
  );

  const childFolders = useMemo(() => {
    const list =
      folderChildren &&
      Object.prototype.hasOwnProperty.call(folderChildren, destinationPath) &&
      // eslint-disable-next-line security/detect-object-injection
      Array.isArray(folderChildren[destinationPath])
        ? // eslint-disable-next-line security/detect-object-injection
          folderChildren[destinationPath]
        : [];
    return list
      .filter((path) => isArchivePath(path) === movingInArchive)
      .map((path) => ({
        path,
        name:
          path.split("/").filter(Boolean).slice(-1)[0] || (movingInArchive ? "Archive" : "Vault"),
        blocked:
          safeTarget.type === "folder" &&
          (path === targetFolder || path.startsWith(`${targetFolder}/`)),
      }));
  }, [destinationPath, folderChildren, movingInArchive, safeTarget.type, targetFolder]);

  const finalPath = destinationPath ? `${destinationPath}/${targetBaseName}` : targetBaseName;
  const invalidDestination =
    safeTarget.type === "folder" &&
    (finalPath === targetFolder || finalPath.startsWith(`${targetFolder}/`));
  const noOp =
    safeTarget.type === "doc" ? finalPath === safeTarget.path : finalPath === targetFolder;
  const hasNewFolderName = Boolean((newFolderName || "").trim());

  const headerNote = movingInArchive
    ? "Moving within Archive. Use Restore to send items back to Vault."
    : "Choose a new folder inside your Vault.";
  const footerHint = invalidDestination
    ? "Pick a folder outside the one you're moving."
    : noOp
      ? "Choose a new destination to move this item."
      : "";

  if (!target) {
    return null;
  }

  return h(
    "div",
    { className: "modal-backdrop" },
    h(
      "div",
      {
        className: "modal move-modal",
        role: "dialog",
        "aria-modal": "true",
      },
      [
        h("div", { className: "modal-head" }, [
          h("h3", null, `Move "${targetBaseName}"`),
          h("p", { className: "muted small" }, headerNote),
          h(
            "p",
            { className: "muted tiny quiet-text current-location" },
            `Current location: ${
              currentCrumbs.map((crumb) => crumb.name).join(" / ") ||
              (movingInArchive ? "Archive" : "Vault")
            }`
          ),
        ]),
        h("div", { className: "modal-body" }, [
          h("div", { className: "move-summary destination-card" }, [
            h("p", { className: "muted tiny summary-label" }, "Destination folder"),
            h(BreadcrumbLine, {
              crumbs: destinationCrumbs.map((crumb) => ({
                ...crumb,
                active: crumb.path === destinationPath,
              })),
              onSelect: onDestinationChange,
            }),
          ]),
          h("div", { className: "move-destination" }, [
            h("div", { className: "destination-header" }, [
              h("p", { className: "section-title" }, "Choose destination folder"),
            ]),
            childFolders.length
              ? h(DestinationList, { items: childFolders, onSelect: onDestinationChange })
              : h("div", { className: "empty-destination" }, [
                  h(
                    "p",
                    { className: "muted small quiet-text" },
                    "This folder has no subfolders yet. You can move the file here or create a new folder."
                  ),
                ]),
          ]),
          h(
            "div",
            { className: "new-folder-area" },
            showNewFolder
              ? [
                  h(
                    "p",
                    { className: "muted tiny quiet-text" },
                    "Create a new folder in this location."
                  ),
                  h(NewFolderRow, {
                    value: newFolderName,
                    creating: creatingFolder,
                    hasName: hasNewFolderName,
                    onChange: onNewFolderNameChange,
                    onCreate: onCreateFolder,
                  }),
                ]
              : h(
                  "button",
                  {
                    type: "button",
                    className: "new-folder-toggle",
                    onClick: () => setShowNewFolder(true),
                    disabled: creatingFolder,
                  },
                  "+ New folder here"
                )
          ),
        ]),
        h(ModalFooter, {
          crumbs: destinationCrumbs.map((crumb) => ({
            ...crumb,
            active: crumb.path === destinationPath,
          })),
          note: footerHint,
          onClose,
          onConfirm,
          confirmDisabled: creatingFolder || invalidDestination || noOp,
          cancelDisabled: creatingFolder,
        }),
      ]
    )
  );
}
