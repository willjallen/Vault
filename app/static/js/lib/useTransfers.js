import { downloadBlob, uploadForm } from "./transferClient.js";

const { useCallback, useEffect, useRef, useState } = React;

const COMPLETE_HOLD_MS = 950;
const ERROR_HOLD_MS = 2600;
const EXIT_MS = 260;

export function useTransfers({ onUnauthorized } = {}) {
  const [transfers, setTransfers] = useState([]);
  const nextId = useRef(1);
  const timers = useRef(new Set());

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
    },
    []
  );

  const createTransfer = useCallback(
    (kind, displayName, size) => {
      const id = nextId.current;
      nextId.current += 1;
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
      return id;
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
        total: progress.total,
      });
    },
    [updateTransfer]
  );

  const failTransfer = useCallback(
    (id, err) => {
      updateTransfer(id, {
        error: err.message || "Transfer failed",
        etaSeconds: null,
        phase: "visible",
        status: "error",
      });
      removeTransfer(id, ERROR_HOLD_MS);
      if ((err.status === 401 || err.status === 0) && onUnauthorized) {
        onUnauthorized();
      }
    },
    [onUnauthorized, removeTransfer, updateTransfer]
  );

  const completeTransfer = useCallback(
    (id, result = {}) => {
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
    async ({ url, formData, name: displayName, size }) => {
      const id = createTransfer("upload", displayName || "Upload", size || null);
      try {
        const result = await uploadForm({
          fallbackTotal: size || null,
          formData,
          onProgress: (progress) => updateProgress(id, progress),
          url,
        });
        completeTransfer(id, { size: size || null });
        return result;
      } catch (err) {
        failTransfer(id, err);
        throw err;
      }
    },
    [completeTransfer, createTransfer, failTransfer, updateProgress]
  );

  const downloadWithProgress = useCallback(
    async ({ url, name: displayName, size, method, body, headers }) => {
      const id = createTransfer("download", displayName || "Download", size || null);
      try {
        const result = await downloadBlob({
          body,
          fallbackName: displayName,
          fallbackTotal: size || null,
          headers,
          method,
          onProgress: (progress) => updateProgress(id, progress),
          url,
        });
        completeTransfer(id, { size: result.size || size || null });
        return result;
      } catch (err) {
        failTransfer(id, err);
        throw err;
      }
    },
    [completeTransfer, createTransfer, failTransfer, updateProgress]
  );

  return { downloadWithProgress, transfers, uploadWithProgress };
}
