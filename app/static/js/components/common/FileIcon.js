import { Icon } from "./Icon.js";

export function FileIcon({ color = "", folderIcon = "", kind }) {
  return Icon({
    className: `file-icon ${color ? `folder-color-${color}` : ""}`,
    icon: kind === "folder" ? folderIcon || "folder" : "file",
    size: kind === "folder" ? 18 : 16,
  });
}
