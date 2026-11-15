struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(perspective,sample) perspective: f32,
    @location(1) @interpolate(linear,sample) linear: f32,
}
@vertex fn vs(@builtin(vertex_index) vertex: u32) -> VertexOut {
    var xy = array<vec2<f32>,3>(vec2<f32>(-1.0,-1.0),vec2<f32>(3.0,-1.0),vec2<f32>(-1.0,3.0));
    var weights = array<f32,3>(1.0,2.0,4.0);
    var values = array<f32,3>(0.0,1.0,0.0);
    let w = weights[vertex];
    return VertexOut(vec4<f32>(xy[vertex]*w,0.0,w),values[vertex],values[vertex]);
}
@fragment fn fs(@builtin(sample_index) sample: u32,
    @location(0) @interpolate(perspective,sample) perspective: f32,
    @location(1) @interpolate(linear,sample) linear: f32) -> @location(0) vec4<f32> {
    return vec4<f32>(f32(17u+sample*97u)/255.0,perspective,linear,1.0);
}
@fragment fn derivatives(
    @location(0) @interpolate(perspective,sample) perspective: f32,
    @location(1) @interpolate(linear,sample) linear: f32) -> @location(0) vec4<f32> {
    return vec4<f32>(dpdxFine(perspective)*8.0,dpdxFine(linear)*16.0,-dpdyFine(perspective)*16.0,1.0);
}
