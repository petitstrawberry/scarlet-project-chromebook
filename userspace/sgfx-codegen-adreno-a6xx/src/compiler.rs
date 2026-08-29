//! Normalized SGFX validation and A618 lowering.

use alloc::{vec, vec::Vec};

use adreno_a6xx_shader_pack::PipelineVariant;
use sgfx_core::ir::{
    AddressMode, BlendState, BufferUsage, CullMode, DepthLoadOp, DrawUniforms, FilterMode,
    FragmentProgram, IndexFormat, LoadOp, PixelRect, SamplerDesc, TextureFormat, TextureSampleMode,
    TextureUsage,
};

use crate::emit::{DrawCall, DrawState, Emitter, IndexedDraw, Surface};
use crate::model::{
    Chip, CompileError, CompileInput, ImageMeta, ImageModifier, ObjectId, Operation, PipelineId,
    PipelineMeta, RelocatablePm4, ResourceKind, ResourceMeta,
};

type VertexAttributes = &'static [(u32, u32)];

#[derive(Clone, Copy)]
struct BufferBinding {
    object: ObjectId,
    offset: u64,
}

#[derive(Clone, Copy)]
struct IndexBinding {
    object: ObjectId,
    offset: u64,
    format: IndexFormat,
}

#[derive(Clone, Copy)]
struct PassState {
    target: ObjectId,
    area: PixelRect,
    has_depth: bool,
}

struct State<'a> {
    pass: Option<PassState>,
    draw_batch_started: bool,
    pipeline: Option<&'a PipelineMeta>,
    vertex: Option<BufferBinding>,
    index: Option<IndexBinding>,
    texture: Option<ObjectId>,
    sampler: Option<SamplerDesc>,
    uniforms: Option<DrawUniforms>,
    scissor: Option<PixelRect>,
}

impl State<'_> {
    fn new() -> Self {
        Self {
            pass: None,
            draw_batch_started: false,
            pipeline: None,
            vertex: None,
            index: None,
            texture: None,
            sampler: None,
            uniforms: None,
            scissor: None,
        }
    }
}

