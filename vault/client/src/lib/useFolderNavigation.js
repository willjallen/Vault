import { folderParent, toBreadcrumbs } from "./utils.js";
import { useMouseNavigation } from "./useMouseNavigation.js";

const { useMemo } = React;

export function useFolderNavigation({
  closeContextMenu,
  folder,
  historyModeRef,
  setContentsAnchor,
  setContentsSelection,
  setFolder,
  setFolderBackStack,
  setFolderForwardStack,
  setSelectedId,
  folderBackStack,
  folderForwardStack,
}) {
  const breadcrumbs = useMemo(() => toBreadcrumbs(folder || ""), [folder]);
  const canGoBack = folderBackStack.length > 0;
  const canGoForward = folderForwardStack.length > 0;
  const canGoUp = Boolean(folder);

  function navigateToFolder(nextFolder) {
    const normalized = nextFolder || "";
    if (normalized === folder) {
      return;
    }
    historyModeRef.current = "push";
    setFolderBackStack((prev) => [...prev, folder || ""]);
    setFolderForwardStack([]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(normalized);
    closeContextMenu();
  }

  function replaceFolder(nextFolder) {
    const normalized = nextFolder || "";
    if (normalized === folder) {
      return;
    }
    historyModeRef.current = "replace";
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(normalized);
    closeContextMenu();
  }

  function navigateBack() {
    if (!folderBackStack.length) {
      return;
    }
    const nextFolder = folderBackStack[folderBackStack.length - 1] || "";
    historyModeRef.current = "replace";
    setFolderBackStack(folderBackStack.slice(0, -1));
    setFolderForwardStack([folder || "", ...folderForwardStack]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(nextFolder);
    closeContextMenu();
  }

  function navigateForward() {
    if (!folderForwardStack.length) {
      return;
    }
    const nextFolder = folderForwardStack[0] || "";
    historyModeRef.current = "replace";
    setFolderForwardStack(folderForwardStack.slice(1));
    setFolderBackStack([...folderBackStack, folder || ""]);
    setSelectedId(null);
    setContentsSelection([]);
    setContentsAnchor(null);
    setFolder(nextFolder);
    closeContextMenu();
  }

  function navigateUp() {
    if (!folder) {
      return;
    }
    navigateToFolder(folderParent(folder));
  }

  useMouseNavigation({ onBack: navigateBack, onForward: navigateForward });

  return {
    breadcrumbs,
    canGoBack,
    canGoForward,
    canGoUp,
    navigateBack,
    navigateForward,
    navigateToFolder,
    navigateUp,
    replaceFolder,
  };
}
