use std::{
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use gdal::{
    vector::{Geometry, LayerAccess},
    Dataset,
};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Transform};
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
    polygons: Vec<Vec<Vec<(f32, f32)>>>,
    vertices: Vec<Vertex>,
    features: usize,
    rings: usize,
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
) -> Result<RealGeometryScene> {
    let dataset = Dataset::open(path).with_context(|| format!("failed to open {path}"))?;
    let mut layer = dataset.layer(0)?;
    layer.set_spatial_filter_rect(bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y);
    let mut scene = RealGeometryScene::default();
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
    for index in indices {
        let point = points[index];
        let x = point[0] / width as f32 * 2.0 - 1.0;
        let y = 1.0 - point[1] / height as f32 * 2.0;
        scene.vertices.push(Vertex {
            position: [x, y, 0.0, 1.0],
            color,
        });
    }
    scene.rings += rings.len();
    scene.polygons.push(rings);
    Ok(())
}

fn cpu_render_real(width: u32, height: u32, scene: &RealGeometryScene) -> Result<Vec<u8>> {
    let mut pixmap = Pixmap::new(width, height).context("invalid image dimensions")?;
    pixmap.fill(Color::TRANSPARENT);
    let mut paint = Paint::default();
    paint.set_color_rgba8(48, 118, 84, 199);
    paint.anti_alias = true;
    for polygon in &scene.polygons {
        let mut builder = PathBuilder::new();
        for ring in polygon {
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
        }
    }
    Ok(pixmap.data().to_vec())
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

    println!("real_dataset,size,features,rings,triangles,gpu_chunks,prepare_ms,upload_ms,cpu_ms,gpu_resident_ms,speedup,vram_mb");
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
        ),
    ];
    for (name, path, width, height, bounds) in real_cases {
        if !Path::new(path).is_file() {
            eprintln!("skipping missing real dataset: {path}");
            continue;
        }
        let prepare_started = Instant::now();
        let real_scene = load_real_scene(path, width, height, bounds)?;
        let prepare_elapsed = prepare_started.elapsed();
        let upload_started = Instant::now();
        let gpu_scene = gpu.prepare_vertices(width, height, &real_scene.vertices);
        let upload_elapsed = upload_started.elapsed();
        let cpu = measure(
            || cpu_render_real(width, height, &real_scene),
            real_iterations,
        )?;
        let gpu_time = measure(|| gpu.render_prepared(&gpu_scene), real_iterations)?;
        let triangles = real_scene.vertices.len() / 3;
        let vram_mb = real_scene.vertices.len() * std::mem::size_of::<Vertex>()
            + (width * height * 8) as usize;
        println!(
            "{name},{}x{height},{},{},{triangles},{},{:.3},{:.3},{:.3},{:.3},{:.2},{:.2}",
            width,
            real_scene.features,
            real_scene.rings,
            gpu_scene.batches.len(),
            prepare_elapsed.as_secs_f64() * 1000.0,
            upload_elapsed.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() * 1000.0,
            gpu_time.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() / gpu_time.as_secs_f64(),
            vram_mb as f64 / (1024.0 * 1024.0),
        );
    }
    Ok(())
}
