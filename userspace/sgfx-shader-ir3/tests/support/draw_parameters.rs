// Reproducible real SPIR-V for the SGFX shader-draw-parameters capability.
// WGSL has no portable names for these three builtins: mutate validated Naga IR
// bindings, then emit a module carrying the required SPIR-V capability/extension.
pub const TEMPLATE: &str = "struct Output { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) params:vec3<u32> };
@vertex fn main(@builtin(vertex_index) vertex:u32,@location(0) base:u32,@location(1) instance:u32,@location(2) draw:u32)->Output {
 let index=vertex%3u;let x=f32((index<<1u)&2u);let y=f32(index&2u);
 return Output(vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0),vec3<u32>(draw,base,instance));
}";
pub const FRAGMENT: &str = "@fragment fn main(@location(0) @interpolate(flat) params:vec3<u32>)->@location(0) vec4<f32>{return vec4<f32>(f32(params.x),f32(bitcast<i32>(params.y)+128),f32(params.z),255.0)/255.0;}";
pub fn spirv(extension: bool) -> Vec<u32> {
    let mut module = naga::front::wgsl::parse_str(TEMPLATE).unwrap();
    for (arg, builtin) in module.entry_points[0]
        .function
        .arguments
        .iter_mut()
        .skip(1)
        .zip([
            naga::BuiltIn::BaseVertex,
            naga::BuiltIn::BaseInstance,
            naga::BuiltIn::DrawID,
        ])
    {
        arg.binding = Some(naga::Binding::BuiltIn(builtin));
    }
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)
    .unwrap();
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options {
            lang_version: if extension { (1, 0) } else { (1, 3) },
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        },
        Some(&naga::back::spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Vertex,
            entry_point: "main".into(),
        }),
    )
    .unwrap();
    // The patched writer itself must emit the required declaration. Do not
    // manufacture it here: normalized output must remain valid as well.
    let mut start = 5;
    let mut capabilities = 0;
    while start < words.len() {
        if words[start] & 0xffff == 17 && words[start + 1] == 4427 {
            capabilities += 1;
        }
        start += (words[start] >> 16) as usize;
    }
    assert_eq!(capabilities, 1);
    words
}
