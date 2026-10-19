use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl BoundingBox {
    pub fn is_valid(self) -> bool {
        self.min_x.is_finite()
            && self.min_y.is_finite()
            && self.max_x.is_finite()
            && self.max_y.is_finite()
            && self.min_x < self.max_x
            && self.min_y < self.max_y
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crs(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    Png,
    Jpeg,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerRequest {
    pub name: String,
    pub style: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapRequest {
    pub layers: Vec<LayerRequest>,
    pub bbox: BoundingBox,
    pub crs: Crs,
    pub width: u32,
    pub height: u32,
    pub background: Rgba,
    pub transparent: bool,
    pub format: OutputFormat,
}

pub struct RenderedImage {
    pub content_type: &'static str,
    pub body: Bytes,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("layer not found: {0}")]
    LayerNotFound(String),
    #[error("unsupported coordinate reference system: {0}")]
    UnsupportedCrs(String),
    #[error("rendering failed: {0}")]
    Internal(String),
}

#[async_trait]
pub trait MapRenderer: Send + Sync {
    async fn render(&self, request: &MapRequest) -> Result<RenderedImage, RenderError>;
}

#[derive(Debug, Clone)]
pub struct SpatialQuery {
    pub bbox: BoundingBox,
    pub crs: Crs,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source does not support CRS {0}")]
    UnsupportedCrs(String),
    #[error("source read failed: {0}")]
    Read(String),
}

#[async_trait]
pub trait LayerSource: Send + Sync {
    async fn extent(&self, crs: &Crs) -> Result<BoundingBox, SourceError>;
    async fn read(&self, query: &SpatialQuery) -> Result<LayerData, SourceError>;
}

#[derive(Debug)]
pub enum LayerData {
    Raster(Vec<u8>),
    Vector(Vec<u8>),
}
