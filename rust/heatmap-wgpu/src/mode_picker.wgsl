struct ViewUniform {
    texture_rect: vec4<f32>,
    color_range: vec4<f32>,
    surface: vec4<f32>,
    selection: vec4<f32>,
};

@group(0) @binding(0) var overlay_texture: texture_2d<f32>;
@group(0) @binding(1) var overlay_sampler: sampler;
@group(0) @binding(2) var<uniform> view: ViewUniform;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(3.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let position = positions[vertex_index];
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.uv = position * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let panel = vec2<f32>(768.0, 520.0);
    let surface = max(view.surface.yz, vec2<f32>(1.0));
    let scale = min(1.0, min((surface.x - 24.0) / panel.x, (surface.y - 24.0) / panel.y));
    let size = panel * max(scale, 0.1);
    let origin = (surface - size) * 0.5;
    let pixel = input.uv * surface;
    if (pixel.x < origin.x || pixel.y < origin.y
        || pixel.x > origin.x + size.x || pixel.y > origin.y + size.y) {
        discard;
    }
    let uv = (pixel - origin) / size;
    return textureSample(overlay_texture, overlay_sampler, uv);
}
