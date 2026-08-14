struct ViewUniform {
    texture_rect: vec4<f32>,
    color_range: vec4<f32>,
    surface: vec4<f32>,
    selection: vec4<f32>,
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
    let aspect = view.surface.x;
    var square_uv = input.uv;
    if (aspect > 1.0) {
        let width_fraction = 1.0 / aspect;
        let left = (1.0 - width_fraction) * 0.5;
        if (input.uv.x < left || input.uv.x > left + width_fraction) {
            return vec4<f32>(0.012, 0.016, 0.025, 1.0);
        }
        square_uv.x = (input.uv.x - left) / width_fraction;
    } else {
        let height_fraction = aspect;
        let top = (1.0 - height_fraction) * 0.5;
        if (input.uv.y < top || input.uv.y > top + height_fraction) {
            return vec4<f32>(0.012, 0.016, 0.025, 1.0);
        }
        square_uv.y = (input.uv.y - top) / height_fraction;
    }

    let texture_uv = view.texture_rect.xy + square_uv * view.texture_rect.zw;
    if (any(texture_uv < vec2<f32>(0.0)) || any(texture_uv > vec2<f32>(1.0))) {
        return vec4<f32>(0.012, 0.016, 0.025, 1.0);
    }
    let intensity = textureSample(intensity_texture, intensity_sampler, texture_uv).r;
    let normalized = (intensity - view.color_range.x)
        / max(view.color_range.y - view.color_range.x, 0.000001);
    var color = heat_color(normalized);
    if (view.selection.z > 0.5) {
        let start = view.selection.x;
        let end = view.selection.y;
        let inside_x = square_uv.x >= start && square_uv.x <= end;
        let inside_y = square_uv.y >= start && square_uv.y <= end;
        if (inside_x || inside_y) {
            color = mix(color, vec3<f32>(0.08, 0.72, 1.0), 0.16);
        }
        let edge = 1.5 / max(min(view.surface.y, view.surface.z), 512.0);
        if (abs(square_uv.x - start) < edge || abs(square_uv.x - end) < edge
            || abs(square_uv.y - start) < edge || abs(square_uv.y - end) < edge) {
            color = vec3<f32>(0.12, 0.85, 1.0);
        }
    }
    return vec4<f32>(color, 1.0);
}
