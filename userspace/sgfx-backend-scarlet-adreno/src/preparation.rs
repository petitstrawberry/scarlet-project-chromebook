//! Pure chunk planning shared by synchronous execution and tracked dispatch.

use crate::wire::BoundObject;
use crate::{IrSubmitError, UnsupportedIrFeature, ir};
use alloc::vec::Vec;
use sgfx_codegen_adreno_a6xx as codegen;

pub(crate) const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;
pub(crate) const UPLOAD_ARENA_BYTES: u64 = 8 * 1024 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

pub(crate) struct PreparedChunk {
    pub(crate) compiled: codegen::RelocatablePm4,
    pub(crate) placements: Vec<(codegen::GeneratedObjectId, u64, u64)>,
}

/// Bounded memo for one normalization pass only. Keys must include table and
/// generation identity (use BufferId, not BufferRef's legacy equality). Entries
/// only refer to metadata appended during this same pass; no backing, contents,
/// descriptors or compiled commands survive a submit boundary. Collisions evict.
pub(crate) struct NormalizationMemo<K, V> {
    entries: [Option<(K, V)>; 16],
}

impl<K: Copy + Eq, V: Copy> NormalizationMemo<K, V> {
    pub(crate) fn new() -> Self {
        Self {
            entries: [None; 16],
        }
    }

    pub(crate) fn resolve<E>(
        &mut self,
        slot: usize,
        key: K,
        resolve: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        let entry = &mut self.entries[slot % 16];
        if let Some((previous, value)) = entry {
            if *previous == key {
                return Ok(*value);
            }
        }
        let value = resolve()?;
        *entry = Some((key, value));
        Ok(value)
    }
}

/// Keep the common external prefix once while replacing only chunk-local
/// generated bindings. Borrowed by wire encoding, never retained by dispatch.
pub(crate) struct ChunkBindings<'a> {
    external: &'a [BoundObject],
    bindings: Vec<BoundObject>,
}

impl<'a> ChunkBindings<'a> {
    pub(crate) fn new(external: &'a [BoundObject]) -> Self {
        // CPU-only submissions never need a binding allocation.
        Self {
            external,
            bindings: Vec::new(),
        }
    }

    pub(crate) fn for_chunk(
        &mut self,
        chunk: &PreparedChunk,
        attachment_token: u64,
    ) -> Result<&[BoundObject], IrSubmitError> {
        self.bindings.truncate(self.external.len());
        let missing = self.external.len() - self.bindings.len();
        let additional = missing
            .checked_add(chunk.placements.len())
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        self.bindings
            .try_reserve(additional)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.bindings
            .extend_from_slice(&self.external[self.bindings.len()..]);
        for &(id, offset, size) in &chunk.placements {
            self.bindings.push(BoundObject {
                object: codegen::ObjectRef::Generated(id),
                attachment_token,
                allocation_offset: offset,
                size,
            });
        }
        Ok(&self.bindings)
    }
}

