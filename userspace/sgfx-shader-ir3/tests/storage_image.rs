#![cfg(feature = "mesa")]
use sgfx_shader_ir3::*;
const SOURCE: &str = "@group(0) @binding(3) var output:texture_storage_2d<rgba8unorm,write>; @compute @workgroup_size(4,4) fn main(@builtin(global_invocation_id) p:vec3<u32>){let size=textureDimensions(output); if all(p.xy<size){textureStore(output,vec2<i32>(p.xy),vec4<f32>(0.2,0.4,0.6,1.0));}}";
fn map() -> BindingMap {
    BindingMap::new(vec![BindingAssignment {
        group: 0,
        binding: 3,
        kind: ResourceKind::Texture,
        slot: 5,
    }])
    .unwrap()
}
#[test]
fn storage_images_are_typed_write_only_with_stable_sparse_ibo_slots() {
    let a = compile_with_bindings(Source::Wgsl(SOURCE), Stage::Compute, "main", &map()).unwrap();
    assert_eq!(a.metadata.num_ssbos, 16);
    assert_eq!(a.metadata.num_uavs, 22);
    assert_eq!(a.metadata.num_samp, 0);
    assert!(a.bindings[0].writable);
    assert_eq!(
        a.sampled_bindings().unwrap().unwrap()[0].ty,
        SampledBindingType::StorageTextureRgba8WriteOnly
    );
    // Normalized SPIR-V retains a real image write, not a buffer/CPU fallback.
    let mut found = false;
    let mut i = 5;
    while i < a.spirv.len() {
        let w = a.spirv[i];
        found |= (w & 65535) == 99;
        i += (w >> 16) as usize;
    }
    assert!(found);
    let plain = compile(Source::Wgsl(SOURCE), Stage::Compute, "main").unwrap();
    let round = compile(Source::SpirV(&plain.spirv), Stage::Compute, "main").unwrap();
    assert_eq!(plain.allocation, round.allocation);
    let wrong = BindingMap::new(vec![BindingAssignment {
        group: 0,
        binding: 3,
        kind: ResourceKind::StorageBuffer,
        slot: 5,
    }])
    .unwrap();
    assert!(compile_with_bindings(Source::Wgsl(SOURCE), Stage::Compute, "main", &wrong).is_err());
    for source in [
        SOURCE.replace("rgba8unorm", "rgba16float"),
        SOURCE.replace("rgba8unorm,write", "rgba8unorm,read_write"),
    ] {
        assert!(validate(Source::Wgsl(&source)).is_err());
    }
}
#[test]
fn source_limits_match_public_four_mib_contract() {
    let suffix = "\n@compute @workgroup_size(1) fn main(){}";
    let source = "//".to_owned() + &"x".repeat(4 * 1024 * 1024 - 2 - suffix.len()) + suffix;
    assert_eq!(source.len(), 4 * 1024 * 1024);
    compile(Source::Wgsl(&source), Stage::Compute, "main").unwrap();
    assert!(
        validate(Source::Wgsl(&(source + " ")))
            .unwrap_err()
            .0
            .contains("4 MiB")
    );
    let words = vec![0; 1024 * 1024 + 1];
    assert!(validate(Source::SpirV(&words)).is_err());
}
