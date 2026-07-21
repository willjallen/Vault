import { readDroppedUploadTree, readSelectedUploadTree } from "./droppedEntries.js";
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
  uploadFolderInput,
  uploadInput,
  uploadWithProgress,
}) {
  function resetInput(inputRef) {
    if (inputRef?.current) {
      inputRef.current.value = "";
    }
  }

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
      resetInput(uploadInput);
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

  async function handleUploadTree(readTree, destination) {
    try {
      const tree = await readTree();
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

  function handleUploadDrop(dataTransfer, destination = targetFolder) {
    return handleUploadTree(() => readDroppedUploadTree(dataTransfer), destination);
  }

  async function handleUploadFolder(files, destination = targetFolder) {
    try {
      return await handleUploadTree(() => readSelectedUploadTree(files), destination);
    } finally {
      resetInput(uploadFolderInput);
    }
  }

  return { handleUpload, handleUploadDrop, handleUploadFolder };
}
