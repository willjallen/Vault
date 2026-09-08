use vault_server::media::{MediaPreview, preview_mime_type};
use vault_server::{
    db,
    previews::{VisualSource, visual_payloads},
};

#[test]
fn media_types_use_canonical_mime_types_and_filename_fallbacks() {
    for (filename, declared, expected) in [
        ("TRACK.MP3", None, "audio/mpeg"),
        ("track.wav", Some("application/octet-stream"), "audio/wav"),
        ("track", Some(" Audio/X-WAV; codecs=1 "), "audio/wav"),
        ("track.m4a", Some("audio/x-m4a"), "audio/mp4"),
        ("clip.MP4", None, "video/mp4"),
        ("clip.webm", None, "video/webm"),
        ("clip.avi", None, "video/x-msvideo"),
        ("image.svg", Some("image/svg+xml"), "image/svg+xml"),
        ("image.png", None, "image/png"),
        ("image.gif", None, "image/gif"),
    ] {
        assert_eq!(preview_mime_type(declared, filename), Some(expected));
    }
}

#[test]
fn media_preview_rejects_active_documents_and_unknown_types() {
    for (filename, declared) in [
        ("page.html", Some("text/html")),
        ("page.png", Some("text/html")),
        ("page.svg", Some("application/xhtml+xml")),
        ("script.js", Some("application/javascript")),
        ("file.pdf", Some("application/pdf")),
        ("script.avs", None),
        ("png", None),
        ("photo.tiff", Some("image/tiff")),
        ("photo.png", Some("image/png\r\nX-Header: injected")),
    ] {
        assert_eq!(preview_mime_type(declared, filename), None);
    }
}

#[tokio::test]
async fn visual_media_is_version_bound_and_requires_read_access() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pool = db::connect(&temp.path().join("vault.db"))
        .await
        .expect("db");
    let sources = [
        VisualSource {
            document_id: 1,
            name: "track.mp3",
            version_id: Some("version/one?"),
            blob_id: None,
            mime_type: None,
            can_read: true,
        },
        VisualSource {
            document_id: 2,
            name: "private.mp3",
            version_id: Some("private-version"),
            blob_id: None,
            mime_type: Some("audio/mpeg"),
            can_read: false,
        },
        VisualSource {
            document_id: 3,
            name: "empty.mp3",
            version_id: None,
            blob_id: None,
            mime_type: None,
            can_read: true,
        },
    ];
    let visuals = visual_payloads(&pool, &sources).await.expect("visuals");
    assert_eq!(
        visuals[&1].media,
        Some(MediaPreview {
            kind: "audio",
            mime_type: "audio/mpeg",
            url: "/api/documents/1/versions/version%2Fone%3F/content".to_string(),
        })
    );
    assert!(visuals[&2].media.is_none());
    assert!(visuals[&3].media.is_none());
}