/// Compile normalized SGFX operations into address-free A6xx PM4.
///
/// The returned artifact contains compiler-local symbolic references only.
/// It is intentionally not serialized for any kernel or userspace ABI.
pub fn compile(input: CompileInput<'_>) -> Result<RelocatablePm4, CompileError> {
    validate_tables(input)?;
    let mut emitter = Emitter::new(input.capabilities.max_pm4_words);
    let mut state = State::new();

    for operation in input.operations {
        match operation {
            Operation::WriteBuffer {
                destination,
                offset,
                data,
            } => {
                require_outside_pass(&state)?;
                let resource = find_resource(input.resources, *destination)?;
                require_buffer_usage(resource, BufferUsage::COPY_DST)?;
                let size = u64::try_from(data.len()).map_err(|_| CompileError::Overflow)?;
                validate_range(resource.size, *offset, size)?;
                emitter.upload_buffer(*destination, *offset, size, data)?;
            }
            Operation::WriteTexture {
                destination,
                area,
                bytes_per_row,
                data,
            } => {
                require_outside_pass(&state)?;
                emit_texture_upload(
                    &mut emitter,
                    input.resources,
                    *destination,
                    *area,
                    *bytes_per_row,
                    data,
                    input.capabilities.max_linear_pitch,
                )?;
            }
            Operation::CopyTextureToTexture {
                source,
                source_rect,
                destination,
                destination_rect,
            } => {
                require_outside_pass(&state)?;
                if !source_rect.same_extent(*destination_rect) || source == destination {
                    return Err(CompileError::InvalidResource);
                }
                let source_resource = find_resource(input.resources, *source)?;
                let destination_resource = find_resource(input.resources, *destination)?;
                let source_surface = require_target_surface(
                    source_resource,
                    TextureUsage::COPY_SRC,
                    input.capabilities.max_linear_pitch,
                )?;
                let destination_surface = require_target_surface(
                    destination_resource,
                    TextureUsage::COPY_DST,
                    input.capabilities.max_linear_pitch,
                )?;
                validate_rect(*source_rect, source_surface)?;
                validate_rect(*destination_rect, destination_surface)?;
                emitter.copy(
                    source_surface,
                    *source_rect,
                    destination_surface,
                    *destination_rect,
                )?;
            }
            Operation::BeginRenderPass(pass) => {
                if state.pass.is_some() {
                    return Err(CompileError::InvalidState);
                }
                let target_resource = find_resource(input.resources, pass.target)?;
                let target = require_target_surface(
                    target_resource,
                    TextureUsage::RENDER_ATTACHMENT,
                    input.capabilities.max_linear_pitch,
                )?;
                validate_rect(pass.area, target)?;
                if let Some(depth) = pass.depth {
                    let depth_resource = find_resource(input.resources, depth.target)?;
                    require_depth_image(depth_resource, target.width, target.height)?;
                    if matches!(depth.load, DepthLoadOp::Clear(_)) {
                        return Err(CompileError::UnsupportedFeature);
                    }
                }
                if let LoadOp::Clear(color) = pass.load {
                    emitter.clear(target, pass.area, color)?;
                }
                state.pass = Some(PassState {
                    target: pass.target,
                    area: pass.area,
                    has_depth: pass.depth.is_some(),
                });
                state.draw_batch_started = false;
                state.pipeline = None;
                state.vertex = None;
                state.index = None;
                state.texture = None;
                state.sampler = None;
                state.uniforms = None;
                state.scissor = None;
            }
            Operation::EndRenderPass => {
                if state.pass.take().is_none() {
                    return Err(CompileError::InvalidState);
                }
                if state.draw_batch_started {
                    emitter.end_draw_batch()?;
                    state.draw_batch_started = false;
                }
                state.pipeline = None;
                state.vertex = None;
                state.index = None;
                state.texture = None;
                state.sampler = None;
                state.uniforms = None;
                state.scissor = None;
            }
            Operation::SetPipeline(id) => {
                require_inside_pass(&state)?;
                state.pipeline = Some(find_pipeline(input.pipelines, *id)?);
            }
            Operation::SetVertexBuffer { buffer, offset } => {
                require_inside_pass(&state)?;
                let resource = find_resource(input.resources, *buffer)?;
                require_buffer_usage(resource, BufferUsage::VERTEX)?;
                if *offset >= resource.size {
                    return Err(CompileError::OutOfBounds);
                }
                state.vertex = Some(BufferBinding {
                    object: *buffer,
                    offset: *offset,
                });
            }
            Operation::SetIndexBuffer {
                buffer,
                offset,
                format,
            } => {
                require_inside_pass(&state)?;
                let resource = find_resource(input.resources, *buffer)?;
                require_buffer_usage(resource, BufferUsage::INDEX)?;
                if *offset >= resource.size || *offset % format.byte_size() != 0 {
                    return Err(CompileError::OutOfBounds);
                }
                state.index = Some(IndexBinding {
                    object: *buffer,
                    offset: *offset,
                    format: *format,
                });
            }
            Operation::SetTexture(texture) => {
                require_inside_pass(&state)?;
                let resource = find_resource(input.resources, *texture)?;
                require_image_surface(
                    resource,
                    TextureUsage::SAMPLED,
                    input.capabilities.max_linear_pitch,
                )?;
                state.texture = Some(*texture);
            }
            Operation::SetSampler(sampler) => {
                require_inside_pass(&state)?;
                state.sampler = Some(*sampler);
            }
            Operation::SetUniforms(uniforms) => {
                require_inside_pass(&state)?;
                state.uniforms = Some(*uniforms);
            }
            Operation::SetScissor(scissor) => {
                let pass = require_inside_pass(&state)?;
                if scissor.is_some_and(|rect| !rect_within(rect, pass.area)) {
                    return Err(CompileError::OutOfBounds);
                }
                state.scissor = *scissor;
            }
            Operation::Draw {
                vertex_count,
                first_vertex,
            } => {
                begin_draw_batch_if_needed(&mut emitter, &mut state)?;
                emit_draw(
                    &mut emitter,
                    input,
                    &state,
                    DrawCall::NonIndexed {
                        vertex_count: *vertex_count,
                        first_vertex: *first_vertex,
                    },
                )?;
            }
            Operation::DrawIndexed {
                index_count,
                first_index,
                base_vertex,
            } => {
                begin_draw_batch_if_needed(&mut emitter, &mut state)?;
                emit_draw_indexed(
                    &mut emitter,
                    input,
                    &state,
                    *index_count,
                    *first_index,
                    *base_vertex,
                )?;
            }
        }
    }

    if state.pass.is_some() {
        return Err(CompileError::InvalidState);
    }
    emitter.finish()
}

