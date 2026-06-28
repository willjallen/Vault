import { Icon } from "./Icon.js";

const fileIconByExtension = new Map([
  ["blend", "app-blender"],
  ["fbx", "cube"],
  ["obj", "cube"],
  ["plasticity", "app-plasticity"],
  ["step", "cube"],
  ["stp", "cube"],
]);

function iconForFileName(fileName = "") {
  const extension = fileName.toLowerCase().split(".").pop();
  return fileIconByExtension.get(extension) || "file";
}

export function FileIcon({ color = "", fileName = "", folderIcon = "", kind, size = null }) {
  const icon = kind === "folder" ? folderIcon || "folder" : iconForFileName(fileName);
  return Icon({
    className: `file-icon ${color ? `folder-color-${color}` : ""}`,
    icon,
    size: size || (kind === "folder" || icon.startsWith("app-") ? 18 : 16),
  });
}