#[derive(Default)]
struct RenderReplayState<'data> {
    _lifetime: core::marker::PhantomData<&'data ()>,
    #[cfg(feature = "programmable")]
    programmable: Option<alloc::sync::Arc<codegen::NativeRenderPipeline>>,
    #[cfg(feature = "programmable")]
    fixed_native: bool,
    #[cfg(feature = "programmable")]
    bind_groups: [Option<Vec<codegen::NativeBinding>>; 4],
    #[cfg(feature = "programmable")]
    vertex_slots: [Option<(codegen::ObjectId, u64)>; 8],
    #[cfg(feature = "programmable")]
    push_constants: [[Option<&'data [u8]>; 32]; 3],
    #[cfg(feature = "programmable")]
    draw_index: Option<u32>,
    pipeline: Option<codegen::PipelineId>,
    vertex: Option<(codegen::ObjectId, u64)>,
    index: Option<(codegen::ObjectId, u64, ir::IndexFormat)>,
    texture: Option<codegen::ObjectId>,
    sampler: Option<ir::SamplerDesc>,
    uniforms: Option<ir::DrawUniforms>,
    scissor: Option<ir::PixelRect>,
    viewport: Option<ir::Viewport>,
}

impl<'data> RenderReplayState<'data> {
    fn append(&self, operations: &mut Vec<codegen::Operation<'data>>) {
        #[cfg(feature = "programmable")]
        {
            if let Some(pipeline) = &self.programmable {
                operations.push(if self.fixed_native {codegen::Operation::SetFixedNativePipeline(alloc::sync::Arc::clone(pipeline))}
                    else {codegen::Operation::SetProgrammablePipeline(alloc::sync::Arc::clone(pipeline))});
            }
            for (index, bindings) in self.bind_groups.iter().enumerate() {
                if let Some(bindings) = bindings {
                    operations.push(codegen::Operation::SetBindGroup {
                        index: index as u32,
                        bindings: bindings.clone(),
                    });
                }
            }
            for (slot, value) in self.vertex_slots.iter().enumerate() {
                if let Some((buffer, offset)) = value {
                    operations.push(codegen::Operation::SetVertexBufferSlot {
                        slot: slot as u32,
                        buffer: *buffer,
                        offset: *offset,
                    });
                }
            }
            for (stage, words) in self.push_constants.iter().enumerate() {
                let stages = [
                    ir::ShaderStages::VERTEX,
                    ir::ShaderStages::FRAGMENT,
                    ir::ShaderStages::COMPUTE,
                ][stage];
                for (word, data) in words.iter().enumerate() {
                    if let Some(data) = data {
                        operations.push(codegen::Operation::SetPushConstants {
                            stages,
                            offset: word as u32 * 4,
                            data,
                        });
                    }
                }
            }
            if let Some(index) = self.draw_index {
                operations.push(codegen::Operation::SetDrawIndex(index));
            }
        }
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
        if let Some(viewport) = self.viewport {
            operations.push(codegen::Operation::SetViewport(viewport));
        }
        if let Some(scissor) = self.scissor {
            operations.push(codegen::Operation::SetScissor(Some(scissor)));
        }
    }
}

pub(crate) fn split_submission_operations<'data>(
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
            codegen::Operation::BeginRenderPass(_) => {
                if chunk_draws >= max_draws && !current.is_empty() {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    chunk_draws = 0;
                }
                pass = Some(operation.clone());
                replay = RenderReplayState::default();
                current.push(operation.clone());
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::BeginNativeRenderPass(_) => {
                if chunk_draws >= max_draws && !current.is_empty() {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    chunk_draws = 0;
                }
                pass = Some(operation.clone());
                replay = RenderReplayState::default();
                current.push(operation.clone());
            }
            codegen::Operation::EndRenderPass => {
                current.push(codegen::Operation::EndRenderPass);
                pass = None;
                replay = RenderReplayState::default();
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::SetProgrammablePipeline(pipeline) | codegen::Operation::SetFixedNativePipeline(pipeline) => {
                replay.fixed_native=matches!(operation,codegen::Operation::SetFixedNativePipeline(_));
                replay.programmable = Some(alloc::sync::Arc::clone(pipeline));
                replay.pipeline = None;
                current.push(operation.clone());
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::SetBindGroup { index, bindings } => {
                *replay
                    .bind_groups
                    .get_mut(*index as usize)
                    .ok_or(codegen::CompileError::InvalidResource)? = Some(bindings.clone());
                current.push(operation.clone());
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::SetVertexBufferSlot {
                slot,
                buffer,
                offset,
            } => {
                *replay
                    .vertex_slots
                    .get_mut(*slot as usize)
                    .ok_or(codegen::CompileError::InvalidResource)? = Some((*buffer, *offset));
                if *slot == 0 {
                    replay.vertex = None;
                }
                current.push(operation.clone());
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::SetDrawIndex(index) => {
                replay.draw_index = Some(*index);
                current.push(operation.clone());
            }
            #[cfg(feature = "programmable")]
            codegen::Operation::SetPushConstants {
                stages,
                offset,
                data,
            } => {
                let end = (*offset as usize)
                    .checked_add(data.len())
                    .ok_or(codegen::CompileError::OutOfBounds)?;
                if offset % 4 != 0 || data.len() % 4 != 0 || end > 128 {
                    return Err(codegen::CompileError::OutOfBounds.into());
                }
                for (stage, stage_flag) in [
                    ir::ShaderStages::VERTEX,
                    ir::ShaderStages::FRAGMENT,
                    ir::ShaderStages::COMPUTE,
                ]
                .iter()
                .enumerate()
                {
                    if stages.contains(*stage_flag) {
                        for (word, bytes) in data.chunks_exact(4).enumerate() {
                            replay.push_constants[stage][*offset as usize / 4 + word] = Some(bytes);
                        }
                    }
                }
                current.push(operation.clone());
            }
            codegen::Operation::SetPipeline(pipeline) => {
                #[cfg(feature = "programmable")]
                {
                    replay.programmable = None;
                }
                replay.pipeline = Some(*pipeline);
                current.push(operation.clone());
            }
            codegen::Operation::SetVertexBuffer { buffer, offset } => {
                replay.vertex = Some((*buffer, *offset));
                #[cfg(feature = "programmable")]
                {
                    replay.vertex_slots[0] = None;
                }
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
            codegen::Operation::SetViewport(viewport) => {
                replay.viewport = Some(*viewport);
                current.push(operation.clone());
            }
            codegen::Operation::SetScissor(scissor) => {
                replay.scissor = *scissor;
                current.push(operation.clone());
            }
            operation if is_draw(operation) => {
                let draw_limit = if replay.texture.is_some() {
                    max_draws.min(MAX_TEXTURED_DRAWS_PER_SUBMIT)
                } else {
                    max_draws
                };
                if chunk_draws >= draw_limit {
                    let mut continuation = pass.clone().ok_or(IrSubmitError::Unsupported(
                        UnsupportedIrFeature::ResourceState,
                    ))?;
                    force_active_pass_store(&mut current)?;
                    current.push(codegen::Operation::EndRenderPass);
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(core::mem::take(&mut current));
                    match &mut continuation {
                        codegen::Operation::BeginRenderPass(pass) => {
                            pass.load = ir::LoadOp::Load;
                            if let Some(depth) = pass.depth.as_mut() {
                                depth.load = ir::DepthLoadOp::Load;
                            }
                        }
                        #[cfg(feature = "programmable")]
                        codegen::Operation::BeginNativeRenderPass(pass) => {
                            for color in &mut pass.colors {
                                color.load = ir::LoadOp::Load;
                            }
                            if let Some(depth) = pass.depth.as_mut() {
                                depth.load = ir::DepthLoadOp::Load;
                            }
                        }
                        _ => {
                            return Err(IrSubmitError::Unsupported(
                                UnsupportedIrFeature::ResourceState,
                            ));
                        }
                    }
                    current.push(continuation);
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

pub(crate) fn is_draw(operation: &codegen::Operation<'_>) -> bool {
    match operation {
        codegen::Operation::Draw { .. } | codegen::Operation::DrawIndexed { .. } => true,
        #[cfg(feature = "programmable")]
        codegen::Operation::DrawInstanced { .. }
        | codegen::Operation::DrawIndexedInstanced { .. } => true,
        _ => false,
    }
}

#[cfg(all(test, feature = "programmable"))]
#[test]
fn split_mrt_preserves_every_attachment_between_chunks() {
    use codegen::{NativeColorAttachment, NativeRenderPass, ObjectId, Operation};
    let colors = (0..8)
        .map(|i| NativeColorAttachment {
            target: ObjectId::new(i + 1),
            load: ir::LoadOp::Clear(ir::Color::rgba(i as f32 / 8.0, 0.0, 0.0, 1.0).unwrap()),
            store: if i % 2 == 0 {
                ir::StoreOp::DontCare
            } else {
                ir::StoreOp::Store
            },
        })
        .collect::<Vec<_>>();
    let pass = NativeRenderPass {
        colors: colors.clone(),
        area: ir::PixelRect::new(0, 0, 8, 8).unwrap(),
        depth: Some(codegen::DepthAttachment {
            target: ObjectId::new(9),
            load: ir::DepthLoadOp::Clear(0.25),
            store: ir::StoreOp::DontCare,
        }),
    };
    let mut ops = alloc::vec![Operation::BeginNativeRenderPass(pass)];
    for _ in 0..3 {
        ops.push(Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        });
    }
    ops.push(Operation::EndRenderPass);
    let chunks = split_submission_operations(&ops, 1).unwrap();
    assert_eq!(chunks.len(), 3);
    for (index, chunk) in chunks.iter().enumerate() {
        let Operation::BeginNativeRenderPass(pass) = &chunk[0] else {
            panic!("native pass lost");
        };
        for (slot, color) in pass.colors.iter().enumerate() {
            assert_eq!(color.target, colors[slot].target);
            assert_eq!(
                color.load,
                if index == 0 {
                    colors[slot].load
                } else {
                    ir::LoadOp::Load
                }
            );
            assert_eq!(
                color.store,
                if index == 2 {
                    colors[slot].store
                } else {
                    ir::StoreOp::Store
                }
            );
        }
        assert_eq!(
            pass.depth.unwrap().load,
            if index == 0 {
                ir::DepthLoadOp::Clear(0.25)
            } else {
                ir::DepthLoadOp::Load
            }
        );
        assert_eq!(
            pass.depth.unwrap().store,
            if index == 2 {
                ir::StoreOp::DontCare
            } else {
                ir::StoreOp::Store
            }
        );
        assert!(matches!(chunk.last(), Some(Operation::EndRenderPass)));
    }
}

fn force_active_pass_store(operations: &mut [codegen::Operation<'_>]) -> Result<(), IrSubmitError> {
    for operation in operations.iter_mut().rev() {
        #[cfg(feature = "programmable")]
        if let codegen::Operation::BeginNativeRenderPass(pass) = operation {
            for color in &mut pass.colors {
                color.store = ir::StoreOp::Store;
            }
            if let Some(depth) = pass.depth.as_mut() {
                depth.store = ir::StoreOp::Store;
            }
            return Ok(());
        }
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

fn generated_placements(
    compiled: &codegen::RelocatablePm4,
    mut end: u64,
) -> Result<(Vec<(codegen::GeneratedObjectId, u64, u64)>, u64), IrSubmitError> {
    let mut placements = Vec::new();
    placements
        .try_reserve_exact(compiled.generated_objects.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for generated in &compiled.generated_objects {
        let alignment = u64::from(generated.alignment);
        if generated.bytes.is_empty() || !alignment.is_power_of_two() {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        end = end
            .checked_add(alignment - 1)
            .map(|value| value & !(alignment - 1))
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
        let size =
            u64::try_from(generated.bytes.len()).map_err(|_| IrSubmitError::SubmissionTooLarge)?;
        placements.push((generated.id, end, size));
        end = end
            .checked_add(size)
            .filter(|end| *end <= UPLOAD_ARENA_BYTES)
            .ok_or(IrSubmitError::SubmissionTooLarge)?;
    }
    Ok((placements, end))
}

/// Outside-pass transfers are independently bounded packets; render-pass
/// splitting preserves load/store and replay state through the shared helper.
pub(crate) fn split_async_operations<'data>(
    source: &[codegen::Operation<'data>],
) -> Result<Vec<Vec<codegen::Operation<'data>>>, IrSubmitError> {
    let mut chunks = Vec::new();
    let mut graphics = Vec::new();
    for operation in source {
        if matches!(
            operation,
            codegen::Operation::WriteBuffer { .. }
                | codegen::Operation::CopyBufferToBuffer { .. }
                | codegen::Operation::ResourceBarrier
                | codegen::Operation::WriteTexture { .. }
                | codegen::Operation::CopyTextureToTexture { .. }
                | codegen::Operation::BlitTexture { .. }
        ) {
            let preceding = split_submission_operations(&graphics, 512)?;
            chunks
                .try_reserve(preceding.len())
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            chunks.extend(preceding);
            graphics.clear();
            match operation {
                codegen::Operation::CopyBufferToBuffer {
                    source,
                    source_offset,
                    destination,
                    destination_offset,
                    size,
                } => {
                    if *size == 0
                        || (source_offset | destination_offset | size) & 3 != 0
                        || source == destination
                    {
                        return Err(codegen::CompileError::InvalidResource.into());
                    }
                    source_offset
                        .checked_add(*size)
                        .ok_or(IrSubmitError::SubmissionTooLarge)?;
                    destination_offset
                        .checked_add(*size)
                        .ok_or(IrSubmitError::SubmissionTooLarge)?;
                    let mut copied = 0u64;
                    while copied < *size {
                        let count = (*size - copied).min(UPLOAD_CHUNK_BYTES as u64);
                        let src = source_offset
                            .checked_add(copied)
                            .ok_or(IrSubmitError::SubmissionTooLarge)?;
                        let dst = destination_offset
                            .checked_add(copied)
                            .ok_or(IrSubmitError::SubmissionTooLarge)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(alloc::vec![codegen::Operation::CopyBufferToBuffer {
                            source: *source,
                            source_offset: src,
                            destination: *destination,
                            destination_offset: dst,
                            size: count,
                        }]);
                        copied += count;
                    }
                }
                codegen::Operation::WriteBuffer {
                    destination,
                    offset,
                    data,
                } => {
                    for (index, data) in data.chunks(UPLOAD_CHUNK_BYTES).enumerate() {
                        let offset = offset
                            .checked_add((index * UPLOAD_CHUNK_BYTES) as u64)
                            .ok_or(IrSubmitError::SubmissionTooLarge)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(alloc::vec![codegen::Operation::WriteBuffer {
                            destination: *destination,
                            offset,
                            data
                        }]);
                    }
                }
                codegen::Operation::WriteTexture {
                    destination,
                    area,
                    bytes_per_row,
                    data,
                } => {
                    let stride = usize::try_from(*bytes_per_row)
                        .map_err(|_| IrSubmitError::SubmissionTooLarge)?;
                    if stride == 0 {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::TextureUpload,
                        ));
                    }
                    // BGRA conversion can expand an R8 source by four. Bound
                    // both source bytes and physical rows before compilation.
                    let physical_row = area.width() as usize * 4;
                    let row_budget = stride.max(physical_row);
                    // Every row contributes an upload object and destination
                    // authority. Narrow/tall images must respect the wire's
                    // resource ceiling even when their pixel bytes are tiny.
                    let resource_rows = adreno_a6xx_submit_wire::MAX_RESOURCES / 2;
                    let max_rows =
                        (UPLOAD_CHUNK_BYTES / row_budget).max(1).min(resource_rows) as u32;
                    let mut row = 0;
                    while row < area.height() {
                        let height = max_rows.min(area.height() - row);
                        let first = row as usize * stride;
                        let end = ((row + height) as usize * stride).min(data.len());
                        let pixels = data.get(first..end).ok_or(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::TextureUpload,
                        ))?;
                        let part =
                            ir::PixelRect::new(area.x(), area.y() + row, area.width(), height)?;
                        chunks
                            .try_reserve(1)
                            .map_err(|_| IrSubmitError::OutOfMemory)?;
                        chunks.push(alloc::vec![codegen::Operation::WriteTexture {
                            destination: *destination,
                            area: part,
                            bytes_per_row: *bytes_per_row,
                            data: pixels
                        }]);
                        row += height;
                    }
                }
                _ => {
                    chunks
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    chunks.push(alloc::vec![operation.clone()]);
                }
            }
        } else {
            graphics
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            graphics.push(operation.clone());
        }
    }
    let trailing = split_submission_operations(&graphics, 512)?;
    chunks
        .try_reserve(trailing.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    chunks.extend(trailing);
    Ok(chunks)
}

/// A speculative native batch owns its ORIGINAL split boundaries until compile
/// preflight succeeds. Failure falls back before acceptance, never by replaying
/// an already submitted prefix. Operations and borrowed upload bytes stay ordered.
pub(crate) struct AsyncOperationBatch<'data> {
    pub(crate) operations: Vec<codegen::Operation<'data>>,
    pub(crate) fallback: Vec<Vec<codegen::Operation<'data>>>,
}

impl<'data> AsyncOperationBatch<'data> {
    pub(crate) fn single(operations: Vec<codegen::Operation<'data>>) -> Self {
        Self { operations, fallback: Vec::new() }
    }
}

const MAX_ADJACENT_CHUNKS: usize = 32;
const MAX_BATCH_OPERATIONS: usize = 4096;

/// Bound speculative compilation independently of all checked compiler/wire
/// limits. Byte-unaligned writes retain their existing ordered CPU fallback and
/// are hard boundaries: they cannot be encoded as CP_MEMCPY or hoisted.
pub(crate) fn coalesce_async_operations<'data>(
    chunks: Vec<Vec<codegen::Operation<'data>>>,
) -> Result<Vec<AsyncOperationBatch<'data>>, IrSubmitError> {
    fn finish<'data>(
        batches: &mut Vec<AsyncOperationBatch<'data>>,
        mut group: Vec<Vec<codegen::Operation<'data>>>,
        count: usize,
    ) -> Result<(), IrSubmitError> {
        if group.is_empty() { return Ok(()); }
        batches.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
        if group.len() == 1 {
            batches.push(AsyncOperationBatch::single(group.pop().unwrap()));
        } else {
            let mut operations = Vec::new();
            operations.try_reserve_exact(count).map_err(|_| IrSubmitError::OutOfMemory)?;
            for chunk in &group { operations.extend(chunk.iter().cloned()); }
            batches.push(AsyncOperationBatch { operations, fallback: group });
        }
        Ok(())
    }
    let mut batches = Vec::new();
    let mut group = Vec::new();
    let mut count = 0usize;
    let mut draws = 0usize;
    let mut upload_bytes = 0usize;
    for chunk in chunks {
        let chunk_draws = chunk.iter().filter(|operation| is_draw(operation)).count();
        let cpu_write = chunk.iter().any(|operation| matches!(operation,
            codegen::Operation::WriteBuffer { offset, data, .. }
                if offset % 4 != 0 || data.len() % 4 != 0));
        let bytes = chunk.iter().try_fold(0usize, |bytes, operation| {
            let size = match operation {
                codegen::Operation::WriteBuffer { data, .. } | codegen::Operation::WriteTexture { data, .. } => data.len(),
                _ => 0,
            };
            bytes.checked_add(size).ok_or(IrSubmitError::SubmissionTooLarge)
        })?;
        if cpu_write || group.len() == MAX_ADJACENT_CHUNKS
            || count.saturating_add(chunk.len()) > MAX_BATCH_OPERATIONS
            || draws.saturating_add(chunk_draws) > MAX_TEXTURED_DRAWS_PER_SUBMIT
            || upload_bytes.saturating_add(bytes) > UPLOAD_ARENA_BYTES as usize
        {
            finish(&mut batches, core::mem::take(&mut group), count)?;
            count = 0;
            draws = 0;
            upload_bytes = 0;
        }
        if cpu_write {
            batches.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
            batches.push(AsyncOperationBatch::single(chunk));
        } else {
            count = count.checked_add(chunk.len()).ok_or(IrSubmitError::SubmissionTooLarge)?;
            draws = draws.checked_add(chunk_draws).ok_or(IrSubmitError::SubmissionTooLarge)?;
            upload_bytes = upload_bytes.checked_add(bytes).ok_or(IrSubmitError::SubmissionTooLarge)?;
            group.try_reserve(1).map_err(|_| IrSubmitError::OutOfMemory)?;
            group.push(chunk);
        }
    }
    finish(&mut batches, group, count)?;
    Ok(batches)
}

pub(crate) fn prepare_chunk(
    capabilities: codegen::Capabilities,
    resources: &[codegen::ResourceMeta],
    pipelines: &[codegen::PipelineMeta],
    operations: &[codegen::Operation<'_>],
    external: &[BoundObject],
    arena_end: u64,
    native_limit: usize,
) -> Result<(PreparedChunk, u64), IrSubmitError> {
    let compiled = codegen::compile(codegen::CompileInput {
        capabilities,
        resources,
        pipelines,
        operations,
    })?;
    let (placements, end) = generated_placements(&compiled, arena_end)?;
    // Count-only preflight avoids allocating, relocating and then discarding
    // a complete wire stream. submit_async performs the actual checked encode
    // for every chunk before publishing any work to the dispatcher.
    let _ = external;
    let size = adreno_a6xx_submit_wire::encoded_len_for_counts(
        compiled.words.len(),
        compiled.accesses.len(),
        compiled.fixups.len(),
    );
    match size {
        Ok(size) if size <= native_limit => {}
        Ok(_) | Err(adreno_a6xx_submit_wire::Error::InvalidSize) => {
            return Err(IrSubmitError::SubmissionTooLarge);
        }
        Err(error) => return Err(IrSubmitError::SubmitWire(error)),
    }
    Ok((
        PreparedChunk {
            compiled,
            placements,
        },
        end,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire;
    use alloc::vec;
    use codegen::{
        Access, Capabilities, GeneratedObjectKind, ImageMeta, ImageModifier, ObjectId, ObjectRef,
        Operation, PlaneLayout, ResourceKind, ResourceMeta,
    };
    use ir::{BufferUsage, Extent2D, PixelRect, TextureFormat, TextureUsage};

    #[test]
    fn normalization_memo_keys_include_table_generation_mip_and_layer() {
        use core::cell::Cell;
        let table = ir::ResourceTable::new();
        let descriptor = ir::TextureDesc::new(
            TextureFormat::Bgra8Unorm,
            Extent2D::new(8, 8).unwrap(),
            TextureUsage::COPY_DST,
        )
        .unwrap();
        let old = table.define_texture(descriptor).unwrap();
        let mut memo = NormalizationMemo::new();
        let calls = Cell::new(0);
        for _ in 0..128 {
            assert_eq!(
                memo.resolve(old.slot(), (old.id(), 0, 0), || {
                    calls.set(calls.get() + 1);
                    Ok::<_, ()>(17)
                })
                .unwrap(),
                17
            );
        }
        assert_eq!(calls.get(), 1, "one alias for repeated atlas writes");
        for (mip, layer, expected) in [(1, 0, 18), (1, 1, 19), (0, 0, 20)] {
            assert_eq!(
                memo.resolve(old.slot(), (old.id(), mip, layer), || Ok::<_, ()>(expected))
                    .unwrap(),
                expected
            );
        }
        table.release_texture(old.id()).unwrap();
        let replacement = table.define_texture(descriptor).unwrap();
        assert_eq!(replacement.slot(), old.slot());
        assert_ne!(replacement.id(), old.id());
        assert_eq!(
            memo.resolve(
                replacement.slot(),
                (replacement.id(), 0, 0),
                || Ok::<_, ()>(21)
            )
            .unwrap(),
            21
        );
        let other = ir::ResourceTable::new();
        let foreign = other.define_texture(descriptor).unwrap();
        assert_eq!(foreign.slot(), replacement.slot());
        assert_eq!(
            memo.resolve(foreign.slot(), (foreign.id(), 0, 0), || Ok::<_, ()>(22))
                .unwrap(),
            22
        );
        // A new submit starts empty: even an unchanged logical ID must resolve
        // against its new metadata/backing; the old compiler-local ID is invalid.
        let mut next_submit = NormalizationMemo::new();
        assert_eq!(
            next_submit
                .resolve(
                    replacement.slot(),
                    (replacement.id(), 0, 0),
                    || Ok::<_, ()>(1)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn buffer_id_memo_does_not_confuse_reused_slots_or_cache_failures() {
        let table = ir::ResourceTable::new();
        let desc = ir::BufferDesc::new(32, BufferUsage::COPY_DST).unwrap();
        let old = table.define_buffer(desc).unwrap();
        let mut memo = NormalizationMemo::new();
        assert_eq!(
            memo.resolve(old.slot(), old.id(), || Ok::<_, ()>(7))
                .unwrap(),
            7
        );
        table.release_buffer(old.id()).unwrap();
        let next = table.define_buffer(desc).unwrap();
        assert_eq!(old.slot(), next.slot());
        assert_ne!(old.id(), next.id());
        assert_eq!(
            memo.resolve(next.slot(), next.id(), || Err::<u32, _>(()))
                .unwrap_err(),
            ()
        );
        assert_eq!(
            memo.resolve(next.slot(), next.id(), || Ok::<_, ()>(9))
                .unwrap(),
            9
        );
        // Hash collisions only cause a new resolution, never a false hit.
        assert_eq!(
            memo.resolve(next.slot() + 16, old.id(), || Ok::<_, ()>(10))
                .unwrap(),
            10
        );
        assert_eq!(
            memo.resolve(next.slot(), next.id(), || Ok::<_, ()>(11))
                .unwrap(),
            11
        );
        let mut new_submit = NormalizationMemo::<ir::BufferId, u32>::new();
        assert!(
            new_submit
                .resolve(old.slot(), old.id(), || table.buffer(old).map(|_| 12))
                .is_err()
        );
    }

    #[test]
    fn reused_chunk_bindings_encode_identically_and_never_reuse_old_arena_or_uploads() {
        let destination = ObjectId::new(1);
        let resources = [buffer(destination, 4096)];
        let external = bindings(&resources);
        let one = prepare(
            &resources,
            &[Operation::WriteBuffer {
                destination,
                offset: 0,
                data: &[1; 16],
            }],
        );
        let two = prepare(
            &resources,
            &[
                Operation::WriteBuffer {
                    destination,
                    offset: 64,
                    data: &[2; 32],
                },
                Operation::WriteBuffer {
                    destination,
                    offset: 128,
                    data: &[3; 12],
                },
            ],
        );
        let empty = prepare(&resources, &[Operation::ResourceBarrier]);
        let mut scratch = ChunkBindings::new(&external);
        assert_eq!(
            scratch.bindings.capacity(),
            0,
            "CPU-only path allocates no bindings"
        );
        let mut saved = Vec::new();
        for (i, chunk) in [&two, &one, &empty, &two, &one].into_iter().enumerate() {
            let token = 100 + i as u64;
            let mut fresh = external.clone();
            for &(id, offset, size) in &chunk.placements {
                fresh.push(BoundObject {
                    object: ObjectRef::Generated(id),
                    attachment_token: token,
                    allocation_offset: offset,
                    size,
                });
            }
            let reused = scratch.for_chunk(chunk, token).unwrap();
            assert_eq!(reused.len(), external.len() + chunk.placements.len());
            let payload = wire::encode(&chunk.compiled, reused).unwrap();
            assert_eq!(payload, wire::encode(&chunk.compiled, &fresh).unwrap());
            saved.push((payload, token));
        }
        // Reusing the CPU workspace cannot mutate already-owned wire payloads.
        for (payload, expected) in saved {
            let decoded = adreno_a6xx_submit_wire::decode(&payload).unwrap();
            for index in 0..decoded.resource_len() {
                let bound = decoded.resource(index).unwrap();
                if bound.attachment_token >= 100 {
                    assert_eq!(bound.attachment_token, expected);
                }
            }
        }
    }

    #[cfg(feature = "programmable")]
    #[test]
    fn native_split_replays_partial_push_constants_bindings_and_instance_state() {
        let initial = [1u8; 16];
        let patch = [9u8; 4];
        let bound = codegen::NativeBinding {
            binding: 3,
            resource: codegen::NativeBindingResource::Buffer {
                object: codegen::ObjectId::new(9),
                offset: 64,
                size: 128,
                write: false,
            },
        };
        let pass = codegen::RenderPass {
            target: codegen::ObjectId::new(1),
            area: PixelRect::new(0, 0, 32, 32).unwrap(),
            load: ir::LoadOp::Load,
            store: ir::StoreOp::DontCare,
            depth: None,
        };
        let operations = [
            Operation::BeginRenderPass(pass),
            Operation::SetBindGroup {
                index: 2,
                bindings: vec![bound.clone()],
            },
            Operation::SetVertexBufferSlot {
                slot: 4,
                buffer: codegen::ObjectId::new(8),
                offset: 16,
            },
            Operation::SetPushConstants {
                stages: ir::ShaderStages::VERTEX | ir::ShaderStages::FRAGMENT,
                offset: 0,
                data: &initial,
            },
            Operation::SetPushConstants {
                stages: ir::ShaderStages::FRAGMENT,
                offset: 4,
                data: &patch,
            },
            Operation::SetDrawIndex(11),
            Operation::DrawInstanced {
                vertex_count: 3,
                first_vertex: 0,
                instance_count: 2,
                first_instance: 7,
            },
            Operation::DrawIndexedInstanced {
                index_count: 6,
                first_index: 3,
                base_vertex: -1,
                instance_count: 4,
                first_instance: 12,
            },
            Operation::EndRenderPass,
        ];
        let split = split_submission_operations(&operations, 1).unwrap();
        assert_eq!(split.len(), 2);
        assert!(matches!(
            split[0][0],
            Operation::BeginRenderPass(codegen::RenderPass {
                store: ir::StoreOp::Store,
                ..
            })
        ));
        assert!(split[1].contains(&Operation::SetBindGroup {
            index: 2,
            bindings: vec![bound]
        }));
        assert!(split[1].contains(&Operation::SetVertexBufferSlot {
            slot: 4,
            buffer: codegen::ObjectId::new(8),
            offset: 16
        }));
        assert!(split[1].contains(&Operation::SetDrawIndex(11)));
        let mut values = [[0u8; 16]; 2];
        for command in &split[1] {
            if let Operation::SetPushConstants {
                stages,
                offset,
                data,
            } = command
            {
                for (i, flag) in [ir::ShaderStages::VERTEX, ir::ShaderStages::FRAGMENT]
                    .into_iter()
                    .enumerate()
                {
                    if stages.contains(flag) {
                        values[i][*offset as usize..*offset as usize + data.len()]
                            .copy_from_slice(data);
                    }
                }
            }
        }
        assert_eq!(values[0], initial);
        let mut expected = initial;
        expected[4..8].copy_from_slice(&patch);
        assert_eq!(values[1], expected);
    }

    #[test]
    fn buffer_copy_chunks_preserve_offsets_and_barrier_order() {
        let source = ObjectId::new(30);
        let destination = ObjectId::new(31);
        let size = 2 * UPLOAD_CHUNK_BYTES as u64 + 4;
        let chunks = split_async_operations(&[
            Operation::CopyBufferToBuffer {
                source,
                source_offset: 64,
                destination,
                destination_offset: 128,
                size,
            },
            Operation::ResourceBarrier,
        ])
        .unwrap();
        assert_eq!(chunks.len(), 4);
        let mut copied = 0;
        for chunk in &chunks[..3] {
            let [
                Operation::CopyBufferToBuffer {
                    source: src,
                    source_offset,
                    destination: dst,
                    destination_offset,
                    size: count,
                },
            ] = chunk.as_slice()
            else {
                panic!("missing copy")
            };
            assert_eq!((*src, *dst), (source, destination));
            assert_eq!(
                (*source_offset, *destination_offset),
                (64 + copied, 128 + copied)
            );
            assert!(*count <= UPLOAD_CHUNK_BYTES as u64);
            copied += count;
        }
        assert_eq!(copied, size);
        assert_eq!(chunks[3], [Operation::ResourceBarrier]);
        for (offset, size) in [(0, 0), (1, 4), (0, 3), (u64::MAX - 3, 8)] {
            assert!(
                split_async_operations(&[Operation::CopyBufferToBuffer {
                    source,
                    source_offset: offset,
                    destination,
                    destination_offset: 0,
                    size,
                }])
                .is_err()
            );
        }
    }

    #[test]
    fn adjacent_batch_preserves_interleaved_upload_copy_barrier_and_original_fallback() {
        let a = ObjectId::new(10);
        let b = ObjectId::new(11);
        let data = [1u8;16];
        let later = [2u8;16];
        let source = vec![
            Operation::WriteBuffer { destination:a, offset:0, data:&data },
            Operation::ResourceBarrier,
            Operation::CopyBufferToBuffer { source:a, source_offset:0, destination:b, destination_offset:0, size:16 },
            Operation::ResourceBarrier,
            Operation::WriteBuffer { destination:a, offset:0, data:&later },
            Operation::CopyBufferToBuffer { source:a, source_offset:0, destination:b, destination_offset:16, size:16 },
        ];
        let original = split_async_operations(&source).unwrap();
        assert_eq!(original.len(), 6);
        let batches = coalesce_async_operations(original.clone()).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].operations, source);
        assert_eq!(batches[0].fallback, original);
        // The real compiler retains BOTH distinct writes to the same buffer.
        // The read/copy between them must see the first bytes, not the last.
        let mut resources = [buffer(a,64),buffer(b,64)];
        for r in &mut resources {r.kind=ResourceKind::Buffer{usage:BufferUsage::COPY_SRC|BufferUsage::COPY_DST};}
        let merged = prepare(&resources, &batches[0].operations);
        let uploads:Vec<_>=merged.compiled.generated_objects.iter()
            .filter(|object| object.kind==GeneratedObjectKind::Upload).map(|object| object.bytes.as_slice()).collect();
        assert_eq!(uploads,[data.as_slice(),later.as_slice()]);
        let external=bindings(&resources);
        let mut scratch=ChunkBindings::new(&external);
        let payload=wire::encode(&merged.compiled,scratch.for_chunk(&merged,100).unwrap()).unwrap();
        let decoded=adreno_a6xx_submit_wire::decode(&payload).unwrap();
        assert_eq!(decoded.pm4_len(),merged.compiled.words.len());
        // A real negotiated wire ceiling can fit every original boundary but
        // reject the candidate. The stored fallback remains exactly reusable.
        let limit=batches[0].fallback.iter().map(|ops| {
            let chunk=prepare(&resources,ops);
            adreno_a6xx_submit_wire::encoded_len_for_counts(chunk.compiled.words.len(),chunk.compiled.accesses.len(),chunk.compiled.fixups.len()).unwrap()
        }).max().unwrap();
        assert!(prepare_chunk(capabilities(),&resources,&[],&batches[0].operations,&bindings(&resources),0,limit).is_err());
        for ops in &batches[0].fallback {
            assert!(prepare_chunk(capabilities(),&resources,&[],ops,&bindings(&resources),0,limit).is_ok());
        }
    }

    #[test]
    fn adjacent_batches_keep_byte_writes_cpu_boundaries_and_hard_limits() {
        let a=ObjectId::new(10);let data=[7u8;16];
        let upload=Operation::WriteBuffer{destination:a,offset:0,data:&data};
        let byte=Operation::WriteBuffer{destination:a,offset:1,data:&data[..3]};
        let ops=vec![upload.clone(),Operation::ResourceBarrier,byte.clone(),Operation::ResourceBarrier,upload.clone()];
        let batches=coalesce_async_operations(split_async_operations(&ops).unwrap()).unwrap();
        assert_eq!(batches.len(),3);
        assert_eq!(batches[1].operations,[byte]);
        assert!(batches[1].fallback.is_empty());
        assert_eq!(batches.iter().flat_map(|b|b.operations.iter().cloned()).collect::<Vec<_>>(),ops);
        let original=vec![vec![Operation::ResourceBarrier];MAX_ADJACENT_CHUNKS+1];
        let limited=coalesce_async_operations(original).unwrap();
        assert_eq!(limited.len(),2);assert_eq!(limited[0].fallback.len(),MAX_ADJACENT_CHUNKS);
        assert!(limited[1].fallback.is_empty());
        let large=vec![vec![Operation::ResourceBarrier;MAX_BATCH_OPERATIONS],vec![Operation::ResourceBarrier]];
        assert_eq!(coalesce_async_operations(large).unwrap().len(),2);
        let draws=vec![vec![Operation::Draw{vertex_count:3,first_vertex:0};MAX_TEXTURED_DRAWS_PER_SUBMIT],vec![Operation::Draw{vertex_count:3,first_vertex:0}]];
        assert_eq!(coalesce_async_operations(draws).unwrap().len(),2);
        let bytes=vec![0;UPLOAD_CHUNK_BYTES];
        let uploads=(0..33).map(|i| vec![Operation::WriteBuffer{destination:a,offset:(i*UPLOAD_CHUNK_BYTES) as u64,data:&bytes}]).collect();
        let limited=coalesce_async_operations(uploads).unwrap();
        assert_eq!(limited.len(),2);
        assert!(coalesce_async_operations(Vec::new()).unwrap().is_empty());
    }

    fn capabilities() -> Capabilities {
        Capabilities::a618(512 * 1024, 64 * 1024)
    }

    fn buffer(id: ObjectId, size: u64) -> ResourceMeta {
        ResourceMeta {
            id,
            size,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::COPY_DST,
            },
        }
    }

    fn image(id: ObjectId, width: u32, height: u32, format: TextureFormat) -> ResourceMeta {
        let stride = (width * 4 + 63) & !63;
        let size = u64::from(stride) * u64::from(height);
        ResourceMeta {
            id,
            size,
            kind: ResourceKind::Image(ImageMeta {
                conversion: None,
                texture: None,
                format,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(width, height).unwrap(),
                usage: TextureUsage::COPY_SRC | TextureUsage::COPY_DST,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride,
                    size,
                }],
            }),
        }
    }

    fn bindings(resources: &[ResourceMeta]) -> Vec<BoundObject> {
        resources
            .iter()
            .map(|resource| BoundObject {
                object: ObjectRef::External(resource.id),
                attachment_token: 10 + u64::from(resource.id.raw()),
                allocation_offset: 0,
                size: resource.size,
            })
            .collect()
    }

    fn prepare(resources: &[ResourceMeta], operations: &[Operation<'_>]) -> PreparedChunk {
        prepare_chunk(
            capabilities(),
            resources,
            &[],
            operations,
            &bindings(resources),
            0,
            adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE,
        )
        .expect("bounded chunk compiles and fits native wire")
        .0
    }

    #[test]
    fn logical_buffer_upload_exceeding_four_arenas_keeps_every_byte_and_offset() {
        let bytes: Vec<_> = (0..4 * UPLOAD_ARENA_BYTES as usize + 12)
            .map(|index| (index % 251) as u8)
            .collect();
        let destination = ObjectId::new(0);
        let operations = [Operation::WriteBuffer {
            destination,
            offset: 68,
            data: &bytes,
        }];
        let chunks = split_async_operations(&operations).unwrap();
        assert!(
            chunks.len() > 8,
            "logical work is independent of native queue depth"
        );
        let mut consumed = 0;
        for chunk in &chunks {
            let [
                Operation::WriteBuffer {
                    destination: actual,
                    offset,
                    data,
                },
            ] = chunk.as_slice()
            else {
                panic!("one bounded upload per transfer chunk")
            };
            assert_eq!(*actual, destination);
            assert_eq!(*offset, 68 + consumed as u64);
            assert!(!data.is_empty() && data.len() <= UPLOAD_CHUNK_BYTES);
            assert_eq!(*data, &bytes[consumed..consumed + data.len()]);
            consumed += data.len();
        }
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn texture_strips_preserve_unpadded_final_row_and_bound_bgra_expansion() {
        const WIDTH: u32 = 257;
        const HEIGHT: u32 = 519;
        const STRIDE: u32 = 272;
        let destination = ObjectId::new(0);
        let source: Vec<_> = (0..((HEIGHT - 1) * STRIDE + WIDTH) as usize)
            .map(|index| (index % 251) as u8)
            .collect();
        let resources = [image(destination, 512, 528, TextureFormat::R8Unorm)];
        let operations = [Operation::WriteTexture {
            destination,
            area: PixelRect::new(3, 5, WIDTH, HEIGHT).unwrap(),
            bytes_per_row: STRIDE,
            data: &source,
        }];
        let chunks = split_async_operations(&operations).unwrap();
        assert!(chunks.len() > 1);
        let mut rows = 0;
        for chunk in chunks {
            let [
                Operation::WriteTexture {
                    area,
                    bytes_per_row,
                    data,
                    ..
                },
            ] = chunk.as_slice()
            else {
                panic!("one texture strip per chunk")
            };
            assert_eq!((area.x(), area.y(), area.width()), (3, 5 + rows, WIDTH));
            assert_eq!(*bytes_per_row, STRIDE);
            assert!(WIDTH as usize * area.height() as usize * 4 <= UPLOAD_CHUNK_BYTES);
            assert_eq!(data.as_ptr(), source[(rows * STRIDE) as usize..].as_ptr());
            let compiled = prepare(&resources, &chunk).compiled;
            let uploads: Vec<_> = compiled
                .generated_objects
                .iter()
                .filter(|object| object.kind == GeneratedObjectKind::Upload)
                .collect();
            assert_eq!(uploads.len(), 1);
            assert_eq!(
                uploads[0].bytes.len(),
                WIDTH as usize * area.height() as usize * 4
            );
            for (local_row, upload) in uploads[0]
                .bytes
                .chunks_exact(WIDTH as usize * 4)
                .enumerate()
            {
                let first = (rows as usize + local_row) * STRIDE as usize;
                for (pixel, &alpha) in upload
                    .chunks_exact(4)
                    .zip(&source[first..first + WIDTH as usize])
                {
                    assert_eq!(pixel, [0, 0, 0, alpha]);
                }
            }
            rows += area.height();
            if rows == HEIGHT {
                assert_eq!(data.len(), ((area.height() - 1) * STRIDE + WIDTH) as usize);
            }
        }
        assert_eq!(rows, HEIGHT);
    }

    #[test]
    fn narrow_texture_rows_are_split_before_native_resource_limit() {
        let destination = ObjectId::new(0);
        for height in [512, 513] {
            // Image stride is 64 bytes: each row adds separate source and
            // destination resources, reaching the wire limit at 512 rows.
            let resources = [image(destination, 1, height, TextureFormat::Bgra8Unorm)];
            let bytes = vec![0x5a; 4 * height as usize];
            let operations = [Operation::WriteTexture {
                destination,
                area: PixelRect::new(0, 0, 1, height).unwrap(),
                bytes_per_row: 4,
                data: &bytes,
            }];
            let chunks = split_async_operations(&operations).unwrap();
            let mut rows = 0;
            for chunk in &chunks {
                let [Operation::WriteTexture { area, .. }] = chunk.as_slice() else {
                    panic!("one strip")
                };
                assert_eq!(area.y(), rows);
                rows += area.height();
                prepare(&resources, chunk);
            }
            assert_eq!(rows, height);
            assert_eq!(chunks.len(), if height == 512 { 1 } else { 2 });
        }
    }

    #[test]
    fn generated_objects_get_disjoint_aligned_arena_ranges_and_reject_overflow() {
        let destination = ObjectId::new(0);
        let resources = [buffer(destination, 128)];
        let first = [1; 12];
        let second = [2; 20];
        let operations = [
            Operation::WriteBuffer {
                destination,
                offset: 0,
                data: &first,
            },
            Operation::WriteBuffer {
                destination,
                offset: 64,
                data: &second,
            },
        ];
        let compiled = codegen::compile(codegen::CompileInput {
            capabilities: capabilities(),
            resources: &resources,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap();
        assert_eq!(compiled.generated_objects.len(), 2);
        let (placements, end) = generated_placements(&compiled, 7).unwrap();
        let mut previous_end = 7;
        for (&(id, offset, size), generated) in placements.iter().zip(&compiled.generated_objects) {
            assert_eq!(id, generated.id);
            assert_eq!(offset % generated.alignment, 0);
            assert!(offset >= previous_end);
            assert_eq!(size, generated.bytes.len() as u64);
            previous_end = offset + size;
        }
        assert_eq!(end, previous_end);
        for initial_end in [UPLOAD_ARENA_BYTES - 64, u64::MAX] {
            assert!(matches!(
                generated_placements(&compiled, initial_end),
                Err(IrSubmitError::SubmissionTooLarge)
            ));
        }
    }

    #[test]
    fn upload_order_and_split_pass_preserve_color_depth_and_latest_bindings() {
        let upload = [7; 16];
        let target = ObjectId::new(0);
        let vertices = ObjectId::new(1);
        let indices = ObjectId::new(2);
        let texture = ObjectId::new(3);
        let scissor = PixelRect::new(1, 2, 8, 8).unwrap();
        let uniforms = ir::DrawUniforms::new(
            ir::Transform::identity(),
            ir::Color::rgba(1.0, 0.5, 0.25, 1.0).unwrap(),
        );
        let sampler = ir::SamplerDesc::new(
            ir::FilterMode::Nearest,
            ir::FilterMode::Nearest,
            ir::AddressMode::ClampToEdge,
            ir::AddressMode::ClampToEdge,
        );
        let pass = codegen::RenderPass {
            target,
            area: PixelRect::new(0, 0, 16, 16).unwrap(),
            load: ir::LoadOp::Clear(ir::Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            store: ir::StoreOp::DontCare,
            depth: Some(codegen::DepthAttachment {
                target: ObjectId::new(4),
                load: ir::DepthLoadOp::Clear(0.25),
                store: ir::StoreOp::DontCare,
            }),
        };
        let latest = vec![
            Operation::SetPipeline(codegen::PipelineId::new(1)),
            Operation::SetVertexBuffer {
                buffer: vertices,
                offset: 16,
            },
            Operation::SetIndexBuffer {
                buffer: indices,
                offset: 4,
                format: ir::IndexFormat::Uint16,
            },
            Operation::SetTexture(texture),
            Operation::SetSampler(sampler),
            Operation::SetUniforms(uniforms),
            Operation::SetViewport(ir::Viewport::new(1.5, 2.5, 8.0, 8.0, 0.25, 0.75).unwrap()),
            Operation::SetScissor(Some(scissor)),
        ];
        let draw = Operation::DrawIndexed {
            index_count: 3,
            first_index: 0,
            base_vertex: 0,
        };
        let mut operations = vec![
            Operation::WriteBuffer {
                destination: vertices,
                offset: 16,
                data: &upload,
            },
            Operation::BeginRenderPass(pass),
            Operation::SetPipeline(codegen::PipelineId::new(0)),
        ];
        operations.extend(latest.clone());
        operations.extend(core::iter::repeat_n(draw.clone(), 513));
        operations.push(Operation::EndRenderPass);
        operations.push(Operation::WriteBuffer {
            destination: vertices,
            offset: 48,
            data: &upload,
        });
        let chunks = split_async_operations(&operations).unwrap();
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], [operations[0].clone()]);
        assert_eq!(chunks[3], [operations.last().unwrap().clone()]);
        let Operation::BeginRenderPass(initial) = chunks[1][0] else {
            panic!("first pass")
        };
        assert_eq!(initial.load, pass.load);
        assert_eq!(initial.store, ir::StoreOp::Store);
        assert_eq!(initial.depth.unwrap().load, pass.depth.unwrap().load);
        assert_eq!(initial.depth.unwrap().store, ir::StoreOp::Store);
        assert_eq!(chunks[1].last(), Some(&Operation::EndRenderPass));
        let Operation::BeginRenderPass(continuation) = chunks[2][0] else {
            panic!("continued pass")
        };
        assert_eq!(continuation.load, ir::LoadOp::Load);
        assert_eq!(continuation.store, ir::StoreOp::DontCare);
        assert_eq!(continuation.depth.unwrap().load, ir::DepthLoadOp::Load);
        assert_eq!(continuation.depth.unwrap().store, ir::StoreOp::DontCare);
        assert_eq!(&chunks[2][1..1 + latest.len()], latest.as_slice());
        assert_eq!(chunks[2].last(), Some(&Operation::EndRenderPass));
        assert_eq!(
            chunks
                .iter()
                .flatten()
                .filter(|operation| **operation == draw)
                .count(),
            513
        );
    }

    #[test]
    fn prepared_texture_write_then_copy_owns_staging_past_borrowed_input_lifetime() {
        const WIDTH: u32 = 1024;
        const HEIGHT: u32 = 513;
        let source = ObjectId::new(0);
        let destination = ObjectId::new(1);
        let resources = [
            image(source, WIDTH, HEIGHT, TextureFormat::Bgra8Unorm),
            image(destination, WIDTH, HEIGHT, TextureFormat::Bgra8Unorm),
        ];
        let area = PixelRect::new(0, 0, WIDTH, HEIGHT).unwrap();
        let prepared: Vec<_> = {
            let pixels = vec![0x6d; WIDTH as usize * HEIGHT as usize * 4];
            let operations = [
                Operation::WriteTexture {
                    destination: source,
                    area,
                    bytes_per_row: WIDTH * 4,
                    data: &pixels,
                },
                Operation::CopyTextureToTexture {
                    source,
                    source_rect: area,
                    destination,
                    destination_rect: area,
                },
            ];
            let chunks = split_async_operations(&operations).unwrap();
            assert!(chunks.len() > 8);
            chunks
                .iter()
                .map(|chunk| prepare(&resources, chunk))
                .collect()
        };
        // The logical source and normalized operations are already gone. A
        // deferred dispatch can still materialize every byte from owned chunks.
        let mut upload_bytes = 0;
        for (index, chunk) in prepared.iter().enumerate() {
            let mut bound = bindings(&resources);
            let mut staging = vec![0; UPLOAD_ARENA_BYTES as usize];
            for (&(id, offset, size), generated) in chunk
                .placements
                .iter()
                .zip(&chunk.compiled.generated_objects)
            {
                assert_eq!(id, generated.id);
                let range = offset as usize..(offset + size) as usize;
                staging[range.clone()].copy_from_slice(&generated.bytes);
                assert_eq!(&staging[range], generated.bytes.as_slice());
                if generated.kind == GeneratedObjectKind::Upload {
                    assert!(generated.bytes.iter().all(|byte| *byte == 0x6d));
                    upload_bytes += generated.bytes.len();
                }
                bound.push(BoundObject {
                    object: ObjectRef::Generated(id),
                    attachment_token: 100 + index as u64,
                    allocation_offset: offset,
                    size,
                });
            }
            let payload = wire::encode(&chunk.compiled, &bound).unwrap();
            assert!(payload.len() <= adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE);
            let decoded = adreno_a6xx_submit_wire::decode(&payload).unwrap();
            assert!(decoded.resource_len() <= adreno_a6xx_submit_wire::MAX_RESOURCES);
            for resource in (0..decoded.resource_len()).map(|i| decoded.resource(i).unwrap()) {
                if resource.attachment_token >= 100 {
                    assert!(resource.range_offset + resource.range_size <= staging.len() as u64);
                }
            }
            if index + 1 == prepared.len() {
                assert!(
                    chunk
                        .compiled
                        .accesses
                        .iter()
                        .any(|access| access.object == ObjectRef::External(source)
                            && access.access.contains(Access::READ))
                );
                assert!(
                    chunk
                        .compiled
                        .accesses
                        .iter()
                        .any(|access| access.object == ObjectRef::External(destination)
                            && access.access.contains(Access::WRITE))
                );
                assert!(
                    !chunk
                        .compiled
                        .generated_objects
                        .iter()
                        .any(|object| object.kind == GeneratedObjectKind::Upload)
                );
            }
        }
        assert_eq!(upload_bytes, WIDTH as usize * HEIGHT as usize * 4);
        assert!(matches!(
            prepare_chunk(
                capabilities(),
                &resources,
                &[],
                &[Operation::CopyTextureToTexture {
                    source,
                    source_rect: area,
                    destination,
                    destination_rect: area
                }],
                &bindings(&resources),
                0,
                1
            ),
            Err(IrSubmitError::SubmissionTooLarge)
        ));
    }
}

#[cfg(all(test,feature="programmable"))]
mod fixed_native_tests {
    #[test]
    fn split_fixed_native_draws_replay_typed_pipeline_and_ordinary_uniform_texture_state() {
        use sgfx_codegen_adreno_a6xx::Operation as O;
        let mut case=crate::fixed_test_fixtures::fixed_native_case(false);
        let last=case.operations.len()-1;
        case.operations.insert(last,O::SetUniforms(crate::ir::DrawUniforms::new(crate::ir::Transform::identity(),crate::ir::Color::rgba(0.5,0.25,0.75,0.5).unwrap())));
        case.operations.insert(last+1,O::Draw {vertex_count:4,first_vertex:0});
        let chunks=super::split_submission_operations(&case.operations,1).unwrap();
        assert_eq!(chunks.len(),2);
        assert!(chunks[1].iter().any(|op|matches!(op,O::SetFixedNativePipeline(_))));
        for operations in chunks {crate::fixed_test_fixtures::Case {resources:case.resources.clone(),operations}.compile().unwrap();}
    }
}
