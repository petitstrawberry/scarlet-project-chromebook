@group(0) @binding(0) var<storage,read> b0:array<u32>;
@group(0) @binding(1) var<storage,read> b1:array<u32>;
@group(0) @binding(2) var<storage,read> b2:array<u32>;
@group(0) @binding(3) var<storage,read> b3:array<u32>;
 @fragment fn main(@builtin(position) pos:vec4<f32>,@location(0) @interpolate(flat) value:vec4<u32>)->@location(0) vec4<f32> {
 let pixel=u32(pos.x)+4u*u32(pos.y);let i=pixel*4369u;
 let v=value+vec4<u32>(b0[min(i,arrayLength(&b0)-1u)],b1[min(i,arrayLength(&b1)-1u)],b2[min(i,arrayLength(&b2)-1u)],b3[min(i,arrayLength(&b3)-1u)]);
 return vec4<f32>(v&vec4<u32>(255u))/255.0; }