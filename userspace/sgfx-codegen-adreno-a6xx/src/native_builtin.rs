//! A618 fixed-function fragment builtins, matched to Mesa 3f1b217b.
//!
//! tu_shader.cc lowers FS PrimitiveId to a flat input and supplies PRIMIDLOC,
//! VFD_CNTL_6.PRIMID4PSEN and PC_PS_CNTL.PRIMITIVEIDEN. The primitive assembler
//! writes that VPC location; the VS must not fabricate an ID or supply a reg.
use super::{CompileError, Metadata, Stage, unsupported};

// compiler/shader_enums.h gl_varying_slot, not gl_system_value (37).
pub(super) const PRIMITIVE_ID_SLOT: u32 = 21;
pub(super) const NO_LOCATION: u32 = 0xff;

pub(super) fn validate_sample_profile(m: &Metadata, stage: Stage) -> Result<(), CompileError> {
    if let Some(samples) = m.rasterization_samples {
        if samples != u32::from(stage == Stage::Fragment) {
            return Err(unsupported("native single-sample compiler profile"));
        }
        // Known single-sample NIR must have lowered SampleId and both
        // perspective/linear sample and centroid barycentrics to pixel center.
        if stage == Stage::Fragment
            && m.inputs
                .iter()
                .any(|i| i.sysval && [29, 60, 61, 64, 65].contains(&i.slot))
        {
            return Err(unsupported("unlowered native single-sample input"));
        }
    }
    Ok(())
}

pub(super) fn primitive_location(m: &Metadata, stage: Stage) -> Result<u32, CompileError> {
    // VS input slots are gl_vert_attrib, not gl_varying_slot. In particular,
    // an ordinary vertex attribute may have numeric slot 21. Never reinterpret
    // it as an FS builtin just because those independent enums overlap.
    if stage != Stage::Fragment {
        return if m.reads_primid {
            Err(unsupported("native primitive ID is fragment-only"))
        } else {
            Ok(NO_LOCATION)
        };
    }
    let mut inputs = m
        .inputs
        .iter()
        .filter(|i| !i.sysval && i.slot == PRIMITIVE_ID_SLOT);
    let Some(input) = inputs.next() else {
        return if m.reads_primid {
            Err(unsupported(
                "native primitive ID requires lowered flat input",
            ))
        } else {
            Ok(NO_LOCATION)
        };
    };
    if stage != Stage::Fragment
        || !m.reads_primid
        || inputs.next().is_some()
        || input.compmask != 1
        || !input.flat
        || !input.bary
        || input.half
        || input.inloc >= m.total_in
        || input.inloc >= 128
    {
        return Err(unsupported("native primitive ID input metadata"));
    }
    // The hardware-generated scalar cannot alias another interpolated input.
    if m.inputs.iter().any(|other| {
        !other.sysval
            && other.slot != PRIMITIVE_ID_SLOT
            && other.compmask != 0
            && other.inloc <= input.inloc
            && input.inloc
                < other
                    .inloc
                    .saturating_add(32 - other.compmask.leading_zeros())
    }) {
        return Err(CompileError::InvalidResource);
    }
    Ok(input.inloc)
}

pub(super) fn primitive_enabled(location: u32) -> u32 {
    u32::from(location != NO_LOCATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_single_sample_profile_rejects_residual_sample_inputs_and_other_counts() {
        let legacy: Metadata =
            serde_json::from_str(include_str!("../tests/native-fixtures/fs.json")).unwrap();
        let mut m = legacy.clone();
        m.rasterization_samples = Some(1);
        validate_sample_profile(&m, Stage::Fragment).unwrap();
        for samples in [0, 2, 4, 8] {
            m.rasterization_samples = Some(samples);
            assert!(validate_sample_profile(&m, Stage::Fragment).is_err());
        }
        m.rasterization_samples = Some(1);
        let mut input = m.inputs[0].clone();
        input.sysval = true;
        for slot in [29, 60, 61, 64, 65] {
            input.slot = slot;
            m.inputs.push(input.clone());
            assert!(validate_sample_profile(&m, Stage::Fragment).is_err());
            m.inputs.pop();
        }
        // Coverage/sample-mask sysval is intentionally not replaced by 1.
        input.slot = 32;
        m.inputs.push(input);
        validate_sample_profile(&m, Stage::Fragment).unwrap();
        validate_sample_profile(&legacy, Stage::Fragment).unwrap();
        assert!(validate_sample_profile(&m, Stage::Vertex).is_err());
        m.rasterization_samples = Some(0);
        validate_sample_profile(&m, Stage::Vertex).unwrap();
    }
    #[test]
    fn vertex_attribute_namespace_does_not_alias_fragment_primitive_slot() {
        let mut m: Metadata =
            serde_json::from_str(include_str!("../tests/native-fixtures/vs.json")).unwrap();
        m.inputs.push(sgfx_shader_ir3::metadata::Input {
            slot: PRIMITIVE_ID_SLOT,
            regid: 0,
            compmask: 1,
            inloc: 0,
            sysval: false,
            bary: false,
            half: false,
            flat: false,
            rasterflat: false,
        });
        assert_eq!(primitive_location(&m, Stage::Vertex).unwrap(), NO_LOCATION);
        m.reads_primid = true;
        assert!(primitive_location(&m, Stage::Vertex).is_err());
    }
}
