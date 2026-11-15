//! Context-local, generational native program cache and interface validation.
//!
//! A successful parse is insufficient: pipeline admission checks the compiled
//! shader's actual resource interface against the application's layout.

use crate::{IrSubmitError, UnsupportedIrFeature, ir, resource::ContextResources};
#[cfg(feature = "programmable")]
use alloc::{sync::Arc, vec::Vec};

#[cfg(feature="programmable")]
#[path="programmable_nv12.rs"]
mod nv12;

#[derive(Default)]
pub(crate) struct ProgramCache {
    #[cfg(feature="programmable")]
    nv12:nv12::Cache,
    #[cfg(feature = "programmable")]
    modules: Vec<ir::ShaderModuleId>,
    #[cfg(feature = "programmable")]
    shaders: Vec<(
        ir::ShaderEntryPoint,
        sgfx_shader_ir3::BindingMap,
        Arc<ShaderRecord>,
    )>,
    #[cfg(feature = "programmable")]
    render: Vec<(
        ir::ProgrammableRenderPipelineId,
        Arc<codegen::NativeRenderPipeline>,
        [Arc<ShaderRecord>; 2],
    )>,
    #[cfg(feature = "programmable")]
    compute: Vec<(
        ir::ComputePipelineId,
        Arc<codegen::NativeComputePipeline>,
        Arc<ShaderRecord>,
    )>,
    #[cfg(feature = "programmable")]
    next_shader: u32,
}

#[cfg(feature = "programmable")]
use crate::{resource::RawBuffer, wire::BoundObject};
#[cfg(feature = "programmable")]
use sgfx_codegen_adreno_a6xx as codegen;

#[cfg(feature = "programmable")]
pub(crate) struct ShaderRecord {
    private_memory: Option<crate::native_private::PrivateMemoryBuffer>,
    native: codegen::NativeShader,
    allocation: Arc<RawBuffer>,
}

