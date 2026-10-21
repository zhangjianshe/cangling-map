//! Compatibility adapter between cangling-map domain types and cangling-gdal.

mod gpu_mesh_cache;

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io::Cursor,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use cangling_gdal_core::{
    BoundingBox as GdalBoundingBox, DataSource, GdalEngine, ImageFormat as GdalImageFormat,
    VectorComparisonOperator, VectorFilter, VectorLiteral, VectorRenderRequest,
    VectorRenderRule as GdalVectorRenderRule, VectorTextStyle as GdalVectorTextStyle,
};
use map_core::BoundingBox;
use map_gpu::{
    AtlasOverlay, AtlasVertex, ByteLruCache, MeshChunk, PreparedScene, Renderer, RendererOptions,
    Vertex, ViewTransform,
};
use map_render::{
    ImageFormat, RasterizeError, RasterizedImage, RasterizedRgba, VectorColor, VectorRasterRequest,
    VectorRasterizer,
};
use map_style::{ComparisonOperator, Filter, Literal};
use serde::Serialize;
use tracing::warn;

use gpu_mesh_cache::{GpuMeshArchive, GpuMeshDiskCache};

pub use cangling_gdal_core::DatasetKind;
#[derive(Debug, Clone, Default)]
pub struct GdalVectorRasterizer;

impl VectorRasterizer for GdalVectorRasterizer {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError> {
        if !request.bbox.is_valid() || request.width == 0 || request.height == 0 {
            return Err(RasterizeError::InvalidRequest(
                "bbox and image dimensions must be valid".into(),
            ));
        }
        let image = GdalEngine
            .render_vector(&to_gdal_request(request)?)
            .map_err(|error| RasterizeError::Backend(error.to_string()))?;
        Ok(RasterizedImage {
            bytes: image.bytes,
            content_type: image.content_type,
        })
    }

    fn render_rgba(&self, request: &VectorRasterRequest) -> Result<RasterizedRgba, RasterizeError> {
        if !request.bbox.is_valid() || request.width == 0 || request.height == 0 {
            return Err(RasterizeError::InvalidRequest(
                "bbox and image dimensions must be valid".into(),
            ));
        }
        let rgba = GdalEngine
            .render_vector_rgba(&to_gdal_request(request)?)
            .map_err(|error| RasterizeError::Backend(error.to_string()))?;
        if rgba.width != request.width || rgba.height != request.height {
            return Err(RasterizeError::Backend(format!(
                "CPU renderer returned {}x{}; expected {}x{}",
                rgba.width, rgba.height, request.width, request.height
            )));
        }
        Ok(RasterizedRgba {
            width: rgba.width,
            height: rgba.height,
            pixels: rgba.pixels,
        })
    }
}

fn to_gdal_request(request: &VectorRasterRequest) -> Result<VectorRenderRequest, RasterizeError> {
    let rgba = |color: map_core::Rgba| [color.red, color.green, color.blue, color.alpha];
    let rules = request
        .rules
        .iter()
        .filter(|rule| {
            rule.minimum_scale_denominator
                .is_none_or(|minimum| request.scale_denominator >= minimum)
                && rule
                    .maximum_scale_denominator
                    .is_none_or(|maximum| request.scale_denominator < maximum)
        })
        .map(|rule| {
            Ok(GdalVectorRenderRule {
                where_clause: rule.filter.as_ref().map(filter_to_ogr_sql).transpose()?,
                filter: rule.filter.as_ref().map(filter_to_gdal),
                fill: rule.paint.fill.as_ref().and_then(|color| match color {
                    VectorColor::Literal(value) => Some(rgba(*value)),
                    VectorColor::Property { .. } => None,
                }),
                fill_field: rule.paint.fill.as_ref().and_then(|color| match color {
                    VectorColor::Property { property, opacity } => {
                        Some((property.clone(), *opacity))
                    }
                    VectorColor::Literal(_) => None,
                }),
                fill_image: rule.paint.fill_image.as_ref().map(|image| {
                    cangling_gdal_core::VectorImageFill {
                        path: image.path.clone(),
                        scale: image.scale,
                        opacity: image.opacity,
                    }
                }),
                stroke: rule.paint.stroke.as_ref().and_then(|color| match color {
                    VectorColor::Literal(value) => Some(rgba(*value)),
                    VectorColor::Property { .. } => None,
                }),
                stroke_field: rule.paint.stroke.as_ref().and_then(|color| match color {
                    VectorColor::Property { property, opacity } => {
                        Some((property.clone(), *opacity))
                    }
                    VectorColor::Literal(_) => None,
                }),
                stroke_width: rule.paint.stroke_width,
                stroke_dasharray: rule.paint.stroke_dasharray.clone(),
                text: rule.paint.text.as_ref().map(|text| GdalVectorTextStyle {
                    field: text.field.clone(),
                    font_family: text.font_family.clone(),
                    font_size: text.font_size,
                    font_italic: text.font_italic,
                    font_bold: text.font_bold,
                    fill: rgba(text.fill),
                    halo: rgba(text.halo),
                    halo_radius: text.halo_radius,
                    spacing: text.spacing,
                    min_area: text.min_area,
                    repeat_distance: text.repeat_distance,
                    repeat_count: text.repeat_count,
                }),
            })
        })
        .collect::<Result<Vec<_>, RasterizeError>>()?;
    Ok(VectorRenderRequest {
        source: DataSource {
            path: request.source.path.clone(),
            layer: request.source.layer.clone(),
            encoding: request.source.encoding.clone(),
        },
        target_crs: request.target_crs.clone(),
        bbox: GdalBoundingBox {
            min_x: request.bbox.min_x,
            min_y: request.bbox.min_y,
            max_x: request.bbox.max_x,
            max_y: request.bbox.max_y,
        },
        width: request.width,
        height: request.height,
        buffer_pixels: request.buffer_pixels,
        simplify_tolerance: request.simplify_tolerance,
        format: match request.format {
            ImageFormat::Png => GdalImageFormat::Png,
            ImageFormat::Jpeg => GdalImageFormat::Jpeg,
        },
        rules: optimize_image_rules(rules),
        background: rgba(request.background),
    })
}

