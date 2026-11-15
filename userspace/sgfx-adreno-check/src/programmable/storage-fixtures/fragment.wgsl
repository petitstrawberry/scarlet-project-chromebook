@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>;
@group(0) @binding(1) var<storage,read> seed:array<u32>;
fn color(p:vec2<u32>)->vec4<f32>{return vec4<f32>(f32(13u+3u*p.x+seed[0]),f32(29u+5u*p.y),f32(51u+7u*(p.x^p.y)),255.0)/255.0;}
@fragment fn main(@builtin(position) p:vec4<f32>){textureStore(image,vec2<i32>(p.xy),color(vec2<u32>(p.xy)));}
