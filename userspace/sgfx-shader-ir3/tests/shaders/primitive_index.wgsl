struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}
@vertex fn vs(@builtin(vertex_index) vertex: u32) -> VertexOut {
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0));
    let xy = positions[vertex];
    return VertexOut(vec4<f32>(xy, 0.0, 1.0), xy * 0.5 + 0.5);
}
@fragment fn fs(@builtin(primitive_index) primitive: u32, @location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(f32(17u + primitive * 97u) / 255.0, uv.x, uv.y, 1.0);
}
@fragment fn plain() -> @location(0) vec4<f32> {
    return vec4<f32>(0.25, 0.5, 0.75, 1.0);
}
