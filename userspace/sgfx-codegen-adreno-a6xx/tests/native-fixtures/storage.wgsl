@group(0) @binding(0) var<storage,read> input:array<u32>;
@group(0) @binding(1) var<storage,read_write> output:array<u32>;
@group(0) @binding(2) var<storage,read_write> checksum:array<u32>;
@compute @workgroup_size(128) fn cs(@builtin(global_invocation_id) id:vec3<u32>) {
    let value=input[id.x];
    output[id.x]=value*value;
    checksum[id.x]=value+7u;
}