fn begin_draw_batch_if_needed(
    emitter: &mut Emitter,
    state: &mut State<'_>,
) -> Result<(), CompileError> {
    require_inside_pass(state)?;
    if !state.draw_batch_started {
        emitter.begin_draw_batch()?;
        state.draw_batch_started = true;
    } else {
        emitter.continue_draw_batch()?;
    }
    Ok(())
}

fn validate_tables(input: CompileInput<'_>) -> Result<(), CompileError> {
    if !matches!(input.capabilities.chip, Chip::A618)
        || input.capabilities.gmem_size == 0
        || input.capabilities.max_pm4_words == 0
        || input.capabilities.max_linear_pitch == 0
    {
        return Err(CompileError::InvalidResource);
    }
    for (index, resource) in input.resources.iter().enumerate() {
        if resource.size == 0
            || input.resources[..index]
                .iter()
                .any(|other| other.id == resource.id)
        {
            return Err(CompileError::InvalidIdentity);
        }
        if let ResourceKind::Image(image) = &resource.kind {
            validate_image_layout(resource.size, image, input.capabilities.max_linear_pitch)?;
        }
    }
    for (index, pipeline) in input.pipelines.iter().enumerate() {
        if input.pipelines[..index]
            .iter()
            .any(|other| other.id == pipeline.id)
        {
            return Err(CompileError::InvalidIdentity);
        }
        validate_pipeline_descriptor(pipeline)?;
    }
    Ok(())
}

fn find_resource(resources: &[ResourceMeta], id: ObjectId) -> Result<&ResourceMeta, CompileError> {
    resources
        .iter()
        .find(|resource| resource.id == id)
        .ok_or(CompileError::InvalidIdentity)
}

fn find_pipeline(
    pipelines: &[PipelineMeta],
    id: PipelineId,
) -> Result<&PipelineMeta, CompileError> {
    pipelines
        .iter()
        .find(|pipeline| pipeline.id == id)
        .ok_or(CompileError::InvalidIdentity)
}

fn require_buffer_usage(
    resource: &ResourceMeta,
    required: BufferUsage,
) -> Result<(), CompileError> {
    match resource.kind {
        ResourceKind::Buffer { usage } if usage.contains(required) => Ok(()),
        _ => Err(CompileError::InvalidResource),
    }
}

fn validate_image_layout(
    allocation_size: u64,
    image: &ImageMeta,
    max_pitch: u32,
) -> Result<(), CompileError> {
    if image.modifier != ImageModifier::Linear || image.planes.len() != 1 {
        return Err(CompileError::UnsupportedFeature);
    }
    let plane = image.planes[0];
    let row_bytes = image
        .extent
        .width()
        .checked_mul(image.storage_format.bytes_per_pixel())
        .ok_or(CompileError::Overflow)?;
    if plane.stride < row_bytes || plane.stride > max_pitch || plane.stride & 63 != 0 {
        return Err(CompileError::InvalidResource);
    }
    let required = u64::from(plane.stride)
        .checked_mul(u64::from(image.extent.height() - 1))
        .and_then(|bytes| bytes.checked_add(u64::from(row_bytes)))
        .ok_or(CompileError::Overflow)?;
    let plane_end = plane
        .offset
        .checked_add(plane.size)
        .ok_or(CompileError::Overflow)?;
    if plane.size < required || plane_end > allocation_size {
        return Err(CompileError::OutOfBounds);
    }
    Ok(())
}

fn require_image_surface(
    resource: &ResourceMeta,
    required: TextureUsage,
    max_pitch: u32,
) -> Result<Surface, CompileError> {
    let ResourceKind::Image(image) = &resource.kind else {
        return Err(CompileError::InvalidResource);
    };
    if !image.usage.contains(required) || image.storage_format != TextureFormat::Bgra8Unorm {
        return Err(CompileError::InvalidResource);
    }
    validate_image_layout(resource.size, image, max_pitch)?;
    let plane = image.planes[0];
    Ok(Surface {
        object: resource.id,
        plane_offset: plane.offset,
        plane_size: plane.size,
        width: image.extent.width(),
        height: image.extent.height(),
        stride: plane.stride,
        alpha_mask: image.format == TextureFormat::R8Unorm,
    })
}

