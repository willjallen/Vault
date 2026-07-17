export const CONTENTS_CACHE_LIMIT = 32;
export const PREFETCH_PRIORITY_SIDEBAR = 0;
export const PREFETCH_PRIORITY_VISIBLE = 1;
export const PREFETCH_PRIORITY_ROOT = 2;
export const SEARCH_DEBOUNCE_MS = 250;

const PREFETCH_CONCURRENCY = 3;
const PREFETCH_QUEUE_LIMIT = 32;

function childrenFromContents(contents) {
  return (contents.folders || []).map((item) => item.path);
}

function metadataFromContents(contents) {
  return Object.fromEntries(
    (contents.folders || []).map((item) => [
      item.path || "",
      {
        access: item.access || {},
        can_delete_empty: item.can_delete_empty === true,
        color: item.color || "",
        default_ttl_action: item.default_ttl_action || "none",
        default_ttl_days: item.default_ttl_days || null,
        effective_ttl_action: item.effective_ttl_action || "none",
        effective_ttl_days: item.effective_ttl_days || null,
        effective_ttl_inherited: Boolean(item.effective_ttl_inherited),
        effective_ttl_source_id: item.effective_ttl_source_id || null,
        icon: item.icon || "",
        id: item.id || null,
      },
    ])
  );
}

export function contentsScopeAffectedByUpload(scopeFolder, recursive, uploadFolder) {
  const scope = scopeFolder || "";
  const target = uploadFolder || "";
  return scope === target || (Boolean(recursive) && (!scope || target.startsWith(`${scope}/`)));
}

export class ContentsPageCache {
  constructor(limit = CONTENTS_CACHE_LIMIT, initialEntries = []) {
    this.limit = Math.max(1, limit);
    this.pages = new Map();
    initialEntries.forEach(([key, value]) => this.set(key, value));
  }

  get size() {
    return this.pages.size;
  }

  clear() {
    this.pages.clear();
  }

  deleteFolder(folder) {
    const targetFolder = folder || "";
    let deleted = false;
    this.pages.forEach((page, key) => {
      if ((page.folder || "") === targetFolder) {
        this.pages.delete(key);
        deleted = true;
      }
    });
    return deleted;
  }

  deleteUploadAffected(folder) {
    let deleted = false;
    this.pages.forEach((page, key) => {
      if (contentsScopeAffectedByUpload(page.folder, page.recursive, folder)) {
        this.pages.delete(key);
        deleted = true;
      }
    });
    return deleted;
  }

  get(key) {
    const value = this.pages.get(key);
    if (value === undefined) {
      return undefined;
    }
    this.pages.delete(key);
    this.pages.set(key, value);
    return value;
  }

  has(key) {
    return this.pages.has(key);
  }

  set(key, value, protectedKeys = []) {
    this.pages.delete(key);
    this.pages.set(key, value);
    const protectedSet = new Set(protectedKeys.filter(Boolean));
    while (this.pages.size > this.limit) {
      let evictionKey = [...this.pages.keys()].find(
        (candidate) => candidate !== key && !protectedSet.has(candidate)
      );
      if (evictionKey === undefined) {
        evictionKey = [...this.pages.keys()].find((candidate) => candidate !== key);
      }
      if (evictionKey === undefined) {
        evictionKey = this.pages.keys().next().value;
      }
      this.pages.delete(evictionKey);
    }
    return value;
  }

  folderData() {
    const children = new Map();
    const metadata = new Map();
    this.pages.forEach((page) => {
      if (page.q || page.recursive) {
        return;
      }
      const parentPath = page.folder || "";
      children.set(parentPath, childrenFromContents(page));
      Object.entries(metadataFromContents(page)).forEach(([path, value]) => {
        metadata.set(path, value);
      });
    });
    return {
      children: Object.fromEntries(children),
      metadata: Object.fromEntries(metadata),
    };
  }

  updateDocument(docId, updater) {
    this.pages.forEach((page, key) => {
      if (!(page.documents || []).some((item) => item.id === docId)) {
        return;
      }
      this.pages.set(key, {
        ...page,
        documents: (page.documents || []).map((item) => (item.id === docId ? updater(item) : item)),
      });
    });
  }
}

export class BoundedPrefetchScheduler {
  constructor({ concurrency = PREFETCH_CONCURRENCY, maxQueued = PREFETCH_QUEUE_LIMIT } = {}) {
    this.concurrency = Math.max(1, concurrency);
    this.maxQueued = Math.max(1, maxQueued);
    this.activeCount = 0;
    this.generation = 0;
    this.nextSequence = 0;
    this.queue = [];
    this.runTask = async () => {};
    this.tasks = new Map();
  }

  get queuedCount() {
    return this.queue.length;
  }

  clear() {
    this.generation += 1;
    this.queue = [];
    this.tasks.forEach((entry) => {
      if (entry.state === "active") {
        entry.controller.abort();
      }
    });
    this.tasks.clear();
  }

  cancel(key) {
    const entry = this.tasks.get(key);
    if (!entry) {
      return false;
    }
    entry.controller.abort();
    this.tasks.delete(key);
    if (entry.state === "queued") {
      this.queue = this.queue.filter((candidate) => candidate !== entry);
    }
    return true;
  }

  has(key) {
    return this.tasks.has(key);
  }

  setRunner(runTask) {
    this.runTask = runTask;
  }

  enqueue(key, payload, priority = PREFETCH_PRIORITY_SIDEBAR) {
    const existing = this.tasks.get(key);
    if (existing) {
      if (existing.state === "queued" && priority > existing.priority) {
        existing.priority = priority;
        this.sortQueue();
      }
      return false;
    }

    if (this.queue.length >= this.maxQueued) {
      const replaceIndex = this.lowestPriorityQueueIndex();
      const replaceable = this.queue.at(replaceIndex);
      if (!replaceable || replaceable.priority >= priority) {
        return false;
      }
      this.queue.splice(replaceIndex, 1);
      if (this.tasks.get(replaceable.key) === replaceable) {
        this.tasks.delete(replaceable.key);
      }
    }

    const entry = {
      controller: new AbortController(),
      generation: this.generation,
      key,
      payload,
      priority,
      sequence: this.nextSequence,
      state: "queued",
    };
    this.nextSequence += 1;
    this.tasks.set(key, entry);
    this.queue.push(entry);
    this.sortQueue();
    this.drain();
    return true;
  }

  lowestPriorityQueueIndex() {
    let result = -1;
    this.queue.forEach((entry, index) => {
      const current = result < 0 ? null : this.queue.at(result);
      if (
        !current ||
        entry.priority < current.priority ||
        (entry.priority === current.priority && entry.sequence > current.sequence)
      ) {
        result = index;
      }
    });
    return result;
  }

  sortQueue() {
    this.queue.sort(
      (left, right) => right.priority - left.priority || left.sequence - right.sequence
    );
  }

  drain() {
    while (this.activeCount < this.concurrency && this.queue.length) {
      const entry = this.queue.shift();
      if (
        entry.generation !== this.generation ||
        this.tasks.get(entry.key) !== entry ||
        entry.controller.signal.aborted
      ) {
        continue;
      }
      entry.state = "active";
      this.activeCount += 1;
      Promise.resolve()
        .then(() => this.runTask(entry.payload, entry.controller.signal))
        .catch(() => {})
        .finally(() => {
          this.activeCount -= 1;
          if (this.tasks.get(entry.key) === entry) {
            this.tasks.delete(entry.key);
          }
          this.drain();
        });
    }
  }
}
