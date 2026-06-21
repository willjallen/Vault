import { Breadcrumbs } from "../common/Breadcrumbs.js";

const h = React.createElement;

export function Toolbar({
  folder,
  breadcrumbs,
  canGoBack,
  canGoForward,
  canGoUp,
  onNavigateBack,
  onNavigateForward,
  onNavigateUp,
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
    h("div", { className: "toolbar-navigation" }, [
      h(
        "button",
        {
          "aria-label": "Back",
          className: "btn ghost nav-button",
          disabled: !canGoBack,
          onClick: onNavigateBack,
          title: "Back",
          type: "button",
        },
        "‹"
      ),
      h(
        "button",
        {
          "aria-label": "Forward",
          className: "btn ghost nav-button",
          disabled: !canGoForward,
          onClick: onNavigateForward,
          title: "Forward",
          type: "button",
        },
        "›"
      ),
      h(
        "button",
        {
          "aria-label": "Up",
          className: "btn ghost nav-button",
          disabled: !canGoUp,
          onClick: onNavigateUp,
          title: "Up",
          type: "button",
        },
        "↑"
      ),
      h(Breadcrumbs, {
        breadcrumbs,
        activePath: folder,
        onSelect: onSelectFolder,
        onDropOnFolder,
        onClearDrop,
      }),
    ]),
    h(
      "div",
      { className: "toolbar-actions" },
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
