//! Bounded tracked submissions with immutable per-submission upload storage.

use alloc::{sync::Arc, vec::Vec};
use gpu_raw::{GPU_ABI_VERSION, GPU_RESULT_SUCCESS, GpuQueue};
use sgfx_codegen_adreno_a6xx as codegen;
use sgfx_core::backend::SubmitError;

use crate::dispatch::Chunk;
use crate::execute::validate_fixed_command_subset;
use crate::preparation::{
    PreparedChunk, UPLOAD_ARENA_BYTES, prepare_chunk, split_async_operations,
    split_submission_operations,
};
use crate::resource::{ContextResources, RawBuffer, RawImage};
use crate::scheduler::AdmissionError;
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, Submission, UnsupportedIrFeature, ir, wire};

/// Four native chunks can use disjoint staging simultaneously; an arbitrarily
/// longer logical stream reuses them after their native fences retire.
const UPLOAD_ARENAS: usize = 4;
const MAX_LOGICAL_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct UploadArena {
    pub(crate) buffer: RawBuffer,
    pub(crate) completion: std::sync::Mutex<Option<Arc<gpu_raw::GpuCompletion>>>,
}

/// Each physical owner retains both the capability and its context attachment.
/// Keeping these snapshots in queued jobs prevents session Drop from revoking
/// authority needed by a native chunk which has not reached the kernel yet.
pub(crate) struct RetainedResources {
    _images: Vec<Arc<RawImage>>,
    _buffers: Vec<Arc<RawBuffer>>,
}

pub(crate) struct DispatchOwner {
    pub(crate) queue: Arc<GpuQueue>,
    pub(crate) _resources: Arc<RetainedResources>,
}

enum PreparedWork {
    Commands(PreparedChunk),
    WriteBuffer {
        buffer: Arc<RawBuffer>,
        offset: u64,
        data: Vec<u8>,
    },
}

impl PreparedWork {
    fn commands(&self) -> Option<&PreparedChunk> {
        match self {
            Self::Commands(chunk) => Some(chunk),
            _ => None,
        }
    }
}

impl ContextResources {
    pub(crate) fn drain_async(&self) -> Result<(), IrSubmitError> {
        self.context.dispatcher.wait_idle()?;
        Ok(())
    }

    fn prepare_upload_arenas(&mut self, prepared: &[PreparedWork]) -> Result<(), IrSubmitError> {
        let mut required = [0u64; UPLOAD_ARENAS];
        for (index, chunk) in prepared
            .iter()
            .filter_map(PreparedWork::commands)
            .enumerate()
        {
            let end = chunk
                .placements
                .iter()
                .map(|&(_, offset, size)| offset + size)
                .max()
                .unwrap_or(0);
            required[index % UPLOAD_ARENAS] = required[index % UPLOAD_ARENAS].max(end);
        }
        let count = prepared
            .iter()
            .filter_map(PreparedWork::commands)
            .count()
            .min(UPLOAD_ARENAS);
        self.async_arenas
            .try_reserve_exact(count.saturating_sub(self.async_arenas.len()))
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for (index, &required) in required[..count].iter().enumerate() {
            let size = upload_arena_size(required)?;
            if self
                .async_arenas
                .get(index)
                .is_some_and(|arena| arena.buffer.logical_size >= size)
            {
                continue;
            }
            // Queued chunks retain their old Arc when a slot grows. Replacing
            // this cache entry never overwrites in-flight bytes or drains GPU.
            let arena = Arc::new(UploadArena {
                buffer: RawBuffer::create(&self.context, size)?,
                completion: std::sync::Mutex::new(None),
            });
            if index < self.async_arenas.len() {
                self.async_arenas[index] = arena;
            } else {
                self.async_arenas.push(arena);
            }
        }
        Ok(())
    }

