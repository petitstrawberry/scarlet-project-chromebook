@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var filtering:sampler;
@vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
    let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);
}
@fragment fn fs(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    let uv=p.xy/vec2<f32>(16.0)*2.5-vec2<f32>(0.75);
    return textureSampleBaseClampToEdge(image,filtering,uv);
}
