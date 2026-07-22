export const DEFAULT_UPLOAD_FILE_CONCURRENCY = 2;
export const MAX_UPLOAD_FILE_CONCURRENCY = 4;
const UPLOAD_QUEUE_COMPACT_THRESHOLD = 64;

function uploadFiles(files) {
  if (!files) {
    return [];
  }
  if (Array.isArray(files)) {
    return files.filter(Boolean);
  }
  if (typeof files[Symbol.iterator] === "function" || Number.isSafeInteger(files.length)) {
    return Array.from(files).filter(Boolean);
  }
  return [files];
}

function boundedConcurrency(concurrency) {
  const requested = Number.isFinite(concurrency)
    ? Math.floor(concurrency)
    : DEFAULT_UPLOAD_FILE_CONCURRENCY;
  return Math.min(MAX_UPLOAD_FILE_CONCURRENCY, Math.max(1, requested));
}

function normalizedUploadFileName(file) {
  return String(file?.name || "")
    .replaceAll("\\", "/")
    .split("/")
    .at(-1)
    .trim();
}

function folderDepth(path) {
  return String(path || "")
    .split("/")
    .filter(Boolean).length;
}

function orderedDirectoryPaths(paths) {
  return [...new Set((paths || []).map((path) => String(path || "")).filter(Boolean))].sort(
    (left, right) => folderDepth(left) - folderDepth(right) || left.localeCompare(right)
  );
}

function joinedFolderPath(parentFolder, child) {
  if (!parentFolder) {
    return child || "";
  }
  return child ? `${parentFolder}/${child}` : parentFolder;
}

function relativeFileFolder(relativePath) {
  return String(relativePath || "")
    .split("/")
    .filter(Boolean)
    .slice(0, -1)
    .join("/");
}

function beginTreeUploadOperation(beginUploadOperation, entries, directories) {
  return beginUploadOperation
    ? beginUploadOperation({
        files: entries.map((entry) => entry.file),
        folders: directories,
      })
    : null;
}

function throwIfUploadOperationCancelled(operation) {
  if (operation?.signal?.aborted) {
    const error = new Error("Upload operation cancelled");
    error.cancelled = true;
    throw error;
  }
}

async function createUploadTreeFolders({
  createFolder,
  directories,
  operation,
  state,
  targetFolder,
}) {
  for (const relativePath of directories) {
    throwIfUploadOperationCancelled(operation);
    operation?.folderStarted?.(relativePath);
    await createFolder(joinedFolderPath(targetFolder, relativePath), {
      signal: operation?.signal,
    });
    state.created += 1;
    operation?.folderFinished?.(relativePath);
    throwIfUploadOperationCancelled(operation);
  }
}

function failUploadOperation(operation, error) {
  operation?.fail?.(error);
}

function treeNeedsErrorRefresh(foldersCreated, operation) {
  return foldersCreated > 0 || operation?.signal?.aborted === true;
}

function queuedUploadCancellation(fileScheduler, operation) {
  const signal = operation?.signal;
  if (!signal) {
    return { dispose: () => {}, group: null, settleIfAborted: () => {} };
  }
  const cancel = () => fileScheduler.cancelQueued(signal);
  signal.addEventListener("abort", cancel, { once: true });
  return {
    dispose: () => signal.removeEventListener("abort", cancel),
    group: signal,
    settleIfAborted: () => {
      if (signal.aborted) {
        cancel();
      }
    },
  };
}

export class UploadFileScheduler {
  constructor(concurrency = DEFAULT_UPLOAD_FILE_CONCURRENCY) {
    this.activeCount = 0;
    this.concurrency = boundedConcurrency(concurrency);
    this.nextQueueIndex = 0;
    this.queue = [];
    this.reservedKeys = new Set();
  }

  get queuedCount() {
    return this.queue.length - this.nextQueueIndex;
  }

  run(
    task,
    { duplicateMessage = "This file is already queued for upload.", group = null, key = "" } = {}
  ) {
    if (key && this.reservedKeys.has(key)) {
      return Promise.reject(new Error(duplicateMessage));
    }
    if (key) {
      this.reservedKeys.add(key);
    }
    return new Promise((resolve, reject) => {
      this.queue.push({ group, key, reject, resolve, task });
      this.drain();
    });
  }

