struct Out { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) value:u32 };
@vertex fn main(@builtin(vertex_index) vertex:u32)->Out {let x=f32((vertex<<1u)&2u);let y=f32(vertex&2u);let position=vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);return Out(position,7u); }
