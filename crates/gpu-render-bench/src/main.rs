use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use cangling_gdal_core::{
    rasterize_text_label, BoundingBox as GdalBoundingBox, DataSource, GdalEngine,
    ImageFormat as GdalImageFormat, PlacedLabel, RasterizedLabel, VectorRenderRequest,
    VectorRenderRule, VectorTextStyle,
};
use gdal::{
    vector::{Geometry, LayerAccess},
    Dataset,
};
use map_gpu::{MeshChunk as IndexedMeshChunk, Renderer as ProductionGpuRenderer};
use tiny_skia::{
    Color, FilterQuality, IntSize, Paint, PathBuilder, Pattern, Pixmap, SpreadMode, StrokeDash,
    Transform,
};
use wgpu::util::DeviceExt;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 4],
    color: [f32; 4],
}

#[derive(Clone, Copy)]
struct Rectangle {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: [f32; 4],
}

struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    adapter_name: String,
    backend: String,
}

struct PreparedGpuScene {
    batches: Vec<GpuDrawBatch>,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    output: wgpu::Buffer,
    width: u32,
    height: u32,
    unpadded_bytes_per_row: u32,
    padded_bytes_per_row: u32,
}

struct GpuDrawBatch {
    bind_group: wgpu::BindGroup,
    vertex_count: u32,
}

impl GpuRenderer {
    async fn new() -> Result<Self> {
        let instance = wgpu::Instance::default();
        let requested_adapter = std::env::var("GPU_BENCH_ADAPTER").ok();
        let adapter = if let Some(requested) = requested_adapter {
            instance
                .enumerate_adapters(wgpu::Backends::all())
                .into_iter()
                .find(|adapter| {
                    adapter
                        .get_info()
                        .name
                        .to_ascii_lowercase()
                        .contains(&requested.to_ascii_lowercase())
                })
                .with_context(|| format!("GPU adapter containing {requested:?} was not found"))?
        } else {
            instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .context("no compatible GPU adapter was found")?
        };
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("cangling-map-gpu-benchmark"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("flat polygon shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vertex storage layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("flat polygon pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("flat polygon pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            adapter_name: info.name,
            backend: format!("{:?}", info.backend),
        })
    }

    fn prepare(&self, width: u32, height: u32, rectangles: &[Rectangle]) -> PreparedGpuScene {
        let vertices = rectangles
            .iter()
            .flat_map(|rectangle| rectangle_vertices(width, height, *rectangle))
            .collect::<Vec<_>>();
        self.prepare_vertices(width, height, &vertices)
    }

    fn prepare_vertices(&self, width: u32, height: u32, vertices: &[Vertex]) -> PreparedGpuScene {
        let max_vertices = (self.device.limits().max_storage_buffer_binding_size as usize
            / std::mem::size_of::<Vertex>())
            / 3
            * 3;
        let batches = vertices
            .chunks(max_vertices)
            .map(|vertices| {
                let vertex_buffer =
                    self.device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("polygon vertex chunk"),
                            contents: bytemuck::cast_slice(vertices),
                            usage: wgpu::BufferUsages::STORAGE,
                        });
                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("polygon vertex chunk"),
                    layout: &self.bind_group_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: vertex_buffer.as_entire_binding(),
                    }],
                });
                GpuDrawBatch {
                    bind_group,
                    vertex_count: vertices.len() as u32,
                }
            })
            .collect();
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let unpadded_bytes_per_row = width * 4;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(256) * 256;
        let output = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GPU readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        PreparedGpuScene {
            batches,
            texture,
            view,
            output,
            width,
            height,
            unpadded_bytes_per_row,
            padded_bytes_per_row,
        }
    }

    fn render(&self, width: u32, height: u32, rectangles: &[Rectangle]) -> Result<Vec<u8>> {
        let scene = self.prepare(width, height, rectangles);
        self.render_prepared(&scene)
    }

    fn render_prepared(&self, scene: &PreparedGpuScene) -> Result<Vec<u8>> {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen polygon pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for batch in &scene.batches {
                pass.set_bind_group(0, &batch.bind_group, &[]);
                pass.draw(0..batch.vertex_count, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            scene.texture.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &scene.output,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(scene.padded_bytes_per_row),
                    rows_per_image: Some(scene.height),
                },
            },
            scene.texture.size(),
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = scene.output.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .context("GPU readback callback was lost")??;
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((scene.width * scene.height * 4) as usize);
        for row in mapped.chunks_exact(scene.padded_bytes_per_row as usize) {
            pixels.extend_from_slice(&row[..scene.unpadded_bytes_per_row as usize]);
        }
        drop(mapped);
        scene.output.unmap();
        Ok(pixels)
    }
}

