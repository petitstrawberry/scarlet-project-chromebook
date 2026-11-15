#![cfg(feature = "frontend")]
use sgfx_shader_ir3::{Source, validate};
const SOURCE: &str = include_str!("shaders/single_sample.wgsl");

#[test]
fn sample_syntax_is_valid_only_in_fragment_interface_and_msaa_resources_stay_closed() {
    validate(Source::Wgsl(SOURCE)).unwrap();
    for source in [
        "@vertex fn main(@builtin(sample_index) p:u32)->@builtin(position) vec4<f32>{return vec4<f32>(f32(p));}",
        "@fragment fn main(@builtin(sample_index) p:f32)->@location(0) vec4<f32>{return vec4<f32>(p);}",
        "@vertex fn main(@builtin(view_index) p:u32)->@builtin(position) vec4<f32>{return vec4<f32>(f32(p));}",
        "@group(0) @binding(0) var t:texture_multisampled_2d<f32>; @fragment fn main()->@location(0) vec4<f32>{return textureLoad(t,vec2<i32>(0),0);}",
        "@group(0) @binding(0) var t:texture_depth_multisampled_2d; @fragment fn main()->@location(0) vec4<f32>{return vec4<f32>(textureLoad(t,vec2<i32>(0),0));}",
    ] {
        assert!(validate(Source::Wgsl(source)).is_err(), "{source}");
    }
}

#[cfg(feature = "mesa")]
#[test]
fn real_mesa_sample_inputs_equal_center_and_sample_id_equals_zero_for_wgsl_and_spirv() {
    use sgfx_shader_ir3::{Stage, compile};
    let center = SOURCE.replace(",sample", ",center");
    for entry in ["fs", "derivatives"] {
        let shader = compile(Source::Wgsl(SOURCE), Stage::Fragment, entry).unwrap();
        let reference = compile(Source::Wgsl(&center), Stage::Fragment, entry).unwrap();
        assert_eq!(
            shader.allocation, reference.allocation,
            "{entry}: sample vs center"
        );
        assert_eq!(shader.metadata.inputs, reference.metadata.inputs);
        assert_eq!(shader.metadata.rasterization_samples, Some(1));
        assert!(!shader.metadata.sample_shading);
        assert!(
            !shader
                .metadata
                .inputs
                .iter()
                .any(|i| i.sysval && [29, 60, 61, 64, 65].contains(&i.slot))
        );
        let centroid = SOURCE.replace(",sample", ",centroid");
        let centroid = compile(Source::Wgsl(&centroid), Stage::Fragment, entry).unwrap();
        assert_eq!(
            shader.allocation, centroid.allocation,
            "{entry}: centroid vs center"
        );
        let roundtrip = compile(Source::SpirV(&shader.spirv), Stage::Fragment, entry).unwrap();
        assert_eq!(shader.allocation, roundtrip.allocation, "{entry}: SPIR-V");
        assert_eq!(shader.metadata.inputs, roundtrip.metadata.inputs);
        assert_eq!(roundtrip.metadata.rasterization_samples, Some(1));
    }
    let with_id = "@fragment fn fs(@builtin(sample_index) sample:u32)->@location(0) vec4<f32>{return vec4<f32>(f32(17u+sample*97u)/255.0,0.25,0.5,1.0);}";
    let zero = with_id.replace("17u+sample*97u", "17u");
    let shader = compile(Source::Wgsl(with_id), Stage::Fragment, "fs").unwrap();
    let reference = compile(Source::Wgsl(&zero), Stage::Fragment, "fs").unwrap();
    assert_eq!(shader.allocation, reference.allocation);
    assert!(!shader.metadata.sample_shading);
    let roundtrip = compile(Source::SpirV(&shader.spirv), Stage::Fragment, "fs").unwrap();
    assert_eq!(shader.allocation, roundtrip.allocation);
}

#[test]
fn multisampled_color_and_depth_spirv_remain_rejected_before_mesa() {
    // Independently create valid SPIR-V, so rejection is tested on the public
    // binary-source path too, not just WGSL parsing or a malformed fixture.
    for ty in [
        "texture_multisampled_2d<f32>",
        "texture_depth_multisampled_2d",
    ] {
        let source = format!(
            "@group(0) @binding(0) var t:{ty}; @fragment fn fs()->@location(0) vec4<f32>{{return vec4<f32>(textureLoad(t,vec2<i32>(0),0));}}"
        );
        let module = naga::front::wgsl::parse_str(&source).unwrap();
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
        let options = naga::back::spv::Options {
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        };
        let pipeline = naga::back::spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Fragment,
            entry_point: "fs".into(),
        };
        let words = naga::back::spv::write_vec(&module, &info, &options, Some(&pipeline)).unwrap();
        let error = validate(Source::SpirV(&words)).unwrap_err();
        assert!(
            error.to_string().contains("multisampled image resources"),
            "{ty}: {error}"
        );
    }
}