pub struct GpuVectorRasterizer {
    renderer: Renderer,
    scenes: Mutex<ByteLruCache<u64, CachedGpuScene>>,
    mesh_cache: GpuMeshDiskCache,
    scene_hits: AtomicU64,
    scene_misses: AtomicU64,
    mesh_disk_hits: AtomicU64,
    mesh_disk_misses: AtomicU64,
    mesh_disk_writes: AtomicU64,
    mesh_disk_errors: AtomicU64,
}

#[derive(Clone)]
struct CachedGpuScene {
    identity: u64,
    coverage: BoundingBox,
    resolution_x: f64,
    resolution_y: f64,
    scene: Arc<PreparedScene>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct GpuSceneCacheStats {
    pub entries: usize,
    pub resident_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub mesh_disk_enabled: bool,
    pub mesh_disk_hits: u64,
    pub mesh_disk_misses: u64,
    pub mesh_disk_writes: u64,
    pub mesh_disk_errors: u64,
}

impl GpuVectorRasterizer {
    pub fn new(adapter_name: Option<String>) -> Result<Self, RasterizeError> {
        let scene_cache_mb = std::env::var("MAP_GPU_SCENE_CACHE_MB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1024);
        Renderer::new(&RendererOptions { adapter_name })
            .map(|renderer| Self {
                renderer,
                scenes: Mutex::new(ByteLruCache::new(
                    scene_cache_mb.saturating_mul(1024 * 1024),
                )),
                mesh_cache: GpuMeshDiskCache::from_environment(),
                scene_hits: AtomicU64::new(0),
                scene_misses: AtomicU64::new(0),
                mesh_disk_hits: AtomicU64::new(0),
                mesh_disk_misses: AtomicU64::new(0),
                mesh_disk_writes: AtomicU64::new(0),
                mesh_disk_errors: AtomicU64::new(0),
            })
            .map_err(|error| RasterizeError::Unavailable(error.to_string()))
    }

    pub fn adapter_info(&self) -> &map_gpu::AdapterInfo {
        self.renderer.adapter_info()
    }

    pub fn scene_cache_stats(&self) -> GpuSceneCacheStats {
        let scenes = self
            .scenes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        GpuSceneCacheStats {
            entries: scenes.len(),
            resident_bytes: scenes.resident_bytes(),
            hits: self.scene_hits.load(Ordering::Relaxed),
            misses: self.scene_misses.load(Ordering::Relaxed),
            mesh_disk_enabled: self.mesh_cache.is_enabled(),
            mesh_disk_hits: self.mesh_disk_hits.load(Ordering::Relaxed),
            mesh_disk_misses: self.mesh_disk_misses.load(Ordering::Relaxed),
            mesh_disk_writes: self.mesh_disk_writes.load(Ordering::Relaxed),
            mesh_disk_errors: self.mesh_disk_errors.load(Ordering::Relaxed),
        }
    }

    fn render_raw_rgba(
        &self,
        request: &VectorRasterRequest,
    ) -> Result<RasterizedRgba, RasterizeError> {
        if !request.bbox.is_valid() || request.width == 0 || request.height == 0 {
            return Err(RasterizeError::InvalidRequest(
                "bbox and image dimensions must be valid".into(),
            ));
        }
        let identity = gpu_scene_identity(request);
        let resolution_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width);
        let resolution_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height);
        let requested_coverage = rendered_bbox(request);
        let cached = {
            let mut scenes = self
                .scenes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            scenes
                .find(|_, cached| {
                    cached.identity == identity
                        && resolution_matches(cached.resolution_x, resolution_x)
                        && resolution_matches(cached.resolution_y, resolution_y)
                        && bbox_contains(cached.coverage, requested_coverage)
                })
                .cloned()
        };
        if let Some(cached) = cached {
            self.scene_hits.fetch_add(1, Ordering::Relaxed);
            let (target_width, target_height) = gpu_target_dimensions(request);
            let rgba = self
                .renderer
                .render_rgba_with_view(
                    &cached.scene,
                    target_width,
                    target_height,
                    scene_view_transform(cached.coverage, requested_coverage),
                )
                .map_err(|error| RasterizeError::Unavailable(error.to_string()))?;
            return crop_gpu_rgba(request, scene_crop(request), rgba);
        }
        self.scene_misses.fetch_add(1, Ordering::Relaxed);
        let prepared_request = gpu_coverage_request(request);
        let coverage = rendered_bbox(&prepared_request);
        let (prepared_width, prepared_height) = gpu_target_dimensions(&prepared_request);
        let scene_key = gpu_scene_key(identity, coverage, prepared_width, prepared_height);
        let mut archive = None;
        if self.mesh_cache.is_enabled() {
            match self.mesh_cache.load(scene_key) {
                Ok(Some(cached))
                    if cached.width == prepared_width && cached.height == prepared_height =>
                {
                    self.mesh_disk_hits.fetch_add(1, Ordering::Relaxed);
                    archive = Some(cached);
                }
                Ok(Some(_)) => {
                    self.mesh_disk_errors.fetch_add(1, Ordering::Relaxed);
                    self.mesh_disk_misses.fetch_add(1, Ordering::Relaxed);
                    let _ = self.mesh_cache.remove(scene_key);
                    warn!(
                        scene_key,
                        "discarded GPU mesh cache entry with unexpected dimensions"
                    );
                }
                Ok(None) => {
                    self.mesh_disk_misses.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    self.mesh_disk_errors.fetch_add(1, Ordering::Relaxed);
                    self.mesh_disk_misses.fetch_add(1, Ordering::Relaxed);
                    let _ = self.mesh_cache.remove(scene_key);
                    warn!(scene_key, %error, "discarded unreadable GPU mesh cache entry");
                }
            }
        }
        let archive = match archive {
            Some(archive) => archive,
            None => {
                let gdal_request = to_gdal_request(&prepared_request)?;
                let prepared = GdalEngine
                    .prepare_vector_scene(&gdal_request)
                    .map_err(|error| {
                        let message = error.to_string();
                        if message.contains("does not yet support") {
                            RasterizeError::Unsupported(message)
                        } else {
                            RasterizeError::Backend(message)
                        }
                    })?;
                let chunks = prepared_mesh_chunks(&prepared);
                let mut atlases = prepared_pattern_atlases(&prepared);
                atlases.extend(pack_placed_labels(
                    prepared.width,
                    prepared.height,
                    &prepared.labels,
                )?);
                let archive = GpuMeshArchive {
                    width: prepared.width,
                    height: prepared.height,
                    background: prepared.background,
                    chunks,
                    atlases,
                };
                if self.mesh_cache.is_enabled() {
                    match self.mesh_cache.store(scene_key, &archive) {
                        Ok(()) => {
                            self.mesh_disk_writes.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            self.mesh_disk_errors.fetch_add(1, Ordering::Relaxed);
                            warn!(scene_key, %error, "failed to persist GPU mesh cache entry");
                        }
                    }
                }
                archive
            }
        };
        let scene = Arc::new(
            self.renderer
                .prepare_with_background_and_atlases(
                    archive.width,
                    archive.height,
                    &archive.chunks,
                    archive.background,
                    &archive.atlases,
                )
                .map_err(classify_gpu_prepare_error)?,
        );
        let scene_bytes = scene.stats().estimated_gpu_bytes;
        let cached = CachedGpuScene {
            identity,
            coverage,
            resolution_x,
            resolution_y,
            scene: scene.clone(),
        };
        {
            let mut scenes = self
                .scenes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _evicted = scenes.insert(scene_key, cached, scene_bytes);
        }
        let (target_width, target_height) = gpu_target_dimensions(request);
        let rgba = self
            .renderer
            .render_rgba_with_view(
                &scene,
                target_width,
                target_height,
                scene_view_transform(coverage, requested_coverage),
            )
            .map_err(|error| RasterizeError::Unavailable(error.to_string()))?;
        crop_gpu_rgba(request, prepared_request.buffer_pixels, rgba)
    }
}

