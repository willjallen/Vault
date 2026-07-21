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
  const detail = String(failures[0]?.reason?.message || "").trim();
  const failed = finiteNonnegative(summary?.failed) || failures.length;
  const attempted = finiteNonnegative(summary?.attempted) || descriptor.totalFiles;
  const error = new Error(`${failed} of ${attempted} files failed${detail ? `: ${detail}` : ""}`);
  error.failedItems = failed;
  return error;
}

function createProgressTracker(descriptor, onProgress) {
  const activeFiles = new Map();
  let completedBytes = 0;
  let completedFolders = 0;
  let currentItem = "";
  let nextFileId = 1;
  let processedFiles = 0;

  function emit(stage) {
    const active = [...activeFiles.values()];
    const activeLoaded = active.reduce((total, file) => total + file.loaded, 0);
    const loaded = Math.min(descriptor.totalBytes, completedBytes + activeLoaded);
    const bytesPerSecond = active.reduce((total, file) => total + file.bytesPerSecond, 0);
    const processedItems = completedFolders + processedFiles;
    const percent = descriptor.totalBytes
      ? (loaded / descriptor.totalBytes) * 100
      : descriptor.totalItems
        ? (processedItems / descriptor.totalItems) * 100
        : null;
    onProgress({
      bytesPerSecond,
      currentItem,
      etaSeconds:
        bytesPerSecond > 0 ? Math.max(0, descriptor.totalBytes - loaded) / bytesPerSecond : null,
      grouped: descriptor.grouped,
      loaded,
      percent,
      processedItems,
      stage,
      total: descriptor.totalBytes || null,
      totalFiles: descriptor.totalFiles,
      totalFolders: descriptor.totalFolders,
      totalItems: descriptor.totalItems,
    });
  }

  return {
    fileFinished(id, completed) {
      const file = activeFiles.get(id);
      if (!file) {
        return;
      }
      completedBytes += completed ? file.size : file.loaded;
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
      activeFiles.set(id, {
        ...file,
        bytesPerSecond: finiteNonnegative(progress?.bytesPerSecond),
        loaded: Math.min(file.size, Math.max(file.loaded, finiteNonnegative(progress?.loaded))),
      });
      emit(progress?.stage || "uploading");
    },
    fileStarted(file) {
      const id = nextFileId;
      nextFileId += 1;
      currentItem = file?.name || "File";
      activeFiles.set(id, {
        bytesPerSecond: 0,
        loaded: 0,
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
      if (signal.aborted || isCancellation(error)) {
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
      if (finiteNonnegative(summary?.cancelled) > 0) {
        onCancelled(final);
      } else if (finiteNonnegative(summary?.failed) > 0) {
        onError(operationFailure(summary, descriptor), final);
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