fn require_target_surface(
    resource: &ResourceMeta,
    required: TextureUsage,
    max_pitch: u32,
) -> Result<Surface, CompileError> {
    let ResourceKind::Image(image) = &resource.kind else {
        return Err(CompileError::InvalidResource);
    };
    if image.format != TextureFormat::Bgra8Unorm {
        return Err(CompileError::InvalidResource);
    }
    require_image_surface(resource, required, max_pitch)
}

fn require_depth_image(
    resource: &ResourceMeta,
    width: u32,
    height: u32,
) -> Result<(), CompileError> {
    let ResourceKind::Image(image) = &resource.kind else {
        return Err(CompileError::InvalidResource);
    };
    if image.format != TextureFormat::Depth32Float
        || !image.usage.contains(TextureUsage::RENDER_ATTACHMENT)
        || image.extent.width() != width
        || image.extent.height() != height
    {
        return Err(CompileError::InvalidResource);
    }
    Ok(())
}

fn validate_rect(rect: PixelRect, surface: Surface) -> Result<(), CompileError> {
    if rect
        .x()
        .checked_add(rect.width())
        .is_none_or(|end| end > surface.width)
        || rect
            .y()
            .checked_add(rect.height())
            .is_none_or(|end| end > surface.height)
        || rect.x() > 0x7fff
        || rect.y() > 0x7fff
        || rect.x() + rect.width() - 1 > 0x7fff
        || rect.y() + rect.height() - 1 > 0x7fff
    {
        return Err(CompileError::OutOfBounds);
    }
    Ok(())
}

fn validate_range(size: u64, offset: u64, length: u64) -> Result<(), CompileError> {
    if length == 0 || offset.checked_add(length).is_none_or(|end| end > size) {
        Err(CompileError::OutOfBounds)
    } else {
        Ok(())
    }
}

fn require_inside_pass(state: &State<'_>) -> Result<PassState, CompileError> {
    state.pass.ok_or(CompileError::InvalidState)
}

fn require_outside_pass(state: &State<'_>) -> Result<(), CompileError> {
    if state.pass.is_some() {
        Err(CompileError::InvalidState)
    } else {
        Ok(())
    }
}

fn require_draw_state<'a>(
    state: &State<'a>,
) -> Result<(PassState, &'a PipelineMeta, BufferBinding, DrawUniforms), CompileError> {
    Ok((
        state.pass.ok_or(CompileError::InvalidState)?,
        state.pipeline.ok_or(CompileError::InvalidState)?,
        state.vertex.ok_or(CompileError::InvalidState)?,
        state.uniforms.ok_or(CompileError::InvalidState)?,
    ))
}

