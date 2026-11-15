struct Params { scale: u32, add: u32, count: u32, pad: u32, }
@group(0) @binding(0) var<storage, read_write> output: array<u32>;
@group(0) @binding(1) var<uniform> params: Params;
@compute @workgroup_size(64)
fn cs(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x < params.count) {
        output[id.x] = (id.x * id.x) * params.scale + params.add;
    }
}
