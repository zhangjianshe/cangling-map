use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    io::Cursor,
    path::{Path as FsPath, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Instant, SystemTime},
};

use gdal::{
    vector::{Feature, Geometry, LayerAccess, OGRwkbGeometryType::wkbPoint},
    Dataset, Metadata,
};
use image::{DynamicImage, ImageFormat as EncoderFormat, RgbImage};
use tiny_skia::{
    Color, FillRule, FilterQuality, IntRect, Paint, Path, PathBuilder, Pattern, Pixmap, SpreadMode,
    Stroke, StrokeDash, Transform,
};

use crate::{
    GdalError, ImageFormat, PlacedLabel, PreparedPatternOverlay, PreparedPatternVertex,
    PreparedVectorScene, PreparedVectorVertex, RasterizedLabel, RasterizedRgba, RenderedImage,
    VectorComparisonOperator, VectorFilter, VectorLiteral, VectorRenderRequest, VectorTextStyle,
};

pub(crate) fn render_projected(
    dataset: &Dataset,
    request: &VectorRenderRequest,
) -> Result<RenderedImage, GdalError> {
    let pixmap = rasterize_projected(dataset, request)?;
    let encode_started = Instant::now();
    let image = encode(pixmap, request.format)?;
    tracing::debug!(
        encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0,
        "vector image encoded"
    );
    Ok(image)
}

pub(crate) fn render_projected_rgba(
    dataset: &Dataset,
    request: &VectorRenderRequest,
) -> Result<RasterizedRgba, GdalError> {
    let pixmap = rasterize_projected(dataset, request)?;
    let width = pixmap.width();
    let height = pixmap.height();
    let mut pixels = pixmap.data().to_vec();
    unpremultiply_rgba(&mut pixels);
    Ok(RasterizedRgba {
        width,
        height,
        pixels,
    })
}

fn rasterize_projected(
    dataset: &Dataset,
    request: &VectorRenderRequest,
) -> Result<Pixmap, GdalError> {
    let started = Instant::now();
    let render_width = request
        .width
        .checked_add(request.buffer_pixels.saturating_mul(2))
        .ok_or_else(|| GdalError::InvalidRequest("buffered image width is too large".into()))?;
    let render_height = request
        .height
        .checked_add(request.buffer_pixels.saturating_mul(2))
        .ok_or_else(|| GdalError::InvalidRequest("buffered image height is too large".into()))?;
    let mut pixmap = Pixmap::new(render_width, render_height)
        .ok_or_else(|| GdalError::InvalidRequest("image dimensions are too large".into()))?;
    pixmap.fill(color(request.background));
    let mut layer = dataset.layer(0)?;
    let buffer_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width)
        * f64::from(request.buffer_pixels);
    let buffer_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height)
        * f64::from(request.buffer_pixels);
    layer.set_spatial_filter_rect(
        request.bbox.min_x - buffer_x,
        request.bbox.min_y - buffer_y,
        request.bbox.max_x + buffer_x,
        request.bbox.max_y + buffer_y,
    );
    let mut stats = RenderStats::default();
    let dataset_id = label_dataset_id(dataset);
    let pixel_transform = PixelTransform::new(request);
    let query_started = Instant::now();
    let single_pass = request.rules.iter().all(|rule| {
        // Filled polygons cannot be merged into one compound path safely.
        // Overlapping features (or mixed ring orientations) can turn valid
        // polygons into holes under a single winding fill operation.
        rule.fill.is_none()
            && rule.fill_image.is_none()
            && rule.fill_field.is_none()
            && rule.stroke_field.is_none()
            && (rule.where_clause.is_none() || rule.filter.is_some())
    });
    if single_pass {
        render_rules_single_pass(
            &mut layer,
            &mut pixmap,
            request,
            &pixel_transform,
            dataset_id,
            &mut stats,
        )?;
    } else {
        render_rules_with_ogr_filters(
            &mut layer,
            &mut pixmap,
            request,
            &pixel_transform,
            dataset_id,
            &mut stats,
        )?;
    }
    let geometry_ms = query_started.elapsed().as_secs_f64() * 1000.0;
    // Label collision decisions need a wider neighborhood than polygon drawing.
    // Query it separately without allocating a larger pixmap, so both sides of
    // a tile seam see the same directly competing labels.
    let label_context_pixels = request
        .rules
        .iter()
        .filter_map(|rule| rule.text.as_ref())
        .map(|text| {
            text.repeat_distance
                .max(text.spacing + text.font_size * 4.0)
        })
        .fold(128.0_f32, f32::max)
        .ceil()
        .min(512.0) as u32;
    let label_buffer_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width)
        * f64::from(label_context_pixels);
    let label_buffer_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height)
        * f64::from(label_context_pixels);
    layer.set_spatial_filter_rect(
        request.bbox.min_x - label_buffer_x,
        request.bbox.min_y - label_buffer_y,
        request.bbox.max_x + label_buffer_x,
        request.bbox.max_y + label_buffer_y,
    );
    let labels_started = Instant::now();
    render_labels(
        &mut layer,
        &mut pixmap,
        request,
        &pixel_transform,
        dataset_id,
        &mut stats,
    )?;
    let labels_ms = labels_started.elapsed().as_secs_f64() * 1000.0;
    let query_and_draw_ms = query_started.elapsed().as_secs_f64() * 1000.0;
    layer.clear_attribute_filter();
    layer.clear_spatial_filter();
    let draw_elapsed = started.elapsed();
    let pixmap = if request.buffer_pixels == 0 {
        pixmap
    } else {
        let rect = IntRect::from_xywh(
            request.buffer_pixels as i32,
            request.buffer_pixels as i32,
            request.width,
            request.height,
        )
        .ok_or_else(|| GdalError::InvalidRequest("invalid buffered crop rectangle".into()))?;
        pixmap
            .clone_rect(rect)
            .ok_or_else(|| GdalError::InvalidRequest("failed to crop buffered image".into()))?
    };
    tracing::debug!(
        features = stats.features,
        polygons = stats.polygons,
        rings = stats.rings,
        skipped_rings = stats.skipped_rings,
        batched_rules = stats.batched_rules,
        labels = stats.labels,
        rejected_labels = stats.rejected_labels,
        label_anchor_cache_hits = stats.label_anchor_cache_hits,
        label_anchor_cache_misses = stats.label_anchor_cache_misses,
        geometry_cache_hits = stats.geometry_cache_hits,
        geometry_cache_misses = stats.geometry_cache_misses,
        single_pass,
        buffer_pixels = request.buffer_pixels,
        geometry_ms,
        labels_ms,
        query_and_draw_ms,
        draw_ms = draw_elapsed.as_secs_f64() * 1000.0,
        "vector pixels rasterized"
    );
    Ok(pixmap)
}

fn render_rules_with_ogr_filters(
    layer: &mut impl LayerAccess,
    pixmap: &mut Pixmap,
    request: &VectorRenderRequest,
    pixel_transform: &PixelTransform,
    dataset_id: u64,
    stats: &mut RenderStats,
) -> Result<(), GdalError> {
    for rule in &request.rules {
        if let Some(filter) = &rule.where_clause {
            layer.set_attribute_filter(filter)?;
        } else {
            layer.clear_attribute_filter();
        }
        layer.reset_feature_reading();
        if layer.feature_count() == 0 {
            continue;
        }
        let fill_index = field_index(layer, rule.fill_field.as_ref().map(|v| v.0.as_str()))?;
        let stroke_index = field_index(layer, rule.stroke_field.as_ref().map(|v| v.0.as_str()))?;
        let fill_image = rule
            .fill_image
            .as_ref()
            .map(|image| load_pattern(&image.path))
            .transpose()?;
        let fill_image_paint = fill_image.as_deref().map(|image| {
            pattern_paint(
                image,
                rule.fill_image.as_ref().map_or(1.0, |image| image.scale),
                rule.fill_image.as_ref().map_or(1.0, |image| image.opacity),
                &pixel_transform,
            )
        });
        // Only stroke-only rules are safe to batch. Combining filled feature
        // paths changes their winding semantics and may punch holes where
        // source polygons overlap.
        let batchable = rule.fill.is_none()
            && rule.fill_field.is_none()
            && rule.fill_image.is_none()
            && rule.stroke_field.is_none();
        let mut batched_path = batchable.then(PathBuilder::new);
        for feature in layer.features() {
            stats.features += 1;
            let fid = feature.fid();
            let Some(geometry) = feature.geometry() else {
                continue;
            };
            let geometry =
                cached_geometry(dataset_id, fid, geometry, request.simplify_tolerance, stats)?;
            if let Some(builder) = batched_path.as_mut() {
                append_cached_geometry(builder, &geometry, pixel_transform, stats);
                continue;
            }
            let fill = resolve_color(&feature, rule.fill, rule.fill_field.as_ref(), fill_index)?;
            let stroke = resolve_color(
                &feature,
                rule.stroke,
                rule.stroke_field.as_ref(),
                stroke_index,
            )?;
            draw_cached_geometry(
                pixmap,
                &geometry,
                &pixel_transform,
                fill,
                fill_image_paint.as_ref(),
                stroke,
                rule.stroke_width,
                &rule.stroke_dasharray,
                stats,
            );
        }
        if let Some(path) = batched_path.and_then(PathBuilder::finish) {
            stats.batched_rules += 1;
            draw_path(
                pixmap,
                &path,
                rule.fill,
                fill_image_paint.as_ref(),
                rule.stroke,
                rule.stroke_width,
                &rule.stroke_dasharray,
                FillRule::Winding,
            );
        }
    }
    Ok(())
}

