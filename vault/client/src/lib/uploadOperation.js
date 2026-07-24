function listed(value) {
  return Array.from(value || []).filter(Boolean);
}

function finiteNonnegative(value) {
  const number = Number(value);
  return Number.isFinite(number) ? Math.max(0, number) : 0;
}

function itemName(value, fallback) {
  const normalized = String(value || "").replaceAll("\\", "/");
  return normalized.split("/").filter(Boolean).at(-1) || fallback;
}

function inferredOperationName(files, folders) {
  const roots = [
    ...new Set(
      folders.map((folder) => String(folder).split("/").filter(Boolean)[0]).filter(Boolean)
    ),
  ];
  if (roots.length === 1) {
    return roots[0];
  }
  if (!folders.length && files.length === 1) {
    return files[0]?.name || "File";
  }
  if (!folders.length) {
    return `${files.length} files`;
  }
  return `${files.length + folders.length} items`;
}

export function describeUploadOperation({ files, folders, name: requestedName = "" } = {}) {
  const selectedFiles = listed(files);
  const selectedFolders = listed(folders);
  const totalFiles = selectedFiles.length;
  const totalFolders = selectedFolders.length;
  const totalItems = totalFiles + totalFolders;
  return {
    grouped: totalItems > 1 || totalFolders > 0,
    name:
      String(requestedName || "").trim() || inferredOperationName(selectedFiles, selectedFolders),
    totalBytes: selectedFiles.reduce((total, file) => total + finiteNonnegative(file?.size), 0),
    totalFiles,
    totalFolders,
    totalItems,
  };
}

function operationFailure(summary, descriptor) {
  const failures = listed(summary?.outcomes).filter((outcome) => outcome?.status === "rejected");
  const primaryFailure = failures.find((failure) => failure?.reason?.status === 401) || failures[0];
  const detail = String(primaryFailure?.reason?.message || "").trim();
  const failed = finiteNonnegative(summary?.failed) || failures.length;
  const attempted = finiteNonnegative(summary?.attempted) || descriptor.totalFiles;
  const error = new Error(`${failed} of ${attempted} files failed${detail ? `: ${detail}` : ""}`);
  error.failedItems = failed;
  if (primaryFailure?.reason?.status !== undefined) {
    error.status = primaryFailure.reason.status;
  }
  return error;
}

