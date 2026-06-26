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
  refresh,
  setBusy,
  setError,
  updateDocument,
  uploadWithProgress,
  downloadWithProgress,
}) {
  function documentItem(docId) {
    return { type: "document", id: docId };
  }

  async function handleSave(docId, file, note, options = {}) {
    setBusy(true);
    setError("");
    try {
      const result = await uploadWithProgress({
        documentId: docId,
        file,
        mode: "checkin",
        name: file.name,
        note,
        renameToUpload: Boolean(options.renameToUploadedName),
        size: file.size,
      });
      if (result.cancelled) {
        return false;
      }
      await refresh();
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
    try {
      const res = await apiFetch("/api/unlock", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ items: [documentItem(docId)] }),
      });
      if (!res.ok) {
        throw new Error("Release failed");
      }
      const payload = await res.json();
      if (payload.failed?.length) {
        throw new Error(payload.failed[0].detail || "Release failed");
      }
      if (updateDocument) {
        updateDocument(docId, (item) => ({ ...item, lock: { by: null, name: null } }));
      }
    } catch (err) {
      setError(err.message || "Could not release the file.");
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
      const res = await apiFetch("/api/lock", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ items: [documentItem(doc.id)] }),
      });
      if (!res.ok) {
        const detail = await res.json().catch(() => ({}));
        throw new Error(detail.detail || "Lock failed");
      }
      const payload = await res.json();
      if (payload.failed?.length) {
        throw new Error(payload.failed[0].detail || "Lock failed");
      }
      if (updateDocument) {
        updateDocument(doc.id, (item) => ({ ...item, lock: optimisticLockFor(item, currentUser) }));
      }
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
      .then((result) => {
        if (!result.cancelled && updateDocument) {
          updateDocument(doc.id, (item) => ({
            ...item,
            lock: optimisticLockFor(item, currentUser),
          }));
        }
      })
      .catch((err) => {
        setError(err.message || "Checkout failed.");
        refresh();
      });
  }

  return { handleLock, handleRelease, handleSave, handleStartEdit, handleVersionUpload };
}
