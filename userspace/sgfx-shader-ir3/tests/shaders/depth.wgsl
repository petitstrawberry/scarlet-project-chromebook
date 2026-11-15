@group(1) @binding(7) var depth: texture_depth_2d;
@group(1) @binding(8) var shadow: sampler_comparison;
@group(1) @binding(9) var regular: sampler;
@group(2) @binding(0) var layers: texture_depth_2d_array;
@group(2) @binding(1) var cube: texture_depth_cube;
@group(3) @binding(0) var<storage, read_write> output: array<f32>;

@vertex fn vertex() -> @builtin(position) vec4<f32> { return vec4<f32>(0.0, 0.0, 0.0, 1.0); }
@fragment fn load(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureLoad(depth, vec2<i32>(p.xy), 0));
}
@fragment fn sample(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSample(depth, regular, p.xy / 16.0));
}
@fragment fn compare(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleCompare(depth, shadow, p.xy / 16.0, 0.5));
}
@fragment fn compare_level(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleCompareLevel(depth, shadow, p.xy / 16.0, 0.5));
}
@fragment fn compare_array(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleCompareLevel(layers, shadow, p.xy / 16.0, 1, 0.5));
}
@fragment fn compare_cube(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleCompareLevel(cube, shadow, vec3<f32>(p.xy / 8.0 - 1.0, 1.0), 0.5));
}
@fragment fn gather_compare(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureGatherCompare(depth, shadow, p.xy / 16.0, 0.5);
}
@compute @workgroup_size(32) fn compute_compare(@builtin(global_invocation_id) id: vec3<u32>) {
    output[id.x] = textureSampleCompareLevel(depth, shadow, vec2<f32>(f32(id.x) / 32.0, 0.5), 0.5);
}