impl ContextResources {
    #[cfg(feature = "programmable")]
    pub(crate) fn programmable_operation<'data>(
        &mut self,
        command: &ir::Command<'_, 'data>,
        resources: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::Operation<'data>, IrSubmitError> {
        use codegen::Operation as O;
        use ir::Command as C;
        let table = alloc::rc::Rc::clone(&self.resources);
        Ok(match command {
            C::SetProgrammablePipeline(reference) => {
                self.validate_programmable_render_pipeline(reference.id())?;
                let (_, pipeline, shaders) = self
                    .programs
                    .render
                    .iter()
                    .find(|(key, _, _)| *key == reference.id())
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                for shader in shaders {
                    append_shader(shader, resources, bindings)?;
                }
                O::SetProgrammablePipeline(Arc::clone(pipeline))
            }
            C::SetComputePipeline(reference) => {
                self.validate_compute_pipeline(reference.id())?;
                let (_, pipeline, shader) = self
                    .programs
                    .compute
                    .iter()
                    .find(|(key, _, _)| *key == reference.id())
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                append_shader(shader, resources, bindings)?;
                O::SetComputePipeline(Arc::clone(pipeline))
            }
            C::SetBindGroup { index, bind_group } => {
                let group = table.bind_group_shared(*bind_group)?;
                let mut normalized = Vec::new();
                normalized
                    .try_reserve_exact(group.entries().len())
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                for entry in group.entries() {
                    let layout = group
                        .layout()
                        .entries()
                        .iter()
                        .find(|layout| layout.binding() == entry.binding())
                        .ok_or(ir::Error::BindingLayoutMismatch)?;
                    let resource = match entry.resource() {
                        ir::BindingResource::Buffer {
                            buffer,
                            offset,
                            size,
                        } => codegen::NativeBindingResource::Buffer {
                            object: self.ensure_buffer(
                                table.buffer_ref(buffer)?,
                                resources,
                                bindings,
                            )?,
                            offset,
                            size,
                            write: matches!(
                                layout.ty(),
                                ir::BindingType::StorageBuffer { read_only: false }
                            ),
                        },
                        ir::BindingResource::Texture(texture) => {
                            codegen::NativeBindingResource::Texture {
                                object: self.ensure_image(
                                    table.texture_ref(texture)?,
                                    resources,
                                    bindings,
                                )?,
                                view: None,
                            }
                        }
                        ir::BindingResource::TextureView { texture, view } => {
                            codegen::NativeBindingResource::Texture {
                                object: self.ensure_image(
                                    table.texture_ref(texture)?,
                                    resources,
                                    bindings,
                                )?,
                                view: Some(view),
                            }
                        }
                        ir::BindingResource::Sampler(sampler) => {
                            codegen::NativeBindingResource::Sampler(
                                table.sampler(table.sampler_ref(sampler)?)?,
                            )
                        }
                    };
                    normalized.push(codegen::NativeBinding {
                        binding: entry.binding(),
                        resource,
                    });
                }
                O::SetBindGroup {
                    index: *index,
                    bindings: normalized,
                }
            }
            C::BeginComputePass => O::BeginComputePass,
            C::EndComputePass => O::EndComputePass,
            C::Dispatch { x, y, z } => O::Dispatch {
                x: *x,
                y: *y,
                z: *z,
            },
            C::SetVertexBufferSlot {
                slot,
                buffer,
                offset,
            } => O::SetVertexBufferSlot {
                slot: *slot,
                buffer: self.ensure_buffer(*buffer, resources, bindings)?,
                offset: *offset,
            },
            C::SetPushConstants {
                stages,
                offset,
                data,
            } => O::SetPushConstants {
                stages: *stages,
                offset: *offset,
                data,
            },
            C::SetDrawIndex(index) => O::SetDrawIndex(*index),
            C::DrawInstanced {
                vertex_count,
                first_vertex,
                instance_count,
                first_instance,
            } => O::DrawInstanced {
                vertex_count: *vertex_count,
                first_vertex: *first_vertex,
                instance_count: *instance_count,
                first_instance: *first_instance,
            },
            C::DrawIndexedInstanced {
                index_count,
                first_index,
                base_vertex,
                instance_count,
                first_instance,
            } => O::DrawIndexedInstanced {
                index_count: *index_count,
                first_index: *first_index,
                base_vertex: *base_vertex,
                instance_count: *instance_count,
                first_instance: *first_instance,
            },
            _ => return Err(unsupported()),
        })
    }

    pub(crate) fn validate_programmable_render_pipeline(
        &mut self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        // Validate the live table identity even on a cache hit, but do not
        // deep-copy its layout and vertex attributes for every pipeline bind.
        let reference = self.resources.programmable_render_pipeline_ref(id)?;
        #[cfg(feature = "programmable")]
        if self.programs.render.iter().any(|(key, _, _)| *key == id) {
            return Ok(());
        }
        let descriptor = self.resources.programmable_render_pipeline(reference)?;
        #[cfg(feature = "programmable")]
        {
            let binding_map = pipeline_binding_map(descriptor.layout())?;
            let vertex = self.compiled_shader(descriptor.vertex(), &binding_map)?;
            let fragment = self.compiled_shader(descriptor.fragment(), &binding_map)?;
            validate_interface(&vertex.native.artifact, descriptor.layout())?;
            validate_interface(&fragment.native.artifact, descriptor.layout())?;
            let pipeline = Arc::new(codegen::NativeRenderPipeline {
                descriptor,
                vertex: vertex.native.clone(),
                fragment: fragment.native.clone(),
            });
            codegen::validate_native_render_pipeline(&pipeline)?;
            self.programs
                .render
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            self.programs
                .render
                .push((id, pipeline, [vertex, fragment]));
            Ok(())
        }
        #[cfg(not(feature = "programmable"))]
        {
            let _ = descriptor;
            Err(unsupported())
        }
    }

    pub(crate) fn validate_compute_pipeline(
        &mut self,
        id: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.compute_pipeline_ref(id)?;
        #[cfg(feature = "programmable")]
        if self.programs.compute.iter().any(|(key, _, _)| *key == id) {
            return Ok(());
        }
        let descriptor = self.resources.compute_pipeline(reference)?;
        #[cfg(feature = "programmable")]
        {
            let binding_map = pipeline_binding_map(descriptor.layout())?;
            let shader = self.compiled_shader(descriptor.shader(), &binding_map)?;
            validate_interface(&shader.native.artifact, descriptor.layout())?;
            let pipeline = Arc::new(codegen::NativeComputePipeline {
                descriptor,
                shader: shader.native.clone(),
            });
            codegen::validate_native_compute_pipeline(&pipeline)?;
            self.programs
                .compute
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            self.programs.compute.push((id, pipeline, shader));
            Ok(())
        }
        #[cfg(not(feature = "programmable"))]
        {
            let _ = descriptor;
            Err(unsupported())
        }
    }

    pub(crate) fn release_programmable_render_pipeline(
        &mut self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        self.resources.programmable_render_pipeline_ref(id)?;
        #[cfg(feature = "programmable")]
        {self.programs.render.retain(|(key, _, _)| *key != id);self.programs.nv12.remove_render(id);}
        Ok(())
    }

    pub(crate) fn release_compute_pipeline(
        &mut self,
        id: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        self.resources.compute_pipeline_ref(id)?;
        #[cfg(feature = "programmable")]
        {self.programs.compute.retain(|(key, _, _)| *key != id);self.programs.nv12.remove_compute(id);}
        Ok(())
    }

    pub(crate) fn validate_shader_module(
        &mut self,
        id: ir::ShaderModuleId,
    ) -> Result<(), IrSubmitError> {
        let module = self
            .resources
            .shader_module(self.resources.shader_module_ref(id)?)?;
        #[cfg(feature = "programmable")]
        {
            if self.programs.modules.contains(&id) {
                return Ok(());
            }
            sgfx_shader_ir3::validate(source(module.source())).map_err(compiler_error)?;
            self.programs
                .modules
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            self.programs.modules.push(id);
            Ok(())
        }
        #[cfg(not(feature = "programmable"))]
        {
            let _ = module;
            Err(unsupported())
        }
    }

    #[cfg(feature = "programmable")]
    pub(crate) fn compiled_shader(
        &mut self,
        entry: &ir::ShaderEntryPoint,
        binding_map: &sgfx_shader_ir3::BindingMap,
    ) -> Result<Arc<ShaderRecord>, IrSubmitError> {
        // Resolve before consulting the cache: a retired generation must never
        // become valid again merely because its binary is still retained.
        let module = self
            .resources
            .shader_module(self.resources.shader_module_ref(entry.module())?)?;
        if let Some((_, _, shader)) = self
            .programs
            .shaders
            .iter()
            .find(|(key, map, _)| key == entry && map == binding_map)
        {
            return Ok(Arc::clone(shader));
        }
        let stage = match entry.stage() {
            ir::ShaderStage::Vertex => sgfx_shader_ir3::Stage::Vertex,
            ir::ShaderStage::Fragment => sgfx_shader_ir3::Stage::Fragment,
            ir::ShaderStage::Compute => sgfx_shader_ir3::Stage::Compute,
        };
        let shader = crate::shader_artifact_cache::compile(
            module.source(),
            stage,
            entry.entry_point(),
            binding_map,
        )
        .map_err(compiler_error)?;
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
        let allocation = Arc::new(RawBuffer::create(
            &self.context,
            shader.allocation.len() as u64,
        )?);
        allocation.write(0, &shader.allocation)?;
        // Reserve a distinct, bounded identity namespace for cache-owned scratch.
        if object.raw() & 0xe000_0000 != 0x4000_0000 { return Err(IrSubmitError::OutOfMemory); }
        let private_memory = crate::native_private::PrivateMemoryBuffer::create(
            &self.context, codegen::ObjectId::new(object.raw() + 0x2000_0000), &shader.metadata,
        )?;
        let record = Arc::new(ShaderRecord {
            native: codegen::NativeShader {
                private_memory: private_memory.as_ref().map(|memory| memory.native),
                object,
                artifact: shader,
            },
            allocation,
            private_memory,
        });
        self.programs
            .shaders
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.programs
            .shaders
            .push((entry.clone(), binding_map.clone(), Arc::clone(&record)));
        Ok(record)
    }

    pub(crate) fn release_shader_module(
        &mut self,
        id: ir::ShaderModuleId,
    ) -> Result<(), IrSubmitError> {
        self.resources.shader_module_ref(id)?;
        #[cfg(feature = "programmable")]
        {
            self.programs.nv12.remove_module(id);
            self.programs.modules.retain(|key| *key != id);
            self.programs
                .shaders
                .retain(|(key, _, _)| key.module() != id);
        }
        Ok(())
    }
}