fn rectangle_vertices(width: u32, height: u32, rectangle: Rectangle) -> [Vertex; 6] {
    let x0 = rectangle.x / width as f32 * 2.0 - 1.0;
    let x1 = (rectangle.x + rectangle.width) / width as f32 * 2.0 - 1.0;
    let y0 = 1.0 - rectangle.y / height as f32 * 2.0;
    let y1 = 1.0 - (rectangle.y + rectangle.height) / height as f32 * 2.0;
    let vertex = |position: [f32; 2]| Vertex {
        position: [position[0], position[1], 0.0, 1.0],
        color: rectangle.color,
    };
    [
        vertex([x0, y0]),
        vertex([x1, y0]),
        vertex([x0, y1]),
        vertex([x0, y1]),
        vertex([x1, y0]),
        vertex([x1, y1]),
    ]
}

fn cpu_render(width: u32, height: u32, rectangles: &[Rectangle]) -> Result<Vec<u8>> {
    let mut pixmap = Pixmap::new(width, height).context("invalid image dimensions")?;
    pixmap.fill(Color::TRANSPARENT);
    for rectangle in rectangles {
        let mut builder = PathBuilder::new();
        builder.push_rect(
            tiny_skia::Rect::from_xywh(rectangle.x, rectangle.y, rectangle.width, rectangle.height)
                .context("invalid rectangle")?,
        );
        let mut paint = Paint::default();
        paint.set_color_rgba8(
            (rectangle.color[0] * 255.0) as u8,
            (rectangle.color[1] * 255.0) as u8,
            (rectangle.color[2] * 255.0) as u8,
            (rectangle.color[3] * 255.0) as u8,
        );
        paint.anti_alias = true;
        pixmap.fill_path(
            &builder.finish().context("empty rectangle path")?,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    Ok(pixmap.data().to_vec())
}

#[derive(Default)]
struct RealGeometryScene {
    polygons: Vec<RealPolygon>,
    mesh_chunks: Vec<IndexedMeshChunk>,
    features: usize,
    rings: usize,
    dashed_boundaries: bool,
}

struct RealPolygon {
    rings: Vec<Vec<(f32, f32)>>,
    color: [f32; 4],
}

#[derive(Clone, Copy)]
struct WorldBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl WorldBounds {
    fn pixel(self, width: u32, height: u32, x: f64, y: f64) -> (f32, f32) {
        (
            ((x - self.min_x) / (self.max_x - self.min_x) * f64::from(width)) as f32,
            ((self.max_y - y) / (self.max_y - self.min_y) * f64::from(height)) as f32,
        )
    }
}

fn load_real_scene(
    path: &str,
    width: u32,
    height: u32,
    bounds: WorldBounds,
    dashed_boundaries: bool,
) -> Result<RealGeometryScene> {
    let dataset = Dataset::open(path).with_context(|| format!("failed to open {path}"))?;
    let mut layer = dataset.layer(0)?;
    layer.set_spatial_filter_rect(bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y);
    let mut scene = RealGeometryScene {
        dashed_boundaries,
        ..Default::default()
    };
    for (feature_index, feature) in layer.features().enumerate() {
        let Some(geometry) = feature.geometry() else {
            continue;
        };
        scene.features += 1;
        let color = [
            0.18 + (feature_index % 5) as f32 * 0.025,
            0.45 + (feature_index % 7) as f32 * 0.018,
            0.32 + (feature_index % 3) as f32 * 0.025,
            0.78,
        ];
        append_real_geometry(geometry, width, height, bounds, color, &mut scene)?;
    }
    Ok(scene)
}

fn append_real_geometry(
    geometry: &Geometry,
    width: u32,
    height: u32,
    bounds: WorldBounds,
    color: [f32; 4],
    scene: &mut RealGeometryScene,
) -> Result<()> {
    match geometry.geometry_name().to_ascii_uppercase().as_str() {
        "POLYGON" => append_real_polygon(geometry, width, height, bounds, color, scene),
        "MULTIPOLYGON" | "GEOMETRYCOLLECTION" => {
            for index in 0..geometry.geometry_count() {
                append_real_geometry(
                    &geometry.get_geometry(index),
                    width,
                    height,
                    bounds,
                    color,
                    scene,
                )?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn append_real_polygon(
    polygon: &Geometry,
    width: u32,
    height: u32,
    bounds: WorldBounds,
    color: [f32; 4],
    scene: &mut RealGeometryScene,
) -> Result<()> {
    let mut rings = Vec::new();
    let mut flat_coordinates = Vec::new();
    let mut holes = Vec::new();
    for ring_index in 0..polygon.geometry_count() {
        let ring = polygon.get_geometry(ring_index);
        let mut points = (0..ring.point_count())
            .map(|point_index| {
                let (x, y, _) = ring.get_point(point_index as i32);
                bounds.pixel(width, height, x, y)
            })
            .collect::<Vec<_>>();
        points.dedup();
        if points.len() > 1 && points.first() == points.last() {
            points.pop();
        }
        if points.len() < 3 {
            continue;
        }
        if !rings.is_empty() {
            holes.push(flat_coordinates.len() / 2);
        }
        for &(x, y) in &points {
            flat_coordinates.extend([f64::from(x), f64::from(y)]);
        }
        rings.push(points);
    }
    if rings.is_empty() {
        return Ok(());
    }
    let indices = earcutr::earcut(&flat_coordinates, &holes, 2)
        .map_err(|error| anyhow::anyhow!("polygon triangulation failed: {error}"))?;
    let points = flat_coordinates
        .chunks_exact(2)
        .map(|point| [point[0] as f32, point[1] as f32])
        .collect::<Vec<_>>();
    let mut mesh_vertices = points
        .iter()
        .map(|point| {
            let x = point[0] / width as f32 * 2.0 - 1.0;
            let y = 1.0 - point[1] / height as f32 * 2.0;
            map_gpu::Vertex {
                position: [x, y, 0.0, 1.0],
                color,
            }
        })
        .collect::<Vec<_>>();
    let mut mesh_indices = indices;
    for ring in &rings {
        append_stroke_mesh(
            ring,
            width,
            height,
            [0.12, 0.24, 0.20, 0.85],
            1.0,
            scene.dashed_boundaries.then_some(&[6.0, 4.0]),
            &mut mesh_vertices,
            &mut mesh_indices,
        );
    }
    append_indexed_polygon(&mut scene.mesh_chunks, mesh_vertices, mesh_indices);
    scene.rings += rings.len();
    scene.polygons.push(RealPolygon { rings, color });
    Ok(())
}

fn append_stroke_mesh(
    points: &[(f32, f32)],
    width: u32,
    height: u32,
    color: [f32; 4],
    line_width: f32,
    dash_pattern: Option<&[f32]>,
    vertices: &mut Vec<map_gpu::Vertex>,
    indices: &mut Vec<usize>,
) {
    let half_width = line_width / 2.0;
    let mut dash_state = dash_pattern.map(|pattern| DashState::new(pattern));
    for (&start, &end) in points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
    {
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let length = dx.hypot(dy);
        if length <= f32::EPSILON {
            continue;
        }
        if let Some(state) = dash_state.as_mut() {
            let direction = (dx / length, dy / length);
            let mut travelled = 0.0;
            while travelled < length {
                let step = state.remaining.min(length - travelled);
                if state.paint {
                    append_stroke_quad(
                        (
                            start.0 + direction.0 * travelled,
                            start.1 + direction.1 * travelled,
                        ),
                        (
                            start.0 + direction.0 * (travelled + step),
                            start.1 + direction.1 * (travelled + step),
                        ),
                        width,
                        height,
                        color,
                        half_width,
                        vertices,
                        indices,
                    );
                }
                travelled += step;
                state.advance(step);
            }
            continue;
        }
        append_stroke_quad(
            start, end, width, height, color, half_width, vertices, indices,
        );
    }
}

struct DashState<'a> {
    pattern: &'a [f32],
    index: usize,
    remaining: f32,
    paint: bool,
}

impl<'a> DashState<'a> {
    fn new(pattern: &'a [f32]) -> Self {
        debug_assert!(!pattern.is_empty());
        debug_assert!(pattern.iter().all(|length| *length > 0.0));
        Self {
            pattern,
            index: 0,
            remaining: pattern[0],
            paint: true,
        }
    }

    fn advance(&mut self, distance: f32) {
        self.remaining -= distance;
        if self.remaining <= f32::EPSILON {
            self.index = (self.index + 1) % self.pattern.len();
            self.remaining = self.pattern[self.index];
            self.paint = !self.paint;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_stroke_quad(
    start: (f32, f32),
    end: (f32, f32),
    width: u32,
    height: u32,
    color: [f32; 4],
    half_width: f32,
    vertices: &mut Vec<map_gpu::Vertex>,
    indices: &mut Vec<usize>,
) {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let length = dx.hypot(dy);
    if length <= f32::EPSILON {
        return;
    }
    let normal = (-dy / length * half_width, dx / length * half_width);
    let corners = [
        (start.0 + normal.0, start.1 + normal.1),
        (end.0 + normal.0, end.1 + normal.1),
        (end.0 - normal.0, end.1 - normal.1),
        (start.0 - normal.0, start.1 - normal.1),
    ];
    let base = vertices.len();
    vertices.extend(corners.map(|point| {
        let x = point.0 / width as f32 * 2.0 - 1.0;
        let y = 1.0 - point.1 / height as f32 * 2.0;
        map_gpu::Vertex {
            position: [x, y, 0.0, 1.0],
            color,
        }
    }));
    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn append_indexed_polygon(
    chunks: &mut Vec<IndexedMeshChunk>,
    vertices: Vec<map_gpu::Vertex>,
    indices: Vec<usize>,
) {
    const TARGET_CHUNK_BYTES: usize = 64 * 1024 * 1024;
    let additional_bytes = vertices.len() * std::mem::size_of::<map_gpu::Vertex>()
        + indices.len() * std::mem::size_of::<u32>();
    let needs_new_chunk = chunks.last().is_none_or(|chunk| {
        !chunk.indices.is_empty()
            && chunk.estimated_gpu_bytes().saturating_add(additional_bytes) > TARGET_CHUNK_BYTES
    });
    if needs_new_chunk {
        chunks.push(IndexedMeshChunk::default());
    }
    let chunk = chunks.last_mut().expect("a mesh chunk was inserted");
    let base = chunk.vertices.len() as u32;
    chunk.vertices.extend(vertices);
    chunk
        .indices
        .extend(indices.into_iter().map(|index| base + index as u32));
}

fn cpu_render_real(width: u32, height: u32, scene: &RealGeometryScene) -> Result<Vec<u8>> {
    let mut pixmap = Pixmap::new(width, height).context("invalid image dimensions")?;
    pixmap.fill(Color::TRANSPARENT);
    for polygon in &scene.polygons {
        let mut paint = Paint::default();
        paint.set_color_rgba8(
            (polygon.color[0] * 255.0) as u8,
            (polygon.color[1] * 255.0) as u8,
            (polygon.color[2] * 255.0) as u8,
            (polygon.color[3] * 255.0) as u8,
        );
        paint.anti_alias = true;
        let mut builder = PathBuilder::new();
        for ring in &polygon.rings {
            builder.move_to(ring[0].0, ring[0].1);
            for &(x, y) in &ring[1..] {
                builder.line_to(x, y);
            }
            builder.close();
        }
        if let Some(path) = builder.finish() {
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
            let mut stroke_paint = Paint::default();
            stroke_paint.set_color_rgba8(31, 61, 51, 217);
            stroke_paint.anti_alias = true;
            let stroke_dash = scene
                .dashed_boundaries
                .then(|| StrokeDash::new(vec![6.0, 4.0], 0.0))
                .flatten();
            pixmap.stroke_path(
                &path,
                &stroke_paint,
                &tiny_skia::Stroke {
                    width: 1.0,
                    dash: stroke_dash,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
    Ok(pixmap.data().to_vec())
}

struct ImageDiff {
    mean_absolute_error: f64,
    pixels_over_16_percent: f64,
}

fn image_diff(left: &[u8], right: &[u8]) -> ImageDiff {
    assert_eq!(left.len(), right.len());
    let absolute_sum = left
        .iter()
        .zip(right)
        .map(|(&left, &right)| u64::from(left.abs_diff(right)))
        .sum::<u64>();
    let pixels = left.len() / 4;
    let pixels_over_16 = left
        .chunks_exact(4)
        .zip(right.chunks_exact(4))
        .filter(|(left, right)| {
            left.iter()
                .zip(*right)
                .any(|(&left, &right)| left.abs_diff(right) > 16)
        })
        .count();
    ImageDiff {
        mean_absolute_error: absolute_sum as f64 / left.len() as f64,
        pixels_over_16_percent: pixels_over_16 as f64 / pixels as f64 * 100.0,
    }
}

fn save_comparison(
    directory: &Path,
    name: &str,
    width: u32,
    height: u32,
    cpu: &[u8],
    gpu: &[u8],
) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    image::save_buffer(
        directory.join(format!("{name}-cpu.png")),
        cpu,
        width,
        height,
        image::ColorType::Rgba8,
    )?;
    image::save_buffer(
        directory.join(format!("{name}-gpu.png")),
        gpu,
        width,
        height,
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

fn scene(width: u32, height: u32, count: usize) -> Vec<Rectangle> {
    (0..count)
        .map(|index| {
            let seed = index as u32;
            let x = seed.wrapping_mul(73) % width;
            let y = seed.wrapping_mul(151) % height;
            let rectangle_width = 8 + seed.wrapping_mul(17) % (width / 5).max(9);
            let rectangle_height = 8 + seed.wrapping_mul(29) % (height / 5).max(9);
            Rectangle {
                x: x as f32,
                y: y as f32,
                width: rectangle_width.min(width - x) as f32,
                height: rectangle_height.min(height - y) as f32,
                color: [
                    (seed.wrapping_mul(31) % 255) as f32 / 255.0,
                    (seed.wrapping_mul(61) % 255) as f32 / 255.0,
                    (seed.wrapping_mul(97) % 255) as f32 / 255.0,
                    0.72,
                ],
            }
        })
        .collect()
}

fn measure(mut render: impl FnMut() -> Result<Vec<u8>>, iterations: u32) -> Result<Duration> {
    let _ = render()?;
    let started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(render()?);
    }
    Ok(started.elapsed() / iterations)
}

fn sample_label_atlas(width: u32, height: u32) -> Result<map_gpu::AtlasOverlay> {
    let style = VectorTextStyle {
        field: "name".into(),
        font_family: "IPAex Gothic".into(),
        font_size: 22.0,
        font_italic: false,
        font_bold: true,
        fill: [32, 53, 42, 255],
        halo: [255, 255, 255, 230],
        halo_radius: 2.0,
        spacing: 0.0,
        min_area: 0.0,
        repeat_distance: 0.0,
        repeat_count: 1,
    };
    let labels = [
        "河南省",
        "郑州市",
        "洛阳市",
        "开封市",
        "南阳市",
        "信阳市",
        "周口市",
        "许昌市",
        "三门峡市",
        "平顶山市",
        "驻马店市",
        "焦作市",
    ]
    .iter()
    .map(|text| rasterize_text_label(text, &style))
    .collect::<Result<Vec<_>, _>>()?;
    pack_label_atlas(width, height, &labels)
}

fn sample_pattern_fill(width: u32, height: u32) -> map_gpu::AtlasOverlay {
    const PATTERN_SIZE: u32 = 16;
    let rgba = checker_pattern_rgba(PATTERN_SIZE);
    let points = sample_pattern_polygon(width, height);
    let flat = points
        .iter()
        .flat_map(|point| [point.0 as f64, point.1 as f64])
        .collect::<Vec<_>>();
    let indices = earcutr::earcut(&flat, &[], 2).expect("sample polygon triangulates");
    let vertices = points
        .into_iter()
        .map(|(x, y)| map_gpu::AtlasVertex {
            position: [
                x / width as f32 * 2.0 - 1.0,
                1.0 - y / height as f32 * 2.0,
                0.0,
                1.0,
            ],
            uv: [x / PATTERN_SIZE as f32, y / PATTERN_SIZE as f32],
            _padding: [0.0; 2],
            color: [1.0; 4],
        })
        .collect();
    map_gpu::AtlasOverlay {
        width: PATTERN_SIZE,
        height: PATTERN_SIZE,
        rgba,
        vertices,
        indices: indices.into_iter().map(|index| index as u32).collect(),
        repeat: true,
    }
}

fn checker_pattern_rgba(size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dark = ((x / 4) + (y / 4)) % 2 == 0;
            rgba.extend(if dark {
                [206, 224, 214, 255]
            } else {
                [238, 244, 240, 255]
            });
        }
    }
    rgba
}

fn sample_pattern_polygon(width: u32, height: u32) -> [(f32, f32); 6] {
    [
        (24.0, 24.0),
        (width as f32 - 24.0, 24.0),
        (width as f32 - 150.0, height as f32 / 2.0),
        (width as f32 - 24.0, height as f32 - 24.0),
        (24.0, height as f32 - 24.0),
        (150.0, height as f32 / 2.0),
    ]
}

fn cpu_render_pattern(width: u32, height: u32) -> Result<Vec<u8>> {
    const PATTERN_SIZE: u32 = 16;
    let image = Pixmap::from_vec(
        checker_pattern_rgba(PATTERN_SIZE),
        IntSize::from_wh(PATTERN_SIZE, PATTERN_SIZE).context("invalid pattern size")?,
    )
    .context("invalid pattern pixels")?;
    let mut pixmap = Pixmap::new(width, height).context("invalid image dimensions")?;
    let points = sample_pattern_polygon(width, height);
    let mut builder = PathBuilder::new();
    builder.move_to(points[0].0, points[0].1);
    for &(x, y) in &points[1..] {
        builder.line_to(x, y);
    }
    builder.close();
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = Pattern::new(
        image.as_ref(),
        SpreadMode::Repeat,
        FilterQuality::Bilinear,
        1.0,
        Transform::identity(),
    );
    pixmap.fill_path(
        &builder.finish().context("invalid sample pattern polygon")?,
        &paint,
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        None,
    );
    Ok(pixmap.data().to_vec())
}

fn pack_label_atlas(
    output_width: u32,
    output_height: u32,
    labels: &[RasterizedLabel],
) -> Result<map_gpu::AtlasOverlay> {
    const ATLAS_WIDTH: u32 = 512;
    const GAP: u32 = 2;
    let mut positions = Vec::with_capacity(labels.len());
    let (mut x, mut y, mut row_height) = (GAP, GAP, 0);
    for label in labels {
        if x + label.width + GAP > ATLAS_WIDTH {
            x = GAP;
            y += row_height + GAP;
            row_height = 0;
        }
        positions.push((x, y));
        x += label.width + GAP;
        row_height = row_height.max(label.height);
    }
    let atlas_height = (y + row_height + GAP).max(1);
    let mut rgba = vec![0; (ATLAS_WIDTH * atlas_height * 4) as usize];
    let mut vertices = Vec::with_capacity(labels.len() * 4);
    let mut indices = Vec::with_capacity(labels.len() * 6);
    for (index, (label, &(atlas_x, atlas_y))) in labels.iter().zip(&positions).enumerate() {
        for row in 0..label.height {
            let source = (row * label.width * 4) as usize;
            let destination = (((atlas_y + row) * ATLAS_WIDTH + atlas_x) * 4) as usize;
            rgba[destination..destination + (label.width * 4) as usize]
                .copy_from_slice(&label.rgba[source..source + (label.width * 4) as usize]);
        }
        let columns = 4;
        let output_x = 70 + (index as u32 % columns) * (output_width.saturating_sub(140) / columns);
        let output_y = 80 + (index as u32 / columns) * 110;
        let left = output_x as f32;
        let top = output_y as f32;
        let right = left + label.width as f32;
        let bottom = top + label.height as f32;
        let positions = [(left, top), (right, top), (right, bottom), (left, bottom)];
        let u0 = atlas_x as f32 / ATLAS_WIDTH as f32;
        let v0 = atlas_y as f32 / atlas_height as f32;
        let u1 = (atlas_x + label.width) as f32 / ATLAS_WIDTH as f32;
        let v1 = (atlas_y + label.height) as f32 / atlas_height as f32;
        let uvs = [[u0, v0], [u1, v0], [u1, v1], [u0, v1]];
        let base = vertices.len() as u32;
        for ((x, y), uv) in positions.into_iter().zip(uvs) {
            vertices.push(map_gpu::AtlasVertex {
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
    Ok(map_gpu::AtlasOverlay {
        width: ATLAS_WIDTH,
        height: atlas_height,
        rgba,
        vertices,
        indices,
        repeat: false,
    })
}

fn pack_placed_label_atlas(
    output_width: u32,
    output_height: u32,
    labels: &[PlacedLabel],
) -> Result<map_gpu::AtlasOverlay> {
    const ATLAS_WIDTH: u32 = 2048;
    const GAP: u32 = 2;
    let mut atlas_positions = Vec::with_capacity(labels.len());
    let (mut x, mut y, mut row_height) = (GAP, GAP, 0);
    for label in labels {
        if x + label.image.width + GAP > ATLAS_WIDTH {
            x = GAP;
            y += row_height + GAP;
            row_height = 0;
        }
        atlas_positions.push((x, y));
        x += label.image.width + GAP;
        row_height = row_height.max(label.image.height);
    }
    let atlas_height = (y + row_height + GAP).max(1);
    let mut rgba = vec![0; (ATLAS_WIDTH * atlas_height * 4) as usize];
    let mut vertices = Vec::with_capacity(labels.len() * 4);
    let mut indices = Vec::with_capacity(labels.len() * 6);
    for (label, &(atlas_x, atlas_y)) in labels.iter().zip(&atlas_positions) {
        for row in 0..label.image.height {
            let source = (row * label.image.width * 4) as usize;
            let destination = (((atlas_y + row) * ATLAS_WIDTH + atlas_x) * 4) as usize;
            rgba[destination..destination + (label.image.width * 4) as usize].copy_from_slice(
                &label.image.rgba[source..source + (label.image.width * 4) as usize],
            );
        }
        let left = label.x as f32;
        let top = label.y as f32;
        let right = left + label.image.width as f32;
        let bottom = top + label.image.height as f32;
        let positions = [(left, top), (right, top), (right, bottom), (left, bottom)];
        let u0 = atlas_x as f32 / ATLAS_WIDTH as f32;
        let v0 = atlas_y as f32 / atlas_height as f32;
        let u1 = (atlas_x + label.image.width) as f32 / ATLAS_WIDTH as f32;
        let v1 = (atlas_y + label.image.height) as f32 / atlas_height as f32;
        let uvs = [[u0, v0], [u1, v0], [u1, v1], [u0, v1]];
        let base = vertices.len() as u32;
        for ((x, y), uv) in positions.into_iter().zip(uvs) {
            vertices.push(map_gpu::AtlasVertex {
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
    Ok(map_gpu::AtlasOverlay {
        width: ATLAS_WIDTH,
        height: atlas_height,
        rgba,
        vertices,
        indices,
        repeat: false,
    })
}

fn real_label_request() -> VectorRenderRequest {
    VectorRenderRequest {
        source: DataSource {
            path: "/opt/cis/core/geoserver/data/china/xian201912.shp".into(),
            layer: None,
            encoding: Some("GB2312".into()),
        },
        target_crs: "EPSG:4326".into(),
        bbox: GdalBoundingBox {
            min_x: 109.5,
            min_y: 30.5,
            max_x: 117.8,
            max_y: 37.0,
        },
        width: 1024,
        height: 1024,
        buffer_pixels: 0,
        simplify_tolerance: None,
        format: GdalImageFormat::Png,
        rules: vec![VectorRenderRule {
            where_clause: None,
            filter: None,
            fill: None,
            fill_field: None,
            fill_image: None,
            stroke: None,
            stroke_field: None,
            stroke_width: 0.0,
            stroke_dasharray: Vec::new(),
            text: Some(VectorTextStyle {
                field: "name".into(),
                font_family: "IPAex Gothic".into(),
                font_size: 18.0,
                font_italic: false,
                font_bold: true,
                fill: [32, 53, 42, 255],
                halo: [255, 255, 255, 230],
                halo_radius: 2.0,
                spacing: 6.0,
                min_area: 100.0,
                repeat_distance: 180.0,
                repeat_count: 3,
            }),
        }],
        background: [0, 0, 0, 0],
    }
}

fn main() -> Result<()> {
    let gpu = pollster::block_on(GpuRenderer::new())?;
    let iterations = std::env::var("GPU_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    println!(
        "adapter={} backend={} iterations={iterations}",
        gpu.adapter_name, gpu.backend
    );
    println!("size,shapes,cpu_ms,gpu_cold_ms,gpu_resident_ms,resident_speedup");
    for (width, height, shapes) in [(256, 256, 250), (1024, 1024, 2_000), (2048, 2048, 8_000)] {
        let rectangles = scene(width, height, shapes);
        let resident_scene = gpu.prepare(width, height, &rectangles);
        let cpu = measure(|| cpu_render(width, height, &rectangles), iterations)?;
        let gpu_cold = measure(|| gpu.render(width, height, &rectangles), iterations)?;
        let gpu_resident = measure(|| gpu.render_prepared(&resident_scene), iterations)?;
        println!(
            "{}x{height},{shapes},{:.3},{:.3},{:.3},{:.2}",
            width,
            cpu.as_secs_f64() * 1000.0,
            gpu_cold.as_secs_f64() * 1000.0,
            gpu_resident.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() / gpu_resident.as_secs_f64(),
        );
    }

    let metatile_width = 1024;
    let metatile_height = 1024;
    let metatile_rectangles = scene(metatile_width, metatile_height, 4_000);
    let metatile_scene = gpu.prepare(metatile_width, metatile_height, &metatile_rectangles);
    let metatile = measure(|| gpu.render_prepared(&metatile_scene), iterations)?;
    println!(
        "metatile=4x4 total_ms={:.3} amortized_256_tile_ms={:.3}",
        metatile.as_secs_f64() * 1000.0,
        metatile.as_secs_f64() * 1000.0 / 16.0,
    );

    println!("real_dataset,size,features,rings,triangles,gpu_chunks,prepare_ms,upload_ms,cpu_ms,gpu_resident_ms,speedup,vram_mb,mean_abs_error,pixels_over_16_pct");
    let production_gpu = ProductionGpuRenderer::new(&map_gpu::RendererOptions {
        adapter_name: std::env::var("GPU_BENCH_ADAPTER").ok(),
    })?;
    if Path::new("/opt/cis/core/geoserver/data/china/xian201912.shp").is_file() {
        let request = real_label_request();
        let label_started = Instant::now();
        let labels = GdalEngine.layout_vector_labels(&request)?;
        let label_layout_ms = label_started.elapsed().as_secs_f64() * 1000.0;
        let atlas = pack_placed_label_atlas(request.width, request.height, &labels)?;
        let scene =
            production_gpu.prepare_with_atlas(request.width, request.height, &[], Some(&atlas))?;
        let gpu_time = measure(|| Ok(production_gpu.render_rgba(&scene)?), iterations)?;
        let gpu_pixels = production_gpu.render_rgba(&scene)?;
        let cpu_image = GdalEngine.render_vector(&request)?;
        let cpu_pixels = image::load_from_memory(&cpu_image.bytes)?
            .to_rgba8()
            .into_raw();
        let diff = image_diff(&cpu_pixels, &gpu_pixels);
        println!(
            "real_label_layout labels={} layout_ms={:.3} gpu_ms={:.3} mae={:.3} pixels_over_16_pct={:.3}",
            labels.len(),
            label_layout_ms,
            gpu_time.as_secs_f64() * 1000.0,
            diff.mean_absolute_error,
            diff.pixels_over_16_percent,
        );
        if std::env::var_os("GPU_BENCH_ASSERT_CORRECTNESS").is_some()
            && (diff.mean_absolute_error > 2.0 || diff.pixels_over_16_percent > 2.0)
        {
            anyhow::bail!(
                "real labels exceeded GPU correctness limits: MAE {:.3}, pixels >16 {:.3}%",
                diff.mean_absolute_error,
                diff.pixels_over_16_percent
            );
        }
        if let Some(output) = std::env::var_os("GPU_BENCH_OUTPUT_DIR") {
            save_comparison(
                Path::new(&output),
                "real-label-layout",
                request.width,
                request.height,
                &cpu_pixels,
                &gpu_pixels,
            )?;
        }
    }
    let label_atlas = sample_label_atlas(1024, 512)?;
    let pattern_fill = sample_pattern_fill(1024, 512);
    let pattern_scene =
        production_gpu.prepare_with_atlases(1024, 512, &[], std::slice::from_ref(&pattern_fill))?;
    let pattern_gpu = production_gpu.render_rgba(&pattern_scene)?;
    let pattern_cpu = cpu_render_pattern(1024, 512)?;
    let pattern_diff = image_diff(&pattern_cpu, &pattern_gpu);
    if std::env::var_os("GPU_BENCH_ASSERT_CORRECTNESS").is_some()
        && (pattern_diff.mean_absolute_error > 3.5 || pattern_diff.pixels_over_16_percent > 4.0)
    {
        anyhow::bail!(
            "pattern fill exceeded GPU correctness limits: MAE {:.3}, pixels >16 {:.3}%",
            pattern_diff.mean_absolute_error,
            pattern_diff.pixels_over_16_percent
        );
    }
    let label_scene = production_gpu.prepare_with_atlases(
        1024,
        512,
        &[],
        &[pattern_fill, label_atlas.clone()],
    )?;
    let label_time = measure(|| Ok(production_gpu.render_rgba(&label_scene)?), iterations)?;
    let label_pixels = production_gpu.render_rgba(&label_scene)?;
    println!(
        "pattern_and_label_atlas=1 pattern+12 labels label_atlas={}x{} resident_ms={:.3} vram_mb={:.2}",
        label_atlas.width,
        label_atlas.height,
        label_time.as_secs_f64() * 1000.0,
        label_scene.stats().estimated_gpu_bytes as f64 / (1024.0 * 1024.0),
    );
    println!(
        "pattern_fill_diff mean_abs_error={:.3} pixels_over_16_pct={:.3}",
        pattern_diff.mean_absolute_error, pattern_diff.pixels_over_16_percent
    );
    if let Some(output) = std::env::var_os("GPU_BENCH_OUTPUT_DIR") {
        std::fs::create_dir_all(&output)?;
        image::save_buffer(
            Path::new(&output).join("label-atlas-gpu.png"),
            &label_pixels,
            1024,
            512,
            image::ColorType::Rgba8,
        )?;
        save_comparison(
            Path::new(&output),
            "pattern-fill",
            1024,
            512,
            &pattern_cpu,
            &pattern_gpu,
        )?;
    }
    let real_iterations = iterations.min(5);
    let real_cases = [
        (
            "province-china",
            "/opt/cis/core/geoserver/data/china/sheng201912.shp",
            2048,
            1536,
            WorldBounds {
                min_x: 73.0,
                min_y: 3.0,
                max_x: 136.0,
                max_y: 54.0,
            },
            false,
        ),
        (
            "county-henan",
            "/opt/cis/core/geoserver/data/china/xian201912.shp",
            1024,
            1024,
            WorldBounds {
                min_x: 109.5,
                min_y: 30.5,
                max_x: 117.8,
                max_y: 37.0,
            },
            false,
        ),
        (
            "township-henan",
            "/opt/cis/core/geoserver/data/china/shape_china_xz.shp",
            1024,
            1024,
            WorldBounds {
                min_x: 109.5,
                min_y: 30.5,
                max_x: 117.8,
                max_y: 37.0,
            },
            true,
        ),
        (
            "county-henan-tile",
            "/opt/cis/core/geoserver/data/china/xian201912.shp",
            256,
            256,
            WorldBounds {
                min_x: 112.8,
                min_y: 33.2,
                max_x: 114.2,
                max_y: 34.6,
            },
            false,
        ),
        (
            "township-henan-tile",
            "/opt/cis/core/geoserver/data/china/shape_china_xz.shp",
            256,
            256,
            WorldBounds {
                min_x: 112.8,
                min_y: 33.2,
                max_x: 114.2,
                max_y: 34.6,
            },
            true,
        ),
    ];
    for (name, path, width, height, bounds, dashed_boundaries) in real_cases {
        if !Path::new(path).is_file() {
            eprintln!("skipping missing real dataset: {path}");
            continue;
        }
        let prepare_started = Instant::now();
        let real_scene = load_real_scene(path, width, height, bounds, dashed_boundaries)?;
        let prepare_elapsed = prepare_started.elapsed();
        let upload_started = Instant::now();
        let gpu_scene = production_gpu.prepare(width, height, &real_scene.mesh_chunks)?;
        let upload_elapsed = upload_started.elapsed();
        let cpu = measure(
            || cpu_render_real(width, height, &real_scene),
            real_iterations,
        )?;
        let gpu_time = measure(
            || Ok(production_gpu.render_rgba(&gpu_scene)?),
            real_iterations,
        )?;
        let cpu_pixels = cpu_render_real(width, height, &real_scene)?;
        let gpu_pixels = production_gpu.render_rgba(&gpu_scene)?;
        let diff = image_diff(&cpu_pixels, &gpu_pixels);
        if let Some(output) = std::env::var_os("GPU_BENCH_OUTPUT_DIR") {
            save_comparison(
                Path::new(&output),
                name,
                width,
                height,
                &cpu_pixels,
                &gpu_pixels,
            )?;
        }
        let changed_pixel_limit = if dashed_boundaries { 7.0 } else { 4.0 };
        if std::env::var_os("GPU_BENCH_ASSERT_CORRECTNESS").is_some()
            && (diff.mean_absolute_error > 3.5 || diff.pixels_over_16_percent > changed_pixel_limit)
        {
            anyhow::bail!(
                "{name} exceeded GPU correctness limits: MAE {:.3}, pixels >16 {:.3}%",
                diff.mean_absolute_error,
                diff.pixels_over_16_percent
            );
        }
        let scene_stats = gpu_scene.stats();
        let triangles = scene_stats.indices / 3;
        println!(
            "{name},{}x{height},{},{},{triangles},{},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2},{:.3},{:.3}",
            width,
            real_scene.features,
            real_scene.rings,
            scene_stats.chunks,
            prepare_elapsed.as_secs_f64() * 1000.0,
            upload_elapsed.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() * 1000.0,
            gpu_time.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() / gpu_time.as_secs_f64(),
            scene_stats.estimated_gpu_bytes as f64 / (1024.0 * 1024.0),
            diff.mean_absolute_error,
            diff.pixels_over_16_percent,
        );
    }
    Ok(())
}
