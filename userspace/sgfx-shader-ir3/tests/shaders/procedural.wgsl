struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec3<f32>,
}
@vertex fn vs(@builtin(vertex_index) index: u32) -> Varyings {
    var positions = array<vec2<f32>, 3>(vec2(-0.8, -0.7), vec2(0.8, -0.7), vec2(0.0, 0.8));
    var out: Varyings;
    out.position = vec4(positions[index], 0.0, 1.0);
    out.color = vec3(f32(index == 0u), f32(index == 1u), f32(index == 2u));
    return out;
}
@fragment fn fs(input: Varyings) -> @location(0) vec4<f32> {
    return vec4(input.color * input.color + vec3(0.05), 1.0);
}
