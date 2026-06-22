const h = React.createElement;

export function FileIcon({ kind }) {
  const glyph = kind === "folder" ? "📁" : "📄";
  return h("span", { className: "file-icon" }, glyph);
}
