@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>;
@group(0) @binding(1) var<storage,read> seed:array<u32>;
fn color(p:vec2<u32>)->vec4<f32>{return vec4<f32>(f32(13u+3u*p.x+seed[0]),f32(29u+5u*p.y),f32(51u+7u*(p.x^p.y)),255.0)/255.0;}
@vertex fn main(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32>{if i==0u {for(var y=0u;y<8u;y++){for(var x=0u;x<8u;x++){let p=vec2<u32>(x,y);textureStore(image,vec2<i32>(p),color(p));}}} let x=f32((i<<1u)&2u);let y=f32(i&2u);return vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.5,1.0);}
