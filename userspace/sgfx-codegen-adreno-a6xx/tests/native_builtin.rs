#![cfg(feature = "programmable")]
mod native_support;
use adreno_a6xx_pm4::{Header, Packets};
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use std::sync::Arc;

fn values(a: &RelocatablePm4, reg: u32) -> Vec<u32> {
    Packets::new(&a.words)
        .filter_map(|p| {
            let p = p.unwrap();
            match p.header {
                Header::Type4 { register, count }
                    if (register..register + u32::from(count)).contains(&reg) =>
                {
                    Some(p.payload[(reg - register) as usize])
                }
                _ => None,
            }
        })
        .collect()
}

#[test]
fn primitive_id_uses_real_metadata_routing_without_dummy_vertex_export_and_resets() {
    let case = native_primitive_id_case(true);
    let artifact = case.compile().unwrap();
    assert_eq!(values(&artifact, 0x9806), [1, 0]); // PC_PS_CNTL
    assert_eq!(values(&artifact, 0xa006), [1, 0]); // VFD_CNTL_6
    assert_eq!((values(&artifact, 0x9304)[0] >> 8) & 255, 0);
    assert_eq!((values(&artifact, 0x9304)[1] >> 8) & 255, 255);
    assert_eq!(values(&artifact, 0xa802)[0], 1); // position only, no fake VS primid
    assert_eq!(values(&artifact, 0xa813)[0], 1); // position follows hardware ID
    assert_eq!(values(&artifact, 0x9200)[0] & 3, 1); // flat interpolation
}

#[test]
fn incomplete_primitive_metadata_is_rejected_before_emission() {
    for mutation in 0..6 {
        let mut case = native_primitive_id_case(false);
        let Operation::SetProgrammablePipeline(p) = &mut case.operations[1] else {
            panic!()
        };
        let m = &mut Arc::make_mut(&mut Arc::make_mut(p).fragment.artifact).metadata;
        let input = m
            .inputs
            .iter_mut()
            .find(|i| !i.sysval && i.slot == 21)
            .unwrap();
        match mutation {
            0 => m.reads_primid = false,
            1 => input.sysval = true,
            2 => input.flat = false,
            3 => input.compmask = 3,
            4 => input.half = true,
            5 => input.inloc = m.total_in,
            _ => unreachable!(),
        }
        assert!(case.compile().is_err(), "mutation={mutation}");
    }
}

#[test]
fn single_sample_real_shaders_emit_pixel_rate_state_without_new_hardware_modes() {
    for derivatives in [false, true] {
        let sample = native_single_sample_case(false, derivatives)
            .compile()
            .unwrap();
        let center = native_single_sample_case(true, derivatives)
            .compile()
            .unwrap();
        assert_eq!(sample.words, center.words);
        assert_eq!(values(&sample, 0x9806), [0]);
        assert_eq!(values(&sample, 0xa006), [0]);
        assert_eq!(values(&sample, 0x8810), [0]); // RB pixel rate
        assert_eq!(values(&sample, 0x8109), [0]); // LRZ pixel rate
        let quad_flags = (1 << 23) | (1 << 26);
        assert_eq!(values(&sample, 0xa980)[0] & quad_flags,
            if derivatives { quad_flags } else { 0 });
    }
}

#[test]
fn fixed_native_lowering_keeps_feedback_and_vertex_bounds_fail_closed() {
    let mut case=native_support::fixed_native_case(false);
    for operation in &mut case.operations {
        if let sgfx_codegen_adreno_a6xx::Operation::SetTexture(id)=operation {*id=native_support::TARGET;}
    }
    assert!(case.compile().is_err());
    let mut case=native_support::fixed_native_case(false);
    case.resources.iter_mut().find(|r|r.id==native_support::VERTICES).unwrap().size=299;
    assert!(case.compile().is_err());
}
