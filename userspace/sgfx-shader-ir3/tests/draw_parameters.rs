#![cfg(feature = "frontend")]
#[path = "support/draw_parameters.rs"]
mod fixture;
use sgfx_shader_ir3::{Source, validate};
#[test]
fn core_and_extension_draw_parameter_sources_are_admitted() {
    for extension in [false, true] {
        let words = fixture::spirv(extension);
        validate(Source::SpirV(&words)).unwrap();
    }
    validate(Source::Wgsl(fixture::FRAGMENT)).unwrap();
}
#[cfg(feature = "mesa")]
#[test]
fn draw_parameters_compile_to_real_driver_constant_allocation() {
    use sgfx_shader_ir3::{Stage, compile};
    let core = compile(Source::SpirV(&fixture::spirv(false)), Stage::Vertex, "main").unwrap();
    let khr = compile(Source::SpirV(&fixture::spirv(true)), Stage::Vertex, "main").unwrap();
    assert_eq!(core.allocation, khr.allocation);
    assert!(core.bindings.is_empty());
    assert!(core.metadata.need_driver_params);
    assert!(core.metadata.num_driver_params >= 3);
    assert!(
        core.metadata
            .const_allocations
            .iter()
            .any(|a| a.kind == 3 && a.size_vec4 > 0)
    );
    assert!(core.metadata.outputs.len() >= 2);
    // The default normalized output is SPIR-V 1.0 and must retain both the
    // capability and KHR extension, not merely be tolerated by Mesa.
    validate(Source::SpirV(&core.spirv)).unwrap();
    let bytes = core
        .spirv
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect::<Vec<_>>();
    assert!(
        bytes
            .windows(b"SPV_KHR_shader_draw_parameters".len())
            .any(|w| w == b"SPV_KHR_shader_draw_parameters")
    );
}
