@group(0) @binding(0) var<storage,read> b0:array<u32>;
@group(0) @binding(1) var<storage,read> b1:array<u32>;
@group(0) @binding(2) var<storage,read> b2:array<u32>;
@group(0) @binding(3) var<storage,read> b3:array<u32>;
 struct Out { @builtin(position) position:vec4<f32>,@location(0) @interpolate(flat) value:vec4<u32> };
@vertex fn main(@builtin(vertex_index) vertex:u32)->Out { let x=f32((vertex<<1u)&2u);let y=f32(vertex&2u);let position=vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0); return Out(position,vec4<u32>(b0[65535u]+b0[32768u],b1[65535u]+b1[32768u],b2[65535u]+b2[32768u],b3[65535u]+b3[32768u])); }