//! Reusable off-screen GPU raster backend for prepared vector meshes.

use std::sync::mpsc;

use bytemuck::{Pod, Zeroable};
use thiserror::Error;
use wgpu::util::DeviceExt;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Vertex {
    /// Clip-space position. Z should be zero and W should be one.
    pub position: [f32; 4],
    pub color: [f32; 4],
}

#[derive(Debug, Clone, Default)]
pub struct MeshChunk {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl MeshChunk {
    pub fn estimated_gpu_bytes(&self) -> usize {
        self.vertices.len() * std::mem::size_of::<Vertex>()
            + self.indices.len() * std::mem::size_of::<u32>()
    }
}

#[derive(Debug, Clone, Default)]
pub struct RendererOptions {
    /// Case-insensitive adapter-name substring, such as `NVIDIA`.
    pub adapter_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub name: String,
    pub backend: String,
    pub device_type: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PreparedSceneStats {
    pub chunks: usize,
    pub vertices: usize,
    pub indices: usize,
    pub estimated_gpu_bytes: usize,
}

#[derive(Debug, Error)]
pub enum GpuError {
    #[error("no compatible GPU adapter was found")]
    AdapterNotFound,
    #[error("GPU adapter containing {0:?} was not found")]
    NamedAdapterNotFound(String),
    #[error("GPU device creation failed: {0}")]
    Device(String),
    #[error("render dimensions must be non-zero")]
    InvalidDimensions,
    #[error("mesh chunk {chunk} has {indices} indices but no vertices")]
    MissingVertices { chunk: usize, indices: usize },
    #[error("mesh chunk {chunk} contains index {index} outside {vertices} vertices")]
    InvalidIndex {
        chunk: usize,
        index: u32,
        vertices: usize,
    },
    #[error("mesh chunk {chunk} vertex buffer requires {bytes} bytes; device limit is {limit}")]
    VertexBufferTooLarge {
        chunk: usize,
        bytes: usize,
        limit: u64,
    },
    #[error("mesh chunk {chunk} index buffer requires {bytes} bytes; device limit is {limit}")]
    IndexBufferTooLarge {
        chunk: usize,
        bytes: usize,
        limit: u64,
    },
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    adapter: AdapterInfo,
}

pub struct PreparedScene {
    batches: Vec<DrawBatch>,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    output: wgpu::Buffer,
    width: u32,
    height: u32,
    unpadded_bytes_per_row: u32,
    padded_bytes_per_row: u32,
    stats: PreparedSceneStats,
}

struct DrawBatch {
    bind_group: wgpu::BindGroup,
    index_buffer: wgpu::Buffer,
    index_count: u32,
}

impl Renderer {
    pub fn new(options: &RendererOptions) -> Result<Self, GpuError> {
        pollster::block_on(Self::new_async(options))
    }

    pub async fn new_async(options: &RendererOptions) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::default();
        let adapter = if let Some(requested) = options.adapter_name.as_deref() {
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
                .ok_or_else(|| GpuError::NamedAdapterNotFound(requested.to_owned()))?
        } else {
            instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .ok_or(GpuError::AdapterNotFound)?
        };
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("cangling-map GPU renderer"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(|error| GpuError::Device(error.to_string()))?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cangling-map flat mesh shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cangling-map mesh layout"),
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
            label: Some("cangling-map mesh pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cangling-map mesh pipeline"),
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
            adapter: AdapterInfo {
                name: info.name,
                backend: format!("{:?}", info.backend),
                device_type: format!("{:?}", info.device_type),
            },
        })
    }

    pub fn adapter_info(&self) -> &AdapterInfo {
        &self.adapter
    }