    pub(crate) fn submit_async<'r, 'data>(
        &mut self,
        context: &ContextInner,
        queue: &Arc<GpuQueue>,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, SubmitError<IrSubmitError, Submission>> {
        validate_fixed_command_subset(commands).map_err(SubmitError::Rejected)?;
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(SubmitError::Rejected(IrSubmitError::ContextMismatch));
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(SubmitError::Rejected(IrSubmitError::ResourceTableMismatch));
        }
        let limits = queue
            .query_async()
            .map_err(|_| SubmitError::Rejected(IrSubmitError::AsyncUnsupported))?;
        if limits.abi_version != GPU_ABI_VERSION
            || limits.result != GPU_RESULT_SUCCESS
            || limits.reserved != 0
            || limits.reserved2 != 0
            || limits.max_pending_submissions == 0
            || limits.max_opaque_command_size == 0
        {
            return Err(SubmitError::Rejected(IrSubmitError::AsyncUnsupported));
        }
        // Materialize every logical resource and validate/compile every chunk
        // before accepting work. Borrowed uploads become owned generated bytes.
        let (prepared, external_bindings) = self
            .prepare_async(context, commands, limits.max_opaque_command_size as usize)
            .map_err(SubmitError::Rejected)?;
        self.prepare_upload_arenas(&prepared)
            .map_err(SubmitError::Rejected)?;
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(prepared.len().max(1))
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        let mut index = 0;
        for work in prepared {
            let chunk = match work {
                PreparedWork::Commands(chunk) => chunk,
                PreparedWork::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    chunks.push(Chunk::WriteBuffer {
                        buffer,
                        offset,
                        data,
                    });
                    continue;
                }
            };
            let arena = Arc::clone(&self.async_arenas[index % UPLOAD_ARENAS]);
            index += 1;
            let mut bindings = Vec::new();
            bindings
                .try_reserve_exact(external_bindings.len() + chunk.placements.len())
                .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
            bindings.extend_from_slice(&external_bindings);
            for &(id, offset, size) in &chunk.placements {
                bindings.push(BoundObject {
                    object: codegen::ObjectRef::Generated(id),
                    attachment_token: arena.buffer.attachment_token,
                    allocation_offset: offset,
                    size,
                });
            }
            let commands =
                wire::encode(&chunk.compiled, &bindings).map_err(SubmitError::Rejected)?;
            let mut uploads = Vec::new();
            uploads
                .try_reserve_exact(chunk.placements.len())
                .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
            for (generated, (_, offset, _)) in chunk
                .compiled
                .generated_objects
                .into_iter()
                .zip(chunk.placements)
            {
                uploads.push((offset, generated.bytes));
            }
            chunks.push(Chunk::Commands {
                commands,
                arena,
                uploads,
            });
        }
        if chunks.is_empty() {
            chunks.push(Chunk::Barrier);
        }
        let mut images = Vec::new();
        let mut buffers = Vec::new();
        images
            .try_reserve_exact(self.images.len())
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        buffers
            .try_reserve_exact(self.buffers.len())
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        images.extend(self.images.iter().flatten().cloned());
        buffers.extend(self.buffers.iter().flatten().cloned());
        let owner = Arc::new(DispatchOwner {
            queue: Arc::clone(queue),
            _resources: Arc::new(RetainedResources {
                _images: images,
                _buffers: buffers,
            }),
        });
        let dispatcher = &self.context.dispatcher;
        match dispatcher.enqueue(owner, chunks) {
            Ok(signal) => Ok(Submission::new(signal)),
            Err(AdmissionError::Busy) => Err(SubmitError::Busy),
            Err(AdmissionError::TooLarge) => {
                Err(SubmitError::Rejected(IrSubmitError::SubmissionTooLarge))
            }
            Err(AdmissionError::OutOfMemory) => {
                Err(SubmitError::Rejected(IrSubmitError::OutOfMemory))
            }
            Err(AdmissionError::Failed(error)) => Err(SubmitError::Rejected(error.into())),
        }
    }

    fn prepare_async<'r, 'data>(
        &mut self,
        context: &ContextInner,
        commands: &ir::CommandBuffer<'r, 'data>,
        native_limit: usize,
    ) -> Result<(Vec<PreparedWork>, Vec<BoundObject>), IrSubmitError> {
        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for command in commands.commands() {
            match command {
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    let destination =
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)?;
                    operations.push(codegen::Operation::WriteBuffer {
                        destination,
                        offset: *offset,
                        data,
                    });
                }
                ir::Command::WriteTexture { texture, write } => {
                    let destination = self.ensure_image_subresource(
                        *texture,
                        write.mip_level(),
                        write.array_layer(),
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    operations.push(codegen::Operation::WriteTexture {
                        destination,
                        area: write.destination(),
                        bytes_per_row: write.bytes_per_row(),
                        data: write.data(),
                    });
                }
                ir::Command::BlitTexture {
                    source,
                    source_mip,
                    destination,
                    destination_mip,
                    source_rect,
                    destination_rect,
                    filter,
                    flips,
                } => {
                    let source = self.ensure_image_subresource(
                        *source,
                        *source_mip,
                        0,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    let destination = self.ensure_image_subresource(
                        *destination,
                        *destination_mip,
                        0,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    operations.push(codegen::Operation::BlitTexture {
                        source,
                        destination,
                        source_rect: *source_rect,
                        destination_rect: *destination_rect,
                        filter: *filter,
                        flips: *flips,
                    });
                }
                // Each transfer and render pass retires its CCUs and the next
                // begins by invalidating them. The ordered queue therefore
                // already enforces the explicit IR dependency.
                ir::Command::ResourceBarrier(_) => {}
                ir::Command::CopyTextureToTexture {
                    source,
                    source_rect,
                    destination,
                    destination_rect,
                } => {
                    let source = self.ensure_image_subresource(
                        *source,
                        0,
                        0,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    let destination = self.ensure_image_subresource(
                        *destination,
                        0,
                        0,
                        &mut resources,
                        &mut external_bindings,
                    )?;
                    operations.push(codegen::Operation::CopyTextureToTexture {
                        source,
                        source_rect: *source_rect,
                        destination,
                        destination_rect: *destination_rect,
                    });
                }
                ir::Command::BeginRenderPass(pass) => {
                    let target = self.ensure_image_subresource(
                        pass.target(),
                        0,
                        0,
                        &mut resources,
                        &mut external_bindings,
                    )?;
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
                ir::Command::SetViewport(viewport) => {
                    operations.push(codegen::Operation::SetViewport(*viewport));
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
                // The published 1.0 core has only fixed commands; development
                // cores also carry programmable operations, rejected preflight.
                #[allow(unreachable_patterns)]
                _ => {
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ProgrammableExecution,
                    ));
                }
            }
        }

        let mut pending = split_async_operations(&operations)?;
        pending.reverse();
        let mut chunks = Vec::new();
        let mut owned_bytes = 0usize;
        while let Some(operations) = pending.pop() {
            if let [
                codegen::Operation::WriteBuffer {
                    destination,
                    offset,
                    data,
                },
            ] = operations.as_slice()
            {
                let resource = resources
                    .iter()
                    .find(|resource| resource.id == *destination)
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                let binding = external_bindings
                    .iter()
                    .find(|binding| binding.object == codegen::ObjectRef::External(*destination))
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                let buffer = self
                    .buffers
                    .iter()
                    .flatten()
                    .find(|buffer| buffer.attachment_token == binding.attachment_token)
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                let data = own_buffer_write(resource, *offset, data)?;
                owned_bytes = owned_bytes
                    .checked_add(data.len())
                    .filter(|bytes| *bytes <= MAX_LOGICAL_BYTES)
                    .ok_or(IrSubmitError::SubmissionTooLarge)?;
                chunks
                    .try_reserve(1)
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                chunks.push(PreparedWork::WriteBuffer {
                    buffer: Arc::clone(buffer),
                    offset: *offset,
                    data,
                });
                continue;
            }
            let result = prepare_chunk(
                context.device.codegen_capabilities,
                &resources,
                &pipelines,
                &operations,
                &external_bindings,
                0,
                native_limit,
            );
            match result {
                Ok((chunk, end)) => {
                    owned_bytes = owned_bytes
                        .checked_add(end as usize)
                        .and_then(|bytes| bytes.checked_add(chunk.compiled.words.len() * 4))
                        .filter(|bytes| *bytes <= MAX_LOGICAL_BYTES)
                        .ok_or(IrSubmitError::SubmissionTooLarge)?;
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(PreparedWork::Commands(chunk));
                }
                Err(
                    error @ (IrSubmitError::SubmissionTooLarge
                    | IrSubmitError::Codegen(codegen::CompileError::CommandBudgetExceeded)),
                ) => {
                    let draws = operations
                        .iter()
                        .filter(|operation| {
                            matches!(
                                operation,
                                codegen::Operation::Draw { .. }
                                    | codegen::Operation::DrawIndexed { .. }
                            )
                        })
                        .count();
                    if draws <= 1 {
                        return Err(error);
                    }
                    let retry = split_submission_operations(&operations, (draws / 2).max(1))?;
                    if retry.len() <= 1 {
                        return Err(error);
                    }

                    pending
                        .try_reserve(retry.len())
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    pending.extend(retry.into_iter().rev());
                }
                Err(error) => return Err(error),
            }
        }
        Ok((chunks, external_bindings))
    }
}

/// Validate before admission, then retain only the caller's written range.
fn own_buffer_write(
    resource: &codegen::ResourceMeta,
    offset: u64,
    data: &[u8],
) -> Result<Vec<u8>, IrSubmitError> {
    let codegen::ResourceKind::Buffer { usage } = resource.kind else {
        return Err(codegen::CompileError::InvalidResource.into());
    };
    if !usage.contains(ir::BufferUsage::COPY_DST) {
        return Err(codegen::CompileError::InvalidResource.into());
    }
    if data.is_empty() || data.len() % 4 != 0 || offset % 4 != 0 {
        return Err(codegen::CompileError::UnsupportedFeature.into());
    }
    if offset
        .checked_add(data.len() as u64)
        .is_none_or(|end| end > resource.size)
    {
        return Err(codegen::CompileError::OutOfBounds.into());
    }
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(data.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    owned.extend_from_slice(data);
    Ok(owned)
}

fn upload_arena_size(required: u64) -> Result<u64, IrSubmitError> {
    required
        .max(64 * 1024)
        .checked_next_power_of_two()
        .filter(|&size| size <= UPLOAD_ARENA_BYTES)
        .ok_or(IrSubmitError::SubmissionTooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_write_owns_only_the_written_range_and_preserves_validation() {
        let resource = codegen::ResourceMeta {
            id: codegen::ObjectId::new(1),
            size: 8 * 1024 * 1024,
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::COPY_DST,
            },
        };
        let mut input = alloc::vec![7; 256 * 1024];
        let retained = own_buffer_write(&resource, 1024, &input).unwrap();
        input.fill(0);
        assert_eq!(retained.len(), 256 * 1024);
        assert!(retained.iter().all(|&byte| byte == 7));
        assert!(own_buffer_write(&resource, resource.size - 4, &[1; 8]).is_err());
        assert!(own_buffer_write(&resource, u64::MAX - 3, &[1; 8]).is_err());
        assert!(own_buffer_write(&resource, 1, &[1; 4]).is_err());
        assert!(own_buffer_write(&resource, 0, &[1; 3]).is_err());
        assert!(own_buffer_write(&resource, 0, &[]).is_err());
        let readonly = codegen::ResourceMeta {
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::VERTEX,
            },
            ..resource
        };
        assert!(own_buffer_write(&readonly, 0, &[1; 4]).is_err());
    }

    #[test]
    fn arenas_grow_with_actual_uploads_and_remain_bounded() {
        assert_eq!(upload_arena_size(0).unwrap(), 64 * 1024);
        assert_eq!(upload_arena_size(80).unwrap(), 64 * 1024);
        assert_eq!(upload_arena_size(65_537).unwrap(), 128 * 1024);
        assert_eq!(
            upload_arena_size(UPLOAD_ARENA_BYTES).unwrap(),
            UPLOAD_ARENA_BYTES
        );
        assert!(upload_arena_size(UPLOAD_ARENA_BYTES + 1).is_err());
        assert!(upload_arena_size(u64::MAX).is_err());
    }
}
