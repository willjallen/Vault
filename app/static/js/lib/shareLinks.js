import { normalizeFolderName } from "./utils.js";
import { readLocalPreference, writeLocalPreference } from "./localPreferences.js";

const { useCallback, useEffect } = React;

export function shareCodeFromLocation() {
  const match = window.location.pathname.match(/^\/s\/([^/]+)\/?$/);
  return match ? decodeURIComponent(match[1]) : "";
}

function readStoredFolder(fallback = "") {
  return normalizeFolderName(readLocalPreference("lastFolder", fallback) || fallback);
}

function writeStoredFolder(folder) {
  writeLocalPreference("lastFolder", folder || "");
}

function writeClipboard(text) {
  function writeWithTextarea() {
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "readonly");
    textarea.style.position = "fixed";
    textarea.style.left = "-9999px";
    document.body.appendChild(textarea);
    textarea.select();
    document.execCommand("copy");
    textarea.remove();
  }

  if (navigator.clipboard?.writeText) {
    return navigator.clipboard.writeText(text).catch(() => {
      writeWithTextarea();
    });
  }
  writeWithTextarea();
  return Promise.resolve();
}

export function initialFolderForApp(fallback, shareCode) {
  return shareCode ? fallback || "" : readStoredFolder(fallback || "");
}

export function useFolderHistory({
  closeContextMenu,
  folder,
  historyModeRef,
  setContentsAnchor,
  setContentsSelection,
  setFolder,
  setFolderAnchor,
  setFolderBackStack,
  setFolderForwardStack,
  setFolderSelection,
  setSelectedId,
  shareResolving,
}) {
  useEffect(() => {
    if (shareResolving) {
      return;
    }
    writeStoredFolder(folder);
    const mode = historyModeRef.current || "push";
    historyModeRef.current = "push";
    if (mode === "none") {
      return;
    }
    const state = { vault: true, folder: folder || "" };
    if (mode === "replace" || !window.history.state?.vault) {
      window.history.replaceState(state, "", "/");
      return;
    }
    window.history.pushState(state, "", "/");
  }, [folder, historyModeRef, shareResolving]);

  useEffect(() => {
    function handlePopState(evt) {
      const nextFolder = evt.state?.vault ? evt.state.folder || "" : readStoredFolder("");
      historyModeRef.current = "none";
      setFolderBackStack([]);
      setFolderForwardStack([]);
      setSelectedId(null);
      setContentsSelection([]);
      setContentsAnchor(null);
      setFolderSelection([]);
      setFolderAnchor(null);
      setFolder(nextFolder);
      closeContextMenu();
    }
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, [
    closeContextMenu,
    historyModeRef,
    setContentsAnchor,
    setContentsSelection,
    setFolder,
    setFolderAnchor,
    setFolderBackStack,
    setFolderForwardStack,
    setFolderSelection,
    setSelectedId,
  ]);
}

export function useShareLinkResolution({
  apiFetch,
  historyModeRef,
  setContentsAnchor,
  setContentsSelection,
  setFolder,
  setFolderAnchor,
  setFolderBackStack,
  setFolderForwardStack,
  setFolderSelection,
  setRecursiveSearch,
  setSearchQuery,
  setSelectedId,
  setShareResolving,
  shareCodeRef,
  showToast,
}) {
  useEffect(() => {
    const code = shareCodeRef.current;
    if (!code) {
      return undefined;
    }
    let cancelled = false;

    async function resolveShareLink() {
      try {
        const res = await apiFetch(`/api/share-links/${encodeURIComponent(code)}`);
        if (!res.ok) {
          const errorPayload = await res.json().catch(() => ({}));
          throw new Error(errorPayload.detail || "Share link not found.");
        }
        const resolved = await res.json();
        if (cancelled) {
          return;
        }
        const targetFolder = normalizeFolderName(resolved.folder || "");
        historyModeRef.current = "replace";
        setFolderBackStack([]);
        setFolderForwardStack([]);
        setFolderSelection([]);
        setFolderAnchor(null);
        setSearchQuery("");
        setRecursiveSearch(false);
        setFolder(targetFolder);
        if (resolved.target_type === "document" && resolved.document_id) {
          const key = `document:${resolved.document_id}`;
          setSelectedId(resolved.document_id);
          setContentsSelection([key]);
          setContentsAnchor(key);
        } else {
          setSelectedId(null);
          setContentsSelection([]);
          setContentsAnchor(null);
        }
      } catch (err) {
        if (!cancelled) {
          showToast(err.message || "Share link not found.");
        }
      } finally {
        if (!cancelled) {
          shareCodeRef.current = "";
          setShareResolving(false);
        }
      }
    }

    resolveShareLink();
    return () => {
      cancelled = true;
    };
  }, [
    apiFetch,
    historyModeRef,
    setContentsAnchor,
    setContentsSelection,
    setFolder,
    setFolderAnchor,
    setFolderBackStack,
    setFolderForwardStack,
    setFolderSelection,
    setRecursiveSearch,
    setSearchQuery,
    setSelectedId,
    setShareResolving,
    shareCodeRef,
    showToast,
  ]);
}

export function useShareActions({ apiFetch, setError, showToast }) {
  return useCallback(
    async (item) => {
      if (!item) {
        return;
      }
      setError("");
      try {
        const body =
          item.type === "document"
            ? { target_type: "document", document_id: item.id }
            : { target_type: "folder", path: item.path || "" };
        const res = await apiFetch("/api/share-links", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        });
        const payload = await res.json().catch(() => ({}));
        if (!res.ok) {
          throw new Error(payload.detail || "Could not create share link.");
        }
        await writeClipboard(payload.url);
        showToast("Share link copied");
      } catch (err) {
        setError(err.message || "Could not create share link.");
      }
    },
    [apiFetch, setError, showToast]
  );
}
