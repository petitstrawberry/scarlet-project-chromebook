struct Params {transform:mat4x4<f32>,color:vec4<f32>,mask_from_red:u32}; var<push_constant> params:Params; struct Out {@builtin(position) position:vec4<f32>,@location(0) color:vec4<f32>,@location(1) uv:vec2<f32>};
@group(0) @binding(0) var image:texture_2d<f32>; @group(0) @binding(1) var filtering:sampler;
@vertex fn vs(@location(0) p:vec4<f32>,@location(1) uv:vec2<f32>)->Out {return Out(params.transform*p,vec4<f32>(1.0),uv);}
@fragment fn fs(input:Out)->@location(0) vec4<f32>{
let raw=textureSample(image,filtering,input.uv); let yuv=vec4<f32>(raw.xyz,1.0);
let component0=clamp(dot(yuv,vec4<f32>(1.1643835,0,1.7927411,-0.97294515)),0.0,1.0);
let component1=clamp(dot(yuv,vec4<f32>(1.1643835,-0.21324858,-0.5329093,0.30148268)),0.0,1.0);
let component2=clamp(dot(yuv,vec4<f32>(1.1643835,2.1124017,0,-1.1334022)),0.0,1.0);
let sampled=vec4<f32>(component0,component1,component2,1.0);
let coverage=select(sampled.a,sampled.r,params.mask_from_red!=0u);
return sampled*params.color;}
