@group(0) @binding(0) var<uniform> params:vec4<u32>;
fn local_value(id:u32)->u32 {
 var a:array<u32,256>;
 for(var i=0u;i<256u;i++){ a[i]=(i*17u+id*13u+params.x)^params.y; }
 let index=(params.z+id*7u)%256u;
 a[index]=a[(index+1u)%256u]^0x5au;
 return a[(index+params.w)%256u];
}
@group(0) @binding(1) var<storage,read_write> output:array<u32>;
@compute @workgroup_size(128) fn main(@builtin(global_invocation_id) gid:vec3<u32>){output[gid.x]=local_value(gid.x);}