#[cfg(feature = "programmable")]
fn append_shader(
    record: &ShaderRecord,
    resources: &mut Vec<codegen::ResourceMeta>,
    bindings: &mut Vec<BoundObject>,
) -> Result<(), IrSubmitError> {
    if let Some(memory) = &record.private_memory { memory.append(resources, bindings)?; }
    let id = record.native.object;
    if resources.iter().any(|resource| resource.id == id) {
        return Ok(());
    }
    resources
        .try_reserve(1)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    bindings
        .try_reserve(2)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    resources.push(codegen::ResourceMeta {
        id,
        size: record.allocation.logical_size,
        kind: codegen::ResourceKind::Buffer {
            usage: ir::BufferUsage::UNIFORM,
        },
    });
    bindings.push(BoundObject {
        object: codegen::ObjectRef::NativeShader(id),
        attachment_token: record.allocation.attachment_token,
        allocation_offset: 0,
        size: record.allocation.logical_size,
    });
    // Embedded constants share the allocation but are data, not executable
    // entry-point relocations. Preserve that distinction on the kernel wire.
    bindings.push(BoundObject {
        object: codegen::ObjectRef::External(id),
        attachment_token: record.allocation.attachment_token,
        allocation_offset: 0,
        size: record.allocation.logical_size,
    });
    Ok(())
}

