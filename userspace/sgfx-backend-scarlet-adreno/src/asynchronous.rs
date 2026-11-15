//! Bounded tracked submissions with immutable per-submission upload storage.

use alloc::{sync::Arc, vec::Vec};
use gpu_raw::GpuQueue;
use sgfx_codegen_adreno_a6xx as codegen;
use sgfx_core::backend::SubmitError;

use crate::dispatch::Chunk;
use crate::execute::validate_fixed_command_subset;
use crate::preparation::{
    AsyncOperationBatch, ChunkBindings, NormalizationMemo, PreparedChunk, UPLOAD_ARENA_BYTES,
    coalesce_async_operations, prepare_chunk,
    split_async_operations, split_submission_operations,
};
use crate::resource::{ContextResources, RawBuffer, RawImage};
use crate::scheduler::AdmissionError;
#[cfg(feature = "profile-submissions")]
use crate::submission_profile::{Count, Outcome, Phase, Profile};
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, Submission, ir, wire};
#[cfg(not(feature = "programmable"))]
use crate::UnsupportedIrFeature;

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
            let slot = (self.next_async_arena + index) % UPLOAD_ARENAS;
            required[slot] = required[slot].max(end);
        }
        // Keep slots across logical submissions. Most frames contain one
        // native chunk; restarting at slot zero serialized all those frames.
        let count = if prepared.iter().any(|work| work.commands().is_some()) {
            UPLOAD_ARENAS
        } else {
            0
        };
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
        // Declared first so rejection cleanup and helper lock release precede
        // aggregation/output. Nothing in this profile observes GPU completion.
        #[cfg(feature = "profile-submissions")]
        let mut profile = Profile::new(commands.commands().len());
        validate_fixed_command_subset(commands).map_err(SubmitError::Rejected)?;
        if context.raw.as_handle().as_raw() != self.context_id() {
            return Err(SubmitError::Rejected(IrSubmitError::ContextMismatch));
        }
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(SubmitError::Rejected(IrSubmitError::ResourceTableMismatch));
        }
        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::QueryLimits);
        let native_limit = context
            .dispatcher
            .async_command_limit(queue)
            .map_err(SubmitError::Rejected)?;
        // Materialize every logical resource and validate/compile every chunk
        // before accepting work. Borrowed uploads become owned generated bytes.
        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::MaterializePlan);
        let (prepared, external_bindings) = self
            .prepare_async(
                context,
                commands,
                native_limit,
                #[cfg(feature = "profile-submissions")]
                &mut profile,
            )
            .map_err(SubmitError::Rejected)?;
        // Mirror planning may allocate/materialize, so finish it before logical
        // acceptance. Busy/rejection drops the plan without publishing writes.
        let publication = self
            .prepare_buffer_publication(commands)
            .map_err(SubmitError::Rejected)?;
        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::ArenaPreparation);
        self.prepare_upload_arenas(&prepared)
            .map_err(SubmitError::Rejected)?;
        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::PackChunks);
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(prepared.len().max(1))
            .map_err(|_| SubmitError::Rejected(IrSubmitError::OutOfMemory))?;
        let mut index = self.next_async_arena;
        let mut binding_scratch = ChunkBindings::new(&external_bindings);
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
            let bindings = binding_scratch
                .for_chunk(&chunk, arena.buffer.attachment_token)
                .map_err(SubmitError::Rejected)?;
            #[cfg(feature = "profile-submissions")]
            profile.enter(Phase::WireEncode);
            let commands =
                wire::encode(&chunk.compiled, bindings).map_err(SubmitError::Rejected)?;
            #[cfg(feature = "profile-submissions")]
            {
                profile.enter(Phase::PackChunks);
                profile.count(Count::WireBytes, commands.len());
            }
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
            #[cfg(feature = "profile-submissions")]
            profile.count(Count::BarrierChunks, 1);
        }
        #[cfg(feature = "profile-submissions")]
        {
            // Includes CPU writes and the empty-stream barrier, not just PM4.
            profile.count(Count::Chunks, chunks.len());
            profile.enter(Phase::ResourceRetention);
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
        self.programs
            .retain_buffers(&external_bindings, &mut buffers)
            .map_err(SubmitError::Rejected)?;
        #[cfg(feature = "programmable")]
        self.fixed.retain(&external_bindings,&mut buffers).map_err(SubmitError::Rejected)?;
        let owner = Arc::new(DispatchOwner {
            queue: Arc::clone(queue),
            _resources: Arc::new(RetainedResources {
                _images: images,
                _buffers: buffers,
            }),
        });
        let dispatcher = &self.context.dispatcher;
        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::Enqueue);
        match dispatcher.enqueue(owner, chunks) {
            Ok(signal) => {
                #[cfg(feature = "profile-submissions")]
                profile.outcome(Outcome::Accepted);
                self.next_async_arena = index % UPLOAD_ARENAS;
                let receipt = Submission::new(signal);
                self.publish_buffer_submission(publication, &receipt)
                    .map_err(|error| SubmitError::Failed {
                        error,
                        completion: receipt.clone(),
                    })?;
                Ok(receipt)
            }
            Err(AdmissionError::Busy) => {
                #[cfg(feature = "profile-submissions")]
                profile.outcome(Outcome::Busy);
                Err(SubmitError::Busy)
            }
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
        #[cfg(feature = "profile-submissions")] profile: &mut Profile,
    ) -> Result<(Vec<PreparedWork>, Vec<BoundObject>), IrSubmitError> {
        let mut resources = Vec::new();
        let mut pipelines = Vec::new();
        let mut operations = Vec::new();
        let mut external_bindings = Vec::new();
        // The command list cannot retire/rebind logical resources while being
        // normalized. These memos are scoped to this single metadata table.
        let mut buffer_memo = NormalizationMemo::new();
        let mut image_memo = NormalizationMemo::new();
        let mut subresource_memo = NormalizationMemo::new();
        let mut pipeline_memo = NormalizationMemo::new();
        let mut sampler_memo = NormalizationMemo::new();
        operations
            .try_reserve(commands.commands().len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        for command in commands.commands() {
            match command {
                ir::Command::CopyBufferToBuffer {
                    source,
                    source_offset,
                    destination,
                    destination_offset,
                    size,
                } => {
                    let source = buffer_memo.resolve(source.slot(), source.id(), || {
                        self.resources.buffer(*source)?;
                        self.ensure_buffer(*source, &mut resources, &mut external_bindings)
                    })?;
                    let destination =
                        buffer_memo.resolve(destination.slot(), destination.id(), || {
                            self.resources.buffer(*destination)?;
                            self.ensure_buffer(*destination, &mut resources, &mut external_bindings)
                        })?;
                    operations.push(codegen::Operation::CopyBufferToBuffer {
                        source,
                        source_offset: *source_offset,
                        destination,
                        destination_offset: *destination_offset,
                        size: *size,
                    });
                }
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    let destination = buffer_memo.resolve(buffer.slot(), buffer.id(), || {
                        self.resources.buffer(*buffer)?;
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)
                    })?;
                    operations.push(codegen::Operation::WriteBuffer {
                        destination,
                        offset: *offset,
                        data,
                    });
                }
                ir::Command::WriteTexture { texture, write } => {
                    let destination = subresource_memo.resolve(
                        texture.slot(),
                        (texture.id(), write.mip_level(), write.array_layer()),
                        || {
                            self.ensure_image_subresource(
                                *texture,
                                write.mip_level(),
                                write.array_layer(),
                                &mut resources,
                                &mut external_bindings,
                            )
                        },
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
                    let source = subresource_memo.resolve(
                        source.slot(),
                        (source.id(), *source_mip, 0),
                        || {
                            self.ensure_image_subresource(
                                *source,
                                *source_mip,
                                0,
                                &mut resources,
                                &mut external_bindings,
                            )
                        },
                    )?;
                    let destination = subresource_memo.resolve(
                        destination.slot(),
                        (destination.id(), *destination_mip, 0),
                        || {
                            self.ensure_image_subresource(
                                *destination,
                                *destination_mip,
                                0,
                                &mut resources,
                                &mut external_bindings,
                            )
                        },
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
                // Flush both CCUs and order CP writes, including global
                // barriers introduced by the current SGFX wire format.
                ir::Command::ResourceBarrier(_) => {
                    operations.push(codegen::Operation::ResourceBarrier)
                }
                ir::Command::CopyTextureToTexture {
                    source,
                    source_rect,
                    destination,
                    destination_rect,
                } => {
                    let source =
                        subresource_memo.resolve(source.slot(), (source.id(), 0, 0), || {
                            self.ensure_image_subresource(
                                *source,
                                0,
                                0,
                                &mut resources,
                                &mut external_bindings,
                            )
                        })?;
                    let destination = subresource_memo.resolve(
                        destination.slot(),
                        (destination.id(), 0, 0),
                        || {
                            self.ensure_image_subresource(
                                *destination,
                                0,
                                0,
                                &mut resources,
                                &mut external_bindings,
                            )
                        },
                    )?;
                    operations.push(codegen::Operation::CopyTextureToTexture {
                        source,
                        source_rect: *source_rect,
                        destination,
                        destination_rect: *destination_rect,
                    });
                }
                ir::Command::BeginRenderPass(pass) => {
                    operations.push(self.normalize_render_pass(*pass, &mut resources, &mut external_bindings)?);
                }
                ir::Command::EndRenderPass => operations.push(codegen::Operation::EndRenderPass),
                ir::Command::SetPipeline(pipeline) => {
                    let pipeline = pipeline_memo.resolve(pipeline.slot(), pipeline.id(), || {
                        self.ensure_pipeline(*pipeline, &mut pipelines)
                    })?;
                    operations.push(codegen::Operation::SetPipeline(pipeline));
                }
                ir::Command::SetVertexBuffer { buffer, offset } => {
                    let buffer = buffer_memo.resolve(buffer.slot(), buffer.id(), || {
                        self.resources.buffer(*buffer)?;
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)
                    })?;
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
                    let buffer = buffer_memo.resolve(buffer.slot(), buffer.id(), || {
                        self.resources.buffer(*buffer)?;
                        self.ensure_buffer(*buffer, &mut resources, &mut external_bindings)
                    })?;
                    operations.push(codegen::Operation::SetIndexBuffer {
                        buffer,
                        offset: *offset,
                        format: *format,
                    });
                }
                ir::Command::SetTexture(texture) => {
                    let texture = image_memo.resolve(texture.slot(), texture.id(), || {
                        self.resources.texture(*texture)?;
                        self.ensure_image(*texture, &mut resources, &mut external_bindings)
                    })?;
                    operations.push(codegen::Operation::SetTexture(texture));
                }
                ir::Command::SetSampler(sampler) => {
                    let descriptor = sampler_memo.resolve(sampler.slot(), sampler.id(), || {
                        self.resources.sampler(*sampler)
                    })?;
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
                    #[cfg(feature = "programmable")]
                    operations.push(self.programmable_operation(
                        command,
                        &mut resources,
                        &mut external_bindings,
                    )?);
                    #[cfg(not(feature = "programmable"))]
                    return Err(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ProgrammableExecution,
                    ));
                }
            }
        }

        #[cfg(feature = "profile-submissions")]
        profile.enter(Phase::Split);
        #[cfg(feature = "programmable")]
        {self.lower_programmable_nv12(&mut resources,&mut external_bindings,&mut operations)?;
        self.lower_fixed(&mut pipelines,&mut resources,&mut external_bindings,&mut operations)?;}
        let mut pending = coalesce_async_operations(split_async_operations(&operations)?)?;
        pending.reverse();
        let mut chunks = Vec::new();
        let mut owned_bytes = 0usize;
        while let Some(AsyncOperationBatch { operations, fallback }) = pending.pop() {
            if let [
                codegen::Operation::WriteBuffer {
                    destination,
                    offset,
                    data,
                },
            ] = operations.as_slice()
                && !buffer_write_uses_staging(*offset, data.len())
            {
                // CP_MEMCPY addresses dwords. Keep legal byte writes on the
                // ordered CPU fallback; never round a write into adjacent data.
                #[cfg(feature = "profile-submissions")]
                profile.enter(Phase::UploadOwnership);
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
                #[cfg(feature = "profile-submissions")]
                {
                    profile.enter(Phase::Split);
                    profile.count(Count::BufferChunks, 1);
                    profile.count(Count::BufferUploadBytes, data.len());
                }
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
            // Aligned WriteBuffer uses the existing generated-upload compiler
            // path below. It owns the borrowed bytes once, then packs them into
            // a Commands chunk with an arena protected by its native receipt.
            // Unlike a mapped write it does not drain earlier native receipts.
            #[cfg(feature = "profile-submissions")]
            {
                profile.count(Count::CompileAttempts, 1);
                profile.enter(Phase::Pm4CompilePrepare);
            }
            let result = prepare_chunk(
                context.device.codegen_capabilities,
                &resources,
                &pipelines,
                &operations,
                &external_bindings,
                0,
                native_limit,
            ).and_then(|(chunk, end)| {
                if !fallback.is_empty() {
                    // A merged stream must pass the complete binding/relocation
                    // encode before discarding its original boundaries. The
                    // real arena token is substituted during final packing.
                    let mut bindings = ChunkBindings::new(&external_bindings);
                    wire::encode(&chunk.compiled, bindings.for_chunk(&chunk, 1)?)?;
                }
                Ok((chunk, end))
            });
            #[cfg(feature = "profile-submissions")]
            profile.enter(Phase::Split);
            match result {
                Ok((chunk, end)) => {
                    #[cfg(feature = "profile-submissions")]
                    {
                        profile.count(Count::Pm4Chunks, 1);
                        profile.count(Count::Pm4Words, chunk.compiled.words.len());
                        for generated in &chunk.compiled.generated_objects {
                            profile.count(Count::GeneratedUploadBytes, generated.bytes.len());
                        }
                    }
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
                Err(_) if !fallback.is_empty() => {
                    // Only a speculative preflight failed. Recompile ORIGINAL
                    // boundaries before logical acceptance; never replay GPU work.
                    pending.try_reserve(fallback.len()).map_err(|_| IrSubmitError::OutOfMemory)?;
                    pending.extend(fallback.into_iter().rev().map(AsyncOperationBatch::single));
                }
                Err(
                    error @ (IrSubmitError::SubmissionTooLarge
                    | IrSubmitError::Codegen(codegen::CompileError::CommandBudgetExceeded)),
                ) => {
                    let draws = operations
                        .iter()
                        .filter(|operation| crate::preparation::is_draw(operation))
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
                    pending.extend(retry.into_iter().rev().map(AsyncOperationBatch::single));
                }
                Err(error) => return Err(error),
            }
        }
        Ok((chunks, external_bindings))
    }
}

/// The GPU copy instruction is dword-addressed. Byte writes remain legal IR,
/// but must use the coherent CPU path after prior GPU readers have retired.
fn buffer_write_uses_staging(offset: u64, length: usize) -> bool {
    length != 0 && offset % 4 == 0 && length % 4 == 0
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
    if data.is_empty() {
        return Err(codegen::CompileError::UnsupportedFeatureReason("empty buffer upload").into());
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
        assert_eq!(own_buffer_write(&resource, 1, &[1; 4]).unwrap(), [1; 4]);
        assert_eq!(own_buffer_write(&resource, 0, &[1; 3]).unwrap(), [1; 3]);
        assert!(own_buffer_write(&resource, 0, &[]).is_err());
        let readonly = codegen::ResourceMeta {
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::VERTEX,
            },
            ..resource
        };
        assert!(own_buffer_write(&readonly, 0, &[1; 4]).is_err());
    }

    fn resource() -> codegen::ResourceMeta {
        codegen::ResourceMeta {
            id: codegen::ObjectId::new(1),
            size: 8 * 1024 * 1024,
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::COPY_DST,
            },
        }
    }

    fn staged(offset: u64, data: &[u8]) -> PreparedChunk {
        assert!(buffer_write_uses_staging(offset, data.len()));
        let resource = resource();
        prepare_chunk(
            codegen::Capabilities::a618(512 * 1024, 64 * 1024),
            core::slice::from_ref(&resource),
            &[],
            &[codegen::Operation::WriteBuffer {
                destination: resource.id,
                offset,
                data,
            }],
            &[],
            0,
            adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE,
        )
        .unwrap()
        .0
    }

    fn encode_staged(chunk: &PreparedChunk) -> Vec<u8> {
        let resource = resource();
        let mut bindings = alloc::vec![BoundObject {
            object: codegen::ObjectRef::External(resource.id),
            attachment_token: 1,
            allocation_offset: 0,
            size: resource.size,
        }];
        for &(id, offset, size) in &chunk.placements {
            bindings.push(BoundObject {
                object: codegen::ObjectRef::Generated(id),
                attachment_token: 2,
                allocation_offset: offset,
                size,
            });
        }
        wire::encode(&chunk.compiled, &bindings).unwrap()
    }

    #[test]
    fn staging_owns_upload_and_preserves_exact_copy_ranges() {
        // Exercise more than one hardware CP_MEMCPY packet, with a nonzero
        // destination offset. The caller may release/reuse its bytes immediately.
        let mut input: Vec<u8> = (0..1032).map(|i| (i % 251) as u8).collect();
        let expected = input.clone();
        let chunk = staged(12, &input);
        input.fill(0);
        drop(input);
        assert_eq!(chunk.compiled.generated_objects.len(), 1);
        assert_eq!(chunk.compiled.generated_objects[0].bytes, expected);
        assert!(!encode_staged(&chunk).is_empty());
        let mut copied = 0;
        for addresses in chunk.compiled.fixups.chunks_exact(2) {
            let (source, destination) = (&addresses[0], &addresses[1]);
            assert_eq!(
                source.object,
                codegen::ObjectRef::Generated(chunk.compiled.generated_objects[0].id)
            );
            assert_eq!(source.object_offset, copied);
            assert_eq!(
                destination.object,
                codegen::ObjectRef::External(resource().id)
            );
            assert_eq!(destination.object_offset, 12 + copied);
            assert_eq!(source.required_size, destination.required_size);
            assert_eq!(source.access, codegen::Access::READ);
            assert_eq!(destination.access, codegen::Access::WRITE);
            copied += source.required_size;
        }
        assert_eq!(copied, expected.len() as u64);
    }

    #[test]
    fn legal_byte_writes_use_cpu_without_widening_neighbor_ranges() {
        let resource = resource();
        for (offset, length) in [(1, 4), (4, 3), (3, 5), (resource.size - 1, 1)] {
            assert!(!buffer_write_uses_staging(offset, length));
            let data = alloc::vec![29; length];
            assert_eq!(own_buffer_write(&resource, offset, &data).unwrap(), data);
        }
        assert!(buffer_write_uses_staging(4, 4));
        assert!(!buffer_write_uses_staging(0, 0));
        assert!(own_buffer_write(&resource, resource.size - 1, &[1, 2]).is_err());
        assert!(own_buffer_write(&resource, u64::MAX, &[1, 2]).is_err());
        // SGFX permits byte-granular writes, independently of the CP packet ABI.
        let table = ir::ResourceTable::new();
        let buffer = table
            .define_buffer(ir::BufferDesc::new(24, ir::BufferUsage::COPY_DST).unwrap())
            .unwrap();
        let mut encoder = ir::CommandEncoder::new(&table);
        encoder.write_buffer(buffer, 1, &[7, 8, 9]).unwrap();
        encoder.finish().unwrap();
    }

    // Drive the production scheduler with delayed native fences. GPU copies
    // are interpreted from production symbolic addresses; no Scarlet syscalls
    // or real GPU execution are substituted by this host ordering test.
    #[test]
    fn many_owned_writes_reuse_four_arenas_without_overtaking_or_early_release() {
        use crate::scheduler::{DispatchError, Scheduler, Transport};
        use alloc::{collections::VecDeque, rc::Rc};
        use core::cell::{Cell, RefCell};
        type Fence = Rc<Cell<bool>>;
        type Signal = Rc<Cell<Option<Result<(), u32>>>>;
        enum Work {
            Gpu { slot: usize, chunk: PreparedChunk },
            Cpu { offset: usize, data: Vec<u8> },
            Read([u8; 24]),
        }
        enum Pending {
            Copy {
                slot: usize,
                ranges: Vec<(usize, usize, usize)>,
                fence: Fence,
            },
            Read {
                expected: [u8; 24],
                fence: Fence,
            },
        }
        struct Fake {
            arenas: RefCell<[Vec<u8>; UPLOAD_ARENAS]>,
            busy: [Cell<bool>; UPLOAD_ARENAS],
            pending: RefCell<VecDeque<Pending>>,
            target: RefCell<[u8; 24]>,
            reads: Cell<usize>,
            accepted: Cell<usize>,
        }
        impl Transport for Fake {
            type Chunk = Work;
            type Owner = Rc<()>;
            type Receipt = Fence;
            type Signal = Signal;
            type Error = u32;
            fn size(_: &Work) -> usize {
                64
            }
            fn requires_idle(work: &Work) -> bool {
                matches!(work, Work::Cpu { .. })
            }
            fn ready(&self, work: &Work) -> Result<bool, u32> {
                Ok(match work {
                    Work::Gpu { slot, .. } => !self.busy[*slot].get(),
                    _ => true,
                })
            }
            fn submit(&self, _: &Rc<()>, work: &Work) -> Result<Fence, DispatchError<u32>> {
                let fence = Rc::new(Cell::new(false));
                match work {
                    Work::Gpu { slot, chunk } => {
                        assert!(!self.busy[*slot].replace(true), "live upload overwritten");
                        self.arenas.borrow_mut()[*slot] =
                            chunk.compiled.generated_objects[0].bytes.clone();
                        let ranges = chunk
                            .compiled
                            .fixups
                            .chunks_exact(2)
                            .map(|pair| {
                                (
                                    pair[0].object_offset as usize,
                                    pair[1].object_offset as usize,
                                    pair[0].required_size as usize,
                                )
                            })
                            .collect();
                        self.pending.borrow_mut().push_back(Pending::Copy {
                            slot: *slot,
                            ranges,
                            fence: fence.clone(),
                        });
                    }
                    Work::Cpu { offset, data } => {
                        assert!(
                            self.pending.borrow().is_empty(),
                            "CPU overwrite passed an outstanding GPU reader"
                        );
                        self.target.borrow_mut()[*offset..*offset + data.len()]
                            .copy_from_slice(data);
                        fence.set(true);
                    }
                    Work::Read(expected) => self.pending.borrow_mut().push_back(Pending::Read {
                        expected: *expected,
                        fence: fence.clone(),
                    }),
                }
                self.accepted.set(self.accepted.get() + 1);
                Ok(fence)
            }
            fn poll(&self, fence: &Fence) -> Result<bool, u32> {
                Ok(fence.get())
            }
            fn complete(&self, signal: &Signal, result: Result<(), u32>) {
                signal.set(Some(result));
            }
        }
        impl Fake {
            fn retire_one(&self) {
                match self.pending.borrow_mut().pop_front().unwrap() {
                    Pending::Copy {
                        slot,
                        ranges,
                        fence,
                    } => {
                        for (source, destination, length) in ranges {
                            self.target.borrow_mut()[destination..destination + length]
                                .copy_from_slice(
                                    &self.arenas.borrow()[slot][source..source + length],
                                );
                        }
                        self.busy[slot].set(false);
                        fence.set(true);
                    }
                    Pending::Read { expected, fence } => {
                        assert_eq!(
                            *self.target.borrow(),
                            expected,
                            "later upload changed an earlier draw's data"
                        );
                        self.reads.set(self.reads.get() + 1);
                        fence.set(true);
                    }
                }
            }
        }
        // Four rounds exceed both arena and logical admission capacities.
        for mixed in [false, true] {
            let fake = Fake {
                arenas: RefCell::new(core::array::from_fn(|_| Vec::new())),
                busy: core::array::from_fn(|_| Cell::new(false)),
                pending: RefCell::new(VecDeque::new()),
                target: RefCell::new([0xa5; 24]),
                reads: Cell::new(0),
                accepted: Cell::new(0),
            };
            let mut queue = Scheduler::<Fake>::new();
            let mut expected = [0xa5; 24];
            let mut arena_index = 0;
            for round in 0..4 {
                let mut owners = Vec::new();
                let mut signals = Vec::new();
                for index in 0..16 {
                    let serial = round * 16 + index;
                    let (offset, length) = if mixed && index % 5 == 4 {
                        (3, 3)
                    } else {
                        (4, 8)
                    };
                    let mut data = alloc::vec![serial as u8; length];
                    let work = if buffer_write_uses_staging(offset as u64, length) {
                        let chunk = staged(offset as u64, &data);
                        let work = Work::Gpu {
                            slot: arena_index % UPLOAD_ARENAS,
                            chunk,
                        };
                        arena_index += 1;
                        work
                    } else {
                        Work::Cpu {
                            offset,
                            data: own_buffer_write(&resource(), offset as u64, &data).unwrap(),
                        }
                    };
                    expected[offset..offset + length].copy_from_slice(&data);
                    data.fill(0xee);
                    let owner = Rc::new(());
                    owners.push(Rc::downgrade(&owner));
                    let signal = Rc::new(Cell::new(None));
                    // Keep only a weak user receipt: dropping it must not cancel.
                    signals.push(Rc::downgrade(&signal));
                    queue
                        .enqueue(alloc::vec![work, Work::Read(expected)], owner, signal)
                        .unwrap();
                }
                queue.advance(&fake);
                assert!(owners.iter().all(|owner| owner.upgrade().is_some()));
                assert!(
                    signals
                        .iter()
                        .all(|signal| signal.upgrade().unwrap().get().is_none())
                );
                assert_eq!(
                    fake.pending.borrow().len(),
                    8,
                    "four uploads and their reads can be outstanding without a drain"
                );
                let mut iterations = 0;
                while !queue.is_empty() {
                    if !fake.pending.borrow().is_empty() {
                        fake.retire_one();
                    }
                    queue.advance(&fake);
                    iterations += 1;
                    assert!(iterations < 128, "dispatcher stopped making progress");
                }
                assert!(owners.iter().all(|owner| owner.upgrade().is_none()));
                assert!(signals.iter().all(|signal| signal.upgrade().is_none()));
            }
            assert_eq!(fake.reads.get(), 64);
            assert_eq!(fake.accepted.get(), 128);
            assert_eq!(*fake.target.borrow(), expected);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    #[ignore = "opt-in host CPU cost comparison; no GPU or queue latency"]
    fn profile_mapped_vs_staged_buffer_preparation() {
        use core::hint::black_box;
        use std::time::Instant;
        for (size, iterations) in [(64, 10_000), (4096, 2_000), (262144, 100)] {
            let data = alloc::vec![13; size];
            let mut mapped = alloc::vec![0; size];
            let mut arena = alloc::vec![0; size];
            for sample in 0..5 {
                let started = Instant::now();
                for _ in 0..iterations {
                    let owned = own_buffer_write(&resource(), 0, black_box(&data)).unwrap();
                    mapped.copy_from_slice(black_box(&owned));
                    black_box(&mapped);
                }
                let cpu_ns = started.elapsed().as_nanos() / iterations;
                let started = Instant::now();
                let mut wire_bytes = 0;
                for _ in 0..iterations {
                    let chunk = staged(0, black_box(&data));
                    let wire = encode_staged(&chunk);
                    wire_bytes = wire.len();
                    arena.copy_from_slice(black_box(&chunk.compiled.generated_objects[0].bytes));
                    black_box((&wire, &arena));
                }
                let gpu_ns = started.elapsed().as_nanos() / iterations;
                std::println!(
                    "BUFFER_CPU_PROFILE bytes={size} sample={sample} mapped_ns={cpu_ns} staged_ns={gpu_ns} wire_bytes={wire_bytes}"
                );
            }
        }
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
