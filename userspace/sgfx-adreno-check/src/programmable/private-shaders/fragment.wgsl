@group(0) @binding(0) var<uniform> params:vec4<u32>;
fn local_value(id:u32)->u32 {
 var a:array<u32,256>;
 for(var i=0u;i<256u;i++){ a[i]=(i*17u+id*13u+params.x)^params.y; }
 let index=(params.z+id*7u)%256u;
 a[index]=a[(index+1u)%256u]^0x5au;
 return a[(index+params.w)%256u];
}
fn encode(v:u32)->vec4<f32>{return vec4<f32>(f32(v&255u),f32((v>>8u)&255u),f32((v>>16u)&255u),255.0)/255.0;}
@fragment fn main(@builtin(position) pos:vec4<f32>,@location(0) @interpolate(flat) value:u32)->@location(0) vec4<f32>{
 let id=u32(pos.x)+8u*u32(pos.y); return encode(local_value(id)^value); }
