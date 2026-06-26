export function fileRenamePrefixSelectionEnd(fileNameValue) {
  const fileName = String(fileNameValue ?? "");
  const extensionStart = fileName.lastIndexOf(".");
  if (extensionStart <= 0 || extensionStart === fileName.length - 1) {
    return fileName.length;
  }
  return extensionStart;
}

export function selectFileRenamePrefix(input) {
  if (!input) {
    return;
  }
  const selectionEnd = fileRenamePrefixSelectionEnd(input.value);
  if (typeof input.setSelectionRange === "function") {
    input.setSelectionRange(0, selectionEnd);
    return;
  }
  input.select?.();
}