impl VectorRasterizer for GpuVectorRasterizer {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError> {
        encode_rgba(request, self.render_raw_rgba(request)?)
    }

    fn render_rgba(&self, request: &VectorRasterRequest) -> Result<RasterizedRgba, RasterizeError> {
        self.render_raw_rgba(request)
    }
}

fn classify_gpu_prepare_error(error: map_gpu::GpuError) -> RasterizeError {
    match error {
        map_gpu::GpuError::InvalidDimensions
        | map_gpu::GpuError::TextureDimensionsTooLarge { .. }
        | map_gpu::GpuError::MissingVertices { .. }
        | map_gpu::GpuError::InvalidIndex { .. }
        | map_gpu::GpuError::VertexBufferTooLarge { .. }
        | map_gpu::GpuError::IndexBufferTooLarge { .. }
        | map_gpu::GpuError::InvalidAtlasDimensions
        | map_gpu::GpuError::InvalidAtlasPixels { .. }
        | map_gpu::GpuError::InvalidAtlasIndex { .. } => {
            RasterizeError::Unsupported(error.to_string())
        }
        map_gpu::GpuError::AdapterNotFound
        | map_gpu::GpuError::NamedAdapterNotFound(_)
        | map_gpu::GpuError::Device(_)
        | map_gpu::GpuError::Readback(_) => RasterizeError::Unavailable(error.to_string()),
    }
}

fn prepared_pattern_atlases(scene: &cangling_gdal_core::PreparedVectorScene) -> Vec<AtlasOverlay> {
    scene
        .patterns
        .iter()
        .map(|pattern| AtlasOverlay {
            width: pattern.width,
            height: pattern.height,
            rgba: pattern.rgba.clone(),
            vertices: pattern
                .vertices
                .iter()
                .map(|vertex| AtlasVertex {
                    position: [
                        vertex.pixel_position[0] / scene.width as f32 * 2.0 - 1.0,
                        1.0 - vertex.pixel_position[1] / scene.height as f32 * 2.0,
                        0.0,
                        1.0,
                    ],
                    uv: vertex.uv,
                    _padding: [0.0; 2],
                    // Atlas pixels are premultiplied, so opacity must scale
                    // RGB and alpha together before premultiplied blending.
                    color: [vertex.opacity; 4],
                })
                .collect(),
            indices: pattern.indices.clone(),
            repeat: true,
            underlay: true,
        })
        .collect()
}

