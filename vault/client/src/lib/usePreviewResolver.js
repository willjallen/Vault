const { useCallback, useMemo, useRef } = React;
const PREVIEW_RESOLVE_BATCH_SIZE = 100;

export function previewResolveDocuments(documents = []) {
  const seen = new Set();
  return documents.flatMap((item) => {
    const documentId = item?.id;
    const preview = item?.visual?.preview;
    const versionId = preview?.version_id;
    const key = `${documentId}:${versionId || ""}`;
    if (
      documentId === null ||
      documentId === undefined ||
      !versionId ||
      !["failed", "pending"].includes(preview?.status) ||
      seen.has(key)
    ) {
      return [];
    }
    seen.add(key);
    return [{ document_id: documentId, version_id: versionId }];
  });
}

export function mergeResolvedDocumentVisuals(documents = [], resolved = [], requested = []) {
  const requestedVersions = new Map(
    requested.map((item) => [String(item.document_id), item.version_id])
  );
  const visuals = new Map(
    resolved
      .filter((item) => item?.document_id !== null && item?.document_id !== undefined)
      .map((item) => [String(item.document_id), item.visual || null])
  );
  return documents.map((item) => {
    const key = String(item?.id);
    const requestedVersion = requestedVersions.get(key);
    if (
      !requestedVersion ||
      item?.visual?.preview?.version_id !== requestedVersion ||
      !visuals.has(key)
    ) {
      return item;
    }
    return { ...item, visual: visuals.get(key) };
  });
}

function previewRequestSignature(requested) {
  return requested
    .map((item) => `${item.document_id}:${item.version_id}`)
    .sort()
    .join("|");
}

async function fetchResolvedPreviews(apiFetch, requested) {
  const resolved = [];
  for (let index = 0; index < requested.length; index += PREVIEW_RESOLVE_BATCH_SIZE) {
    const batch = requested.slice(index, index + PREVIEW_RESOLVE_BATCH_SIZE);
    const response = await apiFetch("/api/previews/resolve", {
      body: JSON.stringify({ documents: batch }),
      headers: { "Content-Type": "application/json" },
      method: "POST",
    });
    if (!response.ok) {
      throw new Error("Could not resolve previews");
    }
    const payload = await response.json();
    resolved.push(...(Array.isArray(payload.documents) ? payload.documents : []));
  }
  return resolved;
}

function mergeResolvedIntoCache(contentsCache, requested, resolved) {
  const resolvedById = new Map(resolved.map((item) => [String(item.document_id), item]));
  requested.forEach((item) => {
    const resolvedItem = resolvedById.get(String(item.document_id));
    if (!resolvedItem) {
      return;
    }
    contentsCache.updateDocument(
      item.document_id,
      (current) => mergeResolvedDocumentVisuals([current], [resolvedItem], [item])[0]
    );
  });
}

export function usePreviewResolver({ apiFetch, contentsCache, documents, setContents }) {
  const documentsRef = useRef(documents);
  const requestPromiseRef = useRef(null);
  const resolveAgainRef = useRef(false);
  const signatureRef = useRef("");
  documentsRef.current = documents;
  const previewDocuments = useMemo(() => previewResolveDocuments(documents), [documents]);
  const previewDocumentsSignature = useMemo(
    () => previewRequestSignature(previewDocuments),
    [previewDocuments]
  );

  const resolveDisplayedPreviews = useCallback(
    async (options = {}) => {
      const requested = previewResolveDocuments(documentsRef.current);
      const signature = previewRequestSignature(requested);
      if (!signature) {
        signatureRef.current = "";
        return null;
      }
      if (requestPromiseRef.current) {
        if (options.force || signature !== signatureRef.current) {
          resolveAgainRef.current = true;
        }
        return requestPromiseRef.current;
      }
      if (!options.force && signature === signatureRef.current) {
        return null;
      }

      signatureRef.current = signature;
      const request = fetchResolvedPreviews(apiFetch, requested).then((resolved) => {
        mergeResolvedIntoCache(contentsCache, requested, resolved);
        setContents((current) => ({
          ...current,
          documents: mergeResolvedDocumentVisuals(current.documents, resolved, requested),
        }));
        return resolved;
      });
      requestPromiseRef.current = request;
      try {
        return await request;
      } catch (_error) {
        if (signatureRef.current === signature) {
          signatureRef.current = "";
        }
        return null;
      } finally {
        requestPromiseRef.current = null;
        if (resolveAgainRef.current) {
          resolveAgainRef.current = false;
          Promise.resolve().then(() => resolveDisplayedPreviews({ force: true }));
        }
      }
    },
    [apiFetch, contentsCache, setContents]
  );

  return { previewDocumentsSignature, resolveDisplayedPreviews };
}