impl ProgramCache {
    /// Retain allocation ownership across cache retirement and session drop.
    /// Only programs actually referenced by this submission are retained.
    pub(crate) fn retain_buffers(
        &self,
        bindings: &[crate::wire::BoundObject],
        output: &mut alloc::vec::Vec<alloc::sync::Arc<crate::resource::RawBuffer>>,
    ) -> Result<(), IrSubmitError> {
        #[cfg(feature = "programmable")]
        {
            self.nv12.retain(bindings,output)?;
            let records = self
                .shaders
                .iter()
                .map(|(_, _, shader)| shader)
                .chain(self.render.iter().flat_map(|(_, _, shaders)| shaders))
                .chain(self.compute.iter().map(|(_, _, shader)| shader));
            for record in records {
                if let Some(memory) = &record.private_memory { memory.retain(bindings, output)?; }
                if bindings
                    .iter()
                    .any(|binding| binding.attachment_token == record.allocation.attachment_token)
                    && !output
                        .iter()
                        .any(|buffer| Arc::ptr_eq(buffer, &record.allocation))
                {
                    output
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    output.push(Arc::clone(&record.allocation));
                }
            }
        }
        #[cfg(not(feature = "programmable"))]
        let _ = (bindings, output);
        Ok(())
    }
}

fn unsupported() -> IrSubmitError {
    IrSubmitError::Unsupported(UnsupportedIrFeature::ProgrammableExecution)
}

#[cfg(feature = "programmable")]
fn source(source: &ir::ShaderSource) -> sgfx_shader_ir3::Source<'_> {
    match source {
        ir::ShaderSource::Wgsl(text) => sgfx_shader_ir3::Source::Wgsl(text),
        ir::ShaderSource::SpirV(words) => sgfx_shader_ir3::Source::SpirV(words),
    }
}

#[cfg(feature = "programmable")]
fn compiler_error(error: sgfx_shader_ir3::Error) -> IrSubmitError {
    IrSubmitError::ShaderCompile(error.0)
}

/// Use the complete pipeline layout for every stage, so separately compiled
/// modules cannot assign different resources to the same native slot. The map
/// also forms part of the context-local shader cache key; compiler version and
/// Mesa revision are fixed for the lifetime of this process.
#[cfg(feature = "programmable")]
fn pipeline_binding_map(
    layout: &ir::PipelineLayoutDesc,
) -> Result<sgfx_shader_ir3::BindingMap, IrSubmitError> {
    use sgfx_shader_ir3::{BindingAssignment, BindingMap, ResourceKind as K};
    let mut assignments = Vec::new();
    let mut next_slot = [0u32; 4];
    for (group, layout) in layout.bind_groups().iter().enumerate() {
        let mut entries: Vec<_> = layout.entries().iter().collect();
        entries.sort_unstable_by_key(|entry| entry.binding());
        for entry in entries {
            let (kind, namespace) = match entry.ty() {
                ir::BindingType::UniformBuffer => (K::UniformBuffer, 0),
                ir::BindingType::StorageBuffer { .. } => (K::StorageBuffer, 1),
                ir::BindingType::SampledTexture
                | ir::BindingType::SampledTextureView { .. }
                | ir::BindingType::StorageTexture { .. } => (K::Texture, 2),
                ir::BindingType::Sampler | ir::BindingType::ComparisonSampler => (K::Sampler, 3),
            };
            assignments
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            assignments.push(BindingAssignment {
                group: group as u32,
                binding: entry.binding(),
                kind,
                slot: next_slot[namespace],
            });
            next_slot[namespace] += 1;
        }
    }
    BindingMap::new(assignments).map_err(compiler_error)
}