fn gpu_scene_identity(request: &VectorRasterRequest) -> u64 {
    let mut normalized = request.clone();
    normalized.rules.retain(|rule| {
        rule.minimum_scale_denominator
            .is_none_or(|minimum| request.scale_denominator >= minimum)
            && rule
                .maximum_scale_denominator
                .is_none_or(|maximum| request.scale_denominator < maximum)
    });
    for rule in &mut normalized.rules {
        rule.minimum_scale_denominator = None;
        rule.maximum_scale_denominator = None;
    }
    // The scene identity is determined by the resulting active rules, not an
    // exact floating-point scale value, so same-zoom requests can reuse it.
    normalized.scale_denominator = 0.0;
    normalized.bbox = BoundingBox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    normalized.width = 1;
    normalized.height = 1;
    normalized.format = ImageFormat::Png;
    let mut hasher = DefaultHasher::new();
    format!("{normalized:?}").hash(&mut hasher);
    hash_source_fingerprint(&request.source.path, &mut hasher);
    hash_style_assets(request, &mut hasher);
    hasher.finish()
}

fn hash_style_assets(request: &VectorRasterRequest, hasher: &mut impl Hasher) {
    for path in request
        .rules
        .iter()
        .filter_map(|rule| rule.paint.fill_image.as_ref())
        .map(|image| image.path.as_path())
    {
        hash_file_metadata(path, hasher);
    }
}

fn hash_source_fingerprint(path: &std::path::Path, hasher: &mut impl Hasher) {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical.hash(hasher);
    let extensions: &[&str] = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("shp"))
    {
        &["shp", "shx", "dbf", "prj", "cpg", "qix", "sbn", "sbx"]
    } else {
        &[]
    };
    if extensions.is_empty() {
        hash_file_metadata(path, hasher);
    } else {
        for extension in extensions {
            let sidecar = path.with_extension(extension);
            if sidecar.exists() {
                extension.hash(hasher);
                hash_file_metadata(&sidecar, hasher);
            }
        }
    }
}

fn hash_file_metadata(path: &std::path::Path, hasher: &mut impl Hasher) {
    path.hash(hasher);
    if let Ok(metadata) = std::fs::metadata(path) {
        metadata.len().hash(hasher);
        metadata.modified().ok().hash(hasher);
    }
}

fn gpu_scene_key(identity: u64, coverage: BoundingBox, width: u32, height: u32) -> u64 {
    let mut hasher = DefaultHasher::new();
    identity.hash(&mut hasher);
    for value in [
        coverage.min_x,
        coverage.min_y,
        coverage.max_x,
        coverage.max_y,
    ] {
        value.to_bits().hash(&mut hasher);
    }
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    hasher.finish()
}

fn gpu_coverage_request(request: &VectorRasterRequest) -> VectorRasterRequest {
    if !request.allow_view_reuse {
        return request.clone();
    }
    let factor = std::env::var("MAP_GPU_VIEW_OVERSCAN")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(1.5)
        .clamp(1.0, 3.0);
    if factor <= 1.0 {
        return request.clone();
    }
    let center_x = (request.bbox.min_x + request.bbox.max_x) * 0.5;
    let center_y = (request.bbox.min_y + request.bbox.max_y) * 0.5;
    let span_x = (request.bbox.max_x - request.bbox.min_x) * factor;
    let span_y = (request.bbox.max_y - request.bbox.min_y) * factor;
    let mut covered = request.clone();
    covered.bbox = BoundingBox {
        min_x: center_x - span_x * 0.5,
        min_y: center_y - span_y * 0.5,
        max_x: center_x + span_x * 0.5,
        max_y: center_y + span_y * 0.5,
    };
    covered.width = (f64::from(request.width) * factor).ceil() as u32;
    covered.height = (f64::from(request.height) * factor).ceil() as u32;
    covered
}

fn rendered_bbox(request: &VectorRasterRequest) -> BoundingBox {
    let buffer_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width)
        * f64::from(request.buffer_pixels);
    let buffer_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height)
        * f64::from(request.buffer_pixels);
    BoundingBox {
        min_x: request.bbox.min_x - buffer_x,
        min_y: request.bbox.min_y - buffer_y,
        max_x: request.bbox.max_x + buffer_x,
        max_y: request.bbox.max_y + buffer_y,
    }
}

fn resolution_matches(cached: f64, requested: f64) -> bool {
    let scale = cached.abs().max(requested.abs()).max(1.0);
    (cached - requested).abs() <= scale * 1.0e-9
}

fn bbox_contains(coverage: BoundingBox, requested: BoundingBox) -> bool {
    coverage.min_x <= requested.min_x
        && coverage.min_y <= requested.min_y
        && coverage.max_x >= requested.max_x
        && coverage.max_y >= requested.max_y
}

