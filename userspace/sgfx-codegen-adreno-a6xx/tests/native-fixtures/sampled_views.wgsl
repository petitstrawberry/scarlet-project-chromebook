@group(0) @binding(0) var depth: texture_depth_2d;
@group(0) @binding(1) var shadow: sampler_comparison;
@group(0) @binding(2) var layers: texture_2d_array<f32>;
@group(0) @binding(3) var colors: sampler;
@group(0) @binding(4) var cube: texture_cube<f32>;

@fragment fn depth_sample(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let loaded = textureLoad(depth, vec2<i32>(p.xy) % vec2<i32>(16), 0);
    let compared = textureSampleCompareLevel(depth, shadow, p.xy / 16.0, 0.5);
    return vec4<f32>(loaded, compared, 0.0, 1.0);
}
@fragment fn array_sample(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureSampleLevel(layers, colors, p.xy / 16.0, 1, 0.0);
}
@fragment fn cube_sample(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureSampleLevel(cube, colors, vec3<f32>(p.xy / 8.0 - 1.0, 1.0), 0.0);
}
