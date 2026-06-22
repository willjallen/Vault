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
  uploadWithProgress,
  downloadWithProgress,
}) {
  async function handleSave(docId, file, note, options = {}) {
    setBusy(true);
    setError("");
    const form = new FormData();
    form.append("file", file);
    if (note) {
      form.append("note", note);
    }
    if (options.renameToUploadedName) {
      form.append("rename_to_upload", "true");
    }
    try {
      await uploadWithProgress({
        formData: form,
        name: file.name,
        size: file.size,
        url: `/documents/${docId}/checkin`,
      });
      await refresh(folder);
      return true;
    } catch (err) {
      setError(err.message || "Save failed. Please try again.");
      return false;
    } finally {
      setBusy(false);
    }
  }

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

  async function handleVersionUpload(doc, file, options = {}) {
    if (!file) {
      return false;
    }
    if (doc.archived) {
      setError("Restore this file from Archive before replacing it.");
      return false;
    }
    const lockedByMe = doc.lock?.by === currentUser.id;
    const lockedByOther = doc.lock?.by && doc.lock.by !== currentUser.id;
    if (lockedByOther) {
      setError(`This file is checked out by ${doc.lock.name || doc.lock.by}.`);
      return false;
    }
    if (!lockedByMe) {
      const locked = await handleLock(doc);
      if (!locked) {
        return false;
      }
    }
    return handleSave(doc.id, file, "", options);
  }

  function handleStartEdit(doc) {
    if (doc.archived) {
      setError("Restore this file from Archive before editing.");
      return;
    }
    downloadWithProgress({
      name: doc.name,
      size: doc.size_bytes,
      url: `/documents/${doc.id}/checkout`,
    })
      .then(() => {
        setTimeout(() => refresh(folder), 800);
      })
      .catch((err) => {
        setError(err.message || "Checkout failed.");
        refresh(folder);
      });
    setState((prev) => ({
      ...prev,
      doc_payloads: (prev.doc_payloads || []).map((d) =>
        d.id === doc.id ? { ...d, lock: optimisticLockFor(doc, currentUser) } : d
      ),
    }));
  }

  return { handleLock, handleRelease, handleSave, handleStartEdit, handleVersionUpload };
}
