function throwIfAborted(signal) {
  if (signal?.aborted) {
    const error = new Error("Download cancelled");
    error.name = "AbortError";
    throw error;
  }
}

export function cleanDownloadName(filename) {
  return (filename || "download").trim().replace(/[\\/:*?"<>|]+/g, "_") || "download";
}

export function supportsFileSystemDownloadWriter() {
  return typeof window.showSaveFilePicker === "function";
}

export function canUseFileSystemDownloadWriter(customDownloadsEnabled) {
  return customDownloadsEnabled === true && supportsFileSystemDownloadWriter();
}

export async function openFileSystemDownloadWriter(filename, signal) {
  throwIfAborted(signal);
  const handle = await window.showSaveFilePicker({
    suggestedName: cleanDownloadName(filename),
  });
  throwIfAborted(signal);
  return handle.createWritable();
}

export function startBrowserDownload(url, filename, signal) {
  throwIfAborted(signal);
  const resolvedUrl = new URL(url, window.location.href);
  if (resolvedUrl.origin !== window.location.origin) {
    throw new Error("Download URL must use the same origin as Vault.");
  }
  const link = document.createElement("a");
  link.download = cleanDownloadName(filename);
  link.href = resolvedUrl.href;
  link.hidden = true;
  document.body.appendChild(link);
  try {
    link.click();
  } finally {
    link.remove();
  }
}