  cancelQueued(group, value = { cancelled: true, status: 0 }) {
    if (!group) {
      return 0;
    }
    const remaining = [];
    let cancelled = 0;
    for (let index = this.nextQueueIndex; index < this.queue.length; index += 1) {
      const entry = this.queue.at(index);
      if (entry.group !== group) {
        remaining.push(entry);
        continue;
      }
      this.releaseReservation(entry);
      entry.resolve(value);
      cancelled += 1;
    }
    this.queue = remaining;
    this.nextQueueIndex = 0;
    this.drain();
    return cancelled;
  }

  drain() {
    while (this.activeCount < this.concurrency && this.queuedCount > 0) {
      const entry = this.queue.at(this.nextQueueIndex);
      this.nextQueueIndex += 1;
      if (this.nextQueueIndex === this.queue.length) {
        this.queue = [];
        this.nextQueueIndex = 0;
      } else if (
        this.nextQueueIndex >= UPLOAD_QUEUE_COMPACT_THRESHOLD &&
        this.nextQueueIndex * 2 >= this.queue.length
      ) {
        this.queue = this.queue.slice(this.nextQueueIndex);
        this.nextQueueIndex = 0;
      }
      this.activeCount += 1;
      Promise.resolve()
        .then(entry.task)
        .then(
          (value) => {
            this.finish(entry);
            entry.resolve(value);
          },
          (reason) => {
            this.finish(entry);
            entry.reject(reason);
          }
        );
    }
  }

  finish(entry) {
    this.releaseReservation(entry);
    this.activeCount -= 1;
    this.drain();
  }

  releaseReservation(entry) {
    if (entry.key) {
      this.reservedKeys.delete(entry.key);
    }
  }
}

function failedUploadMessage(failures) {
  const first = failures[0];
  const detail = String(first.reason?.message || "Upload failed. Please try again.");
  const firstName = String(first.file?.name || "file");
  if (failures.length === 1) {
    return `Could not upload ${firstName}: ${detail}`;
  }
  const visibleNames = failures
    .slice(0, 3)
    .map((failure) => failure.file?.name || "unnamed file")
    .join(", ");
  const remaining = failures.length > 3 ? `, and ${failures.length - 3} more` : "";
  return `${failures.length} files failed to upload (${visibleNames}${remaining}). First failure: ${detail}`;
}

export async function uploadFileBatch({
  beginUploadOperation,
  blocked = false,
  blockedReason = "",
  concurrency = DEFAULT_UPLOAD_FILE_CONCURRENCY,
  files,
  operationName = "",
  refresh,
  scheduler: fileScheduler,
  setError,
  targetFolder = "",
  targetFolderForFile,
  uploadOperation,
  uploadWithProgress,
}) {
  const pendingFiles = uploadFiles(files);
  const outcomes = new Array(pendingFiles.length);
  if (!pendingFiles.length) {
    return { attempted: 0, blocked: 0, cancelled: 0, failed: 0, outcomes, succeeded: 0 };
  }
  const normalizedBlockedReason = String(blockedReason || "").trim();
  if (blocked || normalizedBlockedReason) {
    setError(normalizedBlockedReason || "Wait for the destination folder to finish loading.");
    return {
      attempted: 0,
      blocked: pendingFiles.length,
      cancelled: 0,
      failed: 0,
      outcomes,
      succeeded: 0,
    };
  }

  setError("");
  const uploadScheduler = fileScheduler || new UploadFileScheduler(concurrency);
  const operation =
    uploadOperation || beginUploadOperation?.({ files: pendingFiles, name: operationName });
  const ownsOperation = Boolean(operation && !uploadOperation);
  const upload = operation?.upload || uploadWithProgress;
  const queueCancellation = queuedUploadCancellation(uploadScheduler, operation);
  try {
    const outcomePromises = pendingFiles.map((file, index) => {
      const destination = targetFolderForFile
        ? String(targetFolderForFile(file, index) || "")
        : targetFolder || "";
      const uploadName = normalizedUploadFileName(file);
      const key = uploadName ? JSON.stringify([destination, uploadName]) : "";
      return uploadScheduler
        .run(
          () =>
            upload({
              file,
              folder: destination,
              mode: "create",
              name: file.name,
              size: file.size,
            }),
          {
            duplicateMessage: `A file named ${uploadName || "this name"} is already queued for ${destination || "Vault"}.`,
            group: queueCancellation.group,
            key,
          }
        )
        .then((value) =>
          value?.cancelled
            ? { file, status: "cancelled", value }
            : { file, status: "fulfilled", value }
        )
        .catch((reason) => ({ file, reason, status: "rejected" }));
    });
    queueCancellation.settleIfAborted();
    const orderedOutcomes = await Promise.all(outcomePromises);

    const succeeded = orderedOutcomes.filter((outcome) => outcome.status === "fulfilled").length;
    const cancelled = orderedOutcomes.filter((outcome) => outcome.status === "cancelled").length;
    const failures = orderedOutcomes.filter((outcome) => outcome.status === "rejected");
    if (succeeded > 0) {
      await refresh(targetFolder || "", { invalidateContents: true });
    }
    if (failures.length) {
      setError(failedUploadMessage(failures));
    }

    const result = {
      attempted: pendingFiles.length,
      blocked: 0,
      cancelled,
      failed: failures.length,
      outcomes: orderedOutcomes,
      succeeded,
    };
    if (ownsOperation) {
      operation.finish(result);
    }
    return result;
  } catch (error) {
    if (ownsOperation) {
      operation.fail(error);
    }
    throw error;
  } finally {
    queueCancellation.dispose();
  }
}