fn scene_view_transform(coverage: BoundingBox, requested: BoundingBox) -> ViewTransform {
    let coverage_span_x = coverage.max_x - coverage.min_x;
    let coverage_span_y = coverage.max_y - coverage.min_y;
    let requested_span_x = requested.max_x - requested.min_x;
    let requested_span_y = requested.max_y - requested.min_y;
    let coverage_center_x = (coverage.min_x + coverage.max_x) * 0.5;
    let coverage_center_y = (coverage.min_y + coverage.max_y) * 0.5;
    let requested_center_x = (requested.min_x + requested.max_x) * 0.5;
    let requested_center_y = (requested.min_y + requested.max_y) * 0.5;
    ViewTransform {
        scale_offset: [
            (coverage_span_x / requested_span_x) as f32,
            (coverage_span_y / requested_span_y) as f32,
            (2.0 * (coverage_center_x - requested_center_x) / requested_span_x) as f32,
            (2.0 * (coverage_center_y - requested_center_y) / requested_span_y) as f32,
        ],
    }
}

fn scene_crop(request: &VectorRasterRequest) -> u32 {
    request.buffer_pixels
}

fn gpu_target_dimensions(request: &VectorRasterRequest) -> (u32, u32) {
    (
        request
            .width
            .saturating_add(request.buffer_pixels.saturating_mul(2)),
        request
            .height
            .saturating_add(request.buffer_pixels.saturating_mul(2)),
    )
}

