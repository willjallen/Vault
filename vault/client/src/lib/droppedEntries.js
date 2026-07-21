function listItems(value) {
  return value ? Array.from(value) : [];
}

function droppedEntry(item) {
  const getAsEntry = item?.getAsEntry || item?.webkitGetAsEntry;
  return typeof getAsEntry === "function" ? getAsEntry.call(item) : null;
}

function droppedFile(item) {
  return typeof item?.getAsFile === "function" ? item.getAsFile() : null;
}

function entryReadError(entry) {
  return new Error(`Could not read dropped item ${entry?.name || "from this folder"}.`);
}

function readFileEntry(entry) {
  return new Promise((resolve, reject) => {
    try {
      entry.file(resolve, () => reject(entryReadError(entry)));
    } catch {
      reject(entryReadError(entry));
    }
  });
}

function readDirectoryEntries(entry) {
  return new Promise((resolve, reject) => {
    let reader;
    try {
      reader = entry.createReader();
    } catch {
      reject(entryReadError(entry));
      return;
    }
    const children = [];
    const readNextBatch = () => {
      try {
        reader.readEntries(
          (batch) => {
            const entries = listItems(batch);
            if (!entries.length) {
              resolve(children);
              return;
            }
            children.push(...entries);
            readNextBatch();
          },
          () => reject(entryReadError(entry))
        );
      } catch {
        reject(entryReadError(entry));
      }
    };
    readNextBatch();
  });
}

function joinRelativePath(parentPath, entryName) {
  return parentPath ? `${parentPath}/${entryName}` : entryName;
}

async function appendEntry(tree, entry, parentPath = "") {
  const relativePath = joinRelativePath(parentPath, String(entry?.name || ""));
  if (entry?.isDirectory) {
    tree.directories.push(relativePath);
    const children = await readDirectoryEntries(entry);
    for (const child of children) {
      await appendEntry(tree, child, relativePath);
    }
    return;
  }
  if (entry?.isFile) {
    tree.files.push({ file: await readFileEntry(entry), relativePath });
    return;
  }
  throw entryReadError(entry);
}

function normalizedSelectedPath(file) {
  const relativePath = String(file?.webkitRelativePath || "").replaceAll("\\", "/");
  return relativePath || String(file?.name || "");
}

function appendSelectedFile(tree, file, directories) {
  const relativePath = normalizedSelectedPath(file);
  const parts = relativePath.split("/").filter(Boolean);
  for (let index = 1; index < parts.length; index += 1) {
    const directory = parts.slice(0, index).join("/");
    if (!directories.has(directory)) {
      directories.add(directory);
      tree.directories.push(directory);
    }
  }
  tree.files.push({ file, relativePath });
}

function capturedDropRoots(dataTransfer) {
  const roots = [];
  const items = listItems(dataTransfer?.items).filter((item) => item?.kind === "file");
  for (const item of items) {
    const entry = droppedEntry(item);
    if (entry) {
      roots.push({ entry });
      continue;
    }
    const file = droppedFile(item);
    if (file) {
      roots.push({ file });
    }
  }
  if (roots.length) {
    return roots;
  }
  return listItems(dataTransfer?.files).map((file) => ({ file }));
}

async function readCapturedDropRoots(
  roots,
  emptyMessage = "No readable files or folders were found in this drop."
) {
  const tree = { directories: [], files: [] };
  const selectedDirectories = new Set();
  for (const root of roots) {
    if (root.entry) {
      await appendEntry(tree, root.entry);
    } else if (root.file) {
      appendSelectedFile(tree, root.file, selectedDirectories);
    }
  }
  if (!tree.directories.length && !tree.files.length) {
    throw new Error(emptyMessage);
  }
  return tree;
}

export function readSelectedUploadTree(files) {
  return readCapturedDropRoots(
    listItems(files).map((file) => ({ file })),
    "No readable files were found in the selected folder."
  );
}

// Capture entry handles synchronously while the browser's drag data store is readable.
export function readDroppedUploadTree(dataTransfer) {
  return readCapturedDropRoots(capturedDropRoots(dataTransfer));
}
