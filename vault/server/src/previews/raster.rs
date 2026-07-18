use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits};
use tokio::sync::Semaphore;

use super::{
    PREVIEW_VARIANTS, PreviewProvider, PreviewProviderFailure, PreviewRenderRequest,
    RenderedPreview,
};

const RASTER_MAX_DIMENSION: u32 = 8_192;
const RASTER_MAX_PIXELS: u64 = 40_000_000;
const RASTER_MAX_DECODED_BYTES: u64 = 192 * 1024 * 1024;
const RASTER_RENDER_CONCURRENCY: usize = 2;

/// Safely decodes supported raster inputs and produces the fixed preview
/// rendition set. Blocking work is isolated from Tokio and guarded by a
/// semaphore whose permit lives inside the blocking task; timed-out decoders
/// therefore cannot accumulate without bound.
#[derive(Debug)]
pub struct RasterPreviewProvider {
    render_slots: Arc<Semaphore>,
}

impl Default for RasterPreviewProvider {
    fn default() -> Self {
        Self {
            render_slots: Arc::new(Semaphore::new(RASTER_RENDER_CONCURRENCY)),
        }
    }
}

#[async_trait]
impl PreviewProvider for RasterPreviewProvider {
    fn supports(&self, mime_type: Option<&str>, filename: Option<&str>) -> bool {
        mime_type.is_some_and(is_supported_mime_type)
            || filename.is_some_and(has_supported_extension)
    }

    fn supports_bytes(&self, prefix: &[u8]) -> bool {
        image::guess_format(prefix).is_ok_and(|format| {
            matches!(
                format,
                ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
            )
        })
    }

    async fn render(
        &self,
        request: PreviewRenderRequest,
    ) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
        let permit = Arc::clone(&self.render_slots)
            .acquire_owned()
            .await
            .map_err(|_| PreviewProviderFailure::Failed)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            render_raster_previews(request)
        })
        .await
        .map_err(|_| PreviewProviderFailure::Failed)?
    }
}

fn is_supported_mime_type(value: &str) -> bool {
    matches!(
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "image/jpeg" | "image/jpg" | "image/pjpeg" | "image/png" | "image/x-png" | "image/webp"
    )
}

fn has_supported_extension(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpeg" | "jpg" | "png" | "webp"
            )
        })
}

fn render_raster_previews(
    request: PreviewRenderRequest,
) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
    let mut reader = ImageReader::new(Cursor::new(request.source_bytes))
        .with_guessed_format()
        .map_err(|_| PreviewProviderFailure::InvalidContent)?;
    let format = reader.format().ok_or(PreviewProviderFailure::Unsupported)?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        return Err(PreviewProviderFailure::Unsupported);
    }

    let mut limits = Limits::default();
    limits.max_image_width = Some(RASTER_MAX_DIMENSION);
    limits.max_image_height = Some(RASTER_MAX_DIMENSION);
    limits.max_alloc = Some(RASTER_MAX_DECODED_BYTES);
    reader.limits(limits);

    let mut decoder = reader
        .into_decoder()
        .map_err(|error| classify_image_error(&error))?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0
        || height == 0
        || pixels > RASTER_MAX_PIXELS
        || decoder.total_bytes() > RASTER_MAX_DECODED_BYTES
    {
        return Err(PreviewProviderFailure::InvalidContent);
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut source_image =
        DynamicImage::from_decoder(decoder).map_err(|error| classify_image_error(&error))?;
    source_image.apply_orientation(orientation);

    let large = fit_down(&source_image, 512);
    let medium = fit_down(&large, 256);
    let small = fit_down(&medium, 128);
    [small, medium, large]
        .into_iter()
        .zip(PREVIEW_VARIANTS)
        .map(|(image, (variant, _))| encode_webp(variant, &image))
        .collect()
}

fn fit_down(image: &DynamicImage, bound: u32) -> DynamicImage {
    if image.width().max(image.height()) <= bound {
        image.clone()
    } else {
        image.resize(bound, bound, FilterType::Triangle)
    }
}

fn encode_webp(
    variant: &str,
    image: &DynamicImage,
) -> Result<RenderedPreview, PreviewProviderFailure> {
    let mut output = Cursor::new(Vec::new());
    image
        .write_to(&mut output, ImageFormat::WebP)
        .map_err(|_| PreviewProviderFailure::Failed)?;
    Ok(RenderedPreview {
        variant: variant.to_string(),
        mime_type: "image/webp".to_string(),
        width: i64::from(image.width()),
        height: i64::from(image.height()),
        bytes: output.into_inner(),
    })
}

fn classify_image_error(error: &ImageError) -> PreviewProviderFailure {
    if matches!(error, ImageError::Unsupported(_)) {
        PreviewProviderFailure::Unsupported
    } else {
        PreviewProviderFailure::InvalidContent
    }
}
