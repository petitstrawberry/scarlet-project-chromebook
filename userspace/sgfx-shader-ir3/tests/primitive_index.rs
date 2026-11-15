#![cfg(feature = "frontend")]
use sgfx_shader_ir3::{Source, validate};
const SOURCE: &str = include_str!("shaders/primitive_index.wgsl");

#[test]
fn primitive_index_is_fragment_input_only_and_unrelated_gates_remain_closed() {
    validate(Source::Wgsl(SOURCE)).unwrap();
    for source in [
        "@vertex fn main(@builtin(primitive_index) p:u32)->@builtin(position) vec4<f32>{return vec4<f32>(f32(p));}",
        "@fragment fn main(@builtin(primitive_index) p:f32)->@location(0) vec4<f32>{return vec4<f32>(p);}",
        "@vertex fn main(@builtin(view_index) view:u32)->@builtin(position) vec4<f32>{return vec4<f32>(f32(view));}",
    ] {
        assert!(validate(Source::Wgsl(source)).is_err(), "{source}");
    }
}

#[cfg(feature = "mesa")]
#[test]
fn real_mesa_lowers_primitive_sysval_to_flat_input_and_spirv_roundtrips() {
    use sgfx_shader_ir3::{Stage, compile};
    let shader = compile(Source::Wgsl(SOURCE), Stage::Fragment, "fs").unwrap();
    let m = &shader.metadata;
    assert!(m.reads_primid);
    let input = m.inputs.iter().find(|i| !i.sysval && i.slot == 21).unwrap();
    assert!(input.bary && input.flat && !input.half);
    assert_eq!(input.compmask, 1);
    assert!(input.inloc < m.total_in);
    assert!(m.total_in >= 3); // two user varying components plus real primitive ID
    assert!(!m.inputs.iter().any(|i| i.sysval && i.slot == 37));
    let roundtrip = compile(Source::SpirV(&shader.spirv), Stage::Fragment, "fs").unwrap();
    assert_eq!(shader.allocation, roundtrip.allocation);
    assert_eq!(shader.metadata.inputs, roundtrip.metadata.inputs);
    let plain = compile(Source::Wgsl(SOURCE), Stage::Fragment, "plain").unwrap();
    assert!(!plain.metadata.reads_primid);
    assert!(
        !plain
            .metadata
            .inputs
            .iter()
            .any(|i| !i.sysval && i.slot == 21)
    );
}
