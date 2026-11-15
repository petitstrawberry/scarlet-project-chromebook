struct Push { factor: u32, }
var<push_constant> push: Push;
@group(3) @binding(9) var<storage,read_write> output:array<u32>;
@compute @workgroup_size(128) fn cs(@builtin(global_invocation_id) id:vec3<u32>) {
    output[id.x]=id.x*push.factor;
}
@vertex fn vs() -> @builtin(position) vec4<f32> { return vec4(0.0,0.0,0.0,1.0); }
