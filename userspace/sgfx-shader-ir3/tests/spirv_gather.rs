#![cfg(feature = "frontend")]
use sgfx_shader_ir3::{Source, validate};
fn spirv(source: &str) -> Vec<u32> {
    let module = naga::front::wgsl::parse_str(source).unwrap();
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)
    .unwrap();
    naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options {
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        },
        Some(&naga::back::spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Fragment,
            entry_point: "main".into(),
        }),
    )
    .unwrap()
}
fn cases() -> Vec<String> {
    [
        ("texture_depth_2d","textureGatherCompare(t,s,p.xy/16.0,0.5)"),
        ("texture_depth_2d","textureGatherCompare(t,s,p.xy/16.0,0.5,vec2<i32>(1,-1))"),
        ("texture_depth_2d_array","textureGatherCompare(t,s,p.xy/16.0,1,0.5,vec2<i32>(-1,1))"),
        ("texture_depth_cube","textureGatherCompare(t,s,vec3<f32>(p.xy/8.0-1.0,1.0),0.5)"),
    ].into_iter().map(|(ty,expr)|format!("@group(0) @binding(0) var t:{ty};@group(0) @binding(1) var s:sampler_comparison;@fragment fn main(@builtin(position)p:vec4<f32>)->@location(0) vec4<f32>{{return {expr};}}")).chain(std::iter::once("@group(0) @binding(0) var t:texture_depth_2d;@group(0) @binding(1) var s:sampler_comparison;fn gather(t:texture_depth_2d,s:sampler_comparison,p:vec2<f32>)->vec4<f32>{return textureGatherCompare(t,s,p,0.5);}@fragment fn main(@builtin(position)p:vec4<f32>)->@location(0) vec4<f32>{return gather(t,s,p.xy/16.0);}".into())).collect()
}
#[test]
fn imported_depth_gather_preserves_array_cube_offset_and_helper_semantics() {
    for source in cases() {
        let words = spirv(&source);
        validate(Source::SpirV(&words)).unwrap_or_else(|e| {
            let module = naga::front::spv::Frontend::new(
                words.iter().copied(),
                &naga::front::spv::Options {
                    adjust_coordinate_space: false,
                    ..Default::default()
                },
            )
            .parse()
            .unwrap();
            let detail = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::PUSH_CONSTANT,
            )
            .validate(&module);
            panic!("{e}\n{detail:?}\n{source}")
        });
        #[cfg(feature = "mesa")]
        {
            use sgfx_shader_ir3::{Stage, compile};
            let wgsl = compile(Source::Wgsl(&source), Stage::Fragment, "main").unwrap();
            let spv = compile(Source::SpirV(&words), Stage::Fragment, "main").unwrap();
            assert_eq!(
                wgsl.sampled_bindings().unwrap(),
                spv.sampled_bindings().unwrap()
            );
            assert_eq!(wgsl.allocation, spv.allocation, "{source}");
        }
    }
}
fn replace_gather(words: &[u32], replacement: impl FnOnce(&[u32]) -> Vec<u32>) -> Vec<u32> {
    // SPIR-V 1.x OpImageDrefGather = 97 (pinned spirv crate grammar).
    let mut start = 5;
    while start < words.len() {
        let count = (words[start] >> 16) as usize;
        if words[start] & 0xffff == 97 {
            let mut instruction = replacement(&words[start + 1..start + count]);
            instruction.insert(0, ((instruction.len() as u32 + 1) << 16) | 97);
            let mut result = words[..start].to_vec();
            result.extend(instruction);
            result.extend_from_slice(&words[start + count..]);
            return result;
        }
        start += count;
    }
    panic!("no actual ImageDrefGather emitted");
}
#[test]
fn malformed_or_nonportable_gather_operands_are_errors_not_silently_ignored() {
    let words = spirv(&cases()[0]);
    for trailing in [
        vec![0x10, 1],
        vec![8],
        vec![0, 1],
        vec![0x10000, 1],
        vec![2, 1],
    ] {
        let malformed = replace_gather(&words, |base| {
            let mut inst = base[..5].to_vec();
            inst.extend(trailing);
            inst
        });
        let error = validate(Source::SpirV(&malformed)).unwrap_err().to_string();
        assert!(
            error.contains("invalid operand") || error.contains("invalid word count"),
            "{error}"
        );
    }
    let short = replace_gather(&words, |base| base[..4].to_vec());
    assert!(validate(Source::SpirV(&short)).is_err());
}

