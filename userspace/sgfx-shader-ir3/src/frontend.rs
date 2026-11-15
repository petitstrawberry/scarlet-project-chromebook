// SPDX-License-Identifier: GPL-2.0-only
use crate::{
    Binding, BindingMap, Error, ResourceKind, SampledBinding, SampledBindingType, Source, Stage,
    TextureDimension, TextureSampleType,
};
use naga::{AddressSpace, TypeInner};
#[cfg_attr(not(feature = "mesa"), allow(dead_code))]
pub(crate) struct Prepared {
    pub spirv: Vec<u32>,
    pub bindings: Vec<Binding>,
    pub push_constant_bytes: u32,
    pub sampled_bindings: Vec<SampledBinding>,
}
pub(crate) fn validate(source: Source<'_>) -> Result<(), Error> {
    prepare_module(source).map(|_| ())
}
type ReflectedBinding = (naga::Handle<naga::GlobalVariable>, Binding);
fn prepare_module(source: Source<'_>) -> Result<(naga::Module, Vec<ReflectedBinding>, u32), Error> {
    let module = match source {
        Source::Wgsl(s) => {
            if s.len() > 4 * 1024 * 1024 {
                return Err(Error("shader source exceeds 4 MiB".into()));
            }
            naga::front::wgsl::parse_str(s).map_err(|e| Error(e.emit_to_string(s)))?
        }
        Source::SpirV(words) => {
            if words.len() > 1024 * 1024 || words.len() < 5 || words[0] != 0x07230203 {
                return Err(Error("invalid or oversized SPIR-V".into()));
            }
            let opts = naga::front::spv::Options {
                adjust_coordinate_space: false,
                strict_capabilities: true,
                ..Default::default()
            };
            naga::front::spv::Frontend::new(words.iter().copied(), &opts)
                .parse()
                .map_err(|e| Error(e.to_string()))?
        }
    };
    for ep in &module.entry_points {
        if ep.stage == naga::ShaderStage::Compute
            && (ep.workgroup_size.contains(&0)
                || ep
                    .workgroup_size
                    .iter()
                    .try_fold(1u32, |n, v| n.checked_mul(*v))
                    .is_none_or(|n| n > 1024))
        {
            return Err(Error(
                "compute workgroup must contain 1..1024 invocations".into(),
            ));
        }
    }
    // Fail before Mesa for constructs outside this compiler's current profile.
    for (_, ty) in module.types.iter() {
        match &ty.inner {
            TypeInner::BindingArray { .. } => {
                return Err(Error("descriptor arrays not implemented".into()));
            }
            TypeInner::Scalar(s)
            | TypeInner::Vector { scalar: s, .. }
            | TypeInner::Matrix { scalar: s, .. }
                if s.width > 4 =>
            {
                return Err(Error("64-bit shader values not implemented".into()));
            }

            _ => {}
        }
    }
    // SPIR-V NonReadable specializes a global's image type; its original
    // unqualified OpTypeImage remains in the arena. Check declared resources,
    // not unused historical type handles left by the importer.
    for (_, var) in module.global_variables.iter() {
        if matches!(
            module.types[var.ty].inner,
            TypeInner::Image {
                class: naga::ImageClass::Sampled { multi: true, .. }
                    | naga::ImageClass::Depth { multi: true },
                ..
            }
        ) {
            return Err(Error(
                "multisampled image resources are outside the single-sample profile".into(),
            ));
        }
        if let TypeInner::Image {
            dim,
            arrayed,
            class: naga::ImageClass::Storage { format, access },
        } = module.types[var.ty].inner
        {
            if dim != naga::ImageDimension::D2
                || arrayed
                || format != naga::StorageFormat::Rgba8Unorm
                || access != naga::StorageAccess::STORE
            {
                return Err(Error(
                    "storage images require write-only 2D rgba8unorm".into(),
                ));
            }
        }
    }
    // Naga gates SampleIndex and sample interpolation together with MSAA syntax.
    // Actual multisampled image resources are rejected above; Mesa lowers the
    // accepted fragment interface under our explicit samples==1 profile.
    let capabilities = naga::valid::Capabilities::PUSH_CONSTANT
        | naga::valid::Capabilities::PRIMITIVE_INDEX
        | naga::valid::Capabilities::MULTISAMPLED_SHADING;
    let mut validator =
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), capabilities);
    let info = validator
        .validate(&module)
        .map_err(|e| Error(e.to_string()))?;
    crate::uniformity::validate(&module, &info)?;
    let mut layout = naga::proc::Layouter::default();
    layout
        .update(module.to_ctx())
        .map_err(|e| Error(e.to_string()))?;
    // A618 exposes 32 KiB of shared storage. Check each entry's live variables,
    // not all declarations in a multi-entry module. Naga's logical layout is
    // conservative relative to IR3's scalar-aligned physical shared layout.
    for (index, ep) in module.entry_points.iter().enumerate() {
        let usage = info.get_entry_point(index);
        let mut shared = 0u32;
        for (handle, variable) in module.global_variables.iter() {
            if variable.space != AddressSpace::WorkGroup || usage[handle].is_empty() {
                continue;
            }
            if ep.stage != naga::ShaderStage::Compute {
                return Err(Error(
                    "workgroup storage is only valid in compute entries".into(),
                ));
            }
            let ty = layout[variable.ty];
            if ty.size > 32 * 1024 {
                return Err(Error("workgroup storage exceeds 32 KiB".into()));
            }
            shared = ty
                .alignment
                .round_up(shared)
                .checked_add(ty.size)
                .filter(|size| *size <= 32 * 1024)
                .ok_or_else(|| Error("workgroup storage exceeds 32 KiB".into()))?;
        }
    }
    let mut bindings = Vec::new();
    let mut push_constant_bytes = 0;
    for (handle, variable) in module.global_variables.iter() {
        if variable.space == AddressSpace::PushConstant {
            push_constant_bytes = layout[variable.ty].size;
        }
        let Some(binding) = &variable.binding else {
            continue;
        };
        let (kind, mut writable, minimum_size) = match variable.space {
            AddressSpace::Uniform => (ResourceKind::UniformBuffer, false, layout[variable.ty].size),
            AddressSpace::Storage { access } => (
                ResourceKind::StorageBuffer,
                access.contains(naga::StorageAccess::STORE),
                layout[variable.ty].size,
            ),
            AddressSpace::Handle => match module.types[variable.ty].inner {
                TypeInner::Sampler { .. } => (ResourceKind::Sampler, false, 0),
                TypeInner::Image { .. } => (ResourceKind::Texture, false, 0),
                _ => return Err(Error("unsupported handle resource".into())),
            },
            _ => return Err(Error("unsupported resource address space".into())),
        };
        if matches!(
            module.types[variable.ty].inner,
            TypeInner::Image {
                class: naga::ImageClass::Storage { .. },
                ..
            }
        ) {
            writable = true;
        }
        bindings.push((
            handle,
            Binding {
                group: binding.group,
                binding: binding.binding,
                slot: 0,
                kind,
                writable,
                minimum_size,
            },
        ));
    }
    if push_constant_bytes > 128 {
        return Err(Error("push constants exceed 128 bytes".into()));
    }
    bindings.sort_by_key(|(_, b)| (b.group, b.binding));
    // Hardware descriptor limits are per namespace and per selected entry.
    // A module may declare disjoint interfaces for several entry points.
    for index in 0..module.entry_points.len() {
        let usage = info.get_entry_point(index);
        let mut counts = [0u32; 4];
        for (handle, var) in module.global_variables.iter() {
            if usage[handle].is_empty() {
                continue;
            }
            if var.binding.is_none() {
                continue;
            }
            let (_, reflected) = bindings
                .iter()
                .find(|(id, _)| *id == handle)
                .ok_or_else(|| Error("missing resource reflection".into()))?;
            counts[reflected.kind.namespace() as usize] += 1;
        }
        if counts.iter().any(|count| *count > 16) {
            return Err(Error(
                "entry exceeds 16 resources in a native namespace".into(),
            ));
        }
    }
    Ok((module, bindings, push_constant_bytes))
}
pub(crate) fn prepare(
    source: Source<'_>,
    stage: Stage,
    entry: &str,
    binding_map: Option<&BindingMap>,
) -> Result<Prepared, Error> {
    if entry.is_empty() || entry.as_bytes().contains(&0) {
        return Err(Error("invalid entry name".into()));
    }
    let (mut module, mut reflected, mut push_constant_bytes) = prepare_module(source)?;
    if binding_map.is_none() {
        if reflected.len() > 16 {
            return Err(Error(
                "legacy compile exceeds 16 module bindings; use compile_with_bindings".into(),
            ));
        }
        for (index, (_, binding)) in reflected.iter_mut().enumerate() {
            binding.slot = index as u32;
        }
    }
    let shader_stage = match stage {
        Stage::Vertex => naga::ShaderStage::Vertex,
        Stage::Fragment => naga::ShaderStage::Fragment,
        Stage::Compute => naga::ShaderStage::Compute,
    };
    if !module
        .entry_points
        .iter()
        .any(|p| p.stage == shader_stage && p.name == entry)
    {
        return Err(Error(format!("missing {stage:?} entry {entry}")));
    }
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT
            | naga::valid::Capabilities::PRIMITIVE_INDEX
            | naga::valid::Capabilities::MULTISAMPLED_SHADING,
    );
    let info = validator
        .validate(&module)
        .map_err(|e| Error(e.to_string()))?;
    let ep_index = module
        .entry_points
        .iter()
        .position(|p| p.stage == shader_stage && p.name == entry)
        .unwrap();
    let usage = info.get_entry_point(ep_index);
    if !module
        .global_variables
        .iter()
        .any(|(handle, var)| var.space == AddressSpace::PushConstant && !usage[handle].is_empty())
    {
        push_constant_bytes = 0;
    }
    // Keep identity by global handle, not by group/binding alone: Naga permits
    // disjoint entries to use different declarations at the same binding.
    let used: Vec<_> = module
        .global_variables
        .iter()
        .filter_map(|(handle, var)| {
            (!usage[handle].is_empty() && var.binding.is_some()).then_some(handle)
        })
        .collect();
    let mut bindings: Vec<_> = reflected
        .iter()
        .filter(|(handle, _)| used.contains(handle))
        .map(|(_, binding)| binding.clone())
        .collect();
    if let Some(map) = binding_map {
        for binding in &mut bindings {
            let assignment = map.get(binding.group, binding.binding).ok_or_else(|| {
                Error(format!(
                    "missing binding map entry ({}, {})",
                    binding.group, binding.binding
                ))
            })?;
            if assignment.kind != binding.kind {
                return Err(Error(format!(
                    "binding map kind mismatch ({}, {})",
                    binding.group, binding.binding
                )));
            }
            binding.slot = assignment.slot;
        }
    }
    // Capture source types before descriptor normalization. Identity is by live
    // global handle: disjoint entries may redeclare the same group/binding with
    // different types. Reflection must not include another entry's declaration.
    let mut sampled_bindings = Vec::new();
    for handle in &used {
        let variable = &module.global_variables[*handle];
        let ty = match module.types[variable.ty].inner {
            TypeInner::Sampler { comparison } => SampledBindingType::Sampler { comparison },
            TypeInner::Image {
                class: naga::ImageClass::Storage { .. },
                ..
            } => SampledBindingType::StorageTextureRgba8WriteOnly,
            TypeInner::Image {
                dim,
                arrayed,
                class,
            } => {
                let (sample_type, multisampled) = match class {
                    naga::ImageClass::Depth { multi } => (TextureSampleType::Depth, multi),
                    naga::ImageClass::Sampled { kind, multi } => (
                        match kind {
                            naga::ScalarKind::Float => TextureSampleType::Float,
                            naga::ScalarKind::Sint => TextureSampleType::Sint,
                            naga::ScalarKind::Uint => TextureSampleType::Uint,
                            _ => return Err(Error("unsupported texture sample type".into())),
                        },
                        multi,
                    ),
                    _ => return Err(Error("unsupported sampled image class".into())),
                };
                SampledBindingType::Texture {
                    dimension: match dim {
                        naga::ImageDimension::D1 => TextureDimension::D1,
                        naga::ImageDimension::D2 => TextureDimension::D2,
                        naga::ImageDimension::D3 => TextureDimension::D3,
                        naga::ImageDimension::Cube => TextureDimension::Cube,
                    },
                    arrayed,
                    sample_type,
                    multisampled,
                }
            }
            _ => continue,
        };
        let original = variable.binding.as_ref().unwrap();
        let binding = bindings
            .iter()
            .find(|b| b.group == original.group && b.binding == original.binding)
            .ok_or_else(|| Error("missing sampled resource reflection".into()))?;
        sampled_bindings.push(SampledBinding {
            group: binding.group,
            binding: binding.binding,
            slot: binding.slot,
            ty,
        });
    }
    sampled_bindings.sort_by_key(|b| (b.group, b.binding));
    // Keep only the chosen entry. This avoids imposing its pipeline layout on
    // unrelated entries; the original whole module was already validated above.
    let selected = module.entry_points.swap_remove(ep_index);
    module.entry_points.clear();
    module.entry_points.push(selected);
    for (handle, variable) in module.global_variables.iter_mut() {
        let Some(original) = &mut variable.binding else {
            continue;
        };
        if binding_map.is_some() {
            if used.contains(&handle) {
                let reflected = bindings
                    .iter()
                    .find(|b| b.group == original.group && b.binding == original.binding)
                    .ok_or_else(|| Error("missing selected resource reflection".into()))?;
                original.group = reflected.kind.namespace();
                original.binding = reflected.slot;
            } else {
                // Naga retains unused declarations. Give them unique inert sets,
                // and a bounded binding, so they cannot alias live resources or
                // inflate Mesa's descriptor-array bounds. No live instruction
                // in the selected entry can reference them (GlobalUse above).
                original.group = 4 + handle.index() as u32;
                original.binding = 0;
            }
        } else {
            let (_, binding) = reflected
                .iter()
                .find(|(id, _)| *id == handle)
                .ok_or_else(|| Error("missing module resource reflection".into()))?;
            original.group = 0;
            original.binding = binding.slot;
        }
    }
    let info = validator
        .validate(&module)
        .map_err(|e| Error(e.to_string()))?;
    // SGFX's clip-space is Y-up; do not apply Naga's optional Y flip.
    let options = naga::back::spv::Options {
        flags: naga::back::spv::WriterFlags::empty(),
        // WGSL workgroup variables start at zero, including atomics. Emit real
        // initialization stores and a control barrier, not a Vulkan extension
        // or an assumption that the driver cleared shared memory.
        zero_initialize_workgroup_memory:
            naga::back::spv::ZeroInitializeWorkgroupMemoryMode::Polyfill,
        ..Default::default()
    };
    let spirv = naga::back::spv::write_vec(
        &module,
        &info,
        &options,
        Some(&naga::back::spv::PipelineOptions {
            shader_stage,
            entry_point: entry.into(),
        }),
    )
    .map_err(|e| Error(e.to_string()))?;
    Ok(Prepared {
        spirv,
        bindings,
        push_constant_bytes,
        sampled_bindings,
    })
}
