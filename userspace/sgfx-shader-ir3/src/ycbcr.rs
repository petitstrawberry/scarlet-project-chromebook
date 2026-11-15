// SPDX-License-Identifier: GPL-2.0-only
//! Exact bound-NV12 specialization identity, independent of Rust SGFX enums.
use crate::{Error, Metadata};
use alloc::{string::ToString, vec::Vec};
use serde::{Deserialize, Serialize};
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct Nv12Profile {
    #[serde(rename = "slot")]
    pub texture_slot: u32,
    pub matrix: u32,
    pub range: u32,
    pub chroma_x: u32,
    pub chroma_y: u32,
}
const _: [(); 20] = [(); core::mem::size_of::<Nv12Profile>()];
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Nv12TextureProfile {
    #[serde(flatten)]
    pub profile: Nv12Profile,
    pub sample_instructions: u32,
    pub query_instructions: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct Nv12SamplerProfile {
    pub slot: u32,
    pub address_u: u32,
    pub address_v: u32,
    pub min_filter: u32,
    pub mag_filter: u32,
}
const _: [(); 20] = [(); core::mem::size_of::<Nv12SamplerProfile>()];
#[derive(Clone, Debug, Deserialize)]
pub struct Nv12Reconstruction {
    pub samplers: Vec<Nv12SamplerProfile>,
    pub nearest_sampler: Option<u32>,
}
#[derive(Deserialize)]
struct ProfileMetadata {
    version: u32,
    textures: Vec<Nv12TextureProfile>,
    #[serde(default)]
    samplers: Vec<Nv12SamplerProfile>,
    #[serde(default)]
    nearest_sampler: Option<u32>,
}
pub(crate) fn validate_profiles(profiles: &[Nv12Profile]) -> Result<(), Error> {
    if profiles.len() > 16
        || profiles
            .windows(2)
            .any(|p| p[0].texture_slot >= p[1].texture_slot)
        || profiles.iter().any(|p| {
            p.texture_slot >= 16
                || [p.matrix, p.range, p.chroma_x, p.chroma_y]
                    .iter()
                    .any(|v| !(1..=2).contains(v))
        })
    {
        return Err(Error("invalid NV12 specialization profile".into()));
    }
    Ok(())
}
impl Metadata {
    pub fn nv12_profiles(&self) -> Result<Option<Vec<Nv12TextureProfile>>, Error> {
        let Some(value) = self.extra.get("sgfx_nv12_profile") else {
            return Ok(None);
        };
        let m: ProfileMetadata =
            serde_json::from_value(value.clone()).map_err(|e| Error(e.to_string()))?;
        if m.version != 1 && m.version != 2 {
            return Err(Error("unsupported NV12 lowering metadata version".into()));
        }
        validate_profiles(&m.textures.iter().map(|t| t.profile).collect::<Vec<_>>())?;
        Ok(Some(m.textures))
    }
}

pub(crate) fn validate_samplers(profiles: &[Nv12SamplerProfile]) -> Result<(), Error> {
    if profiles.len()>16 || profiles.windows(2).any(|p|p[0].slot>=p[1].slot)
        || profiles.iter().any(|p|p.slot>=16 || p.address_u>3 || p.address_v>3 || p.min_filter>1 || p.mag_filter>1) {
        return Err(Error("invalid NV12 sampler specialization profile".into()));
    }
    Ok(())
}
impl Metadata {
    pub fn nv12_reconstruction(&self) -> Result<Option<Nv12Reconstruction>, Error> {
        let Some(value)=self.extra.get("sgfx_nv12_profile") else {return Ok(None)};
        let m:ProfileMetadata=serde_json::from_value(value.clone()).map_err(|e|Error(e.to_string()))?;
        if m.version==1 {return Ok(None)}
        if m.version!=2 {return Err(Error("unsupported NV12 reconstruction metadata version".into()))}
        validate_samplers(&m.samplers)?;
        if m.nearest_sampler.is_some_and(|slot|slot>16 || m.samplers.iter().any(|p|p.slot==slot)) {
            return Err(Error("invalid internal NV12 nearest sampler slot".into()));
        }
        Ok(Some(Nv12Reconstruction {samplers:m.samplers,nearest_sampler:m.nearest_sampler}))
    }
}
