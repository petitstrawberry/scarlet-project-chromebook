//! Full SGFX command normalization and A6xx queue submission.

use alloc::{borrow::Cow, vec::Vec};
use core::sync::atomic::{AtomicU8, Ordering};

use gpu_raw::GpuImageBgraRect;
use sgfx_codegen_adreno_a6xx as codegen;

use crate::resource::ContextResources;
use crate::wire::BoundObject;
use crate::{ContextInner, IrSubmitError, UnsupportedIrFeature, ir, wire};

const IMAGE_OBJECT_BASE: u32 = 1;
const BUFFER_OBJECT_BASE: u32 = 1 << 16;
const PIPELINE_OBJECT_BASE: u32 = 1 << 24;
// A canonical A618 draw expands to several hundred PM4 words.  The A618 queue
// advertises a 2 MiB transport so the production 512-item ScarletUI workload
// remains one render pass instead of forcing repeated full-surface load/store
// retirements. This is only the optimistic limit: the submission path bisects
// any unusually resource-heavy chunk whose exact wire encoding exceeds the
// negotiated ABI limit.
const MAX_DRAWS_PER_SUBMIT: usize = 512;
const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;

fn submit_trace_enabled() -> bool {
    static TRACE_CACHE: AtomicU8 = AtomicU8::new(u8::MAX);
    let cached = TRACE_CACHE.load(Ordering::Relaxed);
    if cached != u8::MAX {
        return cached != 0;
    }
    #[cfg(feature = "std")]
    let value = std::env::var("SGFX_ADRENO_TRACE").ok();
    #[cfg(not(feature = "std"))]
    let value = std::env::var("SGFX_ADRENO_TRACE");
    let enabled = value.as_deref().is_some_and(|value| {
        matches!(
            value,
            "1" | "true" | "TRUE" | "debug" | "DEBUG" | "trace" | "TRACE"
        )
    });
    TRACE_CACHE.store(enabled as u8, Ordering::Relaxed);
    enabled
}

