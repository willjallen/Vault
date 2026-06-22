function progressFromEvent(evt, startedAt, fallbackTotal = null) {
  const elapsedSeconds = Math.max((performance.now() - startedAt) / 1000, 0.01);
  const total = evt.lengthComputable ? evt.total : fallbackTotal;
  const loaded = evt.loaded || 0;
  const bytesPerSecond = loaded / elapsedSeconds;
  const etaSeconds =
    total && bytesPerSecond > 0 && loaded < total ? (total - loaded) / bytesPerSecond : null;

  return {
    bytesPerSecond,
    etaSeconds,
    lengthComputable: Boolean(total),
    loaded,
    percent: total ? Math.min(100, Math.round((loaded / total) * 100)) : null,
    total,
  };
}

function parseJson(value) {
  if (!value) {
    return {};
  }
  try {
    return JSON.parse(value);
  } catch {
    return {};
  }
}

function errorFromText(text, fallback, statusCode) {
  const parsed = parseJson(text);
  const error = new Error(parsed.detail || fallback);
  error.status = statusCode;
  return error;
}

function filenameFromDisposition(disposition) {
  if (!disposition) {
    return "";
  }

  const utfMatch = disposition.match(/filename\*=UTF-8''([^;]+)/i);
  if (utfMatch) {
    try {
      return decodeURIComponent(utfMatch[1].replace(/"/g, "").trim());
    } catch {
      return utfMatch[1].replace(/"/g, "").trim();
    }
  }

  const quotedMatch = disposition.match(/filename="([^"]+)"/i);
  if (quotedMatch) {
    return quotedMatch[1].trim();
  }

  const plainMatch = disposition.match(/filename=([^;]+)/i);
  return plainMatch ? plainMatch[1].replace(/"/g, "").trim() : "";
}

function saveBlob(blob, filename) {
  const objectUrl = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = objectUrl;
  link.download = filename || "download";
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(objectUrl), 1000);
}

export function uploadForm({ url, formData, onProgress, method = "POST", fallbackTotal = null }) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const startedAt = performance.now();

    xhr.open(method, url, true);
    xhr.withCredentials = true;
    xhr.upload.onprogress = (evt) => {
      onProgress(progressFromEvent(evt, startedAt, fallbackTotal));
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve({ body: parseJson(xhr.responseText), status: xhr.status });
        return;
      }
      reject(errorFromText(xhr.responseText, "Upload failed", xhr.status));
    };
    xhr.onerror = () => reject(errorFromText("", "Network error during upload", xhr.status));
    xhr.onabort = () => reject(errorFromText("", "Upload cancelled", xhr.status));
    xhr.send(formData);
  });
}

export function downloadBlob({
  url,
  fallbackName,
  fallbackTotal = null,
  headers = {},
  method = "GET",
  body = null,
  onProgress,
}) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const startedAt = performance.now();

    xhr.open(method, url, true);
    xhr.withCredentials = true;
    xhr.responseType = "blob";
    Object.entries(headers || {}).forEach(([key, value]) => xhr.setRequestHeader(key, value));
    xhr.onprogress = (evt) => {
      onProgress(progressFromEvent(evt, startedAt, fallbackTotal));
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        const filename =
          filenameFromDisposition(xhr.getResponseHeader("Content-Disposition")) ||
          fallbackName ||
          "download";
        saveBlob(xhr.response, filename);
        resolve({ filename, size: xhr.response?.size || fallbackTotal || 0, status: xhr.status });
        return;
      }

      if (xhr.response && typeof xhr.response.text === "function") {
        xhr.response.text().then((text) => {
          reject(errorFromText(text, "Download failed", xhr.status));
        });
        return;
      }
      reject(errorFromText("", "Download failed", xhr.status));
    };
    xhr.onerror = () => reject(errorFromText("", "Network error during download", xhr.status));
    xhr.onabort = () => reject(errorFromText("", "Download cancelled", xhr.status));
    xhr.send(body);
  });
}