fn emit_draw(
    emitter: &mut Emitter,
    input: CompileInput<'_>,
    state: &State<'_>,
    draw: DrawCall,
) -> Result<(), CompileError> {
    let (pass, pipeline, vertex, uniforms) = require_draw_state(state)?;
    let (vertex_size, draw_count) = match draw {
        DrawCall::NonIndexed {
            vertex_count,
            first_vertex,
        } => {
            let end_vertex = first_vertex
                .checked_add(vertex_count)
                .ok_or(CompileError::Overflow)?;
            (
                u64::from(end_vertex)
                    .checked_mul(u64::from(pipeline.descriptor.vertex_buffer().stride()))
                    .ok_or(CompileError::Overflow)?,
                vertex_count,
            )
        }
        DrawCall::Indexed(indexed) => (indexed.vertex_size, indexed.index_count),
    };
    if draw_count == 0
        || pass.has_depth
        || pipeline.descriptor.depth_state().is_some()
        || !draw_count.is_multiple_of(3)
    {
        return Err(CompileError::UnsupportedFeature);
    }
    let target_resource = find_resource(input.resources, pass.target)?;
    let target = require_target_surface(
        target_resource,
        TextureUsage::RENDER_ATTACHMENT,
        input.capabilities.max_linear_pitch,
    )?;
    if pipeline.descriptor.target_format() != TextureFormat::Bgra8Unorm {
        return Err(CompileError::InvalidResource);
    }
    let linear_sampler = validate_sample_state(input.resources, state, pipeline)?;
    let vertex_resource = find_resource(input.resources, vertex.object)?;
    require_buffer_usage(vertex_resource, BufferUsage::VERTEX)?;
    let stride = pipeline.descriptor.vertex_buffer().stride();
    validate_range(vertex_resource.size, vertex.offset, vertex_size)?;
    u32::try_from(vertex_size).map_err(|_| CompileError::OutOfBounds)?;
    if state
        .scissor
        .is_some_and(|scissor| !rect_within(scissor, pass.area))
    {
        return Err(CompileError::OutOfBounds);
    }
    let variant = pipeline_variant(&pipeline.descriptor)?;
    let (attributes, source_over) = draw_fixed_state(&pipeline.descriptor)?;
    let texture = match pipeline.descriptor.fragment() {
        FragmentProgram::Texture(_) | FragmentProgram::TextureVertexColor(_) => {
            let id = state.texture.ok_or(CompileError::InvalidState)?;
            let resource = find_resource(input.resources, id)?;
            Some(require_image_surface(
                resource,
                TextureUsage::SAMPLED,
                input.capabilities.max_linear_pitch,
            )?)
        }
        FragmentProgram::Solid | FragmentProgram::VertexColor => None,
    };
    let mut uniform_words = [0_u32; 20];
    for (destination, value) in uniform_words[..16]
        .iter_mut()
        .zip(uniforms.transform().columns())
    {
        *destination = value.to_bits();
    }
    for (destination, value) in uniform_words[16..]
        .iter_mut()
        .zip(uniforms.color().components())
    {
        *destination = value.to_bits();
    }
    let raster = pipeline.descriptor.raster();
    let cull = match raster.cull_mode() {
        CullMode::None => 0,
        CullMode::Front => 1,
        CullMode::Back => 2,
    } | if raster.front_face() == sgfx_core::ir::FrontFace::Clockwise {
        4
    } else {
        0
    };
    emitter.draw(DrawState {
        variant,
        target,
        area: pass.area,
        scissor: state.scissor.unwrap_or(pass.area),
        vertex: vertex.object,
        vertex_offset: vertex.offset,
        vertex_size,
        stride,
        attributes,
        uniforms: uniform_words,
        texture,
        linear_sampler,
        source_over,
        cull,
        draw,
    })
}

fn emit_draw_indexed(
    emitter: &mut Emitter,
    input: CompileInput<'_>,
    state: &State<'_>,
    index_count: u32,
    first_index: u32,
    base_vertex: i32,
) -> Result<(), CompileError> {
    let (_, pipeline, vertex, _) = require_draw_state(state)?;
    let index = state.index.ok_or(CompileError::InvalidState)?;
    if base_vertex < 0 {
        // Without inspecting untrusted index contents, a negative base could
        // address bytes before the authorized VFD buffer base.  Keep the
        // initial A618 dialect safely bounded to non-negative base vertices.
        return Err(CompileError::UnsupportedFeature);
    }
    let index_resource = find_resource(input.resources, index.object)?;
    require_buffer_usage(index_resource, BufferUsage::INDEX)?;
    let index_element_size = index.format.byte_size();
    let available_index_bytes = index_resource
        .size
        .checked_sub(index.offset)
        .ok_or(CompileError::OutOfBounds)?;
    let available_indices = available_index_bytes / index_element_size;
    let max_indices = u32::try_from(available_indices).map_err(|_| CompileError::OutOfBounds)?;
    let end_index = first_index
        .checked_add(index_count)
        .ok_or(CompileError::Overflow)?;
    if index_count == 0 || end_index > max_indices {
        return Err(CompileError::OutOfBounds);
    }
    let index_size = u64::from(max_indices)
        .checked_mul(index_element_size)
        .ok_or(CompileError::Overflow)?;

    let vertex_resource = find_resource(input.resources, vertex.object)?;
    require_buffer_usage(vertex_resource, BufferUsage::VERTEX)?;
    let vertex_size = vertex_resource
        .size
        .checked_sub(vertex.offset)
        .ok_or(CompileError::OutOfBounds)?;
    let stride = u64::from(pipeline.descriptor.vertex_buffer().stride());
    let base_vertex = u32::try_from(base_vertex).map_err(|_| CompileError::UnsupportedFeature)?;
    let minimum_vertex_bytes = u64::from(base_vertex)
        .checked_add(1)
        .and_then(|count| count.checked_mul(stride))
        .ok_or(CompileError::Overflow)?;
    if minimum_vertex_bytes > vertex_size {
        return Err(CompileError::OutOfBounds);
    }

    emit_draw(
        emitter,
        input,
        state,
        DrawCall::Indexed(IndexedDraw {
            index: index.object,
            index_offset: index.offset,
            index_size,
            format: index.format,
            index_count,
            first_index,
            base_vertex,
            max_indices,
            vertex_size,
        }),
    )
}

