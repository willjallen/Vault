use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, RgbaImage};
use tokio::sync::Semaphore;

use super::{
    PREVIEW_VARIANTS, PreviewProvider, PreviewProviderFailure, PreviewRenderRequest,
    RenderedPreview,
};

const RASTER_MAX_DIMENSION: u32 = 8_192;
const RASTER_MAX_PIXELS: u64 = 40_000_000;
const RASTER_MAX_DECODED_BYTES: u64 = 192 * 1024 * 1024;
const RASTER_RENDER_CONCURRENCY: usize = 2;
const SVG_MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const SVG_SNIFF_BYTES: usize = 4 * 1024;

static SVG_FONT_DATABASE: LazyLock<Arc<resvg::usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut database = resvg::usvg::fontdb::Database::new();
    database.load_system_fonts();
    Arc::new(database)
});

/// Safely decodes supported image inputs and produces the fixed raster preview
/// rendition set. Blocking work is isolated from Tokio and guarded by a
/// semaphore whose permit lives inside the blocking task; timed-out renderers
/// therefore cannot accumulate without bound.
#[derive(Debug)]
pub struct RasterPreviewProvider {
    render_slots: Arc<Semaphore>,
    svg_font_database: Arc<resvg::usvg::fontdb::Database>,
}

impl Default for RasterPreviewProvider {
    fn default() -> Self {
        Self {
            render_slots: Arc::new(Semaphore::new(RASTER_RENDER_CONCURRENCY)),
            svg_font_database: Arc::clone(&SVG_FONT_DATABASE),
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
        is_supported_raster_bytes(prefix) || looks_like_svg(prefix)
    }

    async fn render(
        &self,
        request: PreviewRenderRequest,
    ) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
        let permit = Arc::clone(&self.render_slots)
            .acquire_owned()
            .await
            .map_err(|_| PreviewProviderFailure::Failed)?;
        let svg_font_database = Arc::clone(&self.svg_font_database);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            render_image_previews(request, svg_font_database)
        })
        .await
        .map_err(|_| PreviewProviderFailure::Failed)?
    }
}

fn is_supported_mime_type(value: &str) -> bool {
    is_supported_raster_mime_type(value) || is_svg_mime_type(value)
}

fn has_supported_extension(filename: &str) -> bool {
    has_supported_raster_extension(filename) || has_svg_extension(filename)
}

fn is_supported_raster_mime_type(value: &str) -> bool {
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

fn is_svg_mime_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("image/svg+xml")
}

fn has_supported_raster_extension(filename: &str) -> bool {
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

fn has_svg_extension(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
}

fn is_supported_raster_bytes(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok_and(|format| {
        matches!(
            format,
            ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
        )
    })
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(SVG_SNIFF_BYTES)];
    prefix.windows(4).enumerate().any(|(index, window)| {
        window == b"<svg"
            && prefix
                .get(index + 4)
                .is_none_or(|next| next.is_ascii_whitespace() || matches!(*next, b'/' | b'>'))
    })
}

fn render_image_previews(
    request: PreviewRenderRequest,
    svg_font_database: Arc<resvg::usvg::fontdb::Database>,
) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
    if image::guess_format(&request.source_bytes).is_ok() {
        return render_raster_previews(request);
    }
    let svg_hint = request
        .source_mime_type
        .as_deref()
        .is_some_and(is_svg_mime_type)
        || request
            .source_filename
            .as_deref()
            .is_some_and(has_svg_extension);
    if looks_like_svg(&request.source_bytes) || svg_hint {
        return render_svg_previews(&request.source_bytes, svg_font_database);
    }
    render_raster_previews(request)
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

fn render_svg_previews(
    source_bytes: &[u8],
    font_database: Arc<resvg::usvg::fontdb::Database>,
) -> Result<Vec<RenderedPreview>, PreviewProviderFailure> {
    if source_bytes.len() > SVG_MAX_SOURCE_BYTES {
        return Err(PreviewProviderFailure::InvalidContent);
    }
    let options = resvg::usvg::Options {
        font_family: "DejaVu Sans".to_string(),
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: resvg::usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        fontdb: font_database,
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(source_bytes, &options)
        .map_err(|_| PreviewProviderFailure::InvalidContent)?;
    let bound = PREVIEW_VARIANTS
        .iter()
        .map(|(_, dimension)| *dimension)
        .max()
        .and_then(|dimension| u16::try_from(dimension).ok())
        .ok_or(PreviewProviderFailure::Failed)?;
    let source_size = tree.size();
    let bound = f32::from(bound);
    let output_size = if source_size.width() >= source_size.height() {
        source_size.scale_to_width(bound)
    } else {
        source_size.scale_to_height(bound)
    }
    .ok_or(PreviewProviderFailure::InvalidContent)?
    .to_int_size();
    let scale = bound / source_size.width().max(source_size.height());
    if !scale.is_finite() {
        return Err(PreviewProviderFailure::InvalidContent);
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(output_size.width(), output_size.height())
        .ok_or(PreviewProviderFailure::Failed)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let large = pixmap_to_image(&pixmap)?;
    let medium = fit_down(&large, 256);
    let small = fit_down(&medium, 128);
    [small, medium, large]
        .into_iter()
        .zip(PREVIEW_VARIANTS)
        .map(|(image, (variant, _))| encode_webp(variant, &image))
        .collect()
}

fn pixmap_to_image(
    pixmap: &resvg::tiny_skia::Pixmap,
) -> Result<DynamicImage, PreviewProviderFailure> {
    let mut bytes = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        bytes.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    RgbaImage::from_raw(pixmap.width(), pixmap.height(), bytes)
        .map(DynamicImage::ImageRgba8)
        .ok_or(PreviewProviderFailure::Failed)
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
