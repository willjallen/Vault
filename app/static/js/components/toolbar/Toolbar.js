import { Breadcrumbs } from "../common/Breadcrumbs.js";

const h = React.createElement;

export function Toolbar({
  folder,
  breadcrumbs,
  onStartAddingFolder,
  _busy,
  uploading,
  uploadInputRef,
  onUploadFile,
  onTriggerUpload,
  onSelectFolder,
  onDropOnFolder,
  onClearDrop,
}) {
  return h(
    "div",
    { className: "finder-toolbar" },
    h(Breadcrumbs, {
      breadcrumbs,
      activePath: folder,
      onSelect: onSelectFolder,
      onDropOnFolder,
      onClearDrop,
    }),
    h(
      "div",
      { className: "toolbar-actions" },
      h(
        "button",
        { className: "btn secondary", type: "button", onClick: onStartAddingFolder },
        "New folder"
      ),
      h(
        "div",
        { className: "upload-control" },
        h("input", {
          type: "file",
          ref: uploadInputRef,
          className: "hidden-input",
          onChange: (e) => onUploadFile(e.target.files[0]),
        }),
        h(
          "button",
          {
            className: "btn primary",
            type: "button",
            disabled: uploading,
            onClick: onTriggerUpload,
          },
          uploading ? "Uploading..." : "Upload"
        )
      )
    )
  );
}