fn pipeline_variant(
    descriptor: &sgfx_core::ir::RenderPipelineDesc,
) -> Result<PipelineVariant, CompileError> {
    use FragmentProgram::*;
    use TextureSampleMode::*;
    Ok(
        match (descriptor.vertex_buffer().stride(), descriptor.fragment()) {
            (16, Solid) => PipelineVariant::Stride16Solid,
            (16, Texture(Rgba)) => PipelineVariant::Stride16TextureRgba,
            (16, Texture(AlphaMask)) => PipelineVariant::Stride16TextureAlphaMask,
            (40, Solid) => PipelineVariant::Stride40Solid,
            (40, VertexColor) => PipelineVariant::Stride40VertexColor,
            (40, TextureVertexColor(Rgba | AlphaMask)) => {
                // R8 mask descriptors force sampled RGB to one and retain the
                // logical mask in alpha. The canonical RGBA shader therefore
                // implements vertex-colored alpha-mask sampling exactly.
                PipelineVariant::Stride40TextureVertexColorRgba
            }
            (24, Solid) => PipelineVariant::Stride24Solid,
            (24, Texture(Rgba)) => PipelineVariant::Stride24TextureRgba,
            (24, Texture(RgbIgnoreAlpha)) => PipelineVariant::Stride24TextureRgbIgnoreAlpha,
            (24, Texture(AlphaMask)) => PipelineVariant::Stride24TextureAlphaMask,
            (28, VertexColor) => PipelineVariant::Stride28VertexColor,
            (32, Solid) => PipelineVariant::Stride32Solid,
            (32, VertexColor) => PipelineVariant::Stride32VertexColor,
            _ => return Err(CompileError::UnsupportedFeature),
        },
    )
}

fn draw_fixed_state(
    descriptor: &sgfx_core::ir::RenderPipelineDesc,
) -> Result<(VertexAttributes, bool), CompileError> {
    const POS2: &[(u32, u32)] = &[(0x67, 0)];
    const POS2_UV2: &[(u32, u32)] = &[(0x67, 0), (0x67, 8)];
    const POS4: &[(u32, u32)] = &[(0x82, 0)];
    const POS4_UV2: &[(u32, u32)] = &[(0x82, 0), (0x67, 16)];
    const POS4_COLOR3: &[(u32, u32)] = &[(0x82, 0), (0x74, 16)];
    const POS4_COLOR4: &[(u32, u32)] = &[(0x82, 0), (0x82, 16)];
    const POS4_COLOR4_UV2: &[(u32, u32)] = &[(0x82, 0), (0x82, 16), (0x67, 32)];
    let attributes = match (descriptor.vertex_buffer().stride(), descriptor.fragment()) {
        (16, FragmentProgram::Solid) => POS2,
        (16, _) => POS2_UV2,
        (24, FragmentProgram::Solid)
        | (32, FragmentProgram::Solid)
        | (40, FragmentProgram::Solid) => POS4,
        (24, _) => POS4_UV2,
        (28, _) => POS4_COLOR3,
        (32, FragmentProgram::VertexColor) => POS4_COLOR4,
        (40, FragmentProgram::VertexColor) => POS4_COLOR4,
        (40, _) => POS4_COLOR4_UV2,
        _ => return Err(CompileError::UnsupportedFeature),
    };
    Ok((
        attributes,
        descriptor.blend() == BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
    ))
}

