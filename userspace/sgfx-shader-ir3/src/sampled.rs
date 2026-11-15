// SPDX-License-Identifier: GPL-2.0-only
//! Selected-entry texture/sampler types, including public storage images, independent of native Mesa ABI 1.
use crate::{CompiledShader, Error, Metadata};
use alloc::{string::ToString, vec::Vec};
use serde::{Deserialize, Serialize};

const KEY: &str = "sgfx_sampled_interface";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub enum TextureDimension {
    D1,
    D2,
    D3,
    Cube,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub enum TextureSampleType {
    Float,
    Sint,
    Uint,
    Depth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub enum SampledBindingType {
    /// Public SGFX storage-image profile: non-arrayed 2D, RGBA8 UNORM, write-only.
    /// Uses the texture namespace; physical IBO slot is 16 + slot.
    StorageTextureRgba8WriteOnly,
    Texture {
        dimension: TextureDimension,
        arrayed: bool,
        sample_type: TextureSampleType,
        multisampled: bool,
    },
    Sampler {
        comparison: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct SampledBinding {
    /// Original source identity, before native descriptor normalization.
    pub group: u32,
    pub binding: u32,
    /// Independent texture/sampler namespace slot from the binding map.
    pub slot: u32,
    pub ty: SampledBindingType,
}

#[derive(Deserialize, Serialize)]
struct Interface {
    version: u32,
    bindings: Vec<SampledBinding>,
}

impl Metadata {
    /// None means an older artifact without typed reflection. Some(empty) means
    /// a new artifact with no texture/sampler resources. Unknown versions and malformed
    /// metadata are errors, never silently treated as an empty interface.
    /// Consumers must still check the pipeline layout and actual bound resources.
    pub fn sampled_bindings(&self) -> Result<Option<Vec<SampledBinding>>, Error> {
        let Some(value) = self.extra.get(KEY) else {
            return Ok(None);
        };
        let interface: Interface =
            serde_json::from_value(value.clone()).map_err(|e| Error(e.to_string()))?;
        if interface.version != 1 {
            return Err(Error("unsupported sampled interface version".into()));
        }
        Ok(Some(interface.bindings))
    }

    #[cfg(feature = "mesa")]
    pub(crate) fn set_sampled_bindings(
        &mut self,
        bindings: Vec<SampledBinding>,
    ) -> Result<(), Error> {
        let value = serde_json::to_value(Interface {
            version: 1,
            bindings,
        })
        .map_err(|e| Error(e.to_string()))?;
        self.extra.insert(KEY.into(), value);
        Ok(())
    }
}

impl CompiledShader {
    /// Typed requirements for this entry's sampled/storage textures and samplers.
    /// Buffer requirements remain in `bindings`; this does not change slot maps.
    pub fn sampled_bindings(&self) -> Result<Option<Vec<SampledBinding>>, Error> {
        self.metadata.sampled_bindings()
    }
}
