// Cross-invocation reduction; 128 invocations exceed an A618 64-lane wave.
@group(0) @binding(0) var<storage, read> input: array<u32>;
@group(0) @binding(1) var<storage, read_write> output: array<u32>;
var<workgroup> scratch: array<u32, 128>;
var<workgroup> total: atomic<u32>;

@compute @workgroup_size(128)
fn reduction(@builtin(local_invocation_index) lane: u32,
             @builtin(workgroup_id) group: vec3<u32>) {
    scratch[lane] = input[group.x * 128u + lane];
    workgroupBarrier();
    for (var stride = 64u; stride > 0u; stride >>= 1u) {
        if (lane < stride) {
            scratch[lane] += scratch[lane + stride];
        }
        workgroupBarrier();
    }
    if (lane == 0u) { output[group.x] = scratch[0]; }
}

// Relies on WGSL's zero initialization, shared atomic RMW and group rendezvous.
@compute @workgroup_size(128)
fn atomic_sum(@builtin(local_invocation_index) lane: u32,
              @builtin(workgroup_id) group: vec3<u32>) {
    atomicAdd(&total, input[group.x * 128u + lane]);
    workgroupBarrier();
    if (lane == 0u) { output[group.x] = atomicLoad(&total); }
}

// No shared storage or barrier should leak from other entries in this module.
@compute @workgroup_size(1)
fn plain() { output[0] = input[0]; }

var<workgroup> tile: array<vec4<f32>, 64>;
@compute @workgroup_size(8, 8)
fn transpose(@builtin(local_invocation_id) id: vec3<u32>) {
    let index = id.y * 8u + id.x;
    tile[index] = vec4<f32>(f32(input[index]), f32(id.x), f32(id.y), 1.0);
    workgroupBarrier();
    let value = tile[id.x * 8u + id.y];
    output[index] = u32(value.x + value.y * value.z);
}

@compute @workgroup_size(128)
fn atomic_cas(@builtin(local_invocation_index) lane: u32) {
    let result = atomicCompareExchangeWeak(&total, 0u, lane + 1u);
    output[lane] = result.old_value + select(0u, 128u, result.exchanged);
}

@compute @workgroup_size(128)
fn uniform_load(@builtin(local_invocation_index) lane: u32) {
    if (lane == 0u) { scratch[0] = input[0]; }
    output[lane] = workgroupUniformLoad(&scratch[0]);
}

// Crosses the old codegen's implicit 2 KiB shared allocation ceiling.
var<workgroup> wide: array<u32, 1024>;
@compute @workgroup_size(128)
fn wide_shared(@builtin(local_invocation_index) lane: u32,
               @builtin(workgroup_id) group: vec3<u32>) {
    for (var chunk = 0u; chunk < 8u; chunk++) {
        let index = chunk * 128u + lane;
        wide[index] = input[group.x * 1024u + index];
    }
    workgroupBarrier();
    var sum = 0u;
    for (var chunk = 0u; chunk < 8u; chunk++) {
        sum += wide[chunk * 128u + ((lane + 1u) % 128u)];
    }
    output[group.x * 128u + lane] = sum;
}
