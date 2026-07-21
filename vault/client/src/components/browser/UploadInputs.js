const h = React.createElement;

export function UploadInputs({ fileInputRef, folderInputRef, onFiles, onFolder }) {
  return h(React.Fragment, null, [
    h("input", {
      className: "hidden-input",
      key: "files",
      multiple: true,
      onChange: (changeEvent) => onFiles(changeEvent.currentTarget.files),
      ref: fileInputRef,
      type: "file",
    }),
    h("input", {
      className: "hidden-input",
      directory: "",
      key: "folder",
      multiple: true,
      onChange: (changeEvent) => onFolder(changeEvent.currentTarget.files),
      ref: folderInputRef,
      type: "file",
      webkitdirectory: "",
    }),
  ]);
}