#[cfg(feature = "std")]
fn monotonic_time_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(not(feature = "std"))]
fn monotonic_time_ns() -> u64 {
    std::syscall::syscall0(std::syscall::Syscall::MonotonicTime) as u64
}

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
                    // Every A618 SGFX buffer is CPU-visible and mapped into
                    // the GPU SMMU domain with the coherent attribute. Drain
                    // earlier GPU work to preserve IR ordering, then update the
                    // retained shared mapping directly. Encoding CP_MEMCPY for
                    // dynamic UI data duplicated the host copy and generated
                    // thousands of relocation/authority records per frame.
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    self.write_buffer(*buffer, *offset, data)?;
                }
                ir::Command::WriteTexture { texture, write } => {
                    let descriptor = self.resources.texture(*texture)?;
                    require_texture_upload_format(descriptor.format())?;
                    self.ensure_image(*texture, &mut resources, &mut external_bindings)?;

                    // Image uploads can be much larger than the bounded 64 KiB
                    // opaque submit wire. Flush earlier GPU work to preserve IR
                    // ordering, then use Scarlet's synchronous generic BGRA
                    // upload path, which writes the same kernel-owned linear
                    // backing already attached to this context.
                    self.submit_operations(
                        context,
                        queue,
                        &resources,
                        &pipelines,
                        &mut operations,
                        &external_bindings,
                    )?;
                    self.upload_texture_bgra(context, *texture, *write)?;
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

        self.submit_operations(
            context,
            queue,
            &resources,
            &pipelines,
            &mut operations,
            &external_bindings,
        )
    }

    fn submit_operations<'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        resources: &[codegen::ResourceMeta],
        pipelines: &[codegen::PipelineMeta],
        operations: &mut Vec<codegen::Operation<'data>>,
        external_bindings: &[BoundObject],
    ) -> Result<(), IrSubmitError> {
        if operations.is_empty() {
            return Ok(());
        }
        let chunks = split_submission_operations(operations, MAX_DRAWS_PER_SUBMIT)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(chunks.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        pending.extend(chunks.into_iter().rev());
        while let Some(chunk) = pending.pop() {
            let result = self.submit_one(
                context,
                queue,
                resources,
                pipelines,
                &chunk,
                external_bindings,
            );
            match result {
                Ok(()) => {}
                Err(
                    error @ IrSubmitError::SubmitWire(adreno_a6xx_submit_wire::Error::InvalidSize),
                ) => {
                    let draw_count = chunk
                        .iter()
                        .filter(|operation| {
                            matches!(
                                operation,
                                codegen::Operation::Draw { .. }
                                    | codegen::Operation::DrawIndexed { .. }
                            )
                        })
                        .count();
                    if draw_count <= 1 {
                        return Err(error);
                    }
                    let retry_limit = (draw_count / 2).max(1);
                    let retry = split_submission_operations(&chunk, retry_limit)?;
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
        operations.clear();
        Ok(())
    }

    fn submit_one<'data>(
        &mut self,
        context: &ContextInner,
        queue: &gpu_raw::GpuQueue,
        resources: &[codegen::ResourceMeta],
        pipelines: &[codegen::PipelineMeta],
        operations: &[codegen::Operation<'data>],
        external_bindings: &[BoundObject],
    ) -> Result<(), IrSubmitError> {
        let trace = submit_trace_enabled();
        let started = if trace { monotonic_time_ns() } else { 0 };
        let compiled = codegen::compile(codegen::CompileInput {
            capabilities: context.device.codegen_capabilities,
            resources,
            pipelines,
            operations,
        })?;
        let compiled_at = if trace { monotonic_time_ns() } else { 0 };
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(external_bindings.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings.extend_from_slice(external_bindings);
        let progress = self.materialize_generated(&compiled, &mut bindings)?;
        let materialized_at = if trace { monotonic_time_ns() } else { 0 };
        let payload = wire::encode(&compiled, &bindings)?;
        let encoded_at = if trace { monotonic_time_ns() } else { 0 };
        let result = queue.submit(&payload);
        let submitted_at = if trace { monotonic_time_ns() } else { 0 };
        if trace {
            let draws = operations
                .iter()
                .filter(|operation| {
                    matches!(
                        operation,
                        codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. }
                    )
                })
                .count();
            std::println!(
                "[a618-userspace-path] ops={} draws={} wire={} words={} resources={} relocs={} compile_us={} materialize_us={} encode_us={} queue_us={}",
                operations.len(),
                draws,
                payload.len(),
                compiled.words.len(),
                compiled.accesses.len(),
                compiled.fixups.len(),
                compiled_at.saturating_sub(started) / 1_000,
                materialized_at.saturating_sub(compiled_at) / 1_000,
                encoded_at.saturating_sub(materialized_at) / 1_000,
                submitted_at.saturating_sub(encoded_at) / 1_000,
            );
        }
        if let Err(error) = result {
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

    fn upload_texture_bgra(
        &mut self,
        context: &ContextInner,
        reference: ir::TextureRef<'_>,
        write: ir::TextureWrite<'_>,
    ) -> Result<(), IrSubmitError> {
        let descriptor = self.resources.texture(reference)?;
        let image = self.texture(reference)?;
        let upload = prepare_bgra_upload(descriptor.format(), write)?;
        let area = write.destination();
        context.raw.upload_image_bgra(
            &image.raw,
            upload.pixels.as_ref(),
            upload.bytes_per_row,
            GpuImageBgraRect::new(area.x(), area.y(), area.width(), area.height()),
        )?;
        Ok(())
    }

    fn ensure_image(
        &mut self,
        reference: ir::TextureRef<'_>,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        self.ensure_image_with_usage(reference, ir::TextureUsage::empty(), metadata, bindings)
    }

    fn ensure_image_with_usage(
        &mut self,
        reference: ir::TextureRef<'_>,
        additional_usage: ir::TextureUsage,
        metadata: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<codegen::ObjectId, IrSubmitError> {
        let id = image_object_id(reference.slot())?;
        if metadata.iter().any(|resource| resource.id == id) {
            return Ok(id);
        }
        let descriptor = self.resources.texture(reference)?;
        let image = self.texture(reference)?;
        append_image_resource(
            id,
            image.as_ref(),
            descriptor,
            descriptor.usage() | additional_usage,
            metadata,
            bindings,
        )?;
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
        let (scratch_token, scratch_size) = {
            let scratch = self.scratch(total)?;
            // The generated-object offsets already describe disjoint ranges
            // in the retained coherent scratch buffer.  Write each object
            // directly instead of assembling and then copying a second full
            // staging Vec on every submit. Alignment gaps are never addressed
            // by PM4 and therefore do not require initialization.
            for (generated, (_, offset, size)) in compiled
                .generated_objects
                .iter()
                .zip(placements.iter().copied())
            {
                if usize::try_from(size).ok() != Some(generated.bytes.len()) {
                    return Err(IrSubmitError::OutOfMemory);
                }
                scratch.write(offset, &generated.bytes)?;
            }
            (scratch.attachment_token, scratch.logical_size)
        };
        let progress = compiled
            .generated_objects
            .iter()
            .zip(placements.iter())
            .find_map(|(generated, (_, offset, _))| {
                (generated.kind == codegen::GeneratedObjectKind::CcuSequence)
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

#[derive(Default)]
struct RenderReplayState {
    pipeline: Option<codegen::PipelineId>,
    vertex: Option<(codegen::ObjectId, u64)>,
    index: Option<(codegen::ObjectId, u64, ir::IndexFormat)>,
    texture: Option<codegen::ObjectId>,
    sampler: Option<ir::SamplerDesc>,
    uniforms: Option<ir::DrawUniforms>,
    scissor: Option<ir::PixelRect>,
}

impl RenderReplayState {
    fn append<'data>(&self, operations: &mut Vec<codegen::Operation<'data>>) {
        if let Some(pipeline) = self.pipeline {
            operations.push(codegen::Operation::SetPipeline(pipeline));
        }
        if let Some((buffer, offset)) = self.vertex {
            operations.push(codegen::Operation::SetVertexBuffer { buffer, offset });
        }
        if let Some((buffer, offset, format)) = self.index {
            operations.push(codegen::Operation::SetIndexBuffer {
                buffer,
                offset,
                format,
            });
        }
        if let Some(texture) = self.texture {
            operations.push(codegen::Operation::SetTexture(texture));
        }
        if let Some(sampler) = self.sampler {
            operations.push(codegen::Operation::SetSampler(sampler));
        }
        if let Some(uniforms) = self.uniforms {
            operations.push(codegen::Operation::SetUniforms(uniforms));
        }
        if let Some(scissor) = self.scissor {
            operations.push(codegen::Operation::SetScissor(Some(scissor)));
        }
    }
}

fn split_submission_operations<'data>(
    source: &[codegen::Operation<'data>],
    max_draws: usize,
) -> Result<Vec<Vec<codegen::Operation<'data>>>, IrSubmitError> {
    if max_draws == 0 {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ));
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut pass = None;
    let mut replay = RenderReplayState::default();
    let mut chunk_draws = 0usize;

    for operation in source {
        match operation {
            codegen::Operation::BeginRenderPass(descriptor) => {
                if chunk_draws >= max_draws && !current.is_empty() {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    chunk_draws = 0;
                }
                pass = Some(*descriptor);
                replay = RenderReplayState::default();
                current.push(operation.clone());
            }
            codegen::Operation::EndRenderPass => {
                current.push(codegen::Operation::EndRenderPass);
                pass = None;
                replay = RenderReplayState::default();
            }
            codegen::Operation::SetPipeline(pipeline) => {
                replay.pipeline = Some(*pipeline);
                current.push(operation.clone());
            }
            codegen::Operation::SetVertexBuffer { buffer, offset } => {
                replay.vertex = Some((*buffer, *offset));
                current.push(operation.clone());
            }
            codegen::Operation::SetIndexBuffer {
                buffer,
                offset,
                format,
            } => {
                replay.index = Some((*buffer, *offset, *format));
                current.push(operation.clone());
            }
            codegen::Operation::SetTexture(texture) => {
                replay.texture = Some(*texture);
                current.push(operation.clone());
            }
            codegen::Operation::SetSampler(sampler) => {
                replay.sampler = Some(*sampler);
                current.push(operation.clone());
            }
            codegen::Operation::SetUniforms(uniforms) => {
                replay.uniforms = Some(*uniforms);
                current.push(operation.clone());
            }
            codegen::Operation::SetScissor(scissor) => {
                replay.scissor = *scissor;
                current.push(operation.clone());
            }
            codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. } => {
                let draw_limit = if replay.texture.is_some() {
                    max_draws.min(MAX_TEXTURED_DRAWS_PER_SUBMIT)
                } else {
                    max_draws
                };
                if chunk_draws >= draw_limit {
                    let mut continuation = pass.ok_or(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ResourceState,
                    ))?;
                    force_active_pass_store(&mut current)?;
                    current.push(codegen::Operation::EndRenderPass);
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    continuation.load = ir::LoadOp::Load;
                    if let Some(depth) = continuation.depth.as_mut() {
                        depth.load = ir::DepthLoadOp::Load;
                    }
                    current.push(codegen::Operation::BeginRenderPass(continuation));
                    replay.append(&mut current);
                    chunk_draws = 0;
                }
                current.push(operation.clone());
                chunk_draws = chunk_draws
                    .checked_add(1)
                    .ok_or(IrSubmitError::OutOfMemory)?;
            }
            _ => current.push(operation.clone()),
        }
    }

    if !current.is_empty() {
        chunks
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        chunks.push(current);
    }
    Ok(chunks)
}

fn force_active_pass_store(operations: &mut [codegen::Operation<'_>]) -> Result<(), IrSubmitError> {
    for operation in operations.iter_mut().rev() {
        if let codegen::Operation::BeginRenderPass(pass) = operation {
            pass.store = ir::StoreOp::Store;
            if let Some(depth) = pass.depth.as_mut() {
                depth.store = ir::StoreOp::Store;
            }
            return Ok(());
        }
    }
    Err(IrSubmitError::Unsupported(
        UnsupportedIrFeature::ResourceState,
    ))
}

struct PreparedBgraUpload<'data> {
    pixels: Cow<'data, [u8]>,
    bytes_per_row: u32,
}

fn prepare_bgra_upload<'data>(
    format: ir::TextureFormat,
    write: ir::TextureWrite<'data>,
) -> Result<PreparedBgraUpload<'data>, IrSubmitError> {
    if format == ir::TextureFormat::Bgra8Unorm {
        return Ok(PreparedBgraUpload {
            pixels: Cow::Borrowed(write.data()),
            bytes_per_row: write.bytes_per_row(),
        });
    }
    if !matches!(
        format,
        ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::R8Unorm
    ) {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::TextureUpload,
        ));
    }

    let area = write.destination();
    let destination_stride = area
        .width()
        .checked_mul(ir::TextureFormat::Bgra8Unorm.bytes_per_pixel())
        .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
    let destination_len = usize::try_from(
        u64::from(destination_stride)
            .checked_mul(u64::from(area.height()))
            .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?,
    )
    .map_err(|_| IrSubmitError::OutOfMemory)?;
    let logical_row_bytes = area
        .width()
        .checked_mul(format.bytes_per_pixel())
        .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(destination_len)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    pixels.resize(destination_len, 0);

    for row in 0..area.height() {
        let source_start = usize::try_from(
            u64::from(row)
                .checked_mul(u64::from(write.bytes_per_row()))
                .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?,
        )
        .map_err(|_| IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let source_end = source_start
            .checked_add(logical_row_bytes as usize)
            .ok_or(IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let source = write
            .data()
            .get(source_start..source_end)
            .ok_or(IrSubmitError::InvalidIr(ir::Error::OutOfBounds))?;
        let destination_start = usize::try_from(u64::from(row) * u64::from(destination_stride))
            .map_err(|_| IrSubmitError::InvalidIr(ir::Error::Overflow))?;
        let destination =
            &mut pixels[destination_start..destination_start + destination_stride as usize];
        match format {
            ir::TextureFormat::Rgba8Unorm => {
                for (source, destination) in
                    source.chunks_exact(4).zip(destination.chunks_exact_mut(4))
                {
                    destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
                }
            }
            ir::TextureFormat::R8Unorm => {
                for (&alpha, destination) in source.iter().zip(destination.chunks_exact_mut(4)) {
                    destination.copy_from_slice(&[0, 0, 0, alpha]);
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(PreparedBgraUpload {
        pixels: Cow::Owned(pixels),
        bytes_per_row: destination_stride,
    })
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

fn append_image_resource(
    id: codegen::ObjectId,
    image: &crate::resource::RawImage,
    descriptor: ir::TextureDesc,
    usage: ir::TextureUsage,
    metadata: &mut Vec<codegen::ResourceMeta>,
    bindings: &mut Vec<BoundObject>,
) -> Result<(), IrSubmitError> {
    if image.logical_format != descriptor.format()
        || metadata.iter().any(|resource| resource.id == id)
        || bindings
            .iter()
            .any(|binding| binding.object == codegen::ObjectRef::External(id))
    {
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
            usage,
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
    Ok(())
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
    use alloc::{borrow::Cow, vec};

    use super::{
        MAX_DRAWS_PER_SUBMIT, MAX_TEXTURED_DRAWS_PER_SUBMIT, prepare_bgra_upload,
        require_texture_upload_format, split_submission_operations,
    };
    use crate::{IrSubmitError, UnsupportedIrFeature, ir};

    #[test]
    fn oversized_render_pass_is_split_with_store_load_and_state_replay() {
        let target = super::codegen::ObjectId::new(7);
        let pipeline = super::codegen::PipelineId::new(9);
        let area = ir::PixelRect::new(0, 0, 64, 64).unwrap();
        let pass = super::codegen::RenderPass {
            target,
            area,
            load: ir::LoadOp::Clear(ir::Color::rgba(0.1, 0.2, 0.3, 1.0).unwrap()),
            store: ir::StoreOp::DontCare,
            depth: None,
        };
        let mut source = vec![
            super::codegen::Operation::BeginRenderPass(pass),
            super::codegen::Operation::SetPipeline(pipeline),
        ];
        for first_vertex in 0..=MAX_DRAWS_PER_SUBMIT {
            source.push(super::codegen::Operation::Draw {
                vertex_count: 3,
                first_vertex: first_vertex as u32,
            });
        }
        source.push(super::codegen::Operation::EndRenderPass);

        let chunks = split_submission_operations(&source, MAX_DRAWS_PER_SUBMIT).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0]
                .iter()
                .filter(|operation| matches!(operation, super::codegen::Operation::Draw { .. }))
                .count(),
            MAX_DRAWS_PER_SUBMIT
        );
        assert!(matches!(
            chunks[0].first(),
            Some(super::codegen::Operation::BeginRenderPass(
                super::codegen::RenderPass {
                    store: ir::StoreOp::Store,
                    ..
                }
            ))
        ));
        assert!(matches!(
            chunks[0].last(),
            Some(super::codegen::Operation::EndRenderPass)
        ));
        assert!(matches!(
            chunks[1].first(),
            Some(super::codegen::Operation::BeginRenderPass(
                super::codegen::RenderPass {
                    load: ir::LoadOp::Load,
                    store: ir::StoreOp::DontCare,
                    ..
                }
            ))
        ));
        assert_eq!(
            chunks[1].get(1),
            Some(&super::codegen::Operation::SetPipeline(pipeline))
        );
        assert_eq!(
            chunks[1]
                .iter()
                .filter(|operation| matches!(operation, super::codegen::Operation::Draw { .. }))
                .count(),
            1
        );
        assert!(matches!(
            chunks[1].last(),
            Some(super::codegen::Operation::EndRenderPass)
        ));
    }

    #[test]
    fn textured_render_pass_uses_the_smaller_wire_budget() {
        let target = super::codegen::ObjectId::new(7);
        let texture = super::codegen::ObjectId::new(8);
        let area = ir::PixelRect::new(0, 0, 64, 64).unwrap();
        let pass = super::codegen::RenderPass {
            target,
            area,
            load: ir::LoadOp::Load,
            store: ir::StoreOp::Store,
            depth: None,
        };
        let mut source = vec![
            super::codegen::Operation::BeginRenderPass(pass),
            super::codegen::Operation::SetTexture(texture),
        ];
        for first_vertex in 0..=MAX_TEXTURED_DRAWS_PER_SUBMIT {
            source.push(super::codegen::Operation::Draw {
                vertex_count: 3,
                first_vertex: first_vertex as u32,
            });
        }
        source.push(super::codegen::Operation::EndRenderPass);

        let chunks = split_submission_operations(&source, MAX_DRAWS_PER_SUBMIT).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0]
                .iter()
                .filter(|operation| matches!(operation, super::codegen::Operation::Draw { .. }))
                .count(),
            MAX_TEXTURED_DRAWS_PER_SUBMIT
        );
        assert!(chunks[1].iter().any(
            |operation| matches!(operation, super::codegen::Operation::SetTexture(bound) if *bound == texture)
        ));
        assert_eq!(
            chunks[1]
                .iter()
                .filter(|operation| matches!(operation, super::codegen::Operation::Draw { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn small_clear_copy_and_cursor_pass_remain_one_submission() {
        let target = super::codegen::ObjectId::new(7);
        let source = super::codegen::ObjectId::new(8);
        let pipeline = super::codegen::PipelineId::new(9);
        let area = ir::PixelRect::new(0, 0, 64, 64).unwrap();
        let operations = vec![
            super::codegen::Operation::BeginRenderPass(super::codegen::RenderPass {
                target,
                area,
                load: ir::LoadOp::Clear(ir::Color::rgba(0.1, 0.2, 0.3, 1.0).unwrap()),
                store: ir::StoreOp::Store,
                depth: None,
            }),
            super::codegen::Operation::EndRenderPass,
            super::codegen::Operation::CopyTextureToTexture {
                source,
                source_rect: area,
                destination: target,
                destination_rect: area,
            },
            super::codegen::Operation::BeginRenderPass(super::codegen::RenderPass {
                target,
                area,
                load: ir::LoadOp::Load,
                store: ir::StoreOp::Store,
                depth: None,
            }),
            super::codegen::Operation::SetPipeline(pipeline),
            super::codegen::Operation::Draw {
                vertex_count: 6,
                first_vertex: 0,
            },
            super::codegen::Operation::EndRenderPass,
        ];

        let chunks = split_submission_operations(&operations, MAX_DRAWS_PER_SUBMIT).unwrap();
        assert_eq!(chunks, vec![operations]);
    }

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

    #[test]
    fn native_bgra_upload_keeps_large_payload_borrowed() {
        let area = ir::PixelRect::new(0, 0, 640, 640).unwrap();
        let pixels = vec![0x5a; 640 * 640 * 4];
        let write = ir::TextureWrite::new(area, 640 * 4, &pixels).unwrap();
        let upload = prepare_bgra_upload(ir::TextureFormat::Bgra8Unorm, write).unwrap();

        assert!(matches!(upload.pixels, Cow::Borrowed(_)));
        assert_eq!(upload.bytes_per_row, 640 * 4);
        assert_eq!(upload.pixels.as_ref(), pixels.as_slice());
    }

    #[test]
    fn rgba_upload_is_swizzled_to_tight_bgra_rows() {
        let area = ir::PixelRect::new(3, 5, 2, 2).unwrap();
        let pixels = [
            1, 2, 3, 4, 5, 6, 7, 8, 0xaa, 0xbb, 0xcc, 0xdd, 9, 10, 11, 12, 13, 14, 15, 16,
        ];
        let write = ir::TextureWrite::new(area, 12, &pixels).unwrap();
        let upload = prepare_bgra_upload(ir::TextureFormat::Rgba8Unorm, write).unwrap();

        assert!(matches!(upload.pixels, Cow::Owned(_)));
        assert_eq!(upload.bytes_per_row, 8);
        assert_eq!(
            upload.pixels.as_ref(),
            &[3, 2, 1, 4, 7, 6, 5, 8, 11, 10, 9, 12, 15, 14, 13, 16]
        );
    }

    #[test]
    fn alpha_mask_upload_expands_into_bgra_alpha() {
        let area = ir::PixelRect::new(0, 0, 3, 1).unwrap();
        let pixels = [0x11, 0x22, 0x33];
        let write = ir::TextureWrite::new(area, 3, &pixels).unwrap();
        let upload = prepare_bgra_upload(ir::TextureFormat::R8Unorm, write).unwrap();

        assert_eq!(upload.bytes_per_row, 12);
        assert_eq!(
            upload.pixels.as_ref(),
            &[0, 0, 0, 0x11, 0, 0, 0, 0x22, 0, 0, 0, 0x33]
        );
    }
}