fn prepared_mesh_chunks(scene: &cangling_gdal_core::PreparedVectorScene) -> Vec<MeshChunk> {
    const TARGET_BYTES: usize = 64 * 1024 * 1024;
    let mut chunks = Vec::new();
    let mut chunk = MeshChunk::default();
    let mut remap = HashMap::<u32, u32>::new();
    for triangle in scene.indices.chunks_exact(3) {
        let missing = triangle
            .iter()
            .filter(|index| !remap.contains_key(index))
            .count();
        let extra =
            missing * std::mem::size_of::<Vertex>() + triangle.len() * std::mem::size_of::<u32>();
        if !chunk.indices.is_empty() && chunk.estimated_gpu_bytes() + extra > TARGET_BYTES {
            chunks.push(chunk);
            chunk = MeshChunk::default();
            remap.clear();
        }
        for &source_index in triangle {
            let destination = *remap.entry(source_index).or_insert_with(|| {
                let source = scene.vertices[source_index as usize];
                let index = chunk.vertices.len() as u32;
                chunk.vertices.push(Vertex {
                    position: [
                        source.pixel_position[0] / scene.width as f32 * 2.0 - 1.0,
                        1.0 - source.pixel_position[1] / scene.height as f32 * 2.0,
                        0.0,
                        1.0,
                    ],
                    color: source.color.map(|channel| f32::from(channel) / 255.0),
                });
                index
            });
            chunk.indices.push(destination);
        }
    }
    if !chunk.indices.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn pack_placed_labels(
    output_width: u32,
    output_height: u32,
    labels: &[cangling_gdal_core::PlacedLabel],
) -> Result<Vec<AtlasOverlay>, RasterizeError> {
    const WIDTH: u32 = 2048;
    const MAX_HEIGHT: u32 = 2048;
    const GAP: u32 = 2;
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let mut pages = Vec::<Vec<(usize, u32, u32)>>::new();
    let mut page = Vec::new();
    let (mut x, mut y, mut row_height) = (GAP, GAP, 0);
    for (index, label) in labels.iter().enumerate() {
        if label.image.width + GAP * 2 > WIDTH || label.image.height + GAP * 2 > MAX_HEIGHT {
            return Err(RasterizeError::Unsupported(
                "a rasterized label exceeds the GPU atlas page".into(),
            ));
        }
        if x + label.image.width + GAP > WIDTH {
            x = GAP;
            y += row_height + GAP;
            row_height = 0;
        }
        if y + label.image.height + GAP > MAX_HEIGHT {
            pages.push(page);
            page = Vec::new();
            x = GAP;
            y = GAP;
            row_height = 0;
        }
        page.push((index, x, y));
        x += label.image.width + GAP;
        row_height = row_height.max(label.image.height);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
        .into_iter()
        .map(|page| build_label_page(output_width, output_height, labels, &page))
        .collect()
}

fn build_label_page(
    output_width: u32,
    output_height: u32,
    labels: &[cangling_gdal_core::PlacedLabel],
    placements: &[(usize, u32, u32)],
) -> Result<AtlasOverlay, RasterizeError> {
    const WIDTH: u32 = 2048;
    const GAP: u32 = 2;
    let height = placements
        .iter()
        .map(|(index, _, y)| y + labels[*index].image.height + GAP)
        .max()
        .unwrap_or(1);
    let mut rgba = vec![0; (WIDTH * height * 4) as usize];
    let mut vertices = Vec::with_capacity(placements.len() * 4);
    let mut indices = Vec::with_capacity(placements.len() * 6);
    for &(index, atlas_x, atlas_y) in placements {
        let label = &labels[index];
        for row in 0..label.image.height {
            let source = (row * label.image.width * 4) as usize;
            let destination = (((atlas_y + row) * WIDTH + atlas_x) * 4) as usize;
            rgba[destination..destination + (label.image.width * 4) as usize].copy_from_slice(
                &label.image.rgba[source..source + (label.image.width * 4) as usize],
            );
        }
        let left = label.x as f32;
        let top = label.y as f32;
        let right = left + label.image.width as f32;
        let bottom = top + label.image.height as f32;
        let positions = [(left, top), (right, top), (right, bottom), (left, bottom)];
        let uvs = [
            [
                atlas_x as f32 / WIDTH as f32,
                atlas_y as f32 / height as f32,
            ],
            [
                (atlas_x + label.image.width) as f32 / WIDTH as f32,
                atlas_y as f32 / height as f32,
            ],
            [
                (atlas_x + label.image.width) as f32 / WIDTH as f32,
                (atlas_y + label.image.height) as f32 / height as f32,
            ],
            [
                atlas_x as f32 / WIDTH as f32,
                (atlas_y + label.image.height) as f32 / height as f32,
            ],
        ];
        let base = vertices.len() as u32;
        for ((x, y), uv) in positions.into_iter().zip(uvs) {
            vertices.push(AtlasVertex {
                position: [
                    x / output_width as f32 * 2.0 - 1.0,
                    1.0 - y / output_height as f32 * 2.0,
                    0.0,
                    1.0,
                ],
                uv,
                _padding: [0.0; 2],
                color: [1.0; 4],
            });
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Ok(AtlasOverlay {
        width: WIDTH,
        height,
        rgba,
        vertices,
        indices,
        repeat: false,
        underlay: false,
    })
}

fn crop_gpu_rgba(
    request: &VectorRasterRequest,
    crop_pixels: u32,
    rgba: Vec<u8>,
) -> Result<RasterizedRgba, RasterizeError> {
    let width = request.width.saturating_add(crop_pixels.saturating_mul(2));
    let height = request.height.saturating_add(crop_pixels.saturating_mul(2));
    let mut image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| RasterizeError::Backend("invalid GPU RGBA output".into()))?;
    if crop_pixels > 0 {
        image = image::imageops::crop_imm(
            &image,
            crop_pixels,
            crop_pixels,
            request.width,
            request.height,
        )
        .to_image();
    }
    Ok(RasterizedRgba {
        width: request.width,
        height: request.height,
        pixels: image.into_raw(),
    })
}

fn encode_rgba(
    request: &VectorRasterRequest,
    rgba: RasterizedRgba,
) -> Result<RasterizedImage, RasterizeError> {
    let image = image::RgbaImage::from_raw(rgba.width, rgba.height, rgba.pixels)
        .ok_or_else(|| RasterizeError::Backend("invalid GPU RGBA output".into()))?;
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut bytes,
            match request.format {
                ImageFormat::Png => image::ImageFormat::Png,
                ImageFormat::Jpeg => image::ImageFormat::Jpeg,
            },
        )
        .map_err(|error| RasterizeError::Backend(error.to_string()))?;
    Ok(RasterizedImage {
        bytes: bytes.into_inner(),
        content_type: match request.format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        },
    })
}

fn has_fill(rule: &GdalVectorRenderRule) -> bool {
    rule.fill.is_some() || rule.fill_field.is_some() || rule.fill_image.is_some()
}

fn optimize_image_rules(mut rules: Vec<GdalVectorRenderRule>) -> Vec<GdalVectorRenderRule> {
    // Text is an independent symbolizer. Keep its original filter when fill
    // rules are made mutually exclusive; otherwise a base label rule would
    // inherit `NOT (all classified fills)` and match no features.
    let mut text_rules = Vec::new();
    for rule in &mut rules {
        if rule.text.is_some() && has_fill(rule) {
            let mut text_rule = rule.clone();
            text_rule.fill = None;
            text_rule.fill_field = None;
            text_rule.fill_image = None;
            text_rule.stroke = None;
            text_rule.stroke_field = None;
            text_rule.stroke_width = 0.0;
            text_rule.stroke_dasharray.clear();
            text_rules.push(text_rule);
            rule.text = None;
        }
    }
    let mut base_stroke = None;
    for rule in &mut rules {
        if rule.where_clause.is_none() {
            base_stroke = Some((
                rule.stroke,
                rule.stroke_field.clone(),
                rule.stroke_width,
                rule.stroke_dasharray.clone(),
            ));
        } else if has_fill(rule) && rule.stroke.is_none() && rule.stroke_field.is_none() {
            if let Some((stroke, stroke_field, stroke_width, stroke_dasharray)) = &base_stroke {
                rule.stroke = *stroke;
                rule.stroke_field = stroke_field.clone();
                rule.stroke_width = *stroke_width;
                rule.stroke_dasharray = stroke_dasharray.clone();
            }
        }
    }

    for index in 0..rules.len() {
        if !has_fill(&rules[index]) {
            continue;
        }
        let later_fills = rules[index + 1..]
            .iter()
            .filter(|rule| has_fill(rule))
            .filter_map(|rule| rule.where_clause.as_deref())
            .map(|filter| format!("({filter})"))
            .collect::<Vec<_>>();
        let later_filters = rules[index + 1..]
            .iter()
            .filter(|rule| has_fill(rule))
            .filter_map(|rule| rule.filter.clone())
            .collect::<Vec<_>>();
        if later_fills.is_empty() {
            continue;
        }
        let exclusion = format!("NOT ({})", later_fills.join(" OR "));
        rules[index].where_clause = Some(match rules[index].where_clause.take() {
            Some(current) => format!("({current}) AND {exclusion}"),
            None => exclusion,
        });
        if !later_filters.is_empty() {
            let exclusion = VectorFilter::Not {
                filter: Box::new(if later_filters.len() == 1 {
                    later_filters.into_iter().next().expect("one filter")
                } else {
                    VectorFilter::Or {
                        filters: later_filters,
                    }
                }),
            };
            rules[index].filter = Some(match rules[index].filter.take() {
                Some(current) => VectorFilter::And {
                    filters: vec![current, exclusion],
                },
                None => exclusion,
            });
        }
    }
    rules.extend(text_rules);
    rules
}

fn filter_to_gdal(filter: &Filter) -> VectorFilter {
    match filter {
        Filter::Comparison {
            property,
            operator,
            value,
        } => VectorFilter::Comparison {
            property: property.clone(),
            operator: match operator {
                ComparisonOperator::Equal => VectorComparisonOperator::Equal,
                ComparisonOperator::NotEqual => VectorComparisonOperator::NotEqual,
                ComparisonOperator::LessThan => VectorComparisonOperator::LessThan,
                ComparisonOperator::LessThanOrEqual => VectorComparisonOperator::LessThanOrEqual,
                ComparisonOperator::GreaterThan => VectorComparisonOperator::GreaterThan,
                ComparisonOperator::GreaterThanOrEqual => {
                    VectorComparisonOperator::GreaterThanOrEqual
                }
            },
            value: match value {
                Literal::String(value) => VectorLiteral::String(value.clone()),
                Literal::Number(value) => VectorLiteral::Number(*value),
                Literal::Boolean(value) => VectorLiteral::Boolean(*value),
            },
        },
        Filter::And { filters } => VectorFilter::And {
            filters: filters.iter().map(filter_to_gdal).collect(),
        },
    }
}

fn filter_to_ogr_sql(filter: &Filter) -> Result<String, RasterizeError> {
    match filter {
        Filter::Comparison {
            property,
            operator,
            value,
        } => {
            if property.is_empty()
                || !property
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(RasterizeError::InvalidRequest(format!(
                    "invalid filter property: {property}"
                )));
            }
            let operator = match operator {
                ComparisonOperator::Equal => "=",
                ComparisonOperator::NotEqual => "<>",
                ComparisonOperator::LessThan => "<",
                ComparisonOperator::LessThanOrEqual => "<=",
                ComparisonOperator::GreaterThan => ">",
                ComparisonOperator::GreaterThanOrEqual => ">=",
            };
            let value = match value {
                Literal::String(value) => format!("'{}'", value.replace('\'', "''")),
                Literal::Number(value) if value.is_finite() => value.to_string(),
                Literal::Number(_) => {
                    return Err(RasterizeError::InvalidRequest(
                        "filter number must be finite".into(),
                    ))
                }
                Literal::Boolean(value) => {
                    if *value {
                        "TRUE".into()
                    } else {
                        "FALSE".into()
                    }
                }
            };
            Ok(format!("\"{property}\" {operator} {value}"))
        }
        Filter::And { filters } => filters
            .iter()
            .map(filter_to_ogr_sql)
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| format!("({})", parts.join(" AND "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu_request() -> VectorRasterRequest {
        VectorRasterRequest {
            source: map_render::VectorSource {
                path: "fixture.shp".into(),
                layer: None,
                encoding: None,
            },
            target_crs: "EPSG:3857".into(),
            bbox: BoundingBox {
                min_x: 10.0,
                min_y: 20.0,
                max_x: 30.0,
                max_y: 40.0,
            },
            width: 1000,
            height: 500,
            buffer_pixels: 0,
            allow_view_reuse: true,
            simplify_tolerance: None,
            format: ImageFormat::Png,
            rules: Vec::new(),
            scale_denominator: 10_000.0,
            background: map_core::Rgba {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 0,
            },
        }
    }

    #[test]
    fn overscanned_gpu_coverage_preserves_resolution_and_contains_small_pans() {
        let request = gpu_request();
        let covered = gpu_coverage_request(&request);

        assert_eq!(covered.width, 1500);
        assert_eq!(covered.height, 750);
        assert_eq!(covered.bbox.min_x, 5.0);
        assert_eq!(covered.bbox.min_y, 15.0);
        assert_eq!(covered.bbox.max_x, 35.0);
        assert_eq!(covered.bbox.max_y, 45.0);
        assert!(resolution_matches(
            (covered.bbox.max_x - covered.bbox.min_x) / f64::from(covered.width),
            (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width),
        ));
        assert!(bbox_contains(covered.bbox, request.bbox));
        assert!(bbox_contains(
            covered.bbox,
            BoundingBox {
                min_x: 12.0,
                min_y: 21.0,
                max_x: 32.0,
                max_y: 41.0,
            }
        ));
        assert!(!bbox_contains(
            covered.bbox,
            BoundingBox {
                min_x: 16.0,
                min_y: 21.0,
                max_x: 36.0,
                max_y: 41.0,
            }
        ));
    }

    #[test]
    fn view_transform_maps_coverage_center_and_pan_to_requested_view() {
        let coverage = BoundingBox {
            min_x: 5.0,
            min_y: 15.0,
            max_x: 35.0,
            max_y: 45.0,
        };
        let centered = scene_view_transform(coverage, gpu_request().bbox);
        assert_eq!(centered.scale_offset, [1.5, 1.5, 0.0, 0.0]);

        let panned = scene_view_transform(
            coverage,
            BoundingBox {
                min_x: 12.0,
                min_y: 21.0,
                max_x: 32.0,
                max_y: 41.0,
            },
        );
        assert_eq!(panned.scale_offset, [1.5, 1.5, -0.2, -0.1]);
    }

    #[test]
    fn buffered_wms_view_reuse_compares_the_actual_rendered_extent() {
        let mut request = gpu_request();
        request.buffer_pixels = 64;
        let covered = gpu_coverage_request(&request);
        let covered_extent = rendered_bbox(&covered);
        let mut panned = request.clone();
        panned.bbox.min_x += 2.0;
        panned.bbox.max_x += 2.0;
        panned.bbox.min_y += 1.0;
        panned.bbox.max_y += 1.0;

        assert!(bbox_contains(covered_extent, rendered_bbox(&request)));
        assert!(bbox_contains(covered_extent, rendered_bbox(&panned)));
    }

    #[test]
    fn gpu_scene_identity_ignores_scale_drift_when_active_rules_are_unchanged() {
        let first = gpu_request();
        let mut panned = first.clone();
        panned.bbox.min_x += 1.0;
        panned.bbox.max_x += 1.0;
        panned.scale_denominator += 123.456;

        assert_eq!(gpu_scene_identity(&first), gpu_scene_identity(&panned));
    }

    fn image_rule(where_clause: Option<&str>, path: &str) -> GdalVectorRenderRule {
        GdalVectorRenderRule {
            where_clause: where_clause.map(str::to_owned),
            filter: None,
            fill: None,
            fill_field: None,
            fill_image: Some(cangling_gdal_core::VectorImageFill {
                path: path.into(),
                scale: 1.0,
                opacity: 1.0,
            }),
            stroke: None,
            stroke_field: None,
            stroke_width: 1.0,
            stroke_dasharray: Vec::new(),
            text: None,
        }
    }

    #[test]
    fn translates_nested_filters_to_safe_ogr_sql() {
        let filter = Filter::And {
            filters: vec![
                Filter::Comparison {
                    property: "kind".into(),
                    operator: ComparisonOperator::Equal,
                    value: Literal::String("rock'fall".into()),
                },
                Filter::Comparison {
                    property: "level".into(),
                    operator: ComparisonOperator::GreaterThanOrEqual,
                    value: Literal::Number(3.0),
                },
            ],
        };
        assert_eq!(
            filter_to_ogr_sql(&filter).unwrap(),
            "(\"kind\" = 'rock''fall' AND \"level\" >= 3)"
        );
    }

    #[test]
    fn makes_ordered_image_rules_mutually_exclusive_and_inherits_base_stroke() {
        let mut base = image_rule(None, "base.png");
        base.stroke = Some([12, 34, 56, 255]);
        base.stroke_width = 2.0;
        base.text = Some(GdalVectorTextStyle {
            field: "name".into(),
            font_family: "Noto Sans CJK SC".into(),
            font_size: 14.0,
            font_italic: false,
            font_bold: false,
            fill: [0, 0, 0, 255],
            halo: [255, 255, 255, 255],
            halo_radius: 2.0,
            spacing: 8.0,
            min_area: 900.0,
            repeat_distance: 160.0,
            repeat_count: 1,
        });
        let mut missing_image_fallback = image_rule(Some("\"type\" = 2"), "missing.png");
        missing_image_fallback.fill_image = None;
        missing_image_fallback.fill = Some([176, 176, 176, 255]);
        let rules = optimize_image_rules(vec![
            base,
            image_rule(Some("\"type\" = 1"), "one.png"),
            missing_image_fallback,
        ]);

        assert_eq!(
            rules[0].where_clause.as_deref(),
            Some("NOT ((\"type\" = 1) OR (\"type\" = 2))")
        );
        assert_eq!(
            rules[1].where_clause.as_deref(),
            Some("(\"type\" = 1) AND NOT ((\"type\" = 2))")
        );
        assert_eq!(rules[1].stroke, Some([12, 34, 56, 255]));
        assert_eq!(rules[1].stroke_width, 2.0);
        assert_eq!(rules.len(), 4);
        assert!(rules[0].text.is_none());
        assert_eq!(rules[3].where_clause, None);
        assert_eq!(
            rules[3].text.as_ref().map(|text| text.field.as_str()),
            Some("name")
        );
        assert!(!has_fill(&rules[3]));
    }
}

#[derive(Debug, Serialize)]
pub struct SourceInspection {
    pub native_crs: String,
    pub native_extent: BoundingBox,
}

pub fn preview_vector_features(
    path: &str,
    layer_name: Option<&str>,
    encoding: Option<&str>,
    limit: usize,
) -> Result<cangling_gdal_core::VectorFeaturePreview, String> {
    GdalEngine
        .preview_vector_features(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            limit,
        )
        .map_err(|error| error.to_string())
}

pub fn vector_field_stats(
    path: &str,
    layer_name: Option<&str>,
    encoding: Option<&str>,
    field: &str,
) -> Result<cangling_gdal_core::VectorFieldStats, String> {
    GdalEngine
        .vector_field_stats(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            field,
            500,
        )
        .map_err(|error| error.to_string())
}

pub fn inspect_source(
    path: &str,
    kind: DatasetKind,
    layer_name: Option<&str>,
    encoding: Option<&str>,
) -> Result<SourceInspection, String> {
    let info = GdalEngine
        .inspect(
            &DataSource {
                path: path.into(),
                layer: layer_name.map(str::to_owned),
                encoding: encoding.map(str::to_owned),
            },
            kind,
        )
        .map_err(|error| error.to_string())?;
    Ok(SourceInspection {
        native_crs: info.native_crs,
        native_extent: BoundingBox {
            min_x: info.native_extent.min_x,
            min_y: info.native_extent.min_y,
            max_x: info.native_extent.max_x,
            max_y: info.native_extent.max_y,
        },
    })
}
