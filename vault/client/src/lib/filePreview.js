export function filePreview(doc) {
  const media = doc?.visual?.media;
  if (
    doc?.access?.read === false ||
    !["image", "audio", "video"].includes(media?.kind) ||
    typeof media?.url !== "string" ||
    !media.url.startsWith("/api/documents/")
  ) {
    return null;
  }
  return media;
}