fn simplify_for_drawing(
    geometry: &Geometry,
    tolerance: Option<f64>,
) -> Result<Option<Geometry>, GdalError> {
    tolerance
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| geometry.simplify(value).map_err(GdalError::from))
        .transpose()
}

#[derive(Clone)]
enum CachedGeometryPart {
    Polygon(Vec<Vec<(f64, f64)>>),
    Line(Vec<(f64, f64)>),
}

struct CachedGeometry {
    parts: Vec<CachedGeometryPart>,
    estimated_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GeometryCacheKey {
    dataset_id: u64,
    fid: u64,
    simplify_tolerance: Option<u64>,
}

#[derive(Default)]
struct GeometryCacheState {
    entries: HashMap<GeometryCacheKey, Arc<CachedGeometry>>,
    estimated_bytes: usize,
}

fn cached_geometry(
    dataset_id: u64,
    fid: Option<u64>,
    geometry: &Geometry,
    simplify_tolerance: Option<f64>,
    stats: &mut RenderStats,
) -> Result<Arc<CachedGeometry>, GdalError> {
    let key = fid.map(|fid| GeometryCacheKey {
        dataset_id,
        fid,
        simplify_tolerance: simplify_tolerance.map(f64::to_bits),
    });
    let cache = GEOMETRY_CACHE.get_or_init(|| Mutex::new(GeometryCacheState::default()));
    if let Some(key) = key {
        if let Some(geometry) = cache
            .lock()
            .expect("geometry cache poisoned")
            .entries
            .get(&key)
            .cloned()
        {
            stats.geometry_cache_hits += 1;
            return Ok(geometry);
        }
    }

    stats.geometry_cache_misses += 1;
    let simplified = simplify_for_drawing(geometry, simplify_tolerance)?;
    let geometry = simplified.as_ref().unwrap_or(geometry);
    let mut parts = Vec::new();
    collect_cached_geometry(geometry, &mut parts);
    let point_count = parts
        .iter()
        .map(|part| match part {
            CachedGeometryPart::Polygon(rings) => rings.iter().map(Vec::len).sum::<usize>(),
            CachedGeometryPart::Line(points) => points.len(),
        })
        .sum::<usize>();
    let estimated_bytes = point_count * std::mem::size_of::<(f64, f64)>()
        + parts.len() * std::mem::size_of::<CachedGeometryPart>();
    let geometry = Arc::new(CachedGeometry {
        parts,
        estimated_bytes,
    });

    if let Some(key) = key {
        let budget = geometry_cache_budget();
        if geometry.estimated_bytes <= budget {
            let mut cache = cache.lock().expect("geometry cache poisoned");
            if cache
                .estimated_bytes
                .saturating_add(geometry.estimated_bytes)
                > budget
            {
                cache.entries.clear();
                cache.estimated_bytes = 0;
            }
            if let Some(previous) = cache.entries.get(&key) {
                cache.estimated_bytes = cache
                    .estimated_bytes
                    .saturating_sub(previous.estimated_bytes);
            }
            cache.estimated_bytes = cache
                .estimated_bytes
                .saturating_add(geometry.estimated_bytes);
            cache.entries.insert(key, geometry.clone());
        }
    }
    Ok(geometry)
}

fn geometry_cache_budget() -> usize {
    *GEOMETRY_CACHE_BUDGET.get_or_init(|| {
        std::env::var("MAP_VECTOR_GEOMETRY_CACHE_MB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(512)
            .saturating_mul(1024 * 1024)
    })
}

fn collect_cached_geometry(geometry: &Geometry, parts: &mut Vec<CachedGeometryPart>) {
    match geometry.geometry_name().to_ascii_uppercase().as_str() {
        "POLYGON" => {
            let rings = (0..geometry.geometry_count())
                .map(|index| {
                    let ring = geometry.get_geometry(index);
                    let mut points = Vec::with_capacity(ring.point_count());
                    ring.get_points(&mut points);
                    points
                        .into_iter()
                        .map(|(x, y, _)| (x, y))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            parts.push(CachedGeometryPart::Polygon(rings));
        }
        "MULTIPOLYGON" | "GEOMETRYCOLLECTION" | "MULTILINESTRING" => {
            for index in 0..geometry.geometry_count() {
                collect_cached_geometry(&geometry.get_geometry(index), parts);
            }
        }
        "LINESTRING" => {
            let mut points = Vec::with_capacity(geometry.point_count());
            geometry.get_points(&mut points);
            parts.push(CachedGeometryPart::Line(
                points.into_iter().map(|(x, y, _)| (x, y)).collect(),
            ));
        }
        _ => {}
    }
}

fn render_rules_single_pass(
    layer: &mut impl LayerAccess,
    pixmap: &mut Pixmap,
    request: &VectorRenderRequest,
    pixel_transform: &PixelTransform,
    dataset_id: u64,
    stats: &mut RenderStats,
) -> Result<(), GdalError> {
    let mut field_indices = HashMap::new();
    for filter in request.rules.iter().filter_map(|rule| rule.filter.as_ref()) {
        collect_filter_fields(filter, &mut |property| {
            if !field_indices.contains_key(property) {
                let index = layer
                    .defn()
                    .field_index(property)
                    .map_err(GdalError::from)?;
                field_indices.insert(property.to_owned(), index);
            }
            Ok(())
        })?;
    }
    let mut builders = request
        .rules
        .iter()
        .map(|_| PathBuilder::new())
        .collect::<Vec<_>>();
    let combined_filter = request
        .rules
        .iter()
        .map(|rule| rule.where_clause.as_deref())
        .collect::<Option<Vec<_>>>()
        .filter(|filters| !filters.is_empty())
        .map(|filters| {
            filters
                .into_iter()
                .map(|filter| format!("({filter})"))
                .collect::<Vec<_>>()
                .join(" OR ")
        });
    if let Some(filter) = combined_filter.as_deref() {
        layer.set_attribute_filter(filter)?;
    } else {
        layer.clear_attribute_filter();
    }
    layer.reset_feature_reading();
    for feature in layer.features() {
        stats.features += 1;
        let fid = feature.fid();
        let Some(geometry) = feature.geometry() else {
            continue;
        };
        let geometry =
            cached_geometry(dataset_id, fid, geometry, request.simplify_tolerance, stats)?;
        for (index, rule) in request.rules.iter().enumerate() {
            if let Some(filter) = &rule.filter {
                if !filter_matches(&feature, filter, &field_indices)? {
                    continue;
                }
            }
            append_cached_geometry(&mut builders[index], &geometry, pixel_transform, stats);
        }
    }
    for (rule, builder) in request.rules.iter().zip(builders) {
        let Some(path) = builder.finish() else {
            continue;
        };
        stats.batched_rules += 1;
        let fill_image = rule
            .fill_image
            .as_ref()
            .map(|image| load_pattern(&image.path))
            .transpose()?;
        let fill_image_paint = fill_image.as_deref().map(|image| {
            pattern_paint(
                image,
                rule.fill_image.as_ref().map_or(1.0, |image| image.scale),
                rule.fill_image.as_ref().map_or(1.0, |image| image.opacity),
                pixel_transform,
            )
        });
        draw_path(
            pixmap,
            &path,
            rule.fill,
            fill_image_paint.as_ref(),
            rule.stroke,
            rule.stroke_width,
            &rule.stroke_dasharray,
            FillRule::Winding,
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct LabelBounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl LabelBounds {
    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }
}

#[derive(Clone)]
struct LabelCandidate {
    key: (u64, u64, String),
    path: Arc<Path>,
    bounds: LabelBounds,
    fill: [u8; 4],
    halo: [u8; 4],
    halo_radius: f32,
    anchor: (f32, f32),
    repeat_distance: f32,
    bold: bool,
}

fn render_labels(
    layer: &mut impl LayerAccess,
    pixmap: &mut Pixmap,
    request: &VectorRenderRequest,
    pixel_transform: &PixelTransform,
    label_dataset_id: u64,
    stats: &mut RenderStats,
) -> Result<(), GdalError> {
    visit_labels(
        layer,
        request,
        pixel_transform,
        label_dataset_id,
        stats,
        |candidate| {
            draw_label_candidate(pixmap, candidate, Transform::identity());
        },
    )
}

fn visit_labels(
    layer: &mut impl LayerAccess,
    request: &VectorRenderRequest,
    pixel_transform: &PixelTransform,
    label_dataset_id: u64,
    stats: &mut RenderStats,
    mut accept: impl FnMut(&LabelCandidate),
) -> Result<(), GdalError> {
    let mut candidates = Vec::new();
    for rule in request.rules.iter().filter(|rule| rule.text.is_some()) {
        let text_style = rule.text.as_ref().expect("filtered text style");
        if let Some(filter) = &rule.where_clause {
            layer.set_attribute_filter(filter)?;
        } else {
            layer.clear_attribute_filter();
        }
        let field_index = layer.defn().field_index(&text_style.field)?;
        layer.reset_feature_reading();
        for feature in layer.features() {
            let fid = feature.fid();
            let Some(label) = feature.field_as_string(field_index)? else {
                continue;
            };
            let label = label.trim();
            if label.is_empty() {
                continue;
            }
            let Some(geometry) = feature.geometry() else {
                continue;
            };
            let (world_area, label_points) = cached_geometry_label_points(
                label_dataset_id,
                fid,
                geometry,
                pixel_transform,
                text_style.repeat_distance,
                text_style.repeat_count,
                stats,
            );
            let pixel_area =
                world_area * pixel_transform.scale_x.abs() * pixel_transform.scale_y.abs();
            if pixel_area < f64::from(text_style.min_area) {
                stats.rejected_labels += 1;
                continue;
            }
            for &(world_x, world_y) in label_points.iter() {
                let anchor = pixel_transform.apply(world_x, world_y);
                if let Some(candidate) = shape_label(label, anchor, (world_x, world_y), text_style)?
                {
                    candidates.push(candidate);
                }
            }
        }
    }
    // A world-coordinate key makes label priority independent from source query
    // order, so adjacent tiles make the same collision decisions.
    candidates.sort_by(|left, right| left.key.cmp(&right.key));
    let mut prior_bounds = Vec::<LabelBounds>::new();
    let mut prior_repeats = Vec::<(String, (f32, f32))>::new();
    for candidate in candidates {
        if candidate.repeat_distance > 0.0
            && prior_repeats.iter().any(|(text, anchor)| {
                text == &candidate.key.2
                    && (anchor.0 - candidate.anchor.0).hypot(anchor.1 - candidate.anchor.1)
                        < candidate.repeat_distance
            })
        {
            stats.rejected_labels += 1;
            prior_bounds.push(candidate.bounds);
            prior_repeats.push((candidate.key.2, candidate.anchor));
            continue;
        }
        if prior_bounds
            .iter()
            .any(|bounds| bounds.intersects(candidate.bounds))
        {
            stats.rejected_labels += 1;
            prior_bounds.push(candidate.bounds);
            prior_repeats.push((candidate.key.2, candidate.anchor));
            continue;
        }
        prior_bounds.push(candidate.bounds);
        prior_repeats.push((candidate.key.2.clone(), candidate.anchor));
        accept(&candidate);
        stats.labels += 1;
    }
    Ok(())
}

fn draw_label_candidate(pixmap: &mut Pixmap, candidate: &LabelCandidate, transform: Transform) {
    let transform = transform.pre_translate(candidate.anchor.0, candidate.anchor.1);
    if candidate.halo_radius > 0.0 {
        let mut halo = paint(candidate.halo);
        halo.anti_alias = true;
        pixmap.stroke_path(
            candidate.path.as_ref(),
            &halo,
            &Stroke {
                width: candidate.halo_radius * 2.0,
                ..Stroke::default()
            },
            transform,
            None,
        );
    }
    if candidate.bold {
        let mut bold = paint(candidate.fill);
        bold.anti_alias = true;
        pixmap.stroke_path(
            candidate.path.as_ref(),
            &bold,
            &Stroke {
                width: 0.9,
                ..Stroke::default()
            },
            transform,
            None,
        );
    }
    let mut fill = paint(candidate.fill);
    fill.anti_alias = true;
    pixmap.fill_path(
        candidate.path.as_ref(),
        &fill,
        FillRule::Winding,
        transform,
        None,
    );
}

pub(crate) fn layout_projected_labels(
    dataset: &Dataset,
    request: &VectorRenderRequest,
) -> Result<Vec<PlacedLabel>, GdalError> {
    let mut layer = dataset.layer(0)?;
    let pixel_transform = PixelTransform::new(request);
    let context_pixels = request
        .rules
        .iter()
        .filter_map(|rule| rule.text.as_ref())
        .map(|text| {
            text.repeat_distance
                .max(text.spacing + text.font_size * 4.0)
        })
        .fold(128.0_f32, f32::max)
        .ceil()
        .min(512.0) as u32;
    let context_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width)
        * f64::from(context_pixels);
    let context_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height)
        * f64::from(context_pixels);
    layer.set_spatial_filter_rect(
        request.bbox.min_x - context_x,
        request.bbox.min_y - context_y,
        request.bbox.max_x + context_x,
        request.bbox.max_y + context_y,
    );
    let mut stats = RenderStats::default();
    let label_dataset_id = label_dataset_id(dataset);
    let mut candidates = Vec::new();
    visit_labels(
        &mut layer,
        request,
        &pixel_transform,
        label_dataset_id,
        &mut stats,
        |candidate| candidates.push(candidate.clone()),
    )?;
    candidates
        .into_iter()
        .map(rasterize_placed_candidate)
        .collect()
}

fn rasterize_placed_candidate(candidate: LabelCandidate) -> Result<PlacedLabel, GdalError> {
    let x = candidate.bounds.left.floor() as i32;
    let y = candidate.bounds.top.floor() as i32;
    let width = (candidate.bounds.right.ceil() as i32 - x).max(1) as u32;
    let height = (candidate.bounds.bottom.ceil() as i32 - y).max(1) as u32;
    let image = rasterize_candidate(&candidate, x as f32, y as f32, width, height)?;
    Ok(PlacedLabel { x, y, image })
}

pub(crate) fn prepare_projected_scene(
    dataset: &Dataset,
    request: &VectorRenderRequest,
) -> Result<PreparedVectorScene, GdalError> {
    let width = request.width + request.buffer_pixels.saturating_mul(2);
    let height = request.height + request.buffer_pixels.saturating_mul(2);
    let transform = PixelTransform::new(request);
    let mut layer = dataset.layer(0)?;
    let buffer_x = (request.bbox.max_x - request.bbox.min_x) / f64::from(request.width)
        * f64::from(request.buffer_pixels);
    let buffer_y = (request.bbox.max_y - request.bbox.min_y) / f64::from(request.height)
        * f64::from(request.buffer_pixels);
    layer.set_spatial_filter_rect(
        request.bbox.min_x - buffer_x,
        request.bbox.min_y - buffer_y,
        request.bbox.max_x + buffer_x,
        request.bbox.max_y + buffer_y,
    );
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut patterns = Vec::new();
    for rule in &request.rules {
        if let Some(filter) = &rule.where_clause {
            layer.set_attribute_filter(filter)?;
        } else {
            layer.clear_attribute_filter();
        }
        let fill_index = field_index(&layer, rule.fill_field.as_ref().map(|v| v.0.as_str()))?;
        let stroke_index = field_index(&layer, rule.stroke_field.as_ref().map(|v| v.0.as_str()))?;
        let mut pattern = rule
            .fill_image
            .as_ref()
            .map(|style| {
                let image = load_pattern(&style.path)?;
                Ok::<_, GdalError>((
                    style,
                    image.clone(),
                    PreparedPatternOverlay {
                        width: image.width(),
                        height: image.height(),
                        rgba: image.data().to_vec(),
                        vertices: Vec::new(),
                        indices: Vec::new(),
                    },
                ))
            })
            .transpose()?;
        layer.reset_feature_reading();
        for feature in layer.features() {
            let Some(geometry) = feature.geometry() else {
                continue;
            };
            let fill = if pattern.is_none() {
                resolve_color(&feature, rule.fill, rule.fill_field.as_ref(), fill_index)?
            } else {
                None
            };
            let stroke = resolve_color(
                &feature,
                rule.stroke,
                rule.stroke_field.as_ref(),
                stroke_index,
            )?;
            append_prepared_geometry(
                geometry,
                &transform,
                fill,
                stroke,
                rule.stroke_width,
                &rule.stroke_dasharray,
                &mut vertices,
                &mut indices,
            )?;
            if let Some((style, image, overlay)) = pattern.as_mut() {
                append_prepared_pattern_geometry(
                    geometry,
                    &transform,
                    image.width(),
                    image.height(),
                    style.scale,
                    style.opacity,
                    overlay,
                )?;
            }
        }
        if let Some((_, _, pattern)) = pattern.filter(|(_, _, pattern)| !pattern.indices.is_empty())
        {
            patterns.push(pattern);
        }
    }
    let labels = layout_projected_labels(dataset, request)?;
    Ok(PreparedVectorScene {
        width,
        height,
        crop_pixels: request.buffer_pixels,
        background: request.background,
        vertices,
        indices,
        patterns,
        labels,
    })
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_geometry(
    geometry: &Geometry,
    transform: &PixelTransform,
    fill: Option<[u8; 4]>,
    stroke: Option<[u8; 4]>,
    stroke_width: f32,
    dash_pattern: &[f32],
    vertices: &mut Vec<PreparedVectorVertex>,
    indices: &mut Vec<u32>,
) -> Result<(), GdalError> {
    match geometry.geometry_name().to_ascii_uppercase().as_str() {
        "POLYGON" => append_prepared_polygon(
            geometry,
            transform,
            fill,
            stroke,
            stroke_width,
            dash_pattern,
            vertices,
            indices,
        ),
        "MULTIPOLYGON" | "GEOMETRYCOLLECTION" | "MULTILINESTRING" => {
            for index in 0..geometry.geometry_count() {
                append_prepared_geometry(
                    &geometry.get_geometry(index),
                    transform,
                    fill,
                    stroke,
                    stroke_width,
                    dash_pattern,
                    vertices,
                    indices,
                )?;
            }
            Ok(())
        }
        "LINESTRING" | "LINEARRING" => {
            if let Some(color) = stroke {
                append_prepared_stroke(
                    &prepared_points(geometry, transform),
                    false,
                    color,
                    stroke_width,
                    dash_pattern,
                    vertices,
                    indices,
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_polygon(
    polygon: &Geometry,
    transform: &PixelTransform,
    fill: Option<[u8; 4]>,
    stroke: Option<[u8; 4]>,
    stroke_width: f32,
    dash_pattern: &[f32],
    vertices: &mut Vec<PreparedVectorVertex>,
    indices: &mut Vec<u32>,
) -> Result<(), GdalError> {
    let rings = (0..polygon.geometry_count())
        .map(|index| prepared_points(&polygon.get_geometry(index), transform))
        .filter(|points| points.len() >= 3)
        .collect::<Vec<_>>();
    if let Some(color) = fill {
        let mut flat = Vec::new();
        let mut holes = Vec::new();
        for (ring_index, ring) in rings.iter().enumerate() {
            if ring_index > 0 {
                holes.push(flat.len() / 2);
            }
            for &(x, y) in ring {
                flat.extend([f64::from(x), f64::from(y)]);
            }
        }
        if !flat.is_empty() {
            let triangles = earcutr::earcut(&flat, &holes, 2).map_err(|error| {
                GdalError::InvalidRequest(format!("polygon triangulation failed: {error}"))
            })?;
            let base = vertices.len() as u32;
            vertices.extend(flat.chunks_exact(2).map(|point| PreparedVectorVertex {
                pixel_position: [point[0] as f32, point[1] as f32],
                color,
            }));
            indices.extend(triangles.into_iter().map(|index| base + index as u32));
        }
    }
    if let Some(color) = stroke {
        for ring in &rings {
            append_prepared_stroke(
                ring,
                true,
                color,
                stroke_width,
                dash_pattern,
                vertices,
                indices,
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_pattern_geometry(
    geometry: &Geometry,
    transform: &PixelTransform,
    image_width: u32,
    image_height: u32,
    scale: f32,
    opacity: f32,
    overlay: &mut PreparedPatternOverlay,
) -> Result<(), GdalError> {
    match geometry.geometry_name().to_ascii_uppercase().as_str() {
        "POLYGON" => append_prepared_pattern_polygon(
            geometry,
            transform,
            image_width,
            image_height,
            scale,
            opacity,
            overlay,
        ),
        "MULTIPOLYGON" | "GEOMETRYCOLLECTION" => {
            for index in 0..geometry.geometry_count() {
                append_prepared_pattern_geometry(
                    &geometry.get_geometry(index),
                    transform,
                    image_width,
                    image_height,
                    scale,
                    opacity,
                    overlay,
                )?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_pattern_polygon(
    polygon: &Geometry,
    transform: &PixelTransform,
    image_width: u32,
    image_height: u32,
    scale: f32,
    opacity: f32,
    overlay: &mut PreparedPatternOverlay,
) -> Result<(), GdalError> {
    let rings = (0..polygon.geometry_count())
        .map(|index| prepared_points(&polygon.get_geometry(index), transform))
        .filter(|points| points.len() >= 3)
        .collect::<Vec<_>>();
    let mut flat = Vec::new();
    let mut holes = Vec::new();
    for (ring_index, ring) in rings.iter().enumerate() {
        if ring_index > 0 {
            holes.push(flat.len() / 2);
        }
        for &(x, y) in ring {
            flat.extend([f64::from(x), f64::from(y)]);
        }
    }
    if flat.is_empty() {
        return Ok(());
    }
    let triangles = earcutr::earcut(&flat, &holes, 2).map_err(|error| {
        GdalError::InvalidRequest(format!("pattern polygon triangulation failed: {error}"))
    })?;
    let scale = scale.max(f32::EPSILON);
    let period_x = image_width as f32 * scale;
    let period_y = image_height as f32 * scale;
    let (phase_x, phase_y) = transform.pattern_phase(f64::from(period_x), f64::from(period_y));
    let base = overlay.vertices.len() as u32;
    overlay.vertices.extend(flat.chunks_exact(2).map(|point| {
        let x = point[0] as f32;
        let y = point[1] as f32;
        PreparedPatternVertex {
            pixel_position: [x, y],
            uv: [(x - phase_x) / period_x, (y - phase_y) / period_y],
            opacity: opacity.clamp(0.0, 1.0),
        }
    }));
    overlay
        .indices
        .extend(triangles.into_iter().map(|index| base + index as u32));
    Ok(())
}

fn prepared_points(geometry: &Geometry, transform: &PixelTransform) -> Vec<(f32, f32)> {
    let mut points = (0..geometry.point_count())
        .map(|index| {
            let (x, y, _) = geometry.get_point(index as i32);
            transform.apply(x, y)
        })
        .collect::<Vec<_>>();
    points.dedup();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    points
}

#[allow(clippy::too_many_arguments)]
fn append_prepared_stroke(
    points: &[(f32, f32)],
    closed: bool,
    color: [u8; 4],
    width: f32,
    pattern: &[f32],
    vertices: &mut Vec<PreparedVectorVertex>,
    indices: &mut Vec<u32>,
) {
    if points.len() < 2 || width <= 0.0 {
        return;
    }
    let segments = points.len() - usize::from(!closed);
    let mut dash_index = 0;
    let mut dash_remaining = pattern.first().copied().unwrap_or(f32::INFINITY);
    let mut paint_segment = true;
    for index in 0..segments {
        let start = points[index];
        let end = points[(index + 1) % points.len()];
        let delta = (end.0 - start.0, end.1 - start.1);
        let length = delta.0.hypot(delta.1);
        if length <= f32::EPSILON {
            continue;
        }
        let direction = (delta.0 / length, delta.1 / length);
        let mut travelled = 0.0;
        while travelled < length {
            let step = dash_remaining.min(length - travelled);
            if paint_segment {
                append_prepared_stroke_quad(
                    (
                        start.0 + direction.0 * travelled,
                        start.1 + direction.1 * travelled,
                    ),
                    (
                        start.0 + direction.0 * (travelled + step),
                        start.1 + direction.1 * (travelled + step),
                    ),
                    color,
                    width,
                    vertices,
                    indices,
                );
            }
            travelled += step;
            dash_remaining -= step;
            if dash_remaining <= f32::EPSILON && !pattern.is_empty() {
                dash_index = (dash_index + 1) % pattern.len();
                dash_remaining = pattern[dash_index].max(f32::EPSILON);
                paint_segment = !paint_segment;
            }
        }
    }
}

fn append_prepared_stroke_quad(
    start: (f32, f32),
    end: (f32, f32),
    color: [u8; 4],
    width: f32,
    vertices: &mut Vec<PreparedVectorVertex>,
    indices: &mut Vec<u32>,
) {
    let delta = (end.0 - start.0, end.1 - start.1);
    let length = delta.0.hypot(delta.1);
    if length <= f32::EPSILON {
        return;
    }
    let normal = (
        -delta.1 / length * width / 2.0,
        delta.0 / length * width / 2.0,
    );
    let base = vertices.len() as u32;
    for pixel_position in [
        [start.0 + normal.0, start.1 + normal.1],
        [end.0 + normal.0, end.1 + normal.1],
        [end.0 - normal.0, end.1 - normal.1],
        [start.0 - normal.0, start.1 - normal.1],
    ] {
        vertices.push(PreparedVectorVertex {
            pixel_position,
            color,
        });
    }
    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

pub(crate) fn rasterize_text_label(
    text: &str,
    style: &VectorTextStyle,
) -> Result<RasterizedLabel, GdalError> {
    let Some(candidate) = shape_label(text, (0.0, 0.0), (0.0, 0.0), style)? else {
        return Err(GdalError::InvalidRequest(
            "label has no drawable glyphs".into(),
        ));
    };
    let width = (candidate.bounds.right - candidate.bounds.left)
        .ceil()
        .max(1.0) as u32;
    let height = (candidate.bounds.bottom - candidate.bounds.top)
        .ceil()
        .max(1.0) as u32;
    rasterize_candidate(
        &candidate,
        candidate.bounds.left,
        candidate.bounds.top,
        width,
        height,
    )
}

fn rasterize_candidate(
    candidate: &LabelCandidate,
    left: f32,
    top: f32,
    width: u32,
    height: u32,
) -> Result<RasterizedLabel, GdalError> {
    let mut pixmap = Pixmap::new(width, height)
        .ok_or_else(|| GdalError::InvalidRequest("invalid label dimensions".into()))?;
    draw_label_candidate(
        &mut pixmap,
        candidate,
        Transform::from_translate(-left, -top),
    );
    Ok(RasterizedLabel {
        width,
        height,
        rgba: pixmap.data().to_vec(),
    })
}

fn geometry_label_point(geometry: &Geometry) -> Option<(f64, f64)> {
    if geometry
        .geometry_name()
        .eq_ignore_ascii_case("MULTIPOLYGON")
    {
        return (0..geometry.geometry_count())
            .map(|index| geometry.get_geometry(index))
            .max_by(|left, right| left.area().total_cmp(&right.area()))
            .and_then(|polygon| polygon_label_point(&polygon));
    }
    if geometry.geometry_name().eq_ignore_ascii_case("POLYGON") {
        return polygon_label_point(geometry);
    }
    let envelope = geometry.envelope();
    let x = (envelope.MinX + envelope.MaxX) / 2.0;
    let y = (envelope.MinY + envelope.MaxY) / 2.0;
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

fn geometry_label_points(
    geometry: &Geometry,
    pixel_transform: &PixelTransform,
    repeat_distance: f32,
    repeat_count: u32,
) -> Vec<(f64, f64)> {
    let Some(primary) = geometry_label_point(geometry) else {
        return Vec::new();
    };
    if repeat_count <= 1 {
        return vec![primary];
    }
    let repeat = f64::from(repeat_distance.max(0.0));
    let envelope = geometry.envelope();
    let pixel_width = (envelope.MaxX - envelope.MinX).abs() * pixel_transform.scale_x.abs();
    let pixel_height = (envelope.MaxY - envelope.MinY).abs() * pixel_transform.scale_y.abs();
    if repeat <= 0.0 || pixel_width.max(pixel_height) < repeat * 1.8 {
        return vec![primary];
    }

    let step_x = repeat / pixel_transform.scale_x.abs();
    let step_y = repeat / pixel_transform.scale_y.abs();
    let count = repeat_count.clamp(1, 32) as usize;
    let mut points = vec![primary];
    // Search concentric rings around the polygon's primary interior point. A
    // changing phase avoids the rigid rows/columns produced by a world grid.
    const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;
    for ring in 1..=8 {
        let samples = 8 * ring;
        let mut ring_points = Vec::new();
        for sample in 0..samples {
            let angle = GOLDEN_ANGLE * f64::from(ring as u32)
                + std::f64::consts::TAU * sample as f64 / samples as f64;
            let x = primary.0 + step_x * ring as f64 * angle.cos();
            let y = primary.1 + step_y * ring as f64 * angle.sin();
            let mut point = match Geometry::empty(wkbPoint) {
                Ok(point) => point,
                Err(_) => return points,
            };
            point.add_point_2d((x, y));
            if geometry.contains(&point) {
                ring_points.push((x, y));
            }
        }
        let remaining = count - points.len();
        if ring_points.len() <= remaining {
            points.extend(ring_points);
        } else {
            points.extend((0..remaining).map(|index| {
                let point_index = index * ring_points.len() / remaining;
                ring_points[point_index]
            }));
        }
        if points.len() >= count {
            break;
        }
    }
    points
}

fn label_dataset_id(dataset: &Dataset) -> u64 {
    let mut hasher = DefaultHasher::new();
    dataset
        .description()
        .unwrap_or_else(|_| format!("dataset:{:p}", dataset.c_dataset()))
        .hash(&mut hasher);
    hasher.finish()
}

fn cached_geometry_label_points(
    dataset_id: u64,
    fid: Option<u64>,
    geometry: &Geometry,
    pixel_transform: &PixelTransform,
    repeat_distance: f32,
    repeat_count: u32,
    stats: &mut RenderStats,
) -> (f64, Arc<Vec<(f64, f64)>>) {
    let Some(fid) = fid else {
        return (
            geometry.area().abs(),
            Arc::new(geometry_label_points(
                geometry,
                pixel_transform,
                repeat_distance,
                repeat_count,
            )),
        );
    };
    let key = LabelAnchorKey {
        dataset_id,
        fid,
        scale_x: pixel_transform.scale_x.abs().to_bits(),
        scale_y: pixel_transform.scale_y.abs().to_bits(),
        repeat_distance: repeat_distance.max(0.0).to_bits(),
        repeat_count,
    };
    let cache = LABEL_ANCHOR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(entry) = cache
        .lock()
        .expect("label anchor cache poisoned")
        .get(&key)
        .cloned()
    {
        stats.label_anchor_cache_hits += 1;
        return (entry.world_area, entry.points);
    }

    stats.label_anchor_cache_misses += 1;
    let entry = LabelAnchorEntry {
        world_area: geometry.area().abs(),
        points: Arc::new(geometry_label_points(
            geometry,
            pixel_transform,
            repeat_distance,
            repeat_count,
        )),
    };
    let result = (entry.world_area, entry.points.clone());
    let mut cache = cache.lock().expect("label anchor cache poisoned");
    if cache.len() >= 262_144 {
        cache.clear();
    }
    cache.insert(key, entry);
    result
}

fn polygon_label_point(polygon: &Geometry) -> Option<(f64, f64)> {
    if let Some(centroid) = polygon_centroid(polygon) {
        let mut point = Geometry::empty(wkbPoint).ok()?;
        point.add_point_2d(centroid);
        if polygon.contains(&point) {
            return Some(centroid);
        }
    }

    // A concave polygon's area centroid can be outside the surface. In that
    // case, fall back to the midpoint of its widest interior scan segment.
    let envelope = polygon.envelope();
    let height = envelope.MaxY - envelope.MinY;
    let mut best: Option<(f64, f64, f64)> = None;
    for fraction in [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8] {
        let y = envelope.MinY + height * fraction;
        let mut intersections = Vec::new();
        for ring_index in 0..polygon.geometry_count() {
            let ring = polygon.get_geometry(ring_index);
            let mut points = Vec::with_capacity(ring.point_count());
            ring.get_points(&mut points);
            for edge in points.windows(2) {
                let (x1, y1, _) = edge[0];
                let (x2, y2, _) = edge[1];
                if (y1 > y) != (y2 > y) {
                    intersections.push(x1 + (y - y1) * (x2 - x1) / (y2 - y1));
                }
            }
        }
        intersections.sort_by(f64::total_cmp);
        for pair in intersections.chunks_exact(2) {
            let width = pair[1] - pair[0];
            if width > 0.0 && best.is_none_or(|(_, _, best_width)| width > best_width) {
                best = Some(((pair[0] + pair[1]) / 2.0, y, width));
            }
        }
    }
    best.map(|(x, y, _)| (x, y))
}

fn polygon_centroid(polygon: &Geometry) -> Option<(f64, f64)> {
    let mut weighted_x = 0.0;
    let mut weighted_y = 0.0;
    let mut total_weight = 0.0;
    for ring_index in 0..polygon.geometry_count() {
        let ring = polygon.get_geometry(ring_index);
        let mut points = Vec::with_capacity(ring.point_count());
        ring.get_points(&mut points);
        let (x, y, area) = ring_centroid(&points)?;
        let weight = if ring_index == 0 {
            area.abs()
        } else {
            -area.abs()
        };
        weighted_x += x * weight;
        weighted_y += y * weight;
        total_weight += weight;
    }
    if total_weight.abs() <= f64::EPSILON {
        return None;
    }
    let centroid = (weighted_x / total_weight, weighted_y / total_weight);
    (centroid.0.is_finite() && centroid.1.is_finite()).then_some(centroid)
}

fn ring_centroid(points: &[(f64, f64, f64)]) -> Option<(f64, f64, f64)> {
    if points.len() < 3 {
        return None;
    }
    let mut cross_sum = 0.0;
    let mut x_sum = 0.0;
    let mut y_sum = 0.0;
    for (&(x1, y1, _), &(x2, y2, _)) in points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
    {
        let cross = x1 * y2 - x2 * y1;
        cross_sum += cross;
        x_sum += (x1 + x2) * cross;
        y_sum += (y1 + y2) * cross;
    }
    if cross_sum.abs() <= f64::EPSILON {
        return None;
    }
    Some((
        x_sum / (3.0 * cross_sum),
        y_sum / (3.0 * cross_sum),
        cross_sum / 2.0,
    ))
}

fn shape_label(
    text: &str,
    anchor: (f32, f32),
    world_anchor: (f64, f64),
    style: &VectorTextStyle,
) -> Result<Option<LabelCandidate>, GdalError> {
    let Some(shaped) = shape_label_template(text, style)? else {
        return Ok(None);
    };
    let padding = style.halo_radius.max(1.0) + 2.0 + style.spacing.max(0.0) / 2.0;
    Ok(Some(LabelCandidate {
        key: (
            world_anchor.0.to_bits(),
            world_anchor.1.to_bits(),
            text.to_owned(),
        ),
        path: shaped.path.clone(),
        bounds: LabelBounds {
            left: anchor.0 + shaped.left - padding,
            top: anchor.1 + shaped.top - padding,
            right: anchor.0 + shaped.right + padding,
            bottom: anchor.1 + shaped.bottom + padding,
        },
        fill: style.fill,
        halo: style.halo,
        halo_radius: style.halo_radius,
        anchor,
        repeat_distance: style.repeat_distance.max(0.0),
        bold: style.font_bold,
    }))
}

fn shape_label_template(
    text: &str,
    style: &VectorTextStyle,
) -> Result<Option<Arc<ShapedLabel>>, GdalError> {
    let key = LabelShapeKey {
        text: text.to_owned(),
        font_family: style.font_family.clone(),
        font_size: style.font_size.max(1.0).to_bits(),
        italic: style.font_italic,
    };
    let cache = LABEL_SHAPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(shaped) = cache
        .lock()
        .expect("label shape cache poisoned")
        .get(&key)
        .cloned()
    {
        return Ok(Some(shaped));
    }
    let font = load_label_font(&style.font_family)?;
    let rb_face = rustybuzz::Face::from_slice(&font, 0)
        .ok_or_else(|| GdalError::InvalidRequest("cannot parse label font".into()))?;
    let outline_face = ttf_parser::Face::parse(&font, 0).map_err(|error| {
        GdalError::InvalidRequest(format!("cannot parse label font: {error:?}"))
    })?;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    let glyphs = rustybuzz::shape(&rb_face, &[], buffer);
    if glyphs.glyph_infos().is_empty() {
        return Ok(None);
    }
    let scale = style.font_size.max(1.0) / f32::from(outline_face.units_per_em());
    let total_advance = glyphs
        .glyph_positions()
        .iter()
        .map(|position| position.x_advance as f32 * scale)
        .sum::<f32>();
    let ascender = f32::from(outline_face.ascender()) * scale;
    let descender = f32::from(outline_face.descender()) * scale;
    let baseline_x = -total_advance / 2.0;
    let baseline_y = (ascender + descender) / 2.0;
    let mut builder = PathBuilder::new();
    let mut pen_x = baseline_x;
    let mut pen_y = baseline_y;
    for (info, position) in glyphs.glyph_infos().iter().zip(glyphs.glyph_positions()) {
        let origin_x = pen_x + position.x_offset as f32 * scale;
        let origin_y = pen_y - position.y_offset as f32 * scale;
        let mut sink = GlyphPathBuilder {
            builder: &mut builder,
            origin_x,
            origin_y,
            scale,
            italic_shear: if style.font_italic { 0.22 } else { 0.0 },
        };
        outline_face.outline_glyph(ttf_parser::GlyphId(info.glyph_id as u16), &mut sink);
        pen_x += position.x_advance as f32 * scale;
        pen_y -= position.y_advance as f32 * scale;
    }
    let Some(path) = builder.finish() else {
        return Ok(None);
    };
    let shaped = Arc::new(ShapedLabel {
        path: Arc::new(path),
        left: baseline_x,
        top: baseline_y - ascender,
        right: baseline_x
            + total_advance
            + ascender.max(0.0) * if style.font_italic { 0.22 } else { 0.0 },
        bottom: baseline_y - descender,
    });
    let mut cache = cache.lock().expect("label shape cache poisoned");
    if cache.len() >= 8192 {
        cache.clear();
    }
    cache.insert(key, shaped.clone());
    Ok(Some(shaped))
}

struct GlyphPathBuilder<'a> {
    builder: &'a mut PathBuilder,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    italic_shear: f32,
}

impl ttf_parser::OutlineBuilder for GlyphPathBuilder<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(
            self.origin_x + (x + y * self.italic_shear) * self.scale,
            self.origin_y - y * self.scale,
        );
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(
            self.origin_x + (x + y * self.italic_shear) * self.scale,
            self.origin_y - y * self.scale,
        );
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quad_to(
            self.origin_x + (x1 + y1 * self.italic_shear) * self.scale,
            self.origin_y - y1 * self.scale,
            self.origin_x + (x + y * self.italic_shear) * self.scale,
            self.origin_y - y * self.scale,
        );
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_to(
            self.origin_x + (x1 + y1 * self.italic_shear) * self.scale,
            self.origin_y - y1 * self.scale,
            self.origin_x + (x2 + y2 * self.italic_shear) * self.scale,
            self.origin_y - y2 * self.scale,
            self.origin_x + (x + y * self.italic_shear) * self.scale,
            self.origin_y - y * self.scale,
        );
    }
    fn close(&mut self) {
        self.builder.close();
    }
}

fn collect_filter_fields(
    filter: &VectorFilter,
    visit: &mut impl FnMut(&str) -> Result<(), GdalError>,
) -> Result<(), GdalError> {
    match filter {
        VectorFilter::Comparison { property, .. } => visit(property),
        VectorFilter::And { filters } | VectorFilter::Or { filters } => {
            for filter in filters {
                collect_filter_fields(filter, visit)?;
            }
            Ok(())
        }
        VectorFilter::Not { filter } => collect_filter_fields(filter, visit),
    }
}

fn filter_matches(
    feature: &Feature<'_>,
    filter: &VectorFilter,
    indices: &HashMap<String, usize>,
) -> Result<bool, GdalError> {
    Ok(match filter {
        VectorFilter::Comparison {
            property,
            operator,
            value,
        } => {
            let index = *indices.get(property).ok_or_else(|| {
                GdalError::InvalidRequest(format!("filter field `{property}` is unavailable"))
            })?;
            match value {
                VectorLiteral::Number(expected) => feature
                    .field_as_double(index)?
                    .is_some_and(|actual| compare_order(actual, *expected, *operator)),
                VectorLiteral::String(expected) => {
                    feature.field_as_string(index)?.is_some_and(|actual| {
                        compare_order(actual.as_str(), expected.as_str(), *operator)
                    })
                }
                VectorLiteral::Boolean(expected) => feature
                    .field_as_integer(index)?
                    .map(|actual| actual != 0)
                    .is_some_and(|actual| compare_order(actual, *expected, *operator)),
            }
        }
        VectorFilter::And { filters } => {
            for filter in filters {
                if !filter_matches(feature, filter, indices)? {
                    return Ok(false);
                }
            }
            true
        }
        VectorFilter::Or { filters } => {
            for filter in filters {
                if filter_matches(feature, filter, indices)? {
                    return Ok(true);
                }
            }
            false
        }
        VectorFilter::Not { filter } => !filter_matches(feature, filter, indices)?,
    })
}

fn compare_order<T: PartialEq + PartialOrd>(
    actual: T,
    expected: T,
    operator: VectorComparisonOperator,
) -> bool {
    match operator {
        VectorComparisonOperator::Equal => actual == expected,
        VectorComparisonOperator::NotEqual => actual != expected,
        VectorComparisonOperator::LessThan => actual < expected,
        VectorComparisonOperator::LessThanOrEqual => actual <= expected,
        VectorComparisonOperator::GreaterThan => actual > expected,
        VectorComparisonOperator::GreaterThanOrEqual => actual >= expected,
    }
}

#[derive(Default)]
struct RenderStats {
    features: u64,
    polygons: u64,
    rings: u64,
    skipped_rings: u64,
    batched_rules: u64,
    labels: u64,
    rejected_labels: u64,
    label_anchor_cache_hits: u64,
    label_anchor_cache_misses: u64,
    geometry_cache_hits: u64,
    geometry_cache_misses: u64,
}

fn field_index<L: LayerAccess>(layer: &L, field: Option<&str>) -> Result<Option<usize>, GdalError> {
    field
        .map(|name| layer.defn().field_index(name).map_err(GdalError::from))
        .transpose()
}

fn resolve_color(
    feature: &Feature<'_>,
    literal: Option<[u8; 4]>,
    field: Option<&(String, f32)>,
    index: Option<usize>,
) -> Result<Option<[u8; 4]>, GdalError> {
    if let (Some((name, opacity)), Some(index)) = (field, index) {
        return feature.field_as_string(index)?.map(|value| parse_css_color(value.trim(), *opacity).ok_or_else(|| {
            GdalError::InvalidRequest(format!("field `{name}` contains invalid color `{value}`; expected #RGB, #RRGGBB or #RRGGBBAA"))
        })).transpose();
    }
    Ok(literal)
}

fn draw_cached_geometry(
    pixmap: &mut Pixmap,
    geometry: &CachedGeometry,
    pixel_transform: &PixelTransform,
    fill: Option<[u8; 4]>,
    fill_image_paint: Option<&Paint<'_>>,
    stroke: Option<[u8; 4]>,
    stroke_width: f32,
    stroke_dasharray: &[f32],
    stats: &mut RenderStats,
) {
    for part in &geometry.parts {
        match part {
            CachedGeometryPart::Polygon(rings) => {
                stats.polygons += 1;
                let Some(path) = cached_polygon_path(rings, pixel_transform, stats) else {
                    continue;
                };
                draw_path(
                    pixmap,
                    &path,
                    fill,
                    fill_image_paint,
                    stroke,
                    stroke_width,
                    stroke_dasharray,
                    FillRule::EvenOdd,
                );
            }
            CachedGeometryPart::Line(points) => {
                let Some(path) = cached_line_path(points, pixel_transform) else {
                    continue;
                };
                draw_path(
                    pixmap,
                    &path,
                    None,
                    None,
                    stroke,
                    stroke_width,
                    stroke_dasharray,
                    FillRule::Winding,
                );
            }
        }
    }
}

fn draw_path(
    pixmap: &mut Pixmap,
    path: &Path,
    fill: Option<[u8; 4]>,
    fill_image_paint: Option<&Paint<'_>>,
    stroke: Option<[u8; 4]>,
    stroke_width: f32,
    stroke_dasharray: &[f32],
    fill_rule: FillRule,
) {
    if let Some(paint) = fill_image_paint {
        pixmap.fill_path(path, paint, fill_rule, Transform::identity(), None);
    } else if let Some(fill) = fill {
        let mut paint = paint(fill);
        paint.anti_alias = true;
        pixmap.fill_path(path, &paint, fill_rule, Transform::identity(), None);
    }
    if let Some(stroke_color) = stroke.filter(|_| stroke_width > 0.0) {
        let mut paint = paint(stroke_color);
        paint.anti_alias = true;
        let mut stroke = Stroke {
            width: stroke_width,
            ..Stroke::default()
        };
        if !stroke_dasharray.is_empty() {
            stroke.dash = StrokeDash::new(stroke_dasharray.to_vec(), 0.0);
        }
        pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
    }
}

fn pattern_paint<'a>(
    image: &'a Pixmap,
    scale: f32,
    opacity: f32,
    pixel_transform: &PixelTransform,
) -> Paint<'a> {
    let scale = scale.max(f32::EPSILON);
    let (phase_x, phase_y) = pixel_transform.pattern_phase(
        f64::from(image.width()) * f64::from(scale),
        f64::from(image.height()) * f64::from(scale),
    );
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = Pattern::new(
        image.as_ref(),
        SpreadMode::Repeat,
        FilterQuality::Bilinear,
        opacity,
        Transform::from_row(scale, 0.0, 0.0, scale, phase_x, phase_y),
    );
    paint
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PatternCacheKey {
    path: PathBuf,
    modified: Option<SystemTime>,
}

static PATTERN_CACHE: OnceLock<Mutex<HashMap<PatternCacheKey, Arc<Pixmap>>>> = OnceLock::new();
static PATTERN_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static PATTERN_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static FONT_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<u8>>>>> = OnceLock::new();
static LABEL_SHAPE_CACHE: OnceLock<Mutex<HashMap<LabelShapeKey, Arc<ShapedLabel>>>> =
    OnceLock::new();
static LABEL_ANCHOR_CACHE: OnceLock<Mutex<HashMap<LabelAnchorKey, LabelAnchorEntry>>> =
    OnceLock::new();
static GEOMETRY_CACHE: OnceLock<Mutex<GeometryCacheState>> = OnceLock::new();
static GEOMETRY_CACHE_BUDGET: OnceLock<usize> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LabelAnchorKey {
    dataset_id: u64,
    fid: u64,
    scale_x: u64,
    scale_y: u64,
    repeat_distance: u32,
    repeat_count: u32,
}

#[derive(Clone)]
struct LabelAnchorEntry {
    world_area: f64,
    points: Arc<Vec<(f64, f64)>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LabelShapeKey {
    text: String,
    font_family: String,
    font_size: u32,
    italic: bool,
}

struct ShapedLabel {
    path: Arc<Path>,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

fn load_label_font(_family: &str) -> Result<Arc<Vec<u8>>, GdalError> {
    let configured = std::env::var_os("MAP_FONT_PATH").map(PathBuf::from);
    let path = configured
        .or_else(|| {
            [
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
        })
        .ok_or_else(|| {
            GdalError::InvalidRequest(
                "label font is unavailable; configure MAP_FONT_PATH with an offline TTF/TTC font"
                    .into(),
            )
        })?;
    let cache = FONT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(font) = cache
        .lock()
        .expect("font cache poisoned")
        .get(&path)
        .cloned()
    {
        return Ok(font);
    }
    let font = Arc::new(std::fs::read(&path).map_err(|error| {
        GdalError::InvalidRequest(format!(
            "cannot read label font `{}`: {error}",
            path.display()
        ))
    })?);
    cache
        .lock()
        .expect("font cache poisoned")
        .insert(path, font.clone());
    Ok(font)
}

fn load_pattern(path: &FsPath) -> Result<Arc<Pixmap>, GdalError> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(GdalError::InvalidRequest(
            "fill-image must be relative to STYLE_ASSET_ROOT".into(),
        ));
    }
    let root = std::env::var_os("STYLE_ASSET_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/style-assets"));
    let root = root.canonicalize().map_err(|error| {
        GdalError::InvalidRequest(format!("style asset directory is unavailable: {error}"))
    })?;
    let canonical = root.join(path).canonicalize().map_err(|error| {
        GdalError::InvalidRequest(format!(
            "cannot open fill-image `{}`: {error}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(&root) {
        return Err(GdalError::InvalidRequest(
            "fill-image resolves outside STYLE_ASSET_ROOT".into(),
        ));
    }
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("png") && !extension.eq_ignore_ascii_case("webp") {
        return Err(GdalError::InvalidRequest(
            "fill-image must be a PNG or WebP file".into(),
        ));
    }
    let modified = canonical
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok();
    let key = PatternCacheKey {
        path: canonical.clone(),
        modified,
    };
    let cache = PATTERN_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(pixmap) = cache
        .lock()
        .expect("pattern cache poisoned")
        .get(&key)
        .cloned()
    {
        PATTERN_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(pixmap);
    }
    PATTERN_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    let image = image::open(&canonical).map_err(|error| {
        GdalError::InvalidRequest(format!(
            "cannot decode fill-image `{}`: {error}",
            path.display()
        ))
    })?;
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut pixels = rgba.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        pixel[0] = ((u16::from(pixel[0]) * alpha + 127) / 255) as u8;
        pixel[1] = ((u16::from(pixel[1]) * alpha + 127) / 255) as u8;
        pixel[2] = ((u16::from(pixel[2]) * alpha + 127) / 255) as u8;
    }
    let size = tiny_skia::IntSize::from_wh(width, height).ok_or_else(|| {
        GdalError::InvalidRequest("fill-image dimensions must be non-zero".into())
    })?;
    let pixmap = Arc::new(
        Pixmap::from_vec(pixels, size)
            .ok_or_else(|| GdalError::InvalidRequest("invalid fill-image pixel buffer".into()))?,
    );
    let mut cache = cache.lock().expect("pattern cache poisoned");
    cache.retain(|existing, _| existing.path != canonical);
    cache.insert(key, pixmap.clone());
    Ok(pixmap)
}

pub(crate) fn pattern_cache_stats() -> (usize, usize, u64, u64) {
    let (entries, bytes) = PATTERN_CACHE
        .get()
        .map(|cache| {
            let cache = cache.lock().expect("pattern cache poisoned");
            let bytes = cache
                .values()
                .map(|pixmap| pixmap.width() as usize * pixmap.height() as usize * 4)
                .sum();
            (cache.len(), bytes)
        })
        .unwrap_or_default();
    (
        entries,
        bytes,
        PATTERN_CACHE_HITS.load(Ordering::Relaxed),
        PATTERN_CACHE_MISSES.load(Ordering::Relaxed),
    )
}

fn cached_polygon_path(
    rings: &[Vec<(f64, f64)>],
    pixel_transform: &PixelTransform,
    stats: &mut RenderStats,
) -> Option<Path> {
    let mut builder = PathBuilder::new();
    append_cached_polygon(&mut builder, rings, pixel_transform, stats);
    builder.finish()
}

fn cached_line_path(points: &[(f64, f64)], pixel_transform: &PixelTransform) -> Option<Path> {
    let mut builder = PathBuilder::new();
    append_cached_line(&mut builder, points, pixel_transform);
    builder.finish()
}

fn append_cached_geometry(
    builder: &mut PathBuilder,
    geometry: &CachedGeometry,
    pixel_transform: &PixelTransform,
    stats: &mut RenderStats,
) {
    for part in &geometry.parts {
        match part {
            CachedGeometryPart::Polygon(rings) => {
                stats.polygons += 1;
                append_cached_polygon(builder, rings, pixel_transform, stats);
            }
            CachedGeometryPart::Line(points) => {
                append_cached_line(builder, points, pixel_transform);
            }
        }
    }
}

fn append_cached_line(
    builder: &mut PathBuilder,
    points: &[(f64, f64)],
    pixel_transform: &PixelTransform,
) {
    if points.len() < 2 {
        return;
    }
    let (x, y) = pixel_transform.apply(points[0].0, points[0].1);
    builder.move_to(x, y);
    for &(x, y) in &points[1..] {
        let (x, y) = pixel_transform.apply(x, y);
        builder.line_to(x, y);
    }
}

fn append_cached_polygon(
    builder: &mut PathBuilder,
    rings: &[Vec<(f64, f64)>],
    pixel_transform: &PixelTransform,
    stats: &mut RenderStats,
) {
    for (index, ring) in rings.iter().enumerate() {
        stats.rings += 1;
        let mut points = ring
            .iter()
            .map(|&(x, y)| pixel_transform.apply(x, y))
            .collect::<Vec<_>>();
        points.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
        if points.len() > 1 && points.first() == points.last() {
            points.pop();
        }
        if points.len() < 3 || signed_area_twice(&points).abs() < 2.0 {
            stats.skipped_rings += 1;
            if index == 0 {
                return;
            }
            continue;
        }
        let (x, y) = points[0];
        builder.move_to(x, y);
        for &(x, y) in &points[1..] {
            builder.line_to(x, y);
        }
        builder.close();
    }
}

fn signed_area_twice(points: &[(f32, f32)]) -> f32 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(&(x1, y1), &(x2, y2))| x1 * y2 - x2 * y1)
        .sum()
}

struct PixelTransform {
    scale_x: f64,
    scale_y: f64,
    offset_x: f64,
    offset_y: f64,
}

impl PixelTransform {
    fn new(request: &VectorRenderRequest) -> Self {
        let scale_x = f64::from(request.width) / (request.bbox.max_x - request.bbox.min_x);
        let scale_y = f64::from(request.height) / (request.bbox.max_y - request.bbox.min_y);
        Self {
            scale_x,
            scale_y,
            offset_x: f64::from(request.buffer_pixels) - request.bbox.min_x * scale_x,
            offset_y: f64::from(request.buffer_pixels) + request.bbox.max_y * scale_y,
        }
    }

    fn apply(&self, x: f64, y: f64) -> (f32, f32) {
        (
            (x * self.scale_x + self.offset_x) as f32,
            (-y * self.scale_y + self.offset_y) as f32,
        )
    }

    fn pattern_phase(&self, period_x: f64, period_y: f64) -> (f32, f32) {
        (
            self.offset_x.rem_euclid(period_x) as f32,
            self.offset_y.rem_euclid(period_y) as f32,
        )
    }
}

fn paint(value: [u8; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(value[0], value[1], value[2], value[3]);
    paint
}

fn color(value: [u8; 4]) -> Color {
    Color::from_rgba8(value[0], value[1], value[2], value[3])
}

fn parse_css_color(value: &str, opacity: f32) -> Option<[u8; 4]> {
    let hex = value.strip_prefix('#')?;
    let expanded;
    let hex = if hex.len() == 3 || hex.len() == 4 {
        expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
        expanded.as_str()
    } else {
        hex
    };
    if !matches!(hex.len(), 6 | 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let part = |start| u8::from_str_radix(&hex[start..start + 2], 16).ok();
    let alpha = if hex.len() == 8 { part(6)? } else { 255 };
    Some([
        part(0)?,
        part(2)?,
        part(4)?,
        (f32::from(alpha) * opacity.clamp(0.0, 1.0)).round() as u8,
    ])
}

fn encode(pixmap: Pixmap, format: ImageFormat) -> Result<RenderedImage, GdalError> {
    let bytes = match format {
        ImageFormat::Png => pixmap
            .encode_png()
            .map_err(|e| GdalError::InvalidRequest(format!("PNG encoding failed: {e}")))?,
        ImageFormat::Jpeg => {
            let data = pixmap.data();
            let rgb = RgbImage::from_fn(pixmap.width(), pixmap.height(), |x, y| {
                let i = ((y * pixmap.width() + x) * 4) as usize;
                image::Rgb([data[i], data[i + 1], data[i + 2]])
            });
            let mut bytes = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(rgb)
                .write_to(&mut bytes, EncoderFormat::Jpeg)
                .map_err(|e| GdalError::InvalidRequest(format!("JPEG encoding failed: {e}")))?;
            bytes.into_inner()
        }
    };
    Ok(RenderedImage {
        bytes,
        content_type: format.content_type(),
    })
}

fn unpremultiply_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_text_style() -> VectorTextStyle {
        VectorTextStyle {
            field: "name".into(),
            font_family: "sans-serif".into(),
            font_size: 18.0,
            font_italic: false,
            font_bold: true,
            fill: [20, 40, 30, 255],
            halo: [255, 255, 255, 255],
            halo_radius: 1.5,
            spacing: 4.0,
            min_area: 0.0,
            repeat_distance: 0.0,
            repeat_count: 1,
        }
    }

    #[test]
    fn reuses_shaped_label_path_across_anchors() {
        let style = test_text_style();
        let first = shape_label("Test", (10.0, 20.0), (1.0, 2.0), &style)
            .expect("shape first label")
            .expect("first label path");
        let second = shape_label("Test", (110.0, 220.0), (3.0, 4.0), &style)
            .expect("shape second label")
            .expect("second label path");

        assert!(Arc::ptr_eq(&first.path, &second.path));
        assert!((second.bounds.left - first.bounds.left - 100.0).abs() < 1e-4);
        assert!((second.bounds.right - first.bounds.right - 100.0).abs() < 1e-4);
        assert!((second.bounds.top - first.bounds.top - 200.0).abs() < 1e-4);
        assert!((second.bounds.bottom - first.bounds.bottom - 200.0).abs() < 1e-4);
    }

    #[test]
    fn reuses_simplified_world_geometry_by_dataset_and_feature() {
        let geometry = Geometry::from_wkt("POLYGON ((0 0, 20 0, 20 20, 10 20.1, 0 20, 0 0))")
            .expect("test polygon");
        let mut first_stats = RenderStats::default();
        let first = cached_geometry(0xfeed, Some(7), &geometry, Some(0.5), &mut first_stats)
            .expect("first cached geometry");
        let mut second_stats = RenderStats::default();
        let second = cached_geometry(0xfeed, Some(7), &geometry, Some(0.5), &mut second_stats)
            .expect("second cached geometry");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first_stats.geometry_cache_misses, 1);
        assert_eq!(second_stats.geometry_cache_hits, 1);
        assert!(!first.parts.is_empty());
    }

    #[test]
    fn parses_field_colors() {
        assert_eq!(parse_css_color("#58a76f", 0.5), Some([88, 167, 111, 128]));
        assert_eq!(parse_css_color("red", 1.0), None);
    }

    #[test]
    fn detects_degenerate_and_non_degenerate_rings() {
        assert_eq!(
            signed_area_twice(&[(0.0, 0.0), (1.0, 0.0), (2.0, 0.0)]),
            0.0
        );
        assert_eq!(
            signed_area_twice(&[(0.0, 0.0), (2.0, 0.0), (0.0, 2.0)]),
            4.0
        );
    }

    #[test]
    fn converts_tiny_skia_pixels_to_straight_alpha() {
        let mut pixels = vec![100, 50, 25, 128, 10, 20, 30, 255, 9, 8, 7, 0];
        unpremultiply_rgba(&mut pixels);
        assert_eq!(&pixels[0..4], &[199, 100, 50, 128]);
        assert_eq!(&pixels[4..8], &[10, 20, 30, 255]);
        assert_eq!(&pixels[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn calculates_area_weighted_ring_centroid() {
        let centroid = ring_centroid(&[
            (0.0, 0.0, 0.0),
            (6.0, 0.0, 0.0),
            (0.0, 3.0, 0.0),
            (0.0, 0.0, 0.0),
        ])
        .expect("triangle centroid");
        assert!((centroid.0 - 2.0).abs() < 1e-9);
        assert!((centroid.1 - 1.0).abs() < 1e-9);
        assert!((centroid.2 - 9.0).abs() < 1e-9);
    }

    #[test]
    fn anchors_pattern_phase_to_projected_world_coordinates() {
        let left = PixelTransform {
            scale_x: 2.0,
            scale_y: 2.0,
            offset_x: -10.0,
            offset_y: 25.0,
        };
        let right = PixelTransform {
            scale_x: 2.0,
            scale_y: 2.0,
            offset_x: -266.0,
            offset_y: 25.0,
        };

        assert_eq!(left.pattern_phase(30.0, 30.0), (20.0, 25.0));
        assert_eq!(right.pattern_phase(30.0, 30.0), (4.0, 25.0));
        assert_eq!((20.0_f32 - 256.0).rem_euclid(30.0), 4.0);
    }
}
