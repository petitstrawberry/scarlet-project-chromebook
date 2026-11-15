struct Output { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) params:vec3<u32> };
@vertex fn main(@builtin(vertex_index) vertex:u32,@location(0) base:u32,@location(1) instance:u32,@location(2) draw:u32)->Output {
 let index=vertex%3u;let x=f32((index<<1u)&2u);let y=f32(index&2u);
 return Output(vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0),vec3<u32>(draw,base,instance));
}