struct AtlasVertex {
    position: vec4<f32>,
    uv: vec2<f32>,
    color: vec4<f32>,
};

@group(0) @binding(0)
var<storage, read> vertices: array<AtlasVertex>;
@group(0) @binding(1)
var atlas_texture: texture_2d<f32>;
@group(0) @binding(2)
var atlas_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let vertex = vertices[vertex_index];
    var output: VertexOutput;
    output.position = vertex.position;
    output.uv = vertex.uv;
    output.color = vertex.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(atlas_texture, atlas_sampler, input.uv) * input.color;
}
