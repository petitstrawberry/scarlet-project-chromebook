@group(0) @binding(0) var image:texture_2d<f32>;
@group(0) @binding(1) var filtering:sampler;
@vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> {
    let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);
}
@fragment fn fs(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32> {
    let dimensions=vec2<f32>(textureDimensions(image));
    let uv=vec2<f32>(p.x,16.0-p.y)*(vec2<f32>(8.0)/dimensions)+vec2<f32>(0.03125);
    let c=textureSample(image,filtering,uv);
    // Conversion must precede component selection and application arithmetic.
    return vec4<f32>(c.b*0.75,c.g*0.5,c.r,f32(textureNumLevels(image))*0.5);
}
