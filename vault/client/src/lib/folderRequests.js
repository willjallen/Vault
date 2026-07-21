export function createFolderRequestOptions(folderPath, { allowExisting = false } = {}) {
  const body = new URLSearchParams();
  body.set("folder", folderPath);
  if (allowExisting) {
    body.set("exist_ok", "true");
  }
  return { body, method: "POST" };
}
