import { filePreview } from "./filePreview.js";

const { useCallback, useMemo, useState } = React;

export function useFileDialogs({ closeContextMenu, docs, onDownload, selectedDoc, setSelectedId }) {
  const [fileDetailsTarget, setFileDetailsTarget] = useState(null);
  const [filePreviewTarget, setFilePreviewTarget] = useState(null);
  const closeFileDetails = useCallback(() => setFileDetailsTarget(null), []);
  const closeFilePreview = useCallback(() => setFilePreviewTarget(null), []);
  const activeFileDetailsDoc = useMemo(() => {
    if (!fileDetailsTarget) {
      return null;
    }
    if (selectedDoc?.id === fileDetailsTarget.id) {
      return selectedDoc;
    }
    return docs.find((doc) => doc.id === fileDetailsTarget.id) || fileDetailsTarget;
  }, [docs, fileDetailsTarget, selectedDoc]);

  function openFileDetails(doc) {
    setFileDetailsTarget(doc);
    setSelectedId(doc.id);
    closeContextMenu();
  }

  function handleOpenFile(doc) {
    if (filePreview(doc)) {
      closeContextMenu();
      setFilePreviewTarget(doc);
      return;
    }
    return onDownload(doc);
  }

  return {
    activeFileDetailsDoc,
    closeFileDetails,
    closeFilePreview,
    filePreviewTarget,
    handleOpenFile,
    openFileDetails,
  };
}
