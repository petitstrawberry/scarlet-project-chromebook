@group(0) @binding(0) var<storage, read> values: array<f32>;

@vertex fn readonly_vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(vec2(-0.8, -0.7), vec2(0.8, -0.7), vec2(0.0, 0.8));
    return vec4<f32>(positions[index], values[index], 1.0);
}

@fragment fn readonly_fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let value = values[u32(position.x) % 128u];
    return vec4<f32>(value, value * 0.5, 0.0, 1.0);
}