#[cfg(feature = "programmable")]
pub(crate) fn validate_interface(
    shader: &sgfx_shader_ir3::CompiledShader,
    layout: &ir::PipelineLayoutDesc,
) -> Result<(), IrSubmitError> {
    use sgfx_shader_ir3::{ResourceKind as K, Stage};
    let stage = match shader.stage {
        Stage::Vertex => ir::ShaderStages::VERTEX,
        Stage::Fragment => ir::ShaderStages::FRAGMENT,
        Stage::Compute => ir::ShaderStages::COMPUTE,
    };
    for binding in &shader.bindings {
        let entry = layout
            .bind_groups()
            .get(binding.group as usize)
            .and_then(|group| {
                group
                    .entries()
                    .iter()
                    .find(|entry| entry.binding() == binding.binding)
            })
            .ok_or(ir::Error::BindingLayoutMismatch)?;
        if !entry.visibility().contains(stage) {
            return Err(ir::Error::BindingLayoutMismatch.into());
        }
        let compatible = match (binding.kind, entry.ty()) {
            (K::UniformBuffer, ir::BindingType::UniformBuffer) => true,
            (K::StorageBuffer, ir::BindingType::StorageBuffer { read_only }) => {
                !binding.writable || !read_only
            }
            (
                K::Texture,
                ir::BindingType::SampledTexture | ir::BindingType::SampledTextureView { .. },
            ) => !binding.writable,
            (K::Texture, ir::BindingType::StorageTexture { .. }) => binding.writable,
            (K::Sampler, ir::BindingType::Sampler | ir::BindingType::ComparisonSampler) => true,
            _ => false,
        };
        if !compatible {
            return Err(ir::Error::BindingLayoutMismatch.into());
        }
    }
    // The resource kind alone cannot distinguish color/depth, dimensions, or
    // comparison samplers. Validate selected-entry reflection before admission;
    // the IR bind-group validator then checks each actual view against this layout.
    for binding in shader
        .sampled_bindings()
        .map_err(compiler_error)?
        .ok_or_else(unsupported)?
    {
        use sgfx_shader_ir3::{
            SampledBindingType as S, TextureDimension as D, TextureSampleType as T,
        };
        let entry = layout
            .bind_groups()
            .get(binding.group as usize)
            .and_then(|group| {
                group
                    .entries()
                    .iter()
                    .find(|e| e.binding() == binding.binding)
            })
            .ok_or(ir::Error::BindingLayoutMismatch)?;
        let expected = match binding.ty {
            S::StorageTextureRgba8WriteOnly => ir::BindingType::StorageTexture {
                format: ir::TextureFormat::Rgba8Unorm,
                access: ir::StorageTextureAccess::WriteOnly,
            },
            S::Sampler { comparison: false } => ir::BindingType::Sampler,
            S::Sampler { comparison: true } => ir::BindingType::ComparisonSampler,
            S::Texture {
                dimension,
                arrayed,
                sample_type,
                multisampled: false,
            } => {
                let dimension = match (dimension, arrayed) {
                    (D::D1, false) => ir::TextureViewDimension::D1,
                    (D::D1, true) => ir::TextureViewDimension::D1Array,
                    (D::D2, false) => ir::TextureViewDimension::D2,
                    (D::D2, true) => ir::TextureViewDimension::D2Array,
                    (D::Cube, false) => ir::TextureViewDimension::Cube,
                    _ => return Err(unsupported()),
                };
                let depth = match sample_type {
                    T::Depth => true,
                    T::Float => false,
                    _ => return Err(unsupported()),
                };
                ir::BindingType::SampledTextureView { dimension, depth }
            }
            _ => return Err(unsupported()),
        };
        let legacy_color = entry.ty() == ir::BindingType::SampledTexture
            && expected
                == ir::BindingType::SampledTextureView {
                    dimension: ir::TextureViewDimension::D2,
                    depth: false,
                };
        if !entry.visibility().contains(stage) || (entry.ty() != expected && !legacy_color) {
            return Err(ir::Error::BindingLayoutMismatch.into());
        }
    }
    if shader.push_constant_bytes != 0
        && !layout.push_constant_ranges().iter().any(|range| {
            range.stages().contains(stage)
                && range.offset() == 0
                && range.size() >= shader.push_constant_bytes
        })
    {
        return Err(ir::Error::BindingLayoutMismatch.into());
    }
    Ok(())
}

