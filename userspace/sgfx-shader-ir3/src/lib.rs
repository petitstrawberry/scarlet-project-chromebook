// SPDX-License-Identifier: GPL-2.0-only
//! Real, source-built Mesa IR3 compiler. Compilation alone does not grant GPU
//! execution safety: callers must validate resources and isolate GPU addresses.
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;
use alloc::{string::String, vec::Vec};
mod binding_map;
#[cfg(feature = "frontend")]
mod frontend;
#[cfg(feature = "frontend")]
mod uniformity;
pub use binding_map::{BindingAssignment, BindingMap};
pub mod metadata;
mod sampled;
mod ycbcr;
pub use ycbcr::{Nv12Profile,Nv12TextureProfile,Nv12SamplerProfile,Nv12Reconstruction};
pub use sampled::{SampledBinding, SampledBindingType, TextureDimension, TextureSampleType};
#[cfg(feature = "mesa")]
mod native;
pub use metadata::Metadata;
use serde::{Deserialize, Serialize};

/// Bump when compiler frontend/lowering semantics or cache inputs change.
pub const COMPILER_CACHE_VERSION: u32 = 12;
/// This compiler/backend profile is tied to SGFX's single-sample targets.
/// Sample syntax is lowered only after passing this explicit profile to Mesa;
/// it does not advertise multisampled images or per-sample execution.
pub const FRAGMENT_RASTERIZATION_SAMPLES: u32 = 1;
pub const MESA_REVISION: &str = "3f1b217baffffa00cb8f53e158713a33e1bd4632";
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
#[repr(u32)]
pub enum Stage {
    Vertex = 0,
    Fragment = 1,
    Compute = 2,
}
pub enum Source<'a> {
    Wgsl(&'a str),
    SpirV(&'a [u32]),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub enum ResourceKind {
    UniformBuffer,
    StorageBuffer,
    Texture,
    Sampler,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Binding {
    pub group: u32,
    pub binding: u32,
    /// Native descriptor slot in this resource kind's namespace.
    /// Legacy compile assigns dense per-module slots across all resource kinds.
    /// compile_with_bindings uses the caller's shared pipeline map instead.
    /// Reflection always contains only resources actually used by the entry.
    pub slot: u32,
    pub kind: ResourceKind,
    pub writable: bool,
    /// Naga layout size for buffers (runtime arrays: minimum size).
    pub minimum_size: u32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledShader {
    pub stage: Stage,
    pub entry: String,
    /// Upload the WHOLE allocation, 128-byte aligned. Do not truncate to sizedwords.
    pub allocation: Vec<u8>,
    pub metadata: Metadata,
    pub bindings: Vec<Binding>,
    pub push_constant_bytes: u32,
    /// Validated, normalized SPIR-V; useful for cache keys and diagnostics.
    pub spirv: Vec<u32>,
}
#[derive(Debug)]
pub struct Error(pub String);
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}
impl core::error::Error for Error {}

/// Validate WGSL or SPIR-V through Naga, lower resources, and compile the chosen
/// entry using Mesa NIR + IR3. No subprocess, canned shader, or CPU fallback.
#[cfg(feature = "frontend")]
pub fn compile(source: Source<'_>, stage: Stage, entry: &str) -> Result<CompiledShader, Error> {
    let prepared = frontend::prepare(source, stage, entry, None)?;
    compile_prepared(prepared, stage, entry)
}

/// Compile with a shared, canonical pipeline binding map. Pass the same map to
/// both VS and FS. Only the selected entry's used resources must be present;
/// assignments used by other stages are allowed and remain part of cache identity.
/// Original group/binding numbers remain in reflection. Native slots are bounded
/// and independent per resource kind; no shader may alias another binding's slot.
#[cfg(feature = "frontend")]
pub fn compile_with_bindings(
    source: Source<'_>,
    stage: Stage,
    entry: &str,
    bindings: &BindingMap,
) -> Result<CompiledShader, Error> {
    let prepared = frontend::prepare(source, stage, entry, Some(bindings))?;
    compile_prepared(prepared, stage, entry)
}

/// Specialize actual bound, readonly NV12 texture slots. This performs real NIR
/// sample-result conversion; the complete profile is part of cache identity.
#[cfg(feature="frontend")]
pub fn compile_with_bindings_ycbcr(source:Source<'_>,stage:Stage,entry:&str,bindings:&BindingMap,profiles:&[Nv12Profile])->Result<CompiledShader,Error> {
    compile_with_bindings_ycbcr_samplers(source,stage,entry,bindings,profiles,&[])
}
#[cfg(feature="frontend")]
pub fn compile_with_bindings_ycbcr_samplers(source:Source<'_>,stage:Stage,entry:&str,bindings:&BindingMap,profiles:&[Nv12Profile],samplers:&[Nv12SamplerProfile])->Result<CompiledShader,Error> {
    let prepared=frontend::prepare(source,stage,entry,Some(bindings))?;
    ycbcr::validate_profiles(profiles)?;
    ycbcr::validate_samplers(samplers)?;
    for sampler in samplers {
        if !prepared.bindings.iter().any(|b|b.kind==ResourceKind::Sampler && b.slot==sampler.slot) {
            return Err(Error("NV12 sampler profile requires a live sampler binding".into()));
        }
    }
    for profile in profiles {
        if !prepared.sampled_bindings.iter().any(|b|b.slot==profile.texture_slot&&b.ty==SampledBindingType::Texture {
            dimension:TextureDimension::D2,arrayed:false,sample_type:TextureSampleType::Float,multisampled:false}) {
            return Err(Error("NV12 profile requires a live 2D f32 sampled texture binding".into()));
        }
    }
    compile_prepared_ycbcr(prepared,stage,entry,profiles,samplers)
}


#[cfg(feature = "frontend")]
fn compile_prepared(
    prepared: frontend::Prepared,
    stage: Stage,
    entry: &str,
) -> Result<CompiledShader, Error> {
    compile_prepared_ycbcr(prepared,stage,entry,&[],&[])
}
#[cfg(feature="frontend")]
fn compile_prepared_ycbcr(prepared:frontend::Prepared,stage:Stage,entry:&str,profiles:&[Nv12Profile],samplers:&[Nv12SamplerProfile])->Result<CompiledShader,Error> {
    #[cfg(feature = "mesa")]
    {
        let (allocation, mut metadata) = if profiles.is_empty() {
            native::compile(&prepared.spirv, stage, entry, prepared.push_constant_bytes)?
        } else {
            native::compile_ycbcr(&prepared.spirv, stage, entry, prepared.push_constant_bytes,profiles,samplers)?
        };
        let actual=metadata.nv12_profiles()?.unwrap_or_default();
        if actual.iter().map(|p|p.profile).collect::<Vec<_>>()!=profiles {return Err(Error("native NV12 lowering profile mismatch".into()));}
        if !profiles.is_empty() && metadata.nv12_reconstruction()?.as_ref().map(|r|r.samplers.as_slice())!=Some(samplers) {
            return Err(Error("native NV12 sampler profile mismatch".into()));
        }
        metadata.set_sampled_bindings(prepared.sampled_bindings)?;
        Ok(CompiledShader {
            stage,
            entry: entry.into(),
            allocation,
            metadata,
            bindings: prepared.bindings,
            push_constant_bytes: prepared.push_constant_bytes,
            spirv: prepared.spirv,
        })
    }
    #[cfg(not(feature = "mesa"))]
    {
        let _ = (prepared, stage, entry,profiles);
        Err(Error(
            "Mesa compiler not linked; build with the mesa feature and IR3_MESA_LIB_DIR".into(),
        ))
    }
}

/// Validate every entry and resource in the module without choosing an entry or
/// invoking Mesa. Applies the same supported-language profile as compilation.
#[cfg(feature = "frontend")]
pub fn validate(source: Source<'_>) -> Result<(), Error> {
    frontend::validate(source)
}
