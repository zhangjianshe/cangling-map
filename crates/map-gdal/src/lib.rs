//! Compatibility adapter between cangling-map domain types and cangling-gdal.

use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    io::Cursor,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
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
    Vertex,
};
use map_render::{
    ImageFormat, RasterizeError, RasterizedImage, VectorColor, VectorRasterRequest,
    VectorRasterizer,
};
use map_style::{ComparisonOperator, Filter, Literal};
use serde::Serialize;

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
        let gdal_request = to_gdal_request(request)?;
        let image = GdalEngine
            .render_vector(&gdal_request)
            .map_err(|error| RasterizeError::Backend(error.to_string()))?;
        Ok(RasterizedImage {
            bytes: image.bytes,
            content_type: image.content_type,
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
    scenes: Mutex<ByteLruCache<u64, PreparedScene>>,
    scene_hits: AtomicU64,
    scene_misses: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct GpuSceneCacheStats {
    pub entries: usize,
    pub resident_bytes: usize,
    pub hits: u64,
    pub misses: u64,
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
                scene_hits: AtomicU64::new(0),
                scene_misses: AtomicU64::new(0),
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
        }
    }
}

impl VectorRasterizer for GpuVectorRasterizer {
    fn render(&self, request: &VectorRasterRequest) -> Result<RasterizedImage, RasterizeError> {
        if !request.bbox.is_valid() || request.width == 0 || request.height == 0 {
            return Err(RasterizeError::InvalidRequest(
                "bbox and image dimensions must be valid".into(),
            ));
        }
        let scene_key = gpu_scene_key(request);
        {
            let mut scenes = self
                .scenes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(scene) = scenes.get(&scene_key) {
                self.scene_hits.fetch_add(1, Ordering::Relaxed);
                let rgba = self
                    .renderer
                    .render_rgba(scene)
                    .map_err(|error| RasterizeError::Unavailable(error.to_string()))?;
                return encode_gpu_output(request, scene_crop(request), rgba);
            }
        }
        self.scene_misses.fetch_add(1, Ordering::Relaxed);
        let gdal_request = to_gdal_request(request)?;
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
        let scene = self
            .renderer
            .prepare_with_background_and_atlases(
                prepared.width,
                prepared.height,
                &chunks,
                prepared.background,
                &atlases,
            )
            .map_err(|error| RasterizeError::Unavailable(error.to_string()))?;
        let scene_bytes = scene.stats().estimated_gpu_bytes;
        let mut scenes = self
            .scenes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _evicted = scenes.insert(scene_key, scene, scene_bytes);
        let scene = scenes
            .get(&scene_key)
            .expect("the newly inserted GPU scene must be resident");
        let rgba = self
            .renderer
            .render_rgba(scene)
            .map_err(|error| RasterizeError::Unavailable(error.to_string()))?;
        encode_gpu_output(request, prepared.crop_pixels, rgba)
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

fn gpu_scene_key(request: &VectorRasterRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{request:?}").hash(&mut hasher);
    if let Ok(metadata) = std::fs::metadata(&request.source.path) {
        metadata.len().hash(&mut hasher);
        metadata.modified().ok().hash(&mut hasher);
    }
    hasher.finish()
}

fn scene_crop(request: &VectorRasterRequest) -> u32 {
    request.buffer_pixels
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

fn encode_gpu_output(
    request: &VectorRasterRequest,
    crop_pixels: u32,
    rgba: Vec<u8>,
) -> Result<RasterizedImage, RasterizeError> {
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
