export const EXPORT_NO_PROGRESS_NOTICE_MS = 5000;

function nonnegativeNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : null;
}

function exportActivityKey(progress) {
  return [
    progress.serverStatus,
    progress.loaded,
    progress.total,
    progress.processedItems,
    progress.totalItems,
    progress.updatedAt,
  ].join("|");
}

export function createExportProgressTracker(now = performance.now()) {
  return {
    activityKey: null,
    lastActivityAt: now,
  };
}

export function trackExportJobProgress(job, tracker, now = performance.now()) {
  const progress = {
    createdAt: typeof job?.created_at === "string" ? job.created_at : null,
    loaded: nonnegativeNumber(job?.processed_bytes) ?? 0,
    processedItems: nonnegativeNumber(job?.processed_items),
    serverStatus: typeof job?.status === "string" ? job.status : "",
    total: nonnegativeNumber(job?.total_bytes),
    totalItems: nonnegativeNumber(job?.total_items),
    updatedAt: typeof job?.updated_at === "string" ? job.updated_at : null,
  };
  const activityKey = exportActivityKey(progress);
  if (tracker.activityKey !== activityKey) {
    tracker.activityKey = activityKey;
    tracker.lastActivityAt = now;
  }
  const unchangedMs = Math.max(0, now - tracker.lastActivityAt);
  progress.noProgressSeconds =
    unchangedMs >= EXPORT_NO_PROGRESS_NOTICE_MS ? Math.floor(unchangedMs / 1000) : 0;
  return progress;
}
