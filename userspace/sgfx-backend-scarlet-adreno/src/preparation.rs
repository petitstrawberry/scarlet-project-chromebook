//! Pure chunk planning shared by synchronous execution and tracked dispatch.

use crate::wire::BoundObject;
use crate::{IrSubmitError, UnsupportedIrFeature, ir, wire};
use alloc::vec::Vec;
use sgfx_codegen_adreno_a6xx as codegen;

pub(crate) const MAX_TEXTURED_DRAWS_PER_SUBMIT: usize = 512;
pub(crate) const UPLOAD_ARENA_BYTES: u64 = 8 * 1024 * 1024;
const UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

pub(crate) struct PreparedChunk {
    pub(crate) compiled: codegen::RelocatablePm4,
    pub(crate) placements: Vec<(codegen::GeneratedObjectId, u64, u64)>,
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
                | codegen::Operation::WriteTexture { .. }
                | codegen::Operation::CopyTextureToTexture { .. }
        ) {
            let preceding = split_submission_operations(&graphics, 512)?;
            chunks
                .try_reserve(preceding.len())
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            chunks.extend(preceding);
            graphics.clear();
            match operation {
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
    let mut bindings = external.to_vec();
    bindings
        .try_reserve_exact(placements.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for &(id, offset, size) in &placements {
        bindings.push(BoundObject {
            object: codegen::ObjectRef::Generated(id),
            attachment_token: 1,
            allocation_offset: offset,
            size,
        });
    }
    match wire::encode(&compiled, &bindings) {
        Ok(payload) if payload.len() <= native_limit => {}
        Ok(_) | Err(IrSubmitError::SubmitWire(adreno_a6xx_submit_wire::Error::InvalidSize)) => {
            return Err(IrSubmitError::SubmissionTooLarge);
        }
        Err(error) => return Err(error),
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
    use alloc::vec;
    use codegen::{
        Access, Capabilities, GeneratedObjectKind, ImageMeta, ImageModifier, ObjectId, ObjectRef,
        Operation, PlaneLayout, ResourceKind, ResourceMeta,
    };
    use ir::{BufferUsage, Extent2D, PixelRect, TextureFormat, TextureUsage};

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
            assert_eq!(uploads.len(), area.height() as usize);
            for (local_row, upload) in uploads.iter().enumerate() {
                assert_eq!(upload.bytes.len(), WIDTH as usize * 4);
                let first = (rows as usize + local_row) * STRIDE as usize;
                for (pixel, &alpha) in upload
                    .bytes
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
