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

  run(task, { duplicateMessage = "This file is already queued for upload.", key = "" } = {}) {
    if (key && this.reservedKeys.has(key)) {
      return Promise.reject(new Error(duplicateMessage));
    }
    if (key) {
      this.reservedKeys.add(key);
    }
    return new Promise((resolve, reject) => {
      this.queue.push({ key, reject, resolve, task });
      this.drain();
    });
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
    if (entry.key) {
      this.reservedKeys.delete(entry.key);
    }
    this.activeCount -= 1;
    this.drain();
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
  blocked = false,
  blockedReason = "",
  concurrency = DEFAULT_UPLOAD_FILE_CONCURRENCY,
  files,
  refresh,
  scheduler: fileScheduler,
  setError,
  targetFolder = "",
  targetFolderForFile,
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
  const outcomePromises = pendingFiles.map((file, index) => {
    const destination = targetFolderForFile
      ? String(targetFolderForFile(file, index) || "")
      : targetFolder || "";
    const uploadName = normalizedUploadFileName(file);
    const key = uploadName ? JSON.stringify([destination, uploadName]) : "";
    return uploadScheduler
      .run(
        () =>
          uploadWithProgress({
            file,
            folder: destination,
            mode: "create",
            name: file.name,
            size: file.size,
          }),
        {
          duplicateMessage: `A file named ${uploadName || "this name"} is already queued for ${destination || "Vault"}.`,
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

  return {
    attempted: pendingFiles.length,
    blocked: 0,
    cancelled,
    failed: failures.length,
    outcomes: orderedOutcomes,
    succeeded,
  };
}

export async function uploadFileTree({
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

  try {
    for (const relativePath of directories) {
      await createFolder(joinedFolderPath(targetFolder, relativePath));
      foldersCreated += 1;
    }
  } catch (error) {
    if (foldersCreated > 0) {
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
      uploadWithProgress,
    });
  } catch (error) {
    if (foldersCreated > 0) {
      await refresh(targetFolder || "", { sidebar: true });
    }
    throw error;
  }

  if (foldersCreated > 0 || result.succeeded > 0) {
    await refresh(targetFolder || "", { sidebar: foldersCreated > 0 });
  }
  return {
    ...result,
    foldersBlocked: 0,
    foldersCreated,
    foldersRequested: directories.length,
  };
}
