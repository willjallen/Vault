use serde::Serialize;

/// Original content offered to the browser's image, audio, or video decoder.
/// Keep this allowlist separate from downloadable types: uploaded documents
/// must never become executable same-origin pages through the preview route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaPreview {
    pub kind: &'static str,
    pub mime_type: &'static str,
    pub url: String,
}

#[must_use]
pub fn preview_mime_type(mime_type: Option<&str>, filename: &str) -> Option<&'static str> {
    let mime_type = mime_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match mime_type.as_str() {
        "image/jpeg" => Some("image/jpeg"),
        "image/png" | "image/apng" => Some("image/png"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/avif" => Some("image/avif"),
        "image/svg+xml" => Some("image/svg+xml"),
        "image/bmp" | "image/x-ms-bmp" => Some("image/bmp"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some("image/vnd.microsoft.icon"),
        "audio/mpeg" | "audio/mp3" => Some("audio/mpeg"),
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" => Some("audio/wav"),
        "audio/mp4" | "audio/x-m4a" => Some("audio/mp4"),
        "audio/aac" | "audio/x-aac" => Some("audio/aac"),
        "audio/ogg" => Some("audio/ogg"),
        "audio/webm" => Some("audio/webm"),
        "audio/flac" | "audio/x-flac" => Some("audio/flac"),
        "video/mp4" | "video/x-m4v" => Some("video/mp4"),
        "video/webm" => Some("video/webm"),
        "video/ogg" => Some("video/ogg"),
        "video/quicktime" => Some("video/quicktime"),
        "video/x-msvideo" | "video/avi" => Some("video/x-msvideo"),
        "" | "application/octet-stream" | "binary/octet-stream" | "application/ogg" => {
            preview_mime_from_filename(filename)
        }
        _ => None,
    }
}

fn preview_mime_from_filename(filename: &str) -> Option<&'static str> {
    let (_, extension) = filename.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "jfif" => Some("image/jpeg"),
        "png" | "apng" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "avif" => Some("image/avif"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/vnd.microsoft.icon"),
        "mp3" => Some("audio/mpeg"),
        "wav" | "wave" => Some("audio/wav"),
        "m4a" | "m4b" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "ogg" | "oga" | "opus" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "mp4" | "m4v" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "ogv" => Some("video/ogg"),
        "mov" => Some("video/quicktime"),
        "avi" => Some("video/x-msvideo"),
        _ => None,
    }
}

impl MediaPreview {
    #[must_use]
    pub fn for_source(mime_type: Option<&str>, filename: &str, url: String) -> Option<Self> {
        let mime_type = preview_mime_type(mime_type, filename)?;
        let (kind, _) = mime_type.split_once('/')?;
        Some(Self {
            kind,
            mime_type,
            url,
        })
    }
}
