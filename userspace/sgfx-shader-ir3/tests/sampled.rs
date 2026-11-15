#![cfg(feature = "mesa")]
use sgfx_shader_ir3::{
    BindingAssignment as A, BindingMap, CompiledShader, ResourceKind as K, SampledBindingType as T,
    Source, Stage, TextureDimension as D, TextureSampleType as S, compile_with_bindings, validate,
};
const DEPTH: &str = include_str!("shaders/depth.wgsl");
fn map() -> BindingMap {
    BindingMap::new(vec![
        A {
            group: 1,
            binding: 7,
            kind: K::Texture,
            slot: 3,
        },
        A {
            group: 1,
            binding: 8,
            kind: K::Sampler,
            slot: 5,
        },
        A {
            group: 1,
            binding: 9,
            kind: K::Sampler,
            slot: 6,
        },
        A {
            group: 2,
            binding: 0,
            kind: K::Texture,
            slot: 1,
        },
        A {
            group: 2,
            binding: 1,
            kind: K::Texture,
            slot: 2,
        },
        A {
            group: 3,
            binding: 0,
            kind: K::StorageBuffer,
            slot: 0,
        },
    ])
    .unwrap()
}
fn shader(entry: &str) -> CompiledShader {
    let stage = match entry {
        "vertex" => Stage::Vertex,
        "compute_compare" => Stage::Compute,
        _ => Stage::Fragment,
    };
    compile_with_bindings(Source::Wgsl(DEPTH), stage, entry, &map()).unwrap()
}
fn tex(dimension: D, arrayed: bool, sample_type: S) -> T {
    T::Texture {
        dimension,
        arrayed,
        sample_type,
        multisampled: false,
    }
}
fn instructions(shader: &CompiledShader) -> impl Iterator<Item = u64> + '_ {
    shader.allocation[..shader.metadata.sizedwords as usize * 4]
        .chunks_exact(8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
}
// Pinned Mesa ir3-cat5.xml: category[61:63], SV(shadow)[50],
// S2EN_BINDLESS[51], sampler[21:24], texture[25:31]. Check actual machine code.
fn shadow_pair(shader: &CompiledShader, texture: u32, sampler: u32) -> bool {
    instructions(shader).any(|i| {
        i >> 61 == 5
            && i & (1 << 50) != 0
            && i & (1 << 51) == 0
            && ((i >> 21) & 15) == sampler as u64
            && ((i >> 25) & 127) == texture as u64
    })
}
#[test]
fn depth_load_and_regular_sampler_have_distinct_typed_interfaces() {
    validate(Source::Wgsl(DEPTH)).unwrap();
    let load = shader("load");
    let b = load.sampled_bindings().unwrap().unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].group, b[0].binding, b[0].slot), (1, 7, 3));
    assert_eq!(b[0].ty, tex(D::D2, false, S::Depth));
    let sample = shader("sample");
    let b = sample.sampled_bindings().unwrap().unwrap();
    assert_eq!(b.len(), 2);
    assert_eq!(b[1].ty, T::Sampler { comparison: false });
    assert!(!instructions(&sample).any(|i| i >> 61 == 5 && i & (1 << 50) != 0));
    assert!(
        shader("vertex")
            .sampled_bindings()
            .unwrap()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn depth_compare_variants_emit_real_shadow_ops_with_independent_indices() {
    for (entry, dimension, arrayed, slot) in [
        ("compare", D::D2, false, 3),
        ("compare_level", D::D2, false, 3),
        ("compare_array", D::D2, true, 1),
        ("compare_cube", D::Cube, false, 2),
        ("gather_compare", D::D2, false, 3),
        ("compute_compare", D::D2, false, 3),
    ] {
        let compiled = shader(entry);
        let b = compiled.sampled_bindings().unwrap().unwrap();
        assert_eq!(b.len(), 2, "{entry}");
        assert!(
            b.iter()
                .any(|b| b.ty == tex(dimension, arrayed, S::Depth) && b.slot == slot)
        );
        assert!(
            b.iter()
                .any(|b| b.ty == T::Sampler { comparison: true } && b.slot == 5)
        );
        assert!(
            shadow_pair(&compiled, slot, 5),
            "{entry} must encode real compare with tex{slot}/samp5"
        );
    }
}
#[test]
fn source_spirv_retains_depth_compare_types_and_native_mapping() {
    let module = naga::front::wgsl::parse_str(DEPTH).unwrap();
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)
    .unwrap();
    for entry in [
        "load",
        "sample",
        "compare",
        "compare_level",
        "compare_array",
        "compare_cube",
        "gather_compare",
    ] {
        let spv = naga::back::spv::write_vec(
            &module,
            &info,
            &naga::back::spv::Options {
                flags: naga::back::spv::WriterFlags::empty(),
                ..Default::default()
            },
            Some(&naga::back::spv::PipelineOptions {
                shader_stage: naga::ShaderStage::Fragment,
                entry_point: entry.into(),
            }),
        )
        .unwrap();
        let original = shader(entry);
        let roundtrip =
            compile_with_bindings(Source::SpirV(&spv), Stage::Fragment, entry, &map()).unwrap();
        assert_eq!(
            original.sampled_bindings().unwrap(),
            roundtrip.sampled_bindings().unwrap(),
            "{entry}"
        );
        assert_eq!(original.allocation, roundtrip.allocation, "{entry}");
    }
}
#[test]
fn disjoint_entry_aliases_reflect_the_selected_type_only() {
    let source="@group(0) @binding(0) var color:texture_2d<f32>;
@group(0) @binding(0) var depth:texture_depth_2d;
@fragment fn color_entry()->@location(0) vec4<f32>{return textureLoad(color,vec2<i32>(0),0);}
@fragment fn depth_entry()->@location(0) vec4<f32>{return vec4<f32>(textureLoad(depth,vec2<i32>(0),0));}";
    let map = BindingMap::new(vec![A {
        group: 0,
        binding: 0,
        kind: K::Texture,
        slot: 4,
    }])
    .unwrap();
    for (entry, kind) in [("color_entry", S::Float), ("depth_entry", S::Depth)] {
        let shader =
            compile_with_bindings(Source::Wgsl(source), Stage::Fragment, entry, &map).unwrap();
        let b = shader.sampled_bindings().unwrap().unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].ty, tex(D::D2, false, kind));
        assert_eq!(b[0].slot, 4);
    }
}
#[test]
fn invalid_comparison_types_fail_semantic_validation() {
    for (texture, sampler, call) in [
        (
            "texture_2d<f32>",
            "sampler_comparison",
            "textureSampleCompare(t,s,vec2<f32>(0.5),0.5)",
        ),
        (
            "texture_depth_2d",
            "sampler",
            "textureSampleCompare(t,s,vec2<f32>(0.5),0.5)",
        ),
        (
            "texture_depth_2d",
            "sampler_comparison",
            "textureSample(t,s,vec2<f32>(0.5))",
        ),
    ] {
        let source = format!(
            "@group(0) @binding(0) var t:{texture}; @group(0) @binding(1) var s:{sampler}; @fragment fn main()->@location(0) vec4<f32>{{return vec4<f32>({call});}}"
        );
        assert!(validate(Source::Wgsl(&source)).is_err(), "{source}");
    }
}
#[test]
fn metadata_roundtrip_preserves_types_and_legacy_absence_is_explicit() {
    let mut shader = shader("compare");
    let expected = shader.sampled_bindings().unwrap();
    let json = serde_json::to_vec(&shader.metadata).unwrap();
    shader.metadata = serde_json::from_slice(&json).unwrap();
    assert_eq!(shader.sampled_bindings().unwrap(), expected);
    shader.metadata.extra.remove("sgfx_sampled_interface");
    assert_eq!(shader.sampled_bindings().unwrap(), None);
    shader.metadata.extra.insert(
        "sgfx_sampled_interface".into(),
        serde_json::json!({"version":2,"bindings":[]}),
    );
    assert!(shader.sampled_bindings().is_err());
    shader.metadata.extra.insert(
        "sgfx_sampled_interface".into(),
        serde_json::json!({"version":1}),
    );
    assert!(shader.sampled_bindings().is_err());
}
#[test]
fn scalar_sample_kinds_and_portable_dimensions_remain_distinct() {
    for (ty, call, dimension, arrayed, kind) in [
        (
            "texture_1d<f32>",
            "textureLoad(t,0,0)",
            D::D1,
            false,
            S::Float,
        ),
        (
            "texture_2d<u32>",
            "textureLoad(t,vec2<i32>(0),0)",
            D::D2,
            false,
            S::Uint,
        ),
        (
            "texture_2d<i32>",
            "textureLoad(t,vec2<i32>(0),0)",
            D::D2,
            false,
            S::Sint,
        ),
        (
            "texture_2d_array<f32>",
            "textureLoad(t,vec2<i32>(0),1,0)",
            D::D2,
            true,
            S::Float,
        ),
    ] {
        let source = format!(
            "@group(0) @binding(0) var t:{ty}; @fragment fn main()->@location(0) vec4<f32>{{return vec4<f32>({call});}}"
        );
        let map = BindingMap::new(vec![A {
            group: 0,
            binding: 0,
            kind: K::Texture,
            slot: 0,
        }])
        .unwrap();
        let shader =
            compile_with_bindings(Source::Wgsl(&source), Stage::Fragment, "main", &map).unwrap();
        assert_eq!(
            shader.sampled_bindings().unwrap().unwrap()[0].ty,
            tex(dimension, arrayed, kind)
        );
    }
}

#[test]
fn spirv_depth_gather_recompiles_to_the_same_real_shadow_instructions() {
    let compiled = shader("gather_compare");
    validate(Source::SpirV(&compiled.spirv)).unwrap();
    // Normalized SPIR-V uses native identities, so compile again without an
    // original-source binding map. The original-identity case is tested above.
    let imported = sgfx_shader_ir3::compile(
        Source::SpirV(&compiled.spirv),
        Stage::Fragment,
        "gather_compare",
    )
    .unwrap();
    assert!(instructions(&imported).any(|i| i >> 61 == 5 && i & (1 << 50) != 0));
}
