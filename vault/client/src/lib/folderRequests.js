export function createFolderRequestOptions(folderPath) {
  const body = new URLSearchParams();
  body.set("folder", folderPath);
  return { body, method: "POST" };
}
