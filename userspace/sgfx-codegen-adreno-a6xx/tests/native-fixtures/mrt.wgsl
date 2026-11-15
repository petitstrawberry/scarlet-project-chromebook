struct Outputs {
    @location(0) hdr: vec4<f32>,
    @location(1) ldr: vec4<f32>,
}
@fragment fn fs(@builtin(position) p:vec4<f32>)->Outputs {
    var out:Outputs;
    out.hdr=vec4<f32>(p.x*0.25-2.0, 2.5, 0.00001, 0.5);
    out.ldr=vec4<f32>(0.1, p.y/16.0, 0.8, 1.0);
    return out;
}
