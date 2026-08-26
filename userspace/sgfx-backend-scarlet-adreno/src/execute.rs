//! Full SGFX command normalization and A6xx queue submission.

use alloc::{vec, vec::Vec};

use sgfx_codegen_adreno_a6xx as codegen;

use crate::resource::ContextResources;
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, UnsupportedIrFeature, ir, wire};

const IMAGE_OBJECT_BASE: u32 = 1;
const BUFFER_OBJECT_BASE: u32 = 1 << 16;
const PIPELINE_OBJECT_BASE: u32 = 1 << 24;

#[derive(Clone, Copy)]
struct GeneratedProgress {
    offset: u64,
    expected: u32,
}

impl ContextResources {
    pub(crate) fn execute<'r, 'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), IrSubmitError> {
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(IrSubmitError::ContextMismatch);
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }

        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
        resources
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        external_bindings
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;

        for command in commands.commands() {
            match command {
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    let object =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    // CP_MEMCPY operates in dwords. Preserve the complete SGFX
                    // byte-write contract by using the coherent CPU mapping for
                    // unaligned tails, and use pure-codegen PM4 for canonical
                    // aligned uploads.
                    if *offset & 3 == 0 && data.len() & 3 == 0 {
                        operations.push(codegen::Operation::WriteBuffer {
                            destination: object,
                            offset: *offset,
                            data,
                        });
                    } else {
                        self.write_buffer(*buffer, *offset, data)?;
                    }
                }
                ir::Command::WriteTexture { texture, write } => {
                    let descriptor = self.resources.texture(*texture)?;
                    require_texture_upload_format(descriptor.format())?;
                    let destination =
                        self.ensure_image(*texture, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::WriteTexture {
                        destination,
                        area: write.destination(),
                        bytes_per_row: write.bytes_per_row(),
                        data: write.data(),
                    });
                }
                ir::Command::CopyTextureToTexture {
                    source,
                    source_rect,
                    destination,
                    destination_rect,
                } => {
                    let source =
                        self.ensure_image(*source, &mut resources, &mut external_bindings)?;
                    let destination =
                        self.ensure_image(*destination, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::CopyTextureToTexture {
                        source,
                        source_rect: *source_rect,
                        destination,
                        destination_rect: *destination_rect,
                    });
                }
                ir::Command::BeginRenderPass(pass) => {
                    let target =
                        self.ensure_image(pass.target(), &mut resources, &mut external_bindings)?;
                    let depth = if let Some(depth) = pass.depth_attachment() {
                        Some(codegen::DepthAttachment {
                            target: self.ensure_image(
                                depth.target(),
                                &mut resources,
                                &mut external_bindings,
                            )?,
                            load: depth.load(),
                            store: depth.store(),
                        })
                    } else {
                        None
                    };
                    operations.push(codegen::Operation::BeginRenderPass(codegen::RenderPass {
                        target,
                        area: pass.area(),
                        load: pass.load(),
                        store: pass.store(),
                        depth,
                    }));
                }
                ir::Command::EndRenderPass => operations.push(codegen::Operation::EndRenderPass),
                ir::Command::SetPipeline(pipeline) => {
                    let pipeline = self.ensure_pipeline(*pipeline, &mut pipelines)?;
                    operations.push(codegen::Operation::SetPipeline(pipeline));
                }
                ir::Command::SetVertexBuffer { buffer, offset } => {
                    let buffer =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetVertexBuffer {
                        buffer,
                        offset: *offset,
                    });
                }
                ir::Command::SetIndexBuffer {
                    buffer,
                    offset,
                    format,
                } => {
                    let buffer =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetIndexBuffer {
                        buffer,
                        offset: *offset,
                        format: *format,
                    });
                }
                ir::Command::SetTexture(texture) => {
                    let texture =
                        self.ensure_image(*texture, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::SetTexture(texture));
                }
                ir::Command::SetSampler(sampler) => {
                    let descriptor = self.resources.sampler(*sampler)?;
                    operations.push(codegen::Operation::SetSampler(descriptor));
                }
                ir::Command::SetUniforms(uniforms) => {
                    operations.push(codegen::Operation::SetUniforms(*uniforms));
                }
                ir::Command::SetScissor(scissor) => {
                    operations.push(codegen::Operation::SetScissor(*scissor));
                }
                ir::Command::Draw {
                    vertex_count,
                    first_vertex,
                } => operations.push(codegen::Operation::Draw {
                    vertex_count: *vertex_count,
                    first_vertex: *first_vertex,
                }),
                ir::Command::DrawIndexed {
                    index_count,
                    first_index,
                    base_vertex,
                } => operations.push(codegen::Operation::DrawIndexed {
                    index_count: *index_count,
                    first_index: *first_index,
                    base_vertex: *base_vertex,
                }),
            }
        }

        if operations.is_empty() {
            return Ok(());
        }

        let compiled = codegen::compile(codegen::CompileInput {
            capabilities: context.device.codegen_capabilities,
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })?;
        let mut bindings = external_bindings;
        let progress = self.materialize_generated(&compiled, &mut bindings)?;
        let payload = wire::encode(&compiled, &bindings)?;
        if let Err(error) = queue.submit(&payload) {
            if let Some(progress) = progress {
                match self.read_scratch_u32(progress.offset) {
                    Ok(actual) => std::println!(
                        "[a618-userspace] CCU event progress actual={:#x} expected={:#x}",
                        actual,
                        progress.expected,
                    ),
                    Err(_) => std::println!(
                        "[a618-userspace] CCU event progress unavailable expected={:#x}",
                        progress.expected,
                    ),
                }
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn ensure_image(
        &mut self,
        reference: ir::TextureRef<'_>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        let id = image_object_id(reference.slot())?;
        if metadata.iter().any(|resource| resource.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.texture(reference)?;
        let image = self.texture(reference)?;
        if image.logical_format != descriptor.format() {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        let plane_count = usize::try_from(image.layout.plane_count)
            .map_err(|_| IrSubmitError::Unsupported(UnsupportedIrFeature::ImageLayout))?;
        let mut planes = Vec::new();
        planes
            .try_reserve_exact(plane_count)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for plane in &image.layout.planes[..plane_count] {
            planes.push(codegen::PlaneLayout {
                offset: plane.offset,
                stride: plane.row_pitch,
                size: plane.size,
            });
        }
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        metadata.push(codegen::ResourceMeta {
            id,
            size: image.allocation_size(),
            kind: codegen::ResourceKind::Image(codegen::ImageMeta {
                format: descriptor.format(),
                storage_format: match descriptor.format() {
                    ir::TextureFormat::Depth32Float => ir::TextureFormat::Depth32Float,
                    ir::TextureFormat::Bgra8Unorm
                    | ir::TextureFormat::Rgba8Unorm
                    | ir::TextureFormat::R8Unorm => ir::TextureFormat::Bgra8Unorm,
                },
                extent: descriptor.extent(),
                usage: descriptor.usage(),
                modifier: codegen::ImageModifier::Linear,
                planes,
            }),
        });
        bindings
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings.push(BoundObject {
            object: codegen::ObjectRef::External(id),
            attachment_token: image.attachment_token,
            allocation_offset: 0,
            size: image.allocation_size(),
        });
        Ok(id)
    }

    fn ensure_buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        let id = buffer_object_id(reference.slot())?;
        if metadata.iter().any(|resource| resource.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.buffer(reference)?;
        let buffer = self.buffer(reference)?;
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        metadata.push(codegen::ResourceMeta {
            id,
            size: descriptor.size(),
            kind: codegen::ResourceKind::Buffer {
                usage: descriptor.usage(),
            },
        });
        bindings
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings.push(BoundObject {
            object: codegen::ObjectRef::External(id),
            attachment_token: buffer.attachment_token,
            allocation_offset: 0,
            size: descriptor.size(),
        });
        Ok(id)
    }

    fn ensure_pipeline(
        &self,
        reference: ir::RenderPipelineRef<'_>,
        metadata: &mut Vec<codegen::PipelineMeta>,
    ) -> Result<codegen::PipelineId, IrSubmitError> {
        let id = pipeline_object_id(reference.slot())?;
        if metadata.iter().any(|pipeline| pipeline.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.render_pipeline(reference)?;
        metadata
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        metadata.push(codegen::PipelineMeta { id, descriptor });
        Ok(id)
    }

    fn materialize_generated(
        &mut self,
        compiled: &codegen::RelocatablePm4,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<Option<GeneratedProgress>, IrSubmitError> {
        if compiled.generated_objects.is_empty() {
            return Ok(None);
        }
        let mut placements = Vec::new();
        placements
            .try_reserve_exact(compiled.generated_objects.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut total = 0u64;
        for generated in &compiled.generated_objects {
            if generated.bytes.is_empty()
                || generated.alignment == 0
                || !generated.alignment.is_power_of_two()
            {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ));
            }
            total = align_up(total, generated.alignment)?;
            let size =
                u64::try_from(generated.bytes.len()).map_err(|_| IrSubmitError::OutOfMemory)?;
            placements.push((generated.id, total, size));
            total = total.checked_add(size).ok_or(IrSubmitError::OutOfMemory)?;
        }
        let total_usize = usize::try_from(total).map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut upload = vec![0; total_usize];
        for (generated, (_, offset, size)) in compiled
            .generated_objects
            .iter()
            .zip(placements.iter().copied())
        {
            let start = usize::try_from(offset).map_err(|_| IrSubmitError::OutOfMemory)?;
            let size = usize::try_from(size).map_err(|_| IrSubmitError::OutOfMemory)?;
            upload[start..start + size].copy_from_slice(&generated.bytes);
        }
        let (scratch_token, scratch_size) = {
            let scratch = self.scratch(total)?;
            scratch.write(0, &upload)?;
            (scratch.attachment_token, scratch.logical_size)
        };
        let progress = compiled
            .generated_objects
            .iter()
            .zip(placements.iter())
            .find_map(|(generated, (_, offset, _))| {
                (generated.kind == codegen::GeneratedObjectKind::CcuTimestamp)
                    .then_some((generated.id, *offset))
            })
            .map(|(id, offset)| {
                let expected = compiled
                    .fixups
                    .iter()
                    .filter(|fixup| fixup.object == codegen::ObjectRef::Generated(id))
                    .count();
                u32::try_from(expected)
                    .map(|expected| GeneratedProgress { offset, expected })
                    .map_err(|_| IrSubmitError::OutOfMemory)
            })
            .transpose()?;
        bindings
            .try_reserve_exact(placements.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for (id, offset, size) in placements {
            let end = offset.checked_add(size).ok_or(IrSubmitError::OutOfMemory)?;
            if end > scratch_size {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ));
            }
            bindings.push(BoundObject {
                object: codegen::ObjectRef::Generated(id),
                attachment_token: scratch_token,
                allocation_offset: offset,
                size,
            });
        }
        Ok(progress)
    }
}

fn image_object_id(slot: usize) -> Result<codegen::ObjectId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::ObjectId::new(
        IMAGE_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

fn buffer_object_id(slot: usize) -> Result<codegen::ObjectId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::ObjectId::new(
        BUFFER_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

fn pipeline_object_id(slot: usize) -> Result<codegen::PipelineId, IrSubmitError> {
    let slot = u32::try_from(slot).map_err(|_| IrSubmitError::OutOfMemory)?;
    Ok(codegen::PipelineId::new(
        PIPELINE_OBJECT_BASE
            .checked_add(slot)
            .ok_or(IrSubmitError::OutOfMemory)?,
    ))
}

fn align_up(value: u64, alignment: u64) -> Result<u64, IrSubmitError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(IrSubmitError::OutOfMemory)
}

fn require_texture_upload_format(format: ir::TextureFormat) -> Result<(), IrSubmitError> {
    match format {
        ir::TextureFormat::Bgra8Unorm
        | ir::TextureFormat::Rgba8Unorm
        | ir::TextureFormat::R8Unorm => Ok(()),
        ir::TextureFormat::Depth32Float => Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureUpload,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::require_texture_upload_format;
    use crate::{IrSubmitError, UnsupportedIrFeature, ir};

    #[test]
    fn logical_color_and_mask_uploads_are_supported() {
        for format in [
            ir::TextureFormat::Bgra8Unorm,
            ir::TextureFormat::Rgba8Unorm,
            ir::TextureFormat::R8Unorm,
        ] {
            assert!(require_texture_upload_format(format).is_ok());
        }
        assert!(matches!(
            require_texture_upload_format(ir::TextureFormat::Depth32Float),
            Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::TextureUpload
            ))
        ));
    }
}
