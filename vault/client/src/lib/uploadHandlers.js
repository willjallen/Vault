import { readDroppedUploadTree } from "./droppedEntries.js";
import { createFolderRequestOptions } from "./folderRequests.js";
import { uploadFileBatch, uploadFileTree } from "./uploadActions.js";

export function createUploadHandlers({
  apiFetch,
  blocked = false,
  fileScheduler,
  refresh,
  setError,
  setUploadHover,
  targetFolder = "",
  uploadInput,
  uploadWithProgress,
}) {
  async function handleUpload(files, destination = targetFolder) {
    try {
      return await uploadFileBatch({
        blocked,
        files,
        refresh,
        scheduler: fileScheduler,
        setError,
        targetFolder: destination,
        uploadWithProgress,
      });
    } catch (error) {
      setError(error.message || "Upload failed. Please try again.");
      return null;
    } finally {
      setUploadHover(false);
      if (uploadInput.current) {
        uploadInput.current.value = "";
      }
    }
  }

  async function createDroppedFolder(folderPath) {
    const response = await apiFetch(
      "/folders",
      createFolderRequestOptions(folderPath, { allowExisting: true })
    );
    if (!response.ok) {
      const detail = await response.json().catch(() => ({}));
      throw new Error(detail.detail || `Could not create folder ${folderPath}.`);
    }
    return response.json();
  }

  async function handleUploadDrop(dataTransfer, destination = targetFolder) {
    try {
      const tree = await readDroppedUploadTree(dataTransfer);
      return await uploadFileTree({
        blocked,
        createFolder: createDroppedFolder,
        refresh,
        scheduler: fileScheduler,
        setError,
        targetFolder: destination,
        tree,
        uploadWithProgress,
      });
    } catch (error) {
      setError(error.message || "Upload failed. Please try again.");
      return null;
    } finally {
      setUploadHover(false);
    }
  }

  return { handleUpload, handleUploadDrop };
}
