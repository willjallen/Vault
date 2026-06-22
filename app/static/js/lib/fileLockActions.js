import { triggerDownload } from "./utils.js";

function optimisticLockFor(doc, currentUser) {
  return {
    by: currentUser.id,
    name: currentUser.name,
    at: new Date().toISOString(),
    ip: doc.lock?.ip || null,
    user_agent: doc.lock?.user_agent || null,
    force_acquired: false,
  };
}

export function createFileLockActions({
  apiFetch,
  currentUser,
  folder,
  refresh,
  setBusy,
  setError,
  setState,
}) {
  async function handleRelease(docId) {
    setBusy(true);
    setError("");
    const form = new FormData();
    try {
      const res = await apiFetch(`/documents/${docId}/release?mode=json`, {
        method: "POST",
        body: form,
      });
      if (!res.ok) {
        throw new Error("Release failed");
      }
      await refresh(folder);
    } catch {
      setError("Could not release the file.");
    } finally {
      setBusy(false);
    }
  }

  async function handleLock(doc) {
    if (doc.archived) {
      setError("Restore this file from Archive before editing.");
      return false;
    }
    setBusy(true);
    setError("");
    try {
      const res = await apiFetch(`/documents/${doc.id}/lock`, { method: "POST" });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Lock failed");
      }
      await refresh(folder);
      return true;
    } catch (err) {
      setError(err.message || "Could not lock the file.");
      return false;
    } finally {
      setBusy(false);
    }
  }

  function handleStartEdit(doc) {
    if (doc.archived) {
      setError("Restore this file from Archive before editing.");
      return;
    }
    triggerDownload(`/documents/${doc.id}/checkout`);
    setState((prev) => ({
      ...prev,
      doc_payloads: (prev.doc_payloads || []).map((d) =>
        d.id === doc.id ? { ...d, lock: optimisticLockFor(doc, currentUser) } : d
      ),
    }));
    setTimeout(() => refresh(folder), 800);
  }

  return { handleLock, handleRelease, handleStartEdit };
}
