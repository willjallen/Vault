import {
  TransferCancelledError,
  downloadUrl,
  exportAndDownload,
  uploadFileResumable,
} from "./transferClient.js";

const { useCallback, useEffect, useRef, useState } = React;

const COMPLETE_HOLD_MS = 1400;
const ERROR_HOLD_MS = 2600;
const EXIT_MS = 260;

export function useTransfers({ onUnauthorized } = {}) {
  const [transfers, setTransfers] = useState([]);
  const nextId = useRef(1);
  const timers = useRef(new Set());
  const controllers = useRef(new Map());

  const schedule = useCallback((callback, delay) => {
    const timer = setTimeout(() => {
      timers.current.delete(timer);
      callback();
    }, delay);
    timers.current.add(timer);
    return timer;
  }, []);

  const removeTransfer = useCallback(
    (id, delay) => {
      schedule(() => {
        setTransfers((current) =>
          current.map((transfer) =>
            transfer.id === id ? { ...transfer, phase: "leaving" } : transfer
          )
        );
        schedule(() => {
          setTransfers((current) => current.filter((transfer) => transfer.id !== id));
        }, EXIT_MS);
      }, delay);
    },
    [schedule]
  );

  useEffect(
    () => () => {
      timers.current.forEach((timer) => clearTimeout(timer));
      timers.current.clear();
      controllers.current.forEach((controller) => controller.abort());
      controllers.current.clear();
    },
    []
  );

  const createTransfer = useCallback(
    (kind, displayName, size) => {
      const id = nextId.current;
      nextId.current += 1;
      const controller = new AbortController();
      controllers.current.set(id, controller);
      setTransfers((current) => [
        ...current,
        {
          bytesPerSecond: 0,
          etaSeconds: null,
          id,
          kind,
          loaded: 0,
          name: displayName,
          percent: size ? 0 : null,
          phase: "entering",
          size: size || null,
          stage: kind === "upload" ? "uploading" : "starting",
          status: "active",
          total: size || null,
        },
      ]);
      schedule(() => {
        setTransfers((current) =>
          current.map((transfer) =>
            transfer.id === id && transfer.phase === "entering"
              ? { ...transfer, phase: "visible" }
              : transfer
          )
        );
      }, 16);
      return { id, signal: controller.signal };
    },
    [schedule]
  );

  const updateTransfer = useCallback((id, patch) => {
    setTransfers((current) =>
      current.map((transfer) => (transfer.id === id ? { ...transfer, ...patch } : transfer))
    );
  }, []);

  const updateProgress = useCallback(
    (id, progress) => {
      updateTransfer(id, {
        bytesPerSecond: progress.bytesPerSecond,
        etaSeconds: progress.etaSeconds,
        loaded: progress.loaded,
        percent: progress.percent,
        stage: progress.stage || "transfer",
        total: progress.total,
      });
    },
    [updateTransfer]
  );

  const failTransfer = useCallback(
    (id, err) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        error: err.message || "Transfer failed",
        etaSeconds: null,
        phase: "visible",
        status: "error",
      });
      removeTransfer(id, ERROR_HOLD_MS);
      if (err.status === 401 && onUnauthorized) {
        onUnauthorized();
      }
    },
    [onUnauthorized, removeTransfer, updateTransfer]
  );

  const cancelTransfer = useCallback(
    (id) => {
      const controller = controllers.current.get(id);
      if (!controller || controller.signal.aborted) {
        return;
      }
      controller.abort();
      updateTransfer(id, {
        etaSeconds: null,
        phase: "visible",
        status: "cancelling",
      });
    },
    [updateTransfer]
  );

  const markTransferCancelled = useCallback(
    (id) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        etaSeconds: null,
        phase: "visible",
        status: "cancelled",
      });
      removeTransfer(id, 900);
    },
    [removeTransfer, updateTransfer]
  );

  const completeTransfer = useCallback(
    (id, result = {}) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        etaSeconds: null,
        loaded: result.size || result.total || null,
        phase: "completing",
        percent: 100,
        status: "complete",
        total: result.size || result.total || null,
      });
      schedule(() => {
        updateTransfer(id, { phase: "complete" });
      }, 220);
      removeTransfer(id, COMPLETE_HOLD_MS);
    },
    [removeTransfer, schedule, updateTransfer]
  );

  const uploadWithProgress = useCallback(
    async ({ file, folder, mode, documentId, note, renameToUpload, name: displayName, size }) => {
      const transfer = createTransfer(
        "upload",
        displayName || file?.name || "Upload",
        size || file?.size || null
      );
      const { id, signal } = transfer;
      try {
        const result = await uploadFileResumable({
          documentId,
          file,
          folder,
          mode,
          note,
          onProgress: (progress) => updateProgress(id, progress),
          renameToUpload,
          signal,
        });
        completeTransfer(id, { size: result.size || size || file?.size || null });
        return result;
      } catch (err) {
        if (err instanceof TransferCancelledError || err.cancelled) {
          markTransferCancelled(id);
          return { cancelled: true, status: 0 };
        }
        failTransfer(id, err);
        throw err;
      }
    },
    [completeTransfer, createTransfer, failTransfer, markTransferCancelled, updateProgress]
  );

  const downloadWithProgress = useCallback(
    async ({ url, name: displayName, size, exportPayload }) => {
      const { id, signal } = createTransfer("download", displayName || "Download", size || null);
      try {
        const result = exportPayload
          ? await exportAndDownload({
              payload: exportPayload,
              onProgress: (progress) => updateProgress(id, progress),
              signal,
            })
          : await downloadUrl({
              fallbackName: displayName || "download",
              fallbackTotal: size || null,
              onProgress: (progress) => updateProgress(id, progress),
              signal,
              url,
            });
        completeTransfer(id, { size: result.size || size || null });
        return result;
      } catch (err) {
        if (err instanceof TransferCancelledError || err.cancelled) {
          markTransferCancelled(id);
          return { cancelled: true, status: 0 };
        }
        failTransfer(id, err);
        throw err;
      }
    },
    [completeTransfer, createTransfer, failTransfer, markTransferCancelled, updateProgress]
  );

  return { cancelTransfer, downloadWithProgress, transfers, uploadWithProgress };
}