function createProgressTracker(descriptor, onProgress) {
  const activeFiles = new Map();
  let completedBytes = 0;
  let completedFolders = 0;
  let currentItem = "";
  let nextFileId = 1;
  let processedFiles = 0;

  function emit(stage, details = {}) {
    const active = [...activeFiles.values()];
    const activeCommitted = active.reduce((total, file) => total + file.committedBytes, 0);
    const activeInFlight = active.reduce((total, file) => total + file.inFlightBytes, 0);
    const committedBytes = Math.min(descriptor.totalBytes, completedBytes + activeCommitted);
    const inFlightBytes = Math.min(
      Math.max(0, descriptor.totalBytes - committedBytes),
      activeInFlight
    );
    const loaded = Math.min(descriptor.totalBytes, committedBytes + inFlightBytes);
    const bytesPerSecond = active.reduce((total, file) => total + file.bytesPerSecond, 0);
    const processedItems = completedFolders + processedFiles;
    const rawPercent = descriptor.totalBytes
      ? (loaded / descriptor.totalBytes) * 100
      : descriptor.totalItems
        ? (processedItems / descriptor.totalItems) * 100
        : null;
    const percent =
      rawPercent === 100 && committedBytes < descriptor.totalBytes ? 99.9 : rawPercent;
    onProgress({
      attempt: details.attempt ?? null,
      bytesPerSecond,
      committedBytes,
      currentItem,
      etaSeconds:
        bytesPerSecond > 0 ? Math.max(0, descriptor.totalBytes - loaded) / bytesPerSecond : null,
      grouped: descriptor.grouped,
      inFlightBytes,
      loaded,
      maxAttempts: details.maxAttempts ?? null,
      noProgressSeconds: details.noProgressSeconds || 0,
      percent,
      processedItems,
      retryDelayMs: details.retryDelayMs || null,
      stage,
      total: descriptor.totalBytes || null,
      totalFiles: descriptor.totalFiles,
      totalFolders: descriptor.totalFolders,
      totalItems: descriptor.totalItems,
      waitingForAcknowledgement: Boolean(details.waitingForAcknowledgement),
    });
  }

  return {
    fileFinished(id, completed) {
      const file = activeFiles.get(id);
      if (!file) {
        return;
      }
      completedBytes += completed ? file.size : file.committedBytes;
      processedFiles += 1;
      activeFiles.delete(id);
      emit("uploading");
    },
    fileProgress(id, progress) {
      const file = activeFiles.get(id);
      if (!file) {
        return;
      }
      currentItem = file.name;
      const hasDurableProgress =
        progress?.committedBytes !== null && progress?.committedBytes !== undefined;
      const committedBytes = Math.min(
        file.size,
        hasDurableProgress
          ? finiteNonnegative(progress.committedBytes)
          : finiteNonnegative(progress?.loaded)
      );
      const inFlightBytes = hasDurableProgress
        ? Math.min(
            Math.max(0, file.size - committedBytes),
            finiteNonnegative(progress?.inFlightBytes)
          )
        : 0;
      activeFiles.set(id, {
        ...file,
        bytesPerSecond: finiteNonnegative(progress?.bytesPerSecond),
        committedBytes,
        inFlightBytes,
      });
      emit(progress?.stage || "uploading", progress);
    },
    fileStarted(file) {
      const id = nextFileId;
      nextFileId += 1;
      currentItem = file?.name || "File";
      activeFiles.set(id, {
        bytesPerSecond: 0,
        committedBytes: 0,
        inFlightBytes: 0,
        name: currentItem,
        size: finiteNonnegative(file?.size),
      });
      emit("uploading");
      return id;
    },
    folderFinished(folder) {
      completedFolders += 1;
      currentItem = itemName(folder, "Folder");
      emit("creating-folders");
    },
    folderStarted(folder) {
      currentItem = itemName(folder, "Folder");
      emit("creating-folders");
    },
  };
}

export function createUploadOperation({
  abort,
  descriptor,
  isCancellation = (error) => Boolean(error?.cancelled),
  onCancelled,
  onComplete,
  onError,
  onProgress,
  runUpload,
  signal,
}) {
  const tracker = createProgressTracker(descriptor, onProgress);
  let settled = false;

  async function upload(options) {
    if (signal.aborted) {
      return { cancelled: true, status: 0 };
    }
    const fileId = tracker.fileStarted(options.file);
    try {
      const result = await runUpload({
        ...options,
        onProgress: (progress) => tracker.fileProgress(fileId, progress),
        signal,
      });
      tracker.fileFinished(fileId, true);
      return result;
    } catch (error) {
      tracker.fileFinished(fileId, false);
      const cancelled = isCancellation(error);
      if (!signal.aborted && !cancelled && error?.status === 401) {
        abort?.();
        throw error;
      }
      if (signal.aborted || cancelled) {
        return { cancelled: true, status: 0 };
      }
      throw error;
    }
  }

  function finalSummary(summary = {}) {
    return {
      ...summary,
      processedItems:
        descriptor.totalFolders +
        finiteNonnegative(summary.succeeded) +
        finiteNonnegative(summary.failed) +
        finiteNonnegative(summary.cancelled),
      totalFiles: descriptor.totalFiles,
      totalFolders: descriptor.totalFolders,
      totalItems: descriptor.totalItems,
    };
  }

  return {
    descriptor,
    fail(error) {
      if (settled) {
        return;
      }
      settled = true;
      if (signal.aborted || isCancellation(error)) {
        onCancelled(finalSummary());
      } else {
        onError(error, finalSummary());
      }
    },
    finish(summary) {
      if (settled) {
        return;
      }
      settled = true;
      const final = finalSummary(summary);
      if (finiteNonnegative(summary?.failed) > 0) {
        onError(operationFailure(summary, descriptor), final);
      } else if (finiteNonnegative(summary?.cancelled) > 0) {
        onCancelled(final);
      } else {
        onComplete(final);
      }
    },
    folderFinished: tracker.folderFinished,
    folderStarted: tracker.folderStarted,
    signal,
    upload,
  };
}
