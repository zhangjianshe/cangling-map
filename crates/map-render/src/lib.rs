//! Protocol-independent map composition and symbolizer execution.

use std::{io::Cursor, path::PathBuf};

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
}

/// Backend-independent boundary used by WMS, WMTS and previews.
/// Implementations may use GDAL, a CPU renderer or a GPU renderer.
pub trait VectorRasterizer: Send + Sync {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError>;
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