export async function uploadFileTree({
  beginUploadOperation,
  blocked = false,
  blockedReason = "",
  concurrency = DEFAULT_UPLOAD_FILE_CONCURRENCY,
  createFolder,
  refresh,
  scheduler: fileScheduler,
  setError,
  targetFolder = "",
  tree,
  uploadWithProgress,
}) {
  const directories = orderedDirectoryPaths(tree?.directories);
  const entries = Array.from(tree?.files || []).filter((entry) => entry?.file);
  let foldersCreated = 0;
  const normalizedBlockedReason = String(blockedReason || "").trim();
  if (blocked || normalizedBlockedReason) {
    setError(normalizedBlockedReason || "Wait for the destination folder to finish loading.");
    return {
      attempted: 0,
      blocked: entries.length,
      cancelled: 0,
      failed: 0,
      foldersBlocked: directories.length,
      foldersCreated: 0,
      foldersRequested: directories.length,
      outcomes: new Array(entries.length),
      succeeded: 0,
    };
  }

  const operation = beginTreeUploadOperation(beginUploadOperation, entries, directories);
  const folderState = { created: 0 };

  try {
    await createUploadTreeFolders({
      createFolder,
      directories,
      operation,
      state: folderState,
      targetFolder,
    });
    foldersCreated = folderState.created;
  } catch (error) {
    foldersCreated = folderState.created;
    failUploadOperation(operation, error);
    if (treeNeedsErrorRefresh(foldersCreated, operation)) {
      await refresh(targetFolder || "", { sidebar: true });
    }
    throw error;
  }

  let result;
  try {
    result = await uploadFileBatch({
      concurrency,
      files: entries.map((entry) => entry.file),
      refresh: async () => {},
      scheduler: fileScheduler,
      setError,
      targetFolder,
      targetFolderForFile: (_file, index) =>
        joinedFolderPath(targetFolder, relativeFileFolder(entries.at(index)?.relativePath)),
      uploadOperation: operation,
      uploadWithProgress,
    });
  } catch (error) {
    failUploadOperation(operation, error);
    if (treeNeedsErrorRefresh(foldersCreated, operation)) {
      await refresh(targetFolder || "", { sidebar: true });
    }
    throw error;
  }

  if (foldersCreated > 0 || result.succeeded > 0) {
    try {
      await refresh(targetFolder || "", { sidebar: foldersCreated > 0 });
    } catch (error) {
      failUploadOperation(operation, error);
      throw error;
    }
  }
  const finalResult = {
    ...result,
    foldersBlocked: 0,
    foldersCreated,
    foldersRequested: directories.length,
  };
  operation?.finish?.(finalResult);
  return finalResult;
}
