use std::io::Cursor;

use image::{DynamicImage, GenericImageView, ImageFormat, Rgb, RgbImage};
use vault_server::previews::{
    PreviewProvider, PreviewProviderFailure, PreviewRenderRequest, RasterPreviewProvider,
};

fn source_png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            u8::try_from(x % 255).unwrap_or_default(),
            u8::try_from(y % 255).unwrap_or_default(),
            127,
        ])
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .expect("encode source PNG");
    bytes.into_inner()
}

fn source_svg(width: u32, height: u32) -> Vec<u8> {
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
  <rect width="{width}" height="{height}" fill="#1769aa"/>
  <circle cx="{}" cy="{}" r="{}" fill="#ffca28"/>
</svg>"##,
        width / 2,
        height / 2,
        height / 4,
    )
    .into_bytes()
}

#[test]
fn image_support_uses_safe_mime_extension_and_content_hints() {
    /*
     * Probes image detection with raster and SVG MIME types, case-insensitive extensions, real
     * content, an unrelated extension, and junk data. It checks that approved hints and
     * recognizable bytes are accepted without treating arbitrary files as images.
     */
    let provider = RasterPreviewProvider::default();
    assert!(provider.supports(Some("image/png"), None));
    assert!(provider.supports(Some("application/octet-stream"), Some("asset.JPEG")));
    assert!(provider.supports(Some("image/webp; charset=binary"), Some("asset.bin")));
    assert!(provider.supports(Some("image/svg+xml; charset=utf-8"), Some("asset.bin")));
    assert!(provider.supports(Some("application/octet-stream"), Some("asset.SVG")));
    assert!(!provider.supports(None, Some("asset.blend")));
    assert!(provider.supports_bytes(&source_png(1, 1)[..32]));
    assert!(provider.supports_bytes(&source_svg(40, 20)));
    assert!(!provider.supports_bytes(b"not an image"));
}

#[tokio::test]
async fn raster_provider_generates_complete_bounded_webp_set() {
    /*
     * Renders a 640-by-320 PNG through the raster preview provider. It checks that all three
     * named WebP renditions are nonempty, decodable, preserve the source aspect ratio, and
     * stop at their configured 128-, 256-, and 512-pixel bounds.
     */
    let outputs = RasterPreviewProvider::default()
        .render(PreviewRenderRequest {
            source_bytes: source_png(640, 320),
            source_mime_type: Some("image/png".to_string()),
            source_filename: Some("wide.png".to_string()),
        })
        .await
        .expect("render previews");

    assert_eq!(outputs.len(), 3);
    for (output, expected) in outputs.iter().zip([
        ("small", (128_u32, 64_u32)),
        ("medium", (256_u32, 128_u32)),
        ("large", (512_u32, 256_u32)),
    ]) {
        assert_eq!(output.variant, expected.0);
        assert_eq!(
            (output.width, output.height),
            (i64::from(expected.1.0), i64::from(expected.1.1))
        );
        assert_eq!(output.mime_type, "image/webp");
        assert!(!output.bytes.is_empty());
        let decoded = image::load_from_memory_with_format(&output.bytes, ImageFormat::WebP)
            .expect("decode generated WebP");
        assert_eq!(decoded.dimensions(), expected.1);
    }
}

#[tokio::test]
async fn svg_provider_generates_upscaled_bounded_webp_set() {
    /*
     * Renders a small vector through byte detection despite neutral metadata. It checks that the
     * three WebP renditions are decodable, preserve the SVG aspect ratio, and rasterize at each
     * configured bound instead of retaining the SVG's small declared dimensions.
     */
    let outputs = RasterPreviewProvider::default()
        .render(PreviewRenderRequest {
            source_bytes: source_svg(40, 20),
            source_mime_type: Some("application/octet-stream".to_string()),
            source_filename: Some("vector.bin".to_string()),
        })
        .await
        .expect("render SVG previews");

    assert_eq!(outputs.len(), 3);
    for (output, expected) in outputs.iter().zip([
        ("small", (128_u32, 64_u32)),
        ("medium", (256_u32, 128_u32)),
        ("large", (512_u32, 256_u32)),
    ]) {
        assert_eq!(output.variant, expected.0);
        assert_eq!(
            (output.width, output.height),
            (i64::from(expected.1.0), i64::from(expected.1.1))
        );
        assert_eq!(output.mime_type, "image/webp");
        let decoded = image::load_from_memory_with_format(&output.bytes, ImageFormat::WebP)
            .expect("decode generated SVG WebP")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), expected.1);
        assert!(decoded.pixels().any(|pixel| pixel.0[3] != 0));
    }
}

#[tokio::test]
async fn svg_provider_does_not_load_external_files() {
    /*
     * Points an otherwise empty SVG at a real local SVG. It checks that every generated preview
     * remains transparent, proving untrusted SVG resource references cannot read server files.
     */
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let external_path = temp_dir.path().join("external.svg");
    std::fs::write(&external_path, source_svg(16, 16)).expect("write external SVG");
    let source = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
<image href="{}" width="16" height="16"/>
</svg>"#,
        external_path.display(),
    );
    let outputs = RasterPreviewProvider::default()
        .render(PreviewRenderRequest {
            source_bytes: source.into_bytes(),
            source_mime_type: Some("image/svg+xml".to_string()),
            source_filename: Some("external.svg".to_string()),
        })
        .await
        .expect("render SVG with blocked resource");

    for output in outputs {
        let decoded = image::load_from_memory_with_format(&output.bytes, ImageFormat::WebP)
            .expect("decode blocked-resource WebP")
            .to_rgba8();
        assert!(decoded.pixels().all(|pixel| pixel.0[3] == 0));
    }
}

#[tokio::test]
async fn raster_provider_rejects_corrupt_and_unapproved_formats() {
    /*
     * Sends the renderer a corrupt PNG and a GIF disguised with PNG metadata. It distinguishes
     * invalid approved content from a recognizable but unsupported format, ensuring neither
     * input is rendered merely because its filename or MIME type claims PNG.
     */
    let provider = RasterPreviewProvider::default();
    let corrupt = provider
        .render(PreviewRenderRequest {
            source_bytes: b"\x89PNG\r\n\x1a\ninvalid".to_vec(),
            source_mime_type: Some("image/png".to_string()),
            source_filename: Some("broken.png".to_string()),
        })
        .await;
    assert!(matches!(
        corrupt,
        Err(PreviewProviderFailure::InvalidContent)
    ));

    let gif = provider
        .render(PreviewRenderRequest {
            source_bytes: b"GIF89a\x01\0\x01\0".to_vec(),
            source_mime_type: Some("image/png".to_string()),
            source_filename: Some("disguised.png".to_string()),
        })
        .await;
    assert!(matches!(gif, Err(PreviewProviderFailure::Unsupported)));

    let malformed_svg = provider
        .render(PreviewRenderRequest {
            source_bytes: b"<svg width='10' height='10'><path></svg>".to_vec(),
            source_mime_type: Some("image/svg+xml".to_string()),
            source_filename: Some("broken.svg".to_string()),
        })
        .await;
    assert!(matches!(
        malformed_svg,
        Err(PreviewProviderFailure::InvalidContent)
    ));
}
