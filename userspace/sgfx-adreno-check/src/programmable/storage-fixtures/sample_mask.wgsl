struct Output {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
    @builtin(sample_mask) mask: u32,
};
@fragment fn main(@builtin(position) p: vec4<f32>) -> Output {
    // Only bit zero covers a sample on the public single-sample target.
    let mask = select(2u, 1u, ((u32(p.x) + u32(p.y)) & 1u) == 0u);
    return Output(vec4<f32>(0.2, 0.6, 0.4, 1.0), 0.25, mask);
}
