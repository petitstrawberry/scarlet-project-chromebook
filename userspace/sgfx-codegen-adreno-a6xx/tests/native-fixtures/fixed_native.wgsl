struct Params {transform:mat4x4<f32>,color:vec4<f32>,mask_from_red:u32}; var<push_constant> params:Params; struct Out {@builtin(position) position:vec4<f32>,@location(0) color:vec4<f32>,@location(1) uv:vec2<f32>};
@group(0) @binding(0) var image:texture_2d<f32>; @group(0) @binding(1) var filtering:sampler;
@vertex fn vs(@location(0) p:vec3<f32>,@location(1) c:vec4<f32>,@location(2) uv:vec2<f32>)->Out {return Out(params.transform*vec4<f32>(p,1.0),c,uv);}
@fragment fn fs(input:Out)->@location(0) vec4<f32>{
let sampled=textureSample(image,filtering,input.uv); let coverage=select(sampled.a,sampled.r,params.mask_from_red!=0u);
return vec4<f32>(params.color.rgb*input.color.rgb,coverage*input.color.a*params.color.a);}
