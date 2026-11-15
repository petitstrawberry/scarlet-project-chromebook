//! Exact shader-conversion/import profile matching for generic native NV12.
use super::*;
use crate::model::ResourceMeta;
pub fn nv12_profile(slot: u32, c: ir::YcbcrConversion) -> sgfx_shader_ir3::Nv12Profile {
    sgfx_shader_ir3::Nv12Profile {
        texture_slot: slot,
        matrix: match c.matrix {
            ir::YcbcrMatrix::Bt601 => 1,
            ir::YcbcrMatrix::Bt709 => 2,
        },
        range: match c.range {
            ir::YcbcrRange::Limited => 1,
            ir::YcbcrRange::Full => 2,
        },
        chroma_x: match c.chroma_x {
            ir::ChromaLocation::Cosited => 1,
            ir::ChromaLocation::Midpoint => 2,
        },
        chroma_y: match c.chroma_y {
            ir::ChromaLocation::Cosited => 1,
            ir::ChromaLocation::Midpoint => 2,
        },
    }
}
pub(super) fn validate_shader(
    shader: &sgfx_shader_ir3::CompiledShader,
) -> Result<(), CompileError> {
    let Some(profiles) = shader
        .metadata
        .nv12_profiles()
        .map_err(|_| CompileError::InvalidResource)?
    else {
        return Ok(());
    };
    let typed = shader
        .sampled_bindings()
        .map_err(|_| CompileError::InvalidResource)?
        .ok_or(CompileError::InvalidResource)?;
    for p in profiles {
        if !shader.bindings.iter().any(|b| {
            b.kind == sgfx_shader_ir3::ResourceKind::Texture
                && !b.writable
                && b.slot == p.profile.texture_slot
        }) || !typed.iter().any(|b| {
            b.slot == p.profile.texture_slot
                && b.ty
                    == sgfx_shader_ir3::SampledBindingType::Texture {
                        dimension: sgfx_shader_ir3::TextureDimension::D2,
                        arrayed: false,
                        sample_type: sgfx_shader_ir3::TextureSampleType::Float,
                        multisampled: false,
                    }
        }) {
            return Err(CompileError::InvalidResource);
        }
    }
    Ok(())
}
pub(super) fn validate_bound(
    profile: Option<&sgfx_shader_ir3::Nv12TextureProfile>,
    resource: &ResourceMeta,
    slot: u32,
) -> Result<(), CompileError> {
    let ResourceKind::Image(image) = &resource.kind else {
        return Err(CompileError::InvalidResource);
    };
    if image.format != ir::TextureFormat::Nv12 || image.usage != ir::TextureUsage::SAMPLED {
        return Err(CompileError::InvalidResource);
    }
    let expected = nv12_profile(slot, image.conversion.ok_or(CompileError::InvalidResource)?);
    if profile.map(|p| p.profile) != Some(expected) {
        return Err(unsupported(
            "NV12 bound image does not match compiled conversion profile",
        ));
    }
    Ok(())
}

/// Physical sampler state that affects logical NV12 reconstruction.
pub fn nv12_sampler_profile(slot:u32,sampler:ir::SamplerDesc)->sgfx_shader_ir3::Nv12SamplerProfile {
    let address=|v|match v {ir::AddressMode::Repeat=>0,ir::AddressMode::ClampToEdge=>1,
        ir::AddressMode::MirrorRepeat=>2,ir::AddressMode::ClampToBorder=>3};
    sgfx_shader_ir3::Nv12SamplerProfile {slot,address_u:address(sampler.address_u()),
        address_v:address(sampler.address_v()),min_filter:u32::from(sampler.min_filter()==ir::FilterMode::Linear),
        mag_filter:u32::from(sampler.mag_filter()==ir::FilterMode::Linear)}
}