#[cfg(all(test, feature = "programmable"))]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn separate_stage_modules_share_pipeline_slots_and_layout_identity() {
        let layout = ir::PipelineLayoutDesc::new(vec![
            ir::BindGroupLayoutDesc::new(vec![
                ir::BindGroupLayoutEntry::new(
                    9,
                    ir::ShaderStages::VERTEX,
                    ir::BindingType::StorageBuffer { read_only: true },
                ),
                ir::BindGroupLayoutEntry::new(
                    17,
                    ir::ShaderStages::FRAGMENT,
                    ir::BindingType::StorageBuffer { read_only: true },
                ),
            ])
            .unwrap(),
        ])
        .unwrap();
        let map = pipeline_binding_map(&layout).unwrap();
        let vertex = sgfx_shader_ir3::compile_with_bindings(
            sgfx_shader_ir3::Source::Wgsl("@group(0) @binding(9) var<storage,read> positions:array<vec4<f32>>; @vertex fn vs(@builtin(vertex_index) i:u32)->@builtin(position) vec4<f32> { return positions[i]; }"),
            sgfx_shader_ir3::Stage::Vertex, "vs", &map).unwrap();
        let fragment = sgfx_shader_ir3::compile_with_bindings(
            sgfx_shader_ir3::Source::Wgsl("@group(0) @binding(17) var<storage,read> colors:array<vec4<f32>>; @fragment fn fs()->@location(0) vec4<f32> { return colors[0]; }"),
            sgfx_shader_ir3::Stage::Fragment, "fs", &map).unwrap();
        assert_eq!(vertex.bindings[0].slot, 0);
        assert_eq!(fragment.bindings[0].slot, 1);
        validate_interface(&vertex, &layout).unwrap();
        validate_interface(&fragment, &layout).unwrap();
        let fragment_only = ir::PipelineLayoutDesc::new(vec![
            ir::BindGroupLayoutDesc::new(vec![ir::BindGroupLayoutEntry::new(
                17,
                ir::ShaderStages::FRAGMENT,
                ir::BindingType::StorageBuffer { read_only: true },
            )])
            .unwrap(),
        ])
        .unwrap();
        let other_map = pipeline_binding_map(&fragment_only).unwrap();
        assert_ne!(map, other_map);
        assert_eq!(other_map.assignments()[0].slot, 0);
    }

    #[test]
    fn native_interface_rejects_writable_storage_with_readonly_or_hidden_layout() {
        let shader = sgfx_shader_ir3::compile(sgfx_shader_ir3::Source::Wgsl(
            "@group(1) @binding(7) var<storage,read_write> data:array<u32>; @compute @workgroup_size(1) fn cs() { data[0]=73u; }"
        ), sgfx_shader_ir3::Stage::Compute, "cs").unwrap();
        let make = |visibility, read_only| {
            ir::PipelineLayoutDesc::new(vec![
                ir::BindGroupLayoutDesc::new(vec![]).unwrap(),
                ir::BindGroupLayoutDesc::new(vec![ir::BindGroupLayoutEntry::new(
                    7,
                    visibility,
                    ir::BindingType::StorageBuffer { read_only },
                )])
                .unwrap(),
            ])
            .unwrap()
        };
        assert!(validate_interface(&shader, &make(ir::ShaderStages::COMPUTE, false)).is_ok());
        assert!(validate_interface(&shader, &make(ir::ShaderStages::COMPUTE, true)).is_err());
        assert!(validate_interface(&shader, &make(ir::ShaderStages::VERTEX, false)).is_err());
        assert!(
            validate_interface(&shader, &ir::PipelineLayoutDesc::new(vec![]).unwrap()).is_err()
        );
    }

    #[test]
    fn storage_image_interface_requires_write_only_rgba8_and_correct_stage() {
        let shader = sgfx_shader_ir3::compile(sgfx_shader_ir3::Source::Wgsl(
            "@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>; @compute @workgroup_size(1) fn main(){textureStore(image,vec2<i32>(0),vec4<f32>(1.0));}"
        ), sgfx_shader_ir3::Stage::Compute, "main").unwrap();
        let layout = |ty, stage| ir::PipelineLayoutDesc::new(vec![ir::BindGroupLayoutDesc::new(vec![ir::BindGroupLayoutEntry::new(0,stage,ty)]).unwrap()]).unwrap();
        let storage = ir::BindingType::StorageTexture { format:ir::TextureFormat::Rgba8Unorm, access:ir::StorageTextureAccess::WriteOnly };
        assert!(validate_interface(&shader, &layout(storage, ir::ShaderStages::COMPUTE)).is_ok());
        assert!(validate_interface(&shader, &layout(ir::BindingType::SampledTexture, ir::ShaderStages::COMPUTE)).is_err());
        assert!(validate_interface(&shader, &layout(storage, ir::ShaderStages::FRAGMENT)).is_err());
    }

    #[test]
    fn depth_comparison_interface_rejects_color_array_and_noncomparison_layouts() {
        let shader = sgfx_shader_ir3::compile(sgfx_shader_ir3::Source::Wgsl(
            "@group(0) @binding(0) var depth:texture_depth_2d; @group(0) @binding(1) var cmp:sampler_comparison; @fragment fn fs()->@location(0) vec4<f32> { return vec4<f32>(textureSampleCompare(depth,cmp,vec2<f32>(0.5),0.25)); }"
        ), sgfx_shader_ir3::Stage::Fragment, "fs").unwrap();
        let layout = |dimension, depth, sampler| {
            ir::PipelineLayoutDesc::new(vec![
                ir::BindGroupLayoutDesc::new(vec![
                    ir::BindGroupLayoutEntry::new(
                        0,
                        ir::ShaderStages::FRAGMENT,
                        ir::BindingType::SampledTextureView { dimension, depth },
                    ),
                    ir::BindGroupLayoutEntry::new(1, ir::ShaderStages::FRAGMENT, sampler),
                ])
                .unwrap(),
            ])
            .unwrap()
        };
        assert!(
            validate_interface(
                &shader,
                &layout(
                    ir::TextureViewDimension::D2,
                    true,
                    ir::BindingType::ComparisonSampler
                )
            )
            .is_ok()
        );
        assert!(
            validate_interface(
                &shader,
                &layout(
                    ir::TextureViewDimension::D2,
                    false,
                    ir::BindingType::ComparisonSampler
                )
            )
            .is_err()
        );
        assert!(
            validate_interface(
                &shader,
                &layout(
                    ir::TextureViewDimension::D2Array,
                    true,
                    ir::BindingType::ComparisonSampler
                )
            )
            .is_err()
        );
        assert!(
            validate_interface(
                &shader,
                &layout(ir::TextureViewDimension::D2, true, ir::BindingType::Sampler)
            )
            .is_err()
        );
    }

    #[test]
    fn native_interface_requires_complete_stage_specific_push_constant_coverage() {
        let shader = sgfx_shader_ir3::compile(sgfx_shader_ir3::Source::Wgsl(
            "var<push_constant> color:vec4<f32>; @fragment fn fs()->@location(0) vec4<f32> { return color; }"
        ), sgfx_shader_ir3::Stage::Fragment, "fs").unwrap();
        let layout = |stage, size| {
            ir::PipelineLayoutDesc::new(vec![])
                .unwrap()
                .with_push_constant_ranges(vec![
                    ir::PushConstantRange::new(stage, 0, size).unwrap(),
                ])
                .unwrap()
        };
        assert!(validate_interface(&shader, &layout(ir::ShaderStages::FRAGMENT, 16)).is_ok());
        assert!(validate_interface(&shader, &layout(ir::ShaderStages::FRAGMENT, 4)).is_err());
        assert!(validate_interface(&shader, &layout(ir::ShaderStages::VERTEX, 16)).is_err());
    }
}
