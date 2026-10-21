struct Vertex {
    position: vec4<f32>,
    color: vec4<f32>,
};

@group(0) @binding(0)
var<storage, read> vertices: array<Vertex>;

struct ViewTransform {
    scale_offset: vec4<f32>,
};

@group(1) @binding(0)
var<uniform> view_transform: ViewTransform;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let vertex = vertices[vertex_index];
    var output: VertexOutput;
    output.position = vec4<f32>(
        vertex.position.xy * view_transform.scale_offset.xy + view_transform.scale_offset.zw,
        vertex.position.zw,
    );
    output.color = vertex.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
