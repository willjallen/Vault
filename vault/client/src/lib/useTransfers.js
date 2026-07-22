import {
  TransferCancelledError,
  downloadUrl,
  exportAndDownload,
  uploadFileResumable,
} from "./transferClient.js";
import * as browserDownload from "./browserDownload.js";
import { confirmNativeDownload } from "./downloadGuidance.js";
import { createUploadOperation, describeUploadOperation } from "./uploadOperation.js";

const { useCallback, useEffect, useRef, useState } = React;

const COMPLETE_HOLD_MS = 1400;
const ERROR_HOLD_MS = 10_000;
const EXIT_MS = 260;

function operationSummaryPatch(summary = {}) {
  return {
    cancelledItems: summary.cancelled || 0,
    failedItems: summary.failed || 0,
    processedItems: summary.processedItems ?? null,
    succeededItems: summary.succeeded || 0,
    totalFiles: summary.totalFiles ?? null,
    totalFolders: summary.totalFolders ?? null,
    totalItems: summary.totalItems ?? null,
  };
}

export function useTransfers({
  customDownloadsEnabled = false,
  downloadLocationGuidanceDismissed = false,
  onUnauthorized,
  requestConfirm,
  saveDownloadLocationGuidanceDismissed,
} = {}) {
  const [transfers, setTransfers] = useState([]);
  const [guidanceDismissed, setGuidanceDismissed] = useState(
    downloadLocationGuidanceDismissed === true
  );
  const nextId = useRef(1);
  const timers = useRef(new Set());
  const controllers = useRef(new Map());
  const filePickerDownloadsAvailable =
    browserDownload.canUseFileSystemDownloadWriter(customDownloadsEnabled);

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
    (kind, displayName, size, details = {}) => {
      const id = nextId.current;
      nextId.current += 1;
      const controller = new AbortController();
      controllers.current.set(id, controller);
      setTransfers((current) => [
        ...current,
        {
          bytesPerSecond: 0,
          createdAt: null,
          ...details,
          etaSeconds: null,
          id,
          kind,
          loaded: 0,
          name: displayName,
          noProgressSeconds: 0,
          percent: size ? 0 : null,
          phase: "entering",
          size: size || null,
          serverStatus: null,
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
      return { abort: () => controller.abort(), id, signal: controller.signal };
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
        createdAt: progress.createdAt || null,
        etaSeconds: progress.etaSeconds,
        loaded: progress.loaded,
        noProgressSeconds: progress.noProgressSeconds || 0,
        percent: progress.percent,
        ...(progress.processedItems === undefined
          ? {}
          : { processedItems: progress.processedItems ?? null }),
        resumedBytes: progress.resumedBytes || null,
        serverStatus: progress.serverStatus || null,
        stage: progress.stage || "transfer",
        total: progress.total,
        ...(progress.totalItems === undefined ? {} : { totalItems: progress.totalItems ?? null }),
        updatedAt: progress.updatedAt || null,
      });
    },
    [updateTransfer]
  );

  const failTransfer = useCallback(
    (id, err, patch = {}) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        ...patch,
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
    (id, patch = {}) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        ...patch,
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
        ...(result.processedItems === undefined
          ? {}
          : { processedItems: result.processedItems ?? null }),
        phase: "completing",
        percent: 100,
        status: "complete",
        total: result.size || result.total || null,
        ...(result.totalFiles === undefined ? {} : { totalFiles: result.totalFiles ?? null }),
        ...(result.totalFolders === undefined ? {} : { totalFolders: result.totalFolders ?? null }),
        ...(result.totalItems === undefined ? {} : { totalItems: result.totalItems ?? null }),
      });
      schedule(() => {
        updateTransfer(id, { phase: "complete" });
      }, 220);
      removeTransfer(id, COMPLETE_HOLD_MS);
    },
    [removeTransfer, schedule, updateTransfer]
  );

  const handoffDownload = useCallback(
    (id) => {
      controllers.current.delete(id);
      updateTransfer(id, {
        bytesPerSecond: 0,
        etaSeconds: null,
        loaded: 0,
        percent: null,
        phase: "visible",
        stage: "browser-handoff",
        status: "browser-managed",
      });
      removeTransfer(id, ERROR_HOLD_MS);
    },
    [removeTransfer, updateTransfer]
  );

  const dismissDownloadLocationGuidance = useCallback(async () => {
    if (saveDownloadLocationGuidanceDismissed) {
      await saveDownloadLocationGuidanceDismissed();
    }
    setGuidanceDismissed(true);
  }, [saveDownloadLocationGuidanceDismissed]);

  const beginUploadOperation = useCallback(
    (metadata = {}) => {
      const descriptor = describeUploadOperation(metadata);
      const { abort, id, signal } = createTransfer(
        "upload",
        descriptor.name,
        descriptor.totalBytes,
        descriptor
      );
      return createUploadOperation({
        abort,
        descriptor,
        isCancellation: (error) => error instanceof TransferCancelledError || error?.cancelled,
        onCancelled: (summary) => markTransferCancelled(id, operationSummaryPatch(summary)),
        onComplete: (summary) =>
          completeTransfer(id, {
            ...operationSummaryPatch(summary),
            size: descriptor.totalBytes,
          }),
        onError: (error, summary) => failTransfer(id, error, operationSummaryPatch(summary)),
        onProgress: (progress) => updateTransfer(id, progress),
        runUpload: uploadFileResumable,
        signal,
      });
    },
    [completeTransfer, createTransfer, failTransfer, markTransferCancelled, updateTransfer]
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
    async ({ url, name: displayName, size, exportPayload, prepare }) => {
      if (!filePickerDownloadsAvailable) {
        const confirmed = await confirmNativeDownload({
          dismissed: guidanceDismissed,
          onDismiss: dismissDownloadLocationGuidance,
          requestConfirm,
        });
        if (!confirmed) {
          return { cancelled: true, status: 0 };
        }
      }
      const exportItems = Array.from(exportPayload?.items || []);
      const { id, signal } = createTransfer("download", displayName || "Download", size || null, {
        grouped: Boolean(exportPayload),
        totalFiles: exportItems.filter((item) => item?.type === "document").length || null,
        totalFolders: exportItems.filter((item) => item?.type === "folder").length || null,
        totalItems: exportItems.length || null,
      });
      try {
        const result = exportPayload
          ? await exportAndDownload({
              customDownloadsEnabled,
              payload: exportPayload,
              onProgress: (progress) => updateProgress(id, progress),
              signal,
              suggestedName: displayName || "vault-download.zip",
            })
          : await downloadUrl({
              customDownloadsEnabled,
              fallbackName: displayName || "download",
              fallbackTotal: size || null,
              onProgress: (progress) => updateProgress(id, progress),
              prepare,
              signal,
              url,
            });
        if (result.browserManaged) {
          handoffDownload(id);
        } else {
          completeTransfer(id, { size: result.size || size || null });
        }
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
    [
      completeTransfer,
      createTransfer,
      customDownloadsEnabled,
      dismissDownloadLocationGuidance,
      filePickerDownloadsAvailable,
      failTransfer,
      guidanceDismissed,
      handoffDownload,
      markTransferCancelled,
      requestConfirm,
      updateProgress,
    ]
  );

  return {
    beginUploadOperation,
    cancelTransfer,
    downloadWithProgress,
    transfers,
    uploadWithProgress,
  };
}
