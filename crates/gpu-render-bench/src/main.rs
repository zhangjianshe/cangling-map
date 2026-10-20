use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
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

    fn render(&self, width: u32, height: u32, rectangles: &[Rectangle]) -> Result<Vec<u8>> {
        let vertices = rectangles
            .iter()
            .flat_map(|rectangle| rectangle_vertices(width, height, *rectangle))
            .collect::<Vec<_>>();
        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("polygon vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("polygon vertices"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: vertex_buffer.as_entire_binding(),
            }],
        });
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
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen polygon pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..vertices.len() as u32, 0..1);
        }
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &output,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            texture.size(),
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = output.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .context("GPU readback callback was lost")??;
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in mapped.chunks_exact(padded_bytes_per_row as usize) {
            pixels.extend_from_slice(&row[..unpadded_bytes_per_row as usize]);
        }
        drop(mapped);
        output.unmap();
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
    println!("size,shapes,cpu_ms,gpu_total_ms,speedup");
    for (width, height, shapes) in [(256, 256, 250), (1024, 1024, 2_000), (2048, 2048, 8_000)] {
        let rectangles = scene(width, height, shapes);
        let cpu = measure(|| cpu_render(width, height, &rectangles), iterations)?;
        let gpu_time = measure(|| gpu.render(width, height, &rectangles), iterations)?;
        println!(
            "{}x{height},{shapes},{:.3},{:.3},{:.2}",
            width,
            cpu.as_secs_f64() * 1000.0,
            gpu_time.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() / gpu_time.as_secs_f64(),
        );
    }
    Ok(())
}
