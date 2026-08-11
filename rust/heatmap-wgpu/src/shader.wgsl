struct ViewUniform {
    offset_scale: vec4<f32>,
    color_range: vec4<f32>,
};

@group(0) @binding(0) var intensity_texture: texture_2d<f32>;
@group(0) @binding(1) var intensity_sampler: sampler;
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

fn heat_color(value: f32) -> vec3<f32> {
    let v = clamp(value, 0.0, 1.0);
    let low = vec3<f32>(0.035, 0.055, 0.10);
    let middle = vec3<f32>(0.88, 0.16, 0.08);
    let high = vec3<f32>(1.0, 0.93, 0.46);
    return select(mix(low, middle, v * 2.0), mix(middle, high, (v - 0.5) * 2.0), v >= 0.5);
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let scale = view.offset_scale.z;
    let uv = (input.uv - vec2<f32>(0.5)) / scale + vec2<f32>(0.5) - view.offset_scale.xy;
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec4<f32>(0.012, 0.016, 0.025, 1.0);
    }
    let intensity = textureSample(intensity_texture, intensity_sampler, uv).r;
    let normalized = (intensity - view.color_range.x) / max(view.color_range.y - view.color_range.x, 0.000001);
    return vec4<f32>(heat_color(normalized), 1.0);
}