#[test]
fn ordinary_gather_preserves_components_numeric_types_depth_and_view_shape() {
    for component in 0..4 {
        for scalar in ["f32", "i32", "u32"] {
            check_ordinary(
                &format!("texture_2d<{scalar}>"),
                &format!("textureGather({component},t,s,p.xy/16.0,vec2<i32>(1,-1))"),
            );
        }
    }
    check_ordinary("texture_2d_array<f32>", "textureGather(2,t,s,p.xy/16.0,1)");
    check_ordinary(
        "texture_cube<f32>",
        "textureGather(3,t,s,vec3<f32>(p.xy/8.0-1.0,1.0))",
    );
    check_ordinary("texture_depth_2d", "textureGather(t,s,p.xy/16.0)");
    check_ordinary(
        "texture_depth_2d_array",
        "textureGather(t,s,p.xy/16.0,1,vec2<i32>(1,-1))",
    );
}
fn check_ordinary(ty: &str, expression: &str) {
    let source = format!(
        "@group(0) @binding(0) var t:{ty};@group(0) @binding(1) var s:sampler;@fragment fn main(@builtin(position)p:vec4<f32>)->@location(0) vec4<f32>{{return vec4<f32>({expression});}}"
    );
    let words = spirv(&source);
    validate(Source::SpirV(&words)).unwrap_or_else(|e| panic!("{e}\n{source}"));
    #[cfg(feature = "mesa")]
    {
        use sgfx_shader_ir3::{Stage, compile};
        let wgsl = compile(Source::Wgsl(&source), Stage::Fragment, "main").unwrap();
        let spv = compile(Source::SpirV(&words), Stage::Fragment, "main").unwrap();
        assert_eq!(wgsl.allocation, spv.allocation, "{source}");
        assert_eq!(
            wgsl.sampled_bindings().unwrap(),
            spv.sampled_bindings().unwrap()
        );
    }
}

#[test]
fn ordinary_gather_rejects_out_of_range_or_nonconstant_components() {
    let source = "@group(0) @binding(0) var t:texture_2d<f32>;@group(0) @binding(1) var s:sampler;@fragment fn main()->@location(0) vec4<f32>{return textureGather(3,t,s,vec2<f32>(0.5));}";
    let words = spirv(source);
    let mut start = 5;
    let mut component = 0;
    let mut coord = 0;
    let mut gather = 0;
    while start < words.len() {
        let count = (words[start] >> 16) as usize;
        if words[start] & 0xffff == 96 {
            component = words[start + 5];
            coord = words[start + 4];
            gather = start;
            break;
        }
        start += count;
    }
    assert_ne!(component, 0);
    let mut invalid = words.clone();
    invalid[gather + 5] = coord;
    assert!(validate(Source::SpirV(&invalid)).is_err());
    for bad in [4u32, u32::MAX] {
        let mut invalid = words.clone();
        let mut start = 5;
        let mut mutated = false;
        while start < invalid.len() {
            let count = (invalid[start] >> 16) as usize;
            if invalid[start] & 0xffff == 43 && invalid[start + 2] == component {
                invalid[start + 3] = bad;
                mutated = true;
                break;
            }
            start += count;
        }
        assert!(mutated);
        assert!(
            validate(Source::SpirV(&invalid))
                .unwrap_err()
                .to_string()
                .contains("invalid operand")
        );
    }
}