    pub fn prepare(
        &self,
        width: u32,
        height: u32,
        chunks: &[MeshChunk],
    ) -> Result<PreparedScene, GpuError> {
        if width == 0 || height == 0 {
            return Err(GpuError::InvalidDimensions);
        }
        let limits = self.device.limits();
        let mut stats = PreparedSceneStats::default();
        let mut batches = Vec::with_capacity(chunks.len());
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            validate_chunk(chunk_index, chunk, &limits)?;
            if chunk.indices.is_empty() {
                continue;
            }
            let vertex_buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cangling-map mesh vertices"),
                    contents: bytemuck::cast_slice(&chunk.vertices),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let index_buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cangling-map mesh indices"),
                    contents: bytemuck::cast_slice(&chunk.indices),
                    usage: wgpu::BufferUsages::INDEX,
                });
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cangling-map mesh bind group"),
                layout: &self.bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: vertex_buffer.as_entire_binding(),
                }],
            });
            stats.chunks += 1;
            stats.vertices += chunk.vertices.len();
            stats.indices += chunk.indices.len();
            stats.estimated_gpu_bytes += chunk.estimated_gpu_bytes();
            batches.push(DrawBatch {
                bind_group,
                index_buffer,
                index_count: chunk.indices.len() as u32,
            });
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cangling-map offscreen target"),
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
            label: Some("cangling-map readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        stats.estimated_gpu_bytes += (u64::from(width) * u64::from(height) * 4 * 2) as usize;
        Ok(PreparedScene {
            batches,
            texture,
            view,
            output,
            width,
            height,
            unpadded_bytes_per_row,
            padded_bytes_per_row,
            stats,
        })
    }

    pub fn render_rgba(&self, scene: &PreparedScene) -> Result<Vec<u8>, GpuError> {
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cangling-map vector pass"),
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
                pass.set_index_buffer(batch.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..batch.index_count, 0, 0..1);
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
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .map_err(|error| GpuError::Readback(error.to_string()))?
            .map_err(|error| GpuError::Readback(error.to_string()))?;
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

impl PreparedScene {
    pub fn stats(&self) -> PreparedSceneStats {
        self.stats
    }
}

fn validate_chunk(
    chunk_index: usize,
    chunk: &MeshChunk,
    limits: &wgpu::Limits,
) -> Result<(), GpuError> {
    if chunk.vertices.is_empty() && !chunk.indices.is_empty() {
        return Err(GpuError::MissingVertices {
            chunk: chunk_index,
            indices: chunk.indices.len(),
        });
    }
    if let Some(index) = chunk
        .indices
        .iter()
        .copied()
        .find(|index| *index as usize >= chunk.vertices.len())
    {
        return Err(GpuError::InvalidIndex {
            chunk: chunk_index,
            index,
            vertices: chunk.vertices.len(),
        });
    }
    let vertex_bytes = chunk.vertices.len() * std::mem::size_of::<Vertex>();
    if vertex_bytes as u64 > u64::from(limits.max_storage_buffer_binding_size) {
        return Err(GpuError::VertexBufferTooLarge {
            chunk: chunk_index,
            bytes: vertex_bytes,
            limit: u64::from(limits.max_storage_buffer_binding_size),
        });
    }
    let index_bytes = chunk.indices.len() * std::mem::size_of::<u32>();
    if index_bytes as u64 > limits.max_buffer_size {
        return Err(GpuError::IndexBufferTooLarge {
            chunk: chunk_index,
            bytes: index_bytes,
            limit: limits.max_buffer_size,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytemuck::Zeroable;

    use super::{validate_chunk, MeshChunk, Vertex};

    #[test]
    fn rejects_an_out_of_range_index_before_gpu_upload() {
        let chunk = MeshChunk {
            vertices: vec![Vertex {
                position: [0.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
            }],
            indices: vec![1],
        };
        assert!(validate_chunk(0, &chunk, &wgpu::Limits::downlevel_defaults()).is_err());
    }

    #[test]
    fn estimates_indexed_mesh_memory() {
        let chunk = MeshChunk {
            vertices: vec![Vertex::zeroed(); 4],
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        assert_eq!(chunk.estimated_gpu_bytes(), 4 * 32 + 6 * 4);
    }
}
