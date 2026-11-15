@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>;
@group(0) @binding(1) var<storage,read> seed:array<u32>;
fn color(p:vec2<u32>)->vec4<f32>{return vec4<f32>(f32(13u+3u*p.x+seed[0]),f32(29u+5u*p.y),f32(51u+7u*(p.x^p.y)),255.0)/255.0;}
@compute @workgroup_size(4,4) fn main(@builtin(global_invocation_id) p:vec3<u32>){if all(p.xy<textureDimensions(image)){textureStore(image,vec2<i32>(p.xy),color(p.xy));}}
