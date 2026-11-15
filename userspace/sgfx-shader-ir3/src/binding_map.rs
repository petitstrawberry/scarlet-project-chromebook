// SPDX-License-Identifier: GPL-2.0-only
use crate::{Error, ResourceKind};
use alloc::{string::String, vec::Vec};
use serde::{Deserialize, Serialize};

/// One original shader resource mapped to a pipeline-wide native descriptor slot.
/// Slots are independent for UBOs, SSBOs, sampled textures and samplers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct BindingAssignment {
    pub group: u32,
    pub binding: u32,
    pub kind: ResourceKind,
    pub slot: u32,
}

/// Canonical, immutable shared VS/FS (or compute) binding map.
///
/// Include this entire value in the shader cache key, together with the original
/// module generation/source, stage, entry, COMPILER_CACHE_VERSION and Mesa pin.
/// Reordering the constructor input preserves equality/hash. Changing any entry
/// (even one unused in a particular stage) changes the pipeline map identity.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct BindingMap {
    assignments: Vec<BindingAssignment>,
}
impl BindingMap {
    pub fn new(mut assignments: Vec<BindingAssignment>) -> Result<Self, Error> {
        if assignments.len() > 64 {
            return Err(Error(String::from(
                "binding map exceeds four 16-slot namespaces",
            )));
        }
        assignments.sort_unstable_by_key(|b| (b.group, b.binding));
        for (index, binding) in assignments.iter().enumerate() {
            if binding.slot >= 16 {
                return Err(Error(String::from(
                    "binding map native slot must be below 16",
                )));
            }
            if index > 0
                && (assignments[index - 1].group, assignments[index - 1].binding)
                    == (binding.group, binding.binding)
            {
                return Err(Error(String::from("duplicate original binding in map")));
            }
            if assignments[..index]
                .iter()
                .any(|previous| previous.kind == binding.kind && previous.slot == binding.slot)
            {
                return Err(Error(String::from(
                    "native descriptor slot aliases different bindings",
                )));
            }
        }
        Ok(Self { assignments })
    }

    /// Sorted by original (group, binding). No unchecked mutable access is exposed.
    pub fn assignments(&self) -> &[BindingAssignment] {
        &self.assignments
    }

    #[cfg(feature = "frontend")]
    pub(crate) fn get(&self, group: u32, binding: u32) -> Option<&BindingAssignment> {
        self.assignments
            .binary_search_by_key(&(group, binding), |b| (b.group, b.binding))
            .ok()
            .map(|index| &self.assignments[index])
    }
}

#[cfg(feature = "frontend")]
impl ResourceKind {
    /// Internal SPIR-V descriptor sets separate the native namespaces. The Mesa
    /// port consumes binding numbers directly within each resource class.
    pub(crate) fn namespace(self) -> u32 {
        match self {
            Self::UniformBuffer => 0,
            Self::StorageBuffer => 1,
            Self::Texture => 2,
            Self::Sampler => 3,
        }
    }
}
