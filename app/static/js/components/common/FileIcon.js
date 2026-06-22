const h = React.createElement;

const folderGlyphs = new Map([
  ["archive", "🗄️"],
  ["finance", "💼"],
  ["folder", "📁"],
  ["home", "🏠"],
  ["locked", "🔒"],
  ["photos", "🖼️"],
  ["project", "📌"],
]);

export function FileIcon({ color = "", folderIcon = "", kind }) {
  const glyph = kind === "folder" ? folderGlyphs.get(folderIcon) || "📁" : "📄";
  return h("span", { className: `file-icon ${color ? `folder-color-${color}` : ""}` }, glyph);
}
