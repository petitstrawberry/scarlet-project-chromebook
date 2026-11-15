@group(0) @binding(0) var<uniform> params:vec4<u32>;
fn local_value(id:u32)->u32 {
 var a:array<u32,256>;
 for(var i=0u;i<256u;i++){ a[i]=(i*17u+id*13u+params.x)^params.y; }
 let index=(params.z+id*7u)%256u;
 a[index]=a[(index+1u)%256u]^0x5au;
 return a[(index+params.w)%256u];
}
struct Out { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) value:u32 };
@vertex fn main(@builtin(vertex_index) vertex:u32)->Out {let x=f32((vertex<<1u)&2u);let y=f32(vertex&2u);let position=vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);return Out(position,local_value(0u)); }
