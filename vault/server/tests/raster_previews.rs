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

#[test]
fn raster_support_uses_safe_mime_and_extension_hints() {
    let provider = RasterPreviewProvider::default();
    assert!(provider.supports(Some("image/png"), None));
    assert!(provider.supports(Some("application/octet-stream"), Some("asset.JPEG")));
    assert!(provider.supports(Some("image/webp; charset=binary"), Some("asset.bin")));
    assert!(!provider.supports(Some("image/svg+xml"), Some("asset.svg")));
    assert!(!provider.supports(None, Some("asset.blend")));
    assert!(provider.supports_bytes(&source_png(1, 1)[..32]));
    assert!(!provider.supports_bytes(b"not an image"));
}

#[tokio::test]
async fn raster_provider_generates_complete_bounded_webp_set() {
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
async fn raster_provider_rejects_corrupt_and_unapproved_formats() {
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
}
