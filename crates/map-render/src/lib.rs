//! Protocol-independent map composition and symbolizer execution.

use std::{
    io::Cursor,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use image::{DynamicImage, ImageFormat as EncoderFormat, RgbaImage};
use map_core::{BoundingBox, Rgba};
use map_style::Filter;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorSource {
    pub path: PathBuf,
    pub layer: Option<String>,
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorPaint {
    pub fill: Option<VectorColor>,
    pub fill_image: Option<VectorImageFill>,
    pub stroke: Option<VectorColor>,
    pub stroke_width: f32,
    pub stroke_dasharray: Vec<f32>,
    pub text: Option<VectorTextStyle>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorTextStyle {
    pub field: String,
    pub font_family: String,
    pub font_size: f32,
    pub font_italic: bool,
    pub font_bold: bool,
    pub fill: Rgba,
    pub halo: Rgba,
    pub halo_radius: f32,
    pub spacing: f32,
    pub min_area: f32,
    pub repeat_distance: f32,
    pub repeat_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorImageFill {
    pub path: PathBuf,
    pub scale: f32,
    pub opacity: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorColor {
    Literal(Rgba),
    Property { property: String, opacity: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorRenderRule {
    pub filter: Option<Filter>,
    pub minimum_scale_denominator: Option<f64>,
    pub maximum_scale_denominator: Option<f64>,
    pub paint: VectorPaint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VectorRasterRequest {
    pub source: VectorSource,
    pub target_crs: String,
    pub bbox: BoundingBox,
    pub width: u32,
    pub height: u32,
    /// Extra pixels rendered around each edge before cropping to the requested size.
    pub buffer_pixels: u32,
    /// Topology-preserving simplification tolerance in target CRS units.
    pub simplify_tolerance: Option<f64>,
    pub format: ImageFormat,
    pub rules: Vec<VectorRenderRule>,
    pub scale_denominator: f64,
    pub background: Rgba,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterizedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

#[derive(Debug, Error)]
pub enum RasterizeError {
    #[error("invalid vector raster request: {0}")]
    InvalidRequest(String),
    #[error("vector rasterization failed: {0}")]
    Backend(String),
    #[error("vector rasterization is unsupported by this backend: {0}")]
    Unsupported(String),
    #[error("vector rasterization backend is unavailable: {0}")]
    Unavailable(String),
}

/// Backend-independent boundary used by WMS, WMTS and previews.
/// Implementations may use GDAL, a CPU renderer or a GPU renderer.
pub trait VectorRasterizer: Send + Sync {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FallbackRasterizerStats {
    pub primary_attempts: u64,
    pub primary_successes: u64,
    pub fallback_requests: u64,
    pub primary_failures: u64,
    pub primary_disabled: bool,
}

/// Keeps the CPU backend authoritative while a GPU backend is introduced.
/// Unsupported requests fall back individually; device-unavailable failures
/// trip a process-local circuit breaker to prevent repeated GPU failures.
pub struct FallbackVectorRasterizer {
    primary: Arc<dyn VectorRasterizer>,
    fallback: Arc<dyn VectorRasterizer>,
    primary_enabled: AtomicBool,
    primary_attempts: AtomicU64,
    primary_successes: AtomicU64,
    fallback_requests: AtomicU64,
    primary_failures: AtomicU64,
}

impl FallbackVectorRasterizer {
    pub fn new(primary: Arc<dyn VectorRasterizer>, fallback: Arc<dyn VectorRasterizer>) -> Self {
        Self {
            primary,
            fallback,
            primary_enabled: AtomicBool::new(true),
            primary_attempts: AtomicU64::new(0),
            primary_successes: AtomicU64::new(0),
            fallback_requests: AtomicU64::new(0),
            primary_failures: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> FallbackRasterizerStats {
        FallbackRasterizerStats {
            primary_attempts: self.primary_attempts.load(Ordering::Relaxed),
            primary_successes: self.primary_successes.load(Ordering::Relaxed),
            fallback_requests: self.fallback_requests.load(Ordering::Relaxed),
            primary_failures: self.primary_failures.load(Ordering::Relaxed),
            primary_disabled: !self.primary_enabled.load(Ordering::Acquire),
        }
    }

    pub fn enable_primary(&self) {
        self.primary_enabled.store(true, Ordering::Release);
    }

    pub fn disable_primary(&self) {
        self.primary_enabled.store(false, Ordering::Release);
    }
}

impl VectorRasterizer for FallbackVectorRasterizer {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError> {
        if self.primary_enabled.load(Ordering::Acquire) {
            self.primary_attempts.fetch_add(1, Ordering::Relaxed);
            match self.primary.render(request) {
                Ok(image) => {
                    self.primary_successes.fetch_add(1, Ordering::Relaxed);
                    return Ok(image);
                }
                Err(error) => {
                    self.primary_failures.fetch_add(1, Ordering::Relaxed);
                    if matches!(error, RasterizeError::Unavailable(_)) {
                        self.primary_enabled.store(false, Ordering::Release);
                    }
                    tracing::warn!(%error, "primary vector rasterizer failed; using fallback");
                }
            }
        }
        self.fallback_requests.fetch_add(1, Ordering::Relaxed);
        self.fallback.render(request)
    }
}

pub fn blank_canvas(width: u32, height: u32, background: Rgba) -> RgbaImage {
    RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([
            background.red,
            background.green,
            background.blue,
            background.alpha,
        ]),
    )
}

pub fn blank_png(width: u32, height: u32, background: Rgba) -> Result<Vec<u8>, image::ImageError> {
    Ok(blank_image(width, height, background, ImageFormat::Png)?.bytes)
}

pub fn blank_image(
    width: u32,
    height: u32,
    background: Rgba,
    format: ImageFormat,
) -> Result<RasterizedImage, image::ImageError> {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(blank_canvas(width, height, background)).write_to(
        &mut bytes,
        match format {
            ImageFormat::Png => EncoderFormat::Png,
            ImageFormat::Jpeg => EncoderFormat::Jpeg,
        },
    )?;
    Ok(RasterizedImage {
        bytes: bytes.into_inner(),
        content_type: match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use map_core::{BoundingBox, Rgba};

    use super::{
        FallbackVectorRasterizer, ImageFormat, RasterizeError, RasterizedImage,
        VectorRasterRequest, VectorRasterizer, VectorSource,
    };

    struct ErrorRasterizer(bool);

    impl VectorRasterizer for ErrorRasterizer {
        fn render(
            &self,
            _request: &VectorRasterRequest,
        ) -> Result<RasterizedImage, RasterizeError> {
            if self.0 {
                Err(RasterizeError::Unavailable("device lost".into()))
            } else {
                Err(RasterizeError::Unsupported("style".into()))
            }
        }
    }

    struct SuccessRasterizer;

    impl VectorRasterizer for SuccessRasterizer {
        fn render(
            &self,
            _request: &VectorRasterRequest,
        ) -> Result<RasterizedImage, RasterizeError> {
            Ok(RasterizedImage {
                bytes: vec![1],
                content_type: "image/png",
            })
        }
    }

    fn request() -> VectorRasterRequest {
        VectorRasterRequest {
            source: VectorSource {
                path: "unused".into(),
                layer: None,
                encoding: None,
            },
            target_crs: "EPSG:3857".into(),
            bbox: BoundingBox {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1.0,
                max_y: 1.0,
            },
            width: 1,
            height: 1,
            buffer_pixels: 0,
            simplify_tolerance: None,
            format: ImageFormat::Png,
            rules: Vec::new(),
            scale_denominator: 1.0,
            background: Rgba {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 0,
            },
        }
    }

    #[test]
    fn unsupported_requests_fall_back_without_disabling_primary() {
        let rasterizer = FallbackVectorRasterizer::new(
            Arc::new(ErrorRasterizer(false)),
            Arc::new(SuccessRasterizer),
        );
        assert!(rasterizer.render(&request()).is_ok());
        assert!(rasterizer.render(&request()).is_ok());
        let stats = rasterizer.stats();
        assert_eq!(stats.primary_attempts, 2);
        assert_eq!(stats.fallback_requests, 2);
        assert!(!stats.primary_disabled);
    }

    #[test]
    fn unavailable_primary_trips_the_circuit_breaker() {
        let rasterizer = FallbackVectorRasterizer::new(
            Arc::new(ErrorRasterizer(true)),
            Arc::new(SuccessRasterizer),
        );
        assert!(rasterizer.render(&request()).is_ok());
        assert!(rasterizer.render(&request()).is_ok());
        let stats = rasterizer.stats();
        assert_eq!(stats.primary_attempts, 1);
        assert_eq!(stats.primary_failures, 1);
        assert_eq!(stats.fallback_requests, 2);
        assert!(stats.primary_disabled);
    }
}
