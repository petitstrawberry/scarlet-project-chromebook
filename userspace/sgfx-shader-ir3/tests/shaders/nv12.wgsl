@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var filtering:sampler;
@fragment fn implicit_sample(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    let c=textureSample(image,filtering,uv); return c.yxwz*vec4<f32>(0.5,0.75,1.0,0.25);
}
@fragment fn biased_sample(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    return textureSampleBias(image,filtering,uv,0.25);
}
@fragment fn explicit_level(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    return textureSampleLevel(image,filtering,uv,0.0);
}
@fragment fn explicit_gradient(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    return textureSampleGrad(image,filtering,uv,vec2<f32>(0.01,0.0),vec2<f32>(0.0,0.01));
}
@fragment fn sample_offset(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    return textureSample(image,filtering,uv,vec2<i32>(1,-1));
}
@fragment fn dimensions()->@location(0) vec4<f32> {
    return vec4<f32>(vec2<f32>(textureDimensions(image)),0.0,1.0);
}
@fragment fn levels()->@location(0) vec4<f32> {
    return vec4<f32>(f32(textureNumLevels(image)),0.0,0.0,1.0);
}
@fragment fn fetch(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    return textureLoad(image,vec2<i32>(p.xy),0);
}
@fragment fn gather(@location(0) uv:vec2<f32>)->@location(0) vec4<f32> {
    return textureGather(0,image,filtering,uv);
}
@vertex fn vertex_sample(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
    return textureSampleLevel(image,filtering,vec2<f32>(f32(i)/8.0,0.5),0.0);
}
@group(0) @binding(2) var<storage,read_write> output:array<vec4<f32>>;
@compute @workgroup_size(1) fn compute_sample(@builtin(global_invocation_id) i:vec3<u32>) {
    output[i.x]=textureSampleLevel(image,filtering,vec2<f32>(f32(i.x)/8.0,0.5),0.0);
}