fn validate_sample_state(
    resources: &[ResourceMeta],
    state: &State<'_>,
    pipeline: &PipelineMeta,
) -> Result<bool, CompileError> {
    let fragment = pipeline.descriptor.fragment();
    let sample_mode = match fragment {
        FragmentProgram::Solid | FragmentProgram::VertexColor => return Ok(false),
        FragmentProgram::Texture(mode) | FragmentProgram::TextureVertexColor(mode) => mode,
    };
    let texture = state.texture.ok_or(CompileError::InvalidState)?;
    let sampler = state.sampler.ok_or(CompileError::InvalidState)?;
    if sampler.min_filter() != sampler.mag_filter()
        || sampler.address_u() != AddressMode::ClampToEdge
        || sampler.address_v() != AddressMode::ClampToEdge
    {
        return Err(CompileError::UnsupportedFeature);
    }
    let ResourceKind::Image(image) = &find_resource(resources, texture)?.kind else {
        return Err(CompileError::InvalidResource);
    };
    let format_matches = match sample_mode {
        TextureSampleMode::Rgba | TextureSampleMode::RgbIgnoreAlpha => matches!(
            image.format,
            TextureFormat::Bgra8Unorm | TextureFormat::Rgba8Unorm
        ),
        TextureSampleMode::AlphaMask => image.format == TextureFormat::R8Unorm,
    };
    if !format_matches
        || image.storage_format != TextureFormat::Bgra8Unorm
        || !image.usage.contains(TextureUsage::SAMPLED)
    {
        return Err(CompileError::InvalidResource);
    }
    Ok(sampler.min_filter() == FilterMode::Linear)
}

fn emit_texture_upload(
    emitter: &mut Emitter,
    resources: &[ResourceMeta],
    destination: ObjectId,
    area: PixelRect,
    bytes_per_row: u32,
    data: &[u8],
    max_pitch: u32,
) -> Result<(), CompileError> {
    let resource = find_resource(resources, destination)?;
    let surface = require_image_surface(resource, TextureUsage::COPY_DST, max_pitch)?;
    validate_rect(area, surface)?;
    let ResourceKind::Image(image) = &resource.kind else {
        return Err(CompileError::InvalidResource);
    };
    let logical_row_size = area
        .width()
        .checked_mul(image.format.bytes_per_pixel())
        .ok_or(CompileError::Overflow)?;
    if bytes_per_row < logical_row_size {
        return Err(CompileError::OutOfBounds);
    }
    let required_data = u64::from(bytes_per_row)
        .checked_mul(u64::from(area.height() - 1))
        .and_then(|bytes| bytes.checked_add(u64::from(logical_row_size)))
        .ok_or(CompileError::Overflow)?;
    if required_data > data.len() as u64 {
        return Err(CompileError::OutOfBounds);
    }
    for row in 0..area.height() {
        let source_offset = usize::try_from(
            u64::from(row)
                .checked_mul(u64::from(bytes_per_row))
                .ok_or(CompileError::Overflow)?,
        )
        .map_err(|_| CompileError::Overflow)?;
        let source_end = source_offset
            .checked_add(logical_row_size as usize)
            .ok_or(CompileError::Overflow)?;
        let physical_row = convert_upload_row(
            image.format,
            image.storage_format,
            &data[source_offset..source_end],
        )?;
        let destination_offset = surface
            .plane_offset
            .checked_add(
                u64::from(area.y() + row)
                    .checked_mul(u64::from(surface.stride))
                    .ok_or(CompileError::Overflow)?,
            )
            .and_then(|offset| {
                offset.checked_add(
                    u64::from(area.x()) * u64::from(image.storage_format.bytes_per_pixel()),
                )
            })
            .ok_or(CompileError::Overflow)?;
        emitter.upload_buffer(
            destination,
            destination_offset,
            physical_row.len() as u64,
            &physical_row,
        )?;
    }
    Ok(())
}

fn convert_upload_row(
    logical: TextureFormat,
    storage: TextureFormat,
    source: &[u8],
) -> Result<Vec<u8>, CompileError> {
    if storage != TextureFormat::Bgra8Unorm {
        return Err(CompileError::UnsupportedFeature);
    }
    match logical {
        TextureFormat::Bgra8Unorm => Ok(source.to_vec()),
        TextureFormat::Rgba8Unorm => {
            if source.len() & 3 != 0 {
                return Err(CompileError::InvalidResource);
            }
            let mut converted = vec![0; source.len()];
            for (source, destination) in source.chunks_exact(4).zip(converted.chunks_exact_mut(4)) {
                destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
            }
            Ok(converted)
        }
        TextureFormat::R8Unorm => {
            let length = source.len().checked_mul(4).ok_or(CompileError::Overflow)?;
            let mut converted = vec![0; length];
            for (&alpha, destination) in source.iter().zip(converted.chunks_exact_mut(4)) {
                destination.copy_from_slice(&[0, 0, 0, alpha]);
            }
            Ok(converted)
        }
        TextureFormat::Depth32Float => Err(CompileError::UnsupportedFeature),
    }
}

