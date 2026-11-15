//! Binding-time specialization for imported NV12; no changes to ordinary draws.
//! The cache owns real code/scratch allocations, and a separate selection list
//! pins every shader used by a batch even if its variant is evicted meanwhile.
use super::*;
use sgfx_shader_ir3::{Nv12Profile, Nv12SamplerProfile, ResourceKind as ShaderKind};
const VARIANT_LIMIT: usize = 64;
const BYTE_LIMIT: usize = 16 * 1024 * 1024;
const BATCH_SHADER_LIMIT: usize = 128;
const BATCH_BYTE_LIMIT: usize = 32 * 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Render(ir::ProgrammableRenderPipelineId),
    Compute(ir::ComputePipelineId),
}
enum Pipeline {
    Render(Arc<codegen::NativeRenderPipeline>),
    Compute(Arc<codegen::NativeComputePipeline>),
}
struct Variant {
    key: Key,
    profiles: Vec<Nv12Profile>,
    samplers: Vec<Nv12SamplerProfile>,
    pipeline: Pipeline,
    shaders: Vec<Arc<ShaderRecord>>,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct Cache {
    variants: Vec<Variant>,
    selected: Vec<Arc<ShaderRecord>>,
    bytes: usize,
    selected_bytes: usize,
}
impl Cache {
    fn pin(&mut self, shaders: &[Arc<ShaderRecord>]) -> Result<(), IrSubmitError> {
        for shader in shaders {
            if !self.selected.iter().any(|s| Arc::ptr_eq(s, shader)) {
                let bytes = shader_bytes(shader)?;
                if self.selected.len() >= BATCH_SHADER_LIMIT
                    || self
                        .selected_bytes
                        .checked_add(bytes)
                        .is_none_or(|n| n > BATCH_BYTE_LIMIT)
                {
                    return Err(IrSubmitError::OutOfMemory);
                }
                self.selected
                    .try_reserve(1)
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                self.selected_bytes += bytes;
                self.selected.push(shader.clone());
            }
        }
        Ok(())
    }
    pub(super) fn retain(
        &self,
        bindings: &[BoundObject],
        output: &mut Vec<Arc<RawBuffer>>,
    ) -> Result<(), IrSubmitError> {
        for shader in &self.selected {
            if let Some(private) = &shader.private_memory {
                private.retain(bindings, output)?;
            }
            if bindings
                .iter()
                .any(|b| b.attachment_token == shader.allocation.attachment_token)
                && !output.iter().any(|b| Arc::ptr_eq(b, &shader.allocation))
            {
                output
                    .try_reserve(1)
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                output.push(shader.allocation.clone());
            }
        }
        Ok(())
    }
    pub(super) fn remove_render(&mut self, id: ir::ProgrammableRenderPipelineId) {
        self.remove(|v| v.key == Key::Render(id));
    }
    pub(super) fn remove_compute(&mut self, id: ir::ComputePipelineId) {
        self.remove(|v| v.key == Key::Compute(id));
    }
    pub(super) fn remove_module(&mut self, id: ir::ShaderModuleId) {
        self.remove(|v| match &v.pipeline {
            Pipeline::Render(p) => {
                p.descriptor.vertex().module() == id || p.descriptor.fragment().module() == id
            }
            Pipeline::Compute(p) => p.descriptor.shader().module() == id,
        });
    }
    fn remove(&mut self, predicate: impl Fn(&Variant) -> bool) {
        self.variants.retain(|v| !predicate(v));
        self.bytes = self.variants.iter().map(|v| v.bytes).sum();
        // selected may still be needed by preparation/retention. Public release
        // is idle-only; keeping it until the next batch is conservative/bounded.
    }
}
// Conservative cache charge includes real GPU code/scratch and owned CPU
// allocation/SPIR-V/metadata, rather than counting instruction bytes alone.
fn shader_bytes(shader: &ShaderRecord) -> Result<usize, IrSubmitError> {
    let a = &shader.native.artifact;
    let scratch =
        codegen::PrivateMemoryLayout::for_shader(&a.metadata)?.map_or(0, |l| l.allocation_bytes);
    let metadata = serde_json::to_vec(&a.metadata)
        .map_err(|_| IrSubmitError::OutOfMemory)?
        .len();
    let gpu = (shader.allocation.logical_size + 4095) & !4095;
    usize::try_from(gpu.checked_add(scratch).ok_or(IrSubmitError::OutOfMemory)?)
        .ok()
        .and_then(|n| n.checked_add(a.allocation.capacity()))
        .and_then(|n| n.checked_add(a.spirv.capacity().saturating_mul(4)))
        .and_then(|n| n.checked_add(metadata.saturating_mul(4).saturating_add(4096)))
        .ok_or(IrSubmitError::OutOfMemory)
}
fn shader_profiles(
    shader: &codegen::NativeShader,
    groups: &[(u32, Vec<codegen::NativeBinding>)],
    resources: &[codegen::ResourceMeta],
) -> Result<Vec<Nv12Profile>, IrSubmitError> {
    let mut profiles = Vec::new();
    for binding in &shader.artifact.bindings {
        if binding.kind != ShaderKind::Texture || binding.writable {
            continue;
        }
        let Some(resource) = groups
            .iter()
            .find(|(g, _)| *g == binding.group)
            .and_then(|(_, b)| b.iter().find(|b| b.binding == binding.binding))
            .map(|b| &b.resource)
        else {
            continue;
        };
        let object = match resource {
            codegen::NativeBindingResource::Texture { object, view: None }
            | codegen::NativeBindingResource::Nv12Texture(object) => *object,
            _ => continue,
        };
        let meta = resources
            .iter()
            .find(|r| r.id == object)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        let codegen::ResourceKind::Image(image) = &meta.kind else {
            continue;
        };
        if image.format == ir::TextureFormat::Nv12 {
            let conversion = image.conversion.ok_or(ir::Error::InvalidDescriptor)?;
            profiles
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            profiles.push(codegen::nv12_profile(binding.slot, conversion));
        }
    }
    profiles.sort_by_key(|p| p.texture_slot);
    Ok(profiles)
}
fn merge(
    mut first: Vec<Nv12Profile>,
    second: &[Nv12Profile],
) -> Result<Vec<Nv12Profile>, IrSubmitError> {
    for p in second {
        if let Some(old) = first.iter().find(|old| old.texture_slot == p.texture_slot) {
            if old != p {
                return Err(ir::Error::BindingLayoutMismatch.into());
            }
        } else {
            first
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            first.push(*p);
        }
    }
    first.sort_by_key(|p| p.texture_slot);
    Ok(first)
}
fn shader_samplers(shader:&codegen::NativeShader,groups:&[(u32,Vec<codegen::NativeBinding>)])->Result<Vec<Nv12SamplerProfile>,IrSubmitError> {
    let mut out=Vec::new();
    for binding in &shader.artifact.bindings {
        if binding.kind!=ShaderKind::Sampler {continue}
        let resource=groups.iter().find(|(g,_)|*g==binding.group)
            .and_then(|(_,bs)|bs.iter().find(|b|b.binding==binding.binding));
        if let Some(codegen::NativeBinding {resource:codegen::NativeBindingResource::Sampler(sampler),..})=resource {
            out.try_reserve(1).map_err(|_|IrSubmitError::OutOfMemory)?;
            out.push(codegen::nv12_sampler_profile(binding.slot,*sampler));
        }
    }
    out.sort_by_key(|p|p.slot);Ok(out)
}
impl ContextResources {
    fn nv12_shader(
        &mut self,
        entry: &ir::ShaderEntryPoint,
        map: &sgfx_shader_ir3::BindingMap,
        profiles: &[Nv12Profile],
        samplers: &[Nv12SamplerProfile],
    ) -> Result<Arc<ShaderRecord>, IrSubmitError> {
        if profiles.is_empty() {
            return self.compiled_shader(entry, map);
        }
        let module = self
            .resources
            .shader_module(self.resources.shader_module_ref(entry.module())?)?;
        let stage = match entry.stage() {
            ir::ShaderStage::Vertex => sgfx_shader_ir3::Stage::Vertex,
            ir::ShaderStage::Fragment => sgfx_shader_ir3::Stage::Fragment,
            ir::ShaderStage::Compute => sgfx_shader_ir3::Stage::Compute,
        };
        let shader = Arc::new(
            sgfx_shader_ir3::compile_with_bindings_ycbcr_samplers(
                source(module.source()),
                stage,
                entry.entry_point(),
                map,
                profiles,
                samplers,
            )
            .map_err(compiler_error)?,
        );
        let object = codegen::ObjectId::new(
            0x4000_0000u32
                .checked_add(self.programs.next_shader)
                .ok_or(IrSubmitError::OutOfMemory)?,
        );
        self.programs.next_shader = self
            .programs
            .next_shader
            .checked_add(1)
            .ok_or(IrSubmitError::OutOfMemory)?;
        if object.raw() & 0xe000_0000 != 0x4000_0000 {
            return Err(IrSubmitError::OutOfMemory);
        }
        let allocation = Arc::new(RawBuffer::create(
            &self.context,
            shader.allocation.len() as u64,
        )?);
        allocation.write(0, &shader.allocation)?;
        let private_memory = crate::native_private::PrivateMemoryBuffer::create(
            &self.context,
            codegen::ObjectId::new(object.raw() + 0x2000_0000),
            &shader.metadata,
        )?;
        Ok(Arc::new(ShaderRecord {
            native: codegen::NativeShader {
                object,
                private_memory: private_memory.as_ref().map(|m| m.native),
                artifact: shader,
            },
            allocation,
            private_memory,
        }))
    }
    fn nv12_variant(
        &mut self,
        key: Key,
        profiles: Vec<Nv12Profile>,
        render: Option<&Arc<codegen::NativeRenderPipeline>>,
        compute: Option<&Arc<codegen::NativeComputePipeline>>,
        groups: &[(u32, Vec<codegen::NativeBinding>)],
        resources: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<Pipeline, IrSubmitError> {
        let mut samplers=if let Some(base)=render {
            let mut all=shader_samplers(&base.vertex,groups)?;
            for p in shader_samplers(&base.fragment,groups)? {
                if let Some(old)=all.iter().find(|old|old.slot==p.slot) {
                    if *old!=p {return Err(ir::Error::BindingLayoutMismatch.into())}
                } else {all.push(p)}
            }
            all
        } else {shader_samplers(&compute.ok_or(IrSubmitError::ResourceTableMismatch)?.shader,groups)?};
        samplers.sort_by_key(|p|p.slot);
        if let Some(index) = self
            .programs
            .nv12
            .variants
            .iter()
            .position(|v| v.key == key && v.profiles == profiles && v.samplers == samplers)
        {
            let variant = self.programs.nv12.variants.remove(index);
            self.programs.nv12.bytes -= variant.bytes;
            self.programs.nv12.pin(&variant.shaders)?;
            for s in &variant.shaders {
                append_shader(s, resources, bindings)?;
            }
            let output = match &variant.pipeline {
                Pipeline::Render(p) => Pipeline::Render(p.clone()),
                Pipeline::Compute(p) => Pipeline::Compute(p.clone()),
            };
            self.programs.nv12.bytes += variant.bytes;
            self.programs.nv12.variants.push(variant);
            return Ok(output);
        }
        let (pipeline, shaders) = if let Some(base) = render {
            let map = pipeline_binding_map(base.descriptor.layout())?;
            let vs = shader_profiles(&base.vertex, groups, resources)?;
            let fs = shader_profiles(&base.fragment, groups, resources)?;
            let vertex = self.nv12_shader(base.descriptor.vertex(), &map, &vs, &shader_samplers(&base.vertex,groups)?)?;
            let fragment = self.nv12_shader(base.descriptor.fragment(), &map, &fs, &shader_samplers(&base.fragment,groups)?)?;
            validate_interface(&vertex.native.artifact, base.descriptor.layout())?;
            validate_interface(&fragment.native.artifact, base.descriptor.layout())?;
            let p = Arc::new(codegen::NativeRenderPipeline {
                descriptor: base.descriptor.clone(),
                vertex: vertex.native.clone(),
                fragment: fragment.native.clone(),
            });
            codegen::validate_native_render_pipeline(&p)?;
            (Pipeline::Render(p), alloc::vec![vertex, fragment])
        } else {
            let base = compute.ok_or(IrSubmitError::ResourceTableMismatch)?;
            let map = pipeline_binding_map(base.descriptor.layout())?;
            let shader = self.nv12_shader(base.descriptor.shader(), &map, &profiles, &samplers)?;
            validate_interface(&shader.native.artifact, base.descriptor.layout())?;
            let p = Arc::new(codegen::NativeComputePipeline {
                descriptor: base.descriptor.clone(),
                shader: shader.native.clone(),
            });
            codegen::validate_native_compute_pipeline(&p)?;
            (Pipeline::Compute(p), alloc::vec![shader])
        };
        let bytes = shaders.iter().try_fold(4096usize, |total, s| {
            total
                .checked_add(shader_bytes(s)?)
                .ok_or(IrSubmitError::OutOfMemory)
        })?;
        if bytes > BYTE_LIMIT {
            return Err(IrSubmitError::OutOfMemory);
        }
        self.programs.nv12.pin(&shaders)?;
        for s in &shaders {
            append_shader(s, resources, bindings)?;
        }
        let cache = &mut self.programs.nv12;
        while !cache.variants.is_empty()
            && (cache.variants.len() >= VARIANT_LIMIT || cache.bytes + bytes > BYTE_LIMIT)
        {
            let old = cache.variants.remove(0);
            cache.bytes -= old.bytes;
        }
        cache
            .variants
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        cache.bytes += bytes;
        let output = match &pipeline {
            Pipeline::Render(p) => Pipeline::Render(p.clone()),
            Pipeline::Compute(p) => Pipeline::Compute(p.clone()),
        };
        cache.variants.push(Variant {
            key,
            profiles,
            samplers,
            pipeline,
            shaders,
            bytes,
        });
        Ok(output)
    }
    pub(crate) fn lower_programmable_nv12<'d>(
        &mut self,
        resources: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
        operations: &mut Vec<codegen::Operation<'d>>,
    ) -> Result<(), IrSubmitError> {
        use codegen::Operation as O;
        self.programs.nv12.selected.clear();
        self.programs.nv12.selected_bytes = 0;
        if !resources.iter().any(|r|matches!(&r.kind,codegen::ResourceKind::Image(i) if i.format==ir::TextureFormat::Nv12)) {return Ok(());}
        let mut render = None;
        let mut compute = None;
        let mut groups: Vec<(u32, Vec<codegen::NativeBinding>)> = Vec::new();
        let mut output = Vec::new();
        output
            .try_reserve(operations.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for op in operations.iter() {
            match op {
                O::SetProgrammablePipeline(p) => {
                    render = Some(p.clone());
                    compute = None;
                }
                O::SetComputePipeline(p) => {
                    compute = Some(p.clone());
                    render = None;
                }
                O::SetFixedNativePipeline(_) | O::SetPipeline(_) => {
                    render = None;
                    compute = None;
                }
                O::SetBindGroup { index, bindings } => {
                    if let Some((_, b)) = groups.iter_mut().find(|(i, _)| i == index) {
                        *b = bindings.clone();
                    } else {
                        groups.push((*index, bindings.clone()));
                    }
                }
                O::BeginRenderPass(_)
                | O::BeginNativeRenderPass(_)
                | O::BeginComputePass
                | O::EndRenderPass
                | O::EndComputePass => {
                    render = None;
                    compute = None;
                    groups.clear();
                }
                O::Draw { .. }
                | O::DrawIndexed { .. }
                | O::DrawInstanced { .. }
                | O::DrawIndexedInstanced { .. }
                    if render.is_some() =>
                {
                    let p = render.as_ref().unwrap();
                    let profiles = merge(
                        shader_profiles(&p.vertex, &groups, resources)?,
                        &shader_profiles(&p.fragment, &groups, resources)?,
                    )?;
                    let effective = if profiles.is_empty() {
                        p.clone()
                    } else {
                        let id = self
                            .programs
                            .render
                            .iter()
                            .find(|(_, base, _)| Arc::ptr_eq(base, p))
                            .map(|(id, _, _)| *id)
                            .ok_or(IrSubmitError::ResourceTableMismatch)?;
                        let Pipeline::Render(effective) = self.nv12_variant(
                            Key::Render(id),
                            profiles,
                            Some(p),
                            None,
                            &groups,
                            resources,
                            bindings,
                        )?
                        else {
                            unreachable!()
                        };
                        effective
                    };
                    output.push(O::SetProgrammablePipeline(effective));
                    append_groups(&groups, resources, &mut output);
                }
                O::Dispatch { .. } if compute.is_some() => {
                    let p = compute.as_ref().unwrap();
                    let profiles = shader_profiles(&p.shader, &groups, resources)?;
                    let effective = if profiles.is_empty() {
                        p.clone()
                    } else {
                        let id = self
                            .programs
                            .compute
                            .iter()
                            .find(|(_, base, _)| Arc::ptr_eq(base, p))
                            .map(|(id, _, _)| *id)
                            .ok_or(IrSubmitError::ResourceTableMismatch)?;
                        let Pipeline::Compute(effective) = self.nv12_variant(
                            Key::Compute(id),
                            profiles,
                            None,
                            Some(p),
                            &groups,
                            resources,
                            bindings,
                        )?
                        else {
                            unreachable!()
                        };
                        effective
                    };
                    output.push(O::SetComputePipeline(effective));
                    append_groups(&groups, resources, &mut output);
                }
                _ => {}
            }
            output.push(op.clone());
        }
        *operations = output;
        Ok(())
    }
}
fn append_groups<'d>(
    groups: &[(u32, Vec<codegen::NativeBinding>)],
    resources: &[codegen::ResourceMeta],
    out: &mut Vec<codegen::Operation<'d>>,
) {
    for (index, bindings) in groups {
        let mut bindings = bindings.clone();
        for b in &mut bindings {
            if let codegen::NativeBindingResource::Texture { object, view: None } = &b.resource {
                if resources.iter().any(|r|r.id==*object&&matches!(&r.kind,codegen::ResourceKind::Image(i) if i.format==ir::TextureFormat::Nv12)) {b.resource=codegen::NativeBindingResource::Nv12Texture(*object);}
            }
        }
        out.push(codegen::Operation::SetBindGroup {
            index: *index,
            bindings,
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_test_fixtures as f;
    #[test]
    fn actual_shader_binding_slots_and_live_groups_select_only_reachable_nv12_profiles() {
        let case = f::generic_nv12_case();
        let p = case
            .operations
            .iter()
            .find_map(|o| {
                if let codegen::Operation::SetProgrammablePipeline(p) = o {
                    Some(p)
                } else {
                    None
                }
            })
            .unwrap();
        let groups = case
            .operations
            .iter()
            .find_map(|o| {
                if let codegen::Operation::SetBindGroup { index, bindings } = o {
                    Some(alloc::vec![(*index, bindings.clone())])
                } else {
                    None
                }
            })
            .unwrap();
        let fs = shader_profiles(&p.fragment, &groups, &case.resources).unwrap();
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].texture_slot, 0);
        // The same source module has a VS with no sampled resources; binding
        // an NV12 object elsewhere must not invent a profile for that stage.
        assert!(
            shader_profiles(&p.vertex, &groups, &case.resources)
                .unwrap()
                .is_empty()
        );
        assert!(
            shader_profiles(&p.fragment, &[], &case.resources)
                .unwrap()
                .is_empty()
        );
        let mut other = groups.clone();
        other[0].0 = 1;
        assert!(
            shader_profiles(&p.fragment, &other, &case.resources)
                .unwrap()
                .is_empty()
        );
        let mut ordinary = case.resources.clone();
        let r = ordinary.iter_mut().find(|r| r.id == f::SOURCE).unwrap();
        if let codegen::ResourceKind::Image(i) = &mut r.kind {
            i.format = ir::TextureFormat::Bgra8Unorm;
            i.conversion = None;
        }
        assert!(
            shader_profiles(&p.fragment, &groups, &ordinary)
                .unwrap()
                .is_empty()
        );
        assert_eq!(merge(fs.clone(), &fs).unwrap(), fs);
        let mut conflicting = fs.clone();
        conflicting[0].chroma_y = 1;
        assert!(merge(fs, &conflicting).is_err());
    }
}