fn validate_pipeline_descriptor(pipeline: &PipelineMeta) -> Result<(), CompileError> {
    use sgfx_core::ir::{FrontFace, PrimitiveTopology, TextureSampleMode, VertexFormat};

    let descriptor = &pipeline.descriptor;
    if descriptor.target_format() != TextureFormat::Bgra8Unorm
        || descriptor.topology() != PrimitiveTopology::TriangleList
        || descriptor.depth_state().is_some()
        || !matches!(descriptor.vertex_buffer().stride(), 16 | 24 | 28 | 32 | 40)
    {
        return Err(CompileError::UnsupportedFeature);
    }
    let attributes = descriptor.vertex_buffer().attributes();
    let find = |location| {
        attributes
            .iter()
            .find(|attribute| attribute.location() == location)
            .map(|attribute| (attribute.format(), attribute.offset()))
    };
    let layout_ok = match (descriptor.vertex_buffer().stride(), descriptor.fragment()) {
        (16, FragmentProgram::Solid) => find(0) == Some((VertexFormat::Float32x2, 0)),
        (16, FragmentProgram::Texture(TextureSampleMode::Rgba | TextureSampleMode::AlphaMask)) => {
            find(0) == Some((VertexFormat::Float32x2, 0))
                && find(1) == Some((VertexFormat::Float32x2, 8))
        }
        (40, FragmentProgram::Solid) => find(0) == Some((VertexFormat::Float32x4, 0)),
        (40, FragmentProgram::VertexColor) => {
            find(0) == Some((VertexFormat::Float32x4, 0))
                && find(1) == Some((VertexFormat::Float32x4, 16))
        }
        (
            40,
            FragmentProgram::TextureVertexColor(
                TextureSampleMode::Rgba | TextureSampleMode::AlphaMask,
            ),
        ) => {
            find(0) == Some((VertexFormat::Float32x4, 0))
                && find(1) == Some((VertexFormat::Float32x4, 16))
                && find(2) == Some((VertexFormat::Float32x2, 32))
        }
        (24, FragmentProgram::Solid) => find(0) == Some((VertexFormat::Float32x4, 0)),
        (
            24,
            FragmentProgram::Texture(
                TextureSampleMode::Rgba
                | TextureSampleMode::RgbIgnoreAlpha
                | TextureSampleMode::AlphaMask,
            ),
        ) => {
            find(0) == Some((VertexFormat::Float32x4, 0))
                && find(1) == Some((VertexFormat::Float32x2, 16))
        }
        (28, FragmentProgram::VertexColor) => {
            find(0) == Some((VertexFormat::Float32x4, 0))
                && find(1) == Some((VertexFormat::Float32x3, 16))
        }
        (32, FragmentProgram::Solid) => find(0) == Some((VertexFormat::Float32x4, 0)),
        (32, FragmentProgram::VertexColor) => {
            find(0) == Some((VertexFormat::Float32x4, 0))
                && find(1) == Some((VertexFormat::Float32x4, 16))
        }
        _ => false,
    };
    let fixed_state_ok = match descriptor.vertex_buffer().stride() {
        16 | 24 | 32 => {
            descriptor.blend() == BlendState::SOURCE_OVER_STRAIGHT_ALPHA
                && descriptor.raster().cull_mode() == CullMode::None
                && descriptor.raster().front_face() == FrontFace::CounterClockwise
        }
        40 => {
            descriptor.blend() == BlendState::SOURCE_OVER_STRAIGHT_ALPHA
                && matches!(
                    descriptor.raster().cull_mode(),
                    CullMode::None | CullMode::Back
                )
                && descriptor.raster().front_face() == FrontFace::CounterClockwise
        }
        28 => descriptor.blend() == BlendState::REPLACE,
        _ => false,
    };
    if layout_ok && fixed_state_ok {
        Ok(())
    } else {
        Err(CompileError::UnsupportedFeature)
    }
}

fn rect_within(inner: PixelRect, outer: PixelRect) -> bool {
    inner.x() >= outer.x()
        && inner.y() >= outer.y()
        && inner.x() + inner.width() <= outer.x() + outer.width()
        && inner.y() + inner.height() <= outer.y() + outer.height()
}
