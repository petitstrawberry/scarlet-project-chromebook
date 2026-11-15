//! Native IR3 pipeline state for A618. Register policy follows Mesa 3f1b217b.
//! Shader allocations are caller-owned and cached; no fixed shader substitution.
use super::*;
#[path = "native_builtin.rs"]
mod builtin;
#[path = "native_color.rs"]
mod color;
#[path="native_ycbcr.rs"]
mod ycbcr;
pub use ycbcr::{nv12_profile,nv12_sampler_profile};
#[path = "native_storage.rs"]
mod storage;
use alloc::{sync::Arc, vec};
pub use color::{NativeColorAttachment, NativeRenderPass};
use sgfx_core::ir::{self, ShaderStages};
use sgfx_shader_ir3::{CompiledShader, Metadata, Stage};

#[derive(Clone, Debug, PartialEq)]
pub struct NativeShader {
    pub private_memory: Option<crate::NativePrivateMemory>,
    pub object: ObjectId,
    pub artifact: Arc<CompiledShader>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct NativeRenderPipeline {
    pub descriptor: ir::ProgrammableRenderPipelineDesc,
    pub vertex: NativeShader,
    pub fragment: NativeShader,
}
#[derive(Clone, Debug, PartialEq)]
pub struct NativeComputePipeline {
    pub descriptor: ir::ComputePipelineDesc,
    pub shader: NativeShader,
}
#[derive(Clone, Debug, PartialEq)]
pub struct NativeBinding {
    pub binding: u32,
    pub resource: NativeBindingResource,
}
#[derive(Clone, Debug, PartialEq)]
pub enum NativeBindingResource {
    Buffer {
        object: ObjectId,
        offset: u64,
        size: u64,
        write: bool,
    },
    Texture {
        object: ObjectId,
        view: Option<ir::TextureViewDesc>,
    },
    /// Raw combined NV12 descriptor. Only a shader with actual YCbCr
    /// conversion may use this internal binding; ordinary textures reject it.
    Nv12Texture(ObjectId),
    Sampler(ir::SamplerDesc),
}

use crate::compiler::{self as c, State};
use crate::model::{CompileInput, Operation, ResourceKind};

#[derive(Clone)]
pub(crate) struct NativeState {
    fixed: bool,
    pub(crate) compute_pass: bool,
    render: Option<Arc<NativeRenderPipeline>>,
    compute: Option<Arc<NativeComputePipeline>>,
    groups: Vec<(u32, Vec<NativeBinding>)>,
    push: [[u8; 128]; 3],
    vertices: Vec<(u32, ObjectId, u64)>,
    draw_index: u32,
    colors: Option<Vec<NativeColorAttachment>>,
}

impl Default for NativeState {
    fn default() -> Self {
        Self {
            fixed: false,
            compute_pass: false,
            render: None,
            compute: None,
            groups: Vec::new(),
            push: [[0; 128]; 3],
            vertices: Vec::new(),
            draw_index: 0,
            colors: None,
        }
    }
}

fn unsupported(reason: &'static str) -> CompileError {
    CompileError::UnsupportedFeatureReason(reason)
}
const INVALID_REG: u32 = 252;
fn sys(m: &Metadata, slot: u32) -> u32 {
    m.inputs
        .iter()
        .find(|v| v.sysval && v.slot == slot)
        .map_or(INVALID_REG, |v| v.regid)
}
fn packed4(v: [u32; 4]) -> u32 {
    v[0] | v[1] << 8 | v[2] << 16 | v[3] << 24
}
fn stage_index(stage: Stage) -> usize {
    stage as usize
}
fn stage_load(stage: Stage) -> (u8, u32, u32) {
    match stage {
        Stage::Vertex => (0x32, 8, 0),
        Stage::Fragment => (0x34, 12, 4),
        Stage::Compute => (0x34, 13, 5),
    }
}

fn validate_shader(shader: &NativeShader, stage: Stage) -> Result<(), CompileError> {
    let a = &shader.artifact;
    let m = &a.metadata;
    if a.stage != stage
        || m.abi_version != 1
        || m.gpu_id != 618
        || a.allocation.is_empty()
        || a.allocation.len() != m.allocation_bytes as usize
        || a.allocation.len() % 128 != 0
        || m.sizedwords == 0
        || m.instrlen == 0
        || m.instrlen > 1023
        || u64::from(m.instrlen) * 128 > u64::from(m.allocation_bytes)
        || u64::from(m.sizedwords) * 4 > u64::from(m.allocation_bytes)
        || m.constlen > 256
        || m.max_reg < -1
        || m.max_reg >= 63
        || m.max_half_reg < -1
        || m.max_half_reg >= 63
        || m.branchstack_hw > 127
        || m.num_samp < 0
        || (m.num_samp > 16 && !(m.num_samp == 17 && m.nv12_reconstruction()
            .map_err(|_|CompileError::InvalidResource)?.is_some_and(|r|r.nearest_sampler==Some(16))))
        || m.num_uavs > 32
        || m.num_ubos > 32
        || a.push_constant_bytes > 128
    {
        return Err(CompileError::InvalidResource);
    }
    crate::validate_private_memory(m, shader.private_memory.as_ref())?;
    if m.early_preamble || m.num_sampler_prefetch != 0 {
        return Err(unsupported("native private memory/preamble/prefetch"));
    }
    // Sample-mask output is meaningful even for the public single-sample
    // target: bit zero can suppress coverage. It needs no MSAA resource API.
    if (stage != Stage::Fragment && m.writes_smask)
        || (stage == Stage::Fragment
            && (m.writes_smask != m.outputs.iter().any(|o| o.slot == 3)
                || m.outputs.iter().any(|o| o.slot == 3 && o.half)))
    {
        return Err(unsupported("native sample-mask output metadata"));
    }
    if m.writes_stencilref
        || m.writes_viewport
        || m.sample_shading
    {
        return Err(unsupported(
            "native multisample/layer interface",
        ));
    }
    builtin::primitive_location(m, stage)?;
    builtin::validate_sample_profile(m, stage)?;
    if m.total_in > 128
        || m.fragcoord_compmask > 15
        || m.const_allocations
            .iter()
            .any(|c| c.size_vec4 > 0 && ![0, 3, 4, 10].contains(&c.kind))
        || m.driver_ubos.iter().skip(1).any(|u| u.idx >= 0)
        || m.image_mapping.num_tex > 32
        || m.image_mapping.tex_base > 32
    {
        return Err(unsupported("native constant/interface metadata"));
    }
    for i in &m.inputs {
        if i.sysval
            && !match stage {
                Stage::Vertex => [9, 10].contains(&i.slot),
                Stage::Fragment => [19, 27, 29, 32, 59, 60, 61, 62, 63, 64, 65].contains(&i.slot),
                Stage::Compute => [42, 47].contains(&i.slot),
            }
        {
            return Err(unsupported("native system value interface"));
        }
        if i.regid > INVALID_REG || i.compmask > 15 || (!i.sysval && i.inloc > 127) {
            return Err(CompileError::InvalidResource);
        }
    }
    for o in &m.outputs {
        if o.regid >= INVALID_REG || o.aliased_components != 0 || o.view != 0 {
            return Err(unsupported("native output register alias/view"));
        }
    }
    ycbcr::validate_shader(a)?;
    let typed = a.sampled_bindings().map_err(|_| CompileError::InvalidResource)?;
    for b in &a.bindings {
        if storage::is_image(b) && (b.slot >= 16 || m.num_ssbos != 16 || 16 + b.slot >= m.num_uavs
            || !typed.as_ref().is_some_and(|list| list.iter().any(|t| t.group == b.group && t.binding == b.binding && t.slot == b.slot && t.ty == sgfx_shader_ir3::SampledBindingType::StorageTextureRgba8WriteOnly))) {
            return Err(CompileError::InvalidResource);
        }
        if b.group >= 4
            || b.binding >= 32
            || b.slot >= 32
            || (b.kind == sgfx_shader_ir3::ResourceKind::StorageBuffer && b.slot >= m.num_uavs)
            || (matches!(
                b.kind,
                sgfx_shader_ir3::ResourceKind::Texture | sgfx_shader_ir3::ResourceKind::Sampler
            ) && !storage::is_image(b) && b.slot >= m.num_samp as u32)
        {
            return Err(CompileError::InvalidResource);
        }
    }
    for r in &m.ubo_ranges {
        if r.bindless
            || r.global
            || r.end <= r.start
            || (r.start | r.end | r.offset) % 16 != 0
            || r.offset
                .checked_add(r.end - r.start)
                .is_none_or(|e| e > m.constlen * 16)
        {
            return Err(unsupported("native UBO range metadata"));
        }
    }
    if m.immediate_offset_dwords
        .checked_add(m.imm_values.len() as u32)
        .is_none_or(|e| e > m.constlen * 4)
        || m.constant_data_offset
            .checked_add(m.constant_data_bytes)
            .is_none_or(|e| e > m.allocation_bytes)
    {
        return Err(CompileError::OutOfBounds);
    }
    Ok(())
}

/// Validate native stage metadata and linkage without accessing device resources.
pub fn validate_native_render_pipeline(p: &NativeRenderPipeline) -> Result<(), CompileError> {
    validate_shader(&p.vertex, Stage::Vertex)?;
    validate_shader(&p.fragment, Stage::Fragment)?;
    let targets: Vec<_> = p.descriptor.color_targets().collect();
    if targets.is_empty()
        || targets.len() > 8
        || targets.iter().any(|t| !color::supported_format(t.format()))
    {
        return Err(unsupported("native color target/raster state"));
    }
    if p.descriptor
        .depth_state()
        .is_some_and(|d| d.format() != ir::TextureFormat::Depth32Float)
    {
        return Err(unsupported("native depth format"));
    }
    if !p
        .vertex
        .artifact
        .metadata
        .outputs
        .iter()
        .any(|o| o.slot == 0)
    {
        return Err(unsupported("native vertex position output required"));
    }
    if p.vertex
        .artifact
        .metadata
        .outputs
        .iter()
        .any(|o| !matches!(o.slot, 0 | 4 | 32..=63))
    {
        return Err(unsupported("native vertex layer/clip output interface"));
    }
    let f = &p.fragment.artifact.metadata;
    if f.outputs
        .iter()
        .any(|o| o.slot != 0 && o.slot != 1 && !(o.slot == 3 && f.writes_smask) && !(4..4 + targets.len() as u32).contains(&o.slot))
        || (targets.len() > 1 && f.outputs.iter().any(|o| o.slot == 1))
    {
        return Err(unsupported("native fragment color output interface"));
    }
    for v in &p.vertex.artifact.bindings {
        if p.fragment
            .artifact
            .bindings
            .iter()
            .any(|b| (b.group, b.binding) == (v.group, v.binding) && b.kind != v.kind)
        {
            return Err(unsupported("native graphics binding type mismatch"));
        }
        let Some(v_slot) = storage::slot(v) else { continue; };
        // Graphics UAVs share one hardware table. A pipeline-wide compiler
        // BindingMap prevents collisions; reject legacy per-module artifacts
        // with conflicting slots before one stage can replace another's UAV.
        if p.fragment.artifact.bindings.iter().any(|b| {
            storage::slot(b) == Some(v_slot)
                && (b.group, b.binding) != (v.group, v.binding)
        }) {
            return Err(unsupported("native graphics UAV slot collision"));
        }
    }
    link(&p.vertex.artifact.metadata, f)?;
    let attrs: Vec<_> = p
        .descriptor
        .vertex_buffers()
        .iter()
        .flat_map(|b| b.attributes())
        .collect();
    for i in p
        .vertex
        .artifact
        .metadata
        .inputs
        .iter()
        .filter(|i| !i.sysval)
    {
        if !attrs
            .iter()
            .any(|a| a.location().checked_add(15) == Some(i.slot))
        {
            return Err(unsupported("native vertex input missing from layout"));
        }
    }
    if attrs.len() > 32
        || p.descriptor.vertex_buffers().len() > 32
        || p.descriptor
            .vertex_buffers()
            .iter()
            .any(|b| b.stride() > 4095)
    {
        return Err(CompileError::InvalidResource);
    }
    Ok(())
}

/// Validate the native compute program without accessing device resources.
pub fn validate_native_compute_pipeline(p: &NativeComputePipeline) -> Result<(), CompileError> {
    validate_shader(&p.shader, Stage::Compute)?;
    let m = &p.shader.artifact.metadata;
    let local = m.local_size;
    if m.local_size_variable
        || local.contains(&0)
        || local.iter().any(|x| *x > 1024)
        || local
            .iter()
            .try_fold(1u32, |a, b| a.checked_mul(*b))
            .is_none_or(|v| v > 1024)
        || m.compute.as_ref().is_none_or(|c| {
            c.req_local_mem.max(m.shared_size) > 32768
                || c.local_invocation_id > 252
                || c.work_group_id > 252
        })
    {
        return Err(unsupported("native compute local size/shared state"));
    }
    Ok(())
}

pub(crate) fn compile_operation(
    e: &mut Emitter,
    s: &mut State<'_>,
    input: CompileInput<'_>,
    op: &Operation<'_>,
) -> Result<bool, CompileError> {
    // Only native draw parameters and bindings preserve active program state.
    // Pass boundaries, fixed/compute paths, transfers and barriers invalidate
    // it even if a later bind returns to the same Arc without drawing in between.
    match op {
        Operation::SetProgrammablePipeline(p) | Operation::SetFixedNativePipeline(p) => {
            if e.last_native_program
                .as_ref()
                .is_some_and(|last| !Arc::ptr_eq(last, p))
            {
                e.last_native_program = None;
            }
        }
        Operation::Draw { .. }
        | Operation::DrawIndexed { .. }
        | Operation::DrawInstanced { .. }
        | Operation::DrawIndexedInstanced { .. }
        | Operation::SetBindGroup { .. }
        | Operation::SetPushConstants { .. }
        | Operation::SetVertexBuffer { .. }
        | Operation::SetVertexBufferSlot { .. }
        | Operation::SetIndexBuffer { .. }
        | Operation::SetDrawIndex(_)
        | Operation::SetViewport(_)
        | Operation::SetScissor(_) => {}
        _ => e.last_native_program = None,
    }
    match op {
        Operation::BeginNativeRenderPass(pass) => color::begin(e, s, input, pass)?,
        Operation::BlitTexture {
            source,
            source_rect,
            destination,
            destination_rect,
            filter,
            flips,
        } if matches!(&c::find_resource(input.resources,*source)?.kind,ResourceKind::Image(i) if matches!(i.format,ir::TextureFormat::Rgba16Float|ir::TextureFormat::Rgba32Float)) =>
        {
            if s.pass.is_some() || s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            color::blit(
                e,
                input,
                *source,
                *source_rect,
                *destination,
                *destination_rect,
                *filter,
                *flips,
            )?;
        }
        Operation::CopyTextureToTexture {
            source,
            source_rect,
            destination,
            destination_rect,
        } if matches!(&c::find_resource(input.resources,*source)?.kind,ResourceKind::Image(i) if matches!(i.format,ir::TextureFormat::Rgba16Float|ir::TextureFormat::Rgba32Float|ir::TextureFormat::Depth32Float)) =>
        {
            if s.pass.is_some() || s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            color::copy(
                e,
                input,
                *source,
                *source_rect,
                *destination,
                *destination_rect,
            )?;
        }
        Operation::WriteTexture {
            destination,
            area,
            bytes_per_row,
            data,
        } if matches!(&c::find_resource(input.resources,*destination)?.kind,ResourceKind::Image(i) if matches!(i.format,ir::TextureFormat::Rgba16Float|ir::TextureFormat::Rgba32Float|ir::TextureFormat::Depth32Float)) =>
        {
            if s.pass.is_some() || s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            color::upload(e, input, *destination, *area, *bytes_per_row, data)?;
        }
        Operation::SetViewport(viewport) if s.native.colors.is_some() => {
            let pass = s.pass.ok_or(CompileError::InvalidState)?;
            let target = color::surface(
                c::find_resource(input.resources, pass.target)?,
                ir::TextureUsage::RENDER_ATTACHMENT,
                input.capabilities.max_linear_pitch,
            )?
            .surface;
            let extent = ir::Extent2D::new(target.width, target.height)
                .map_err(|_| CompileError::InvalidResource)?;
            if !viewport.is_within(extent) {
                return Err(CompileError::OutOfBounds);
            }
            s.viewport = Some(*viewport);
        }
        Operation::SetPipeline(_) if s.native.colors.is_some() => {
            return Err(unsupported("fixed pipeline in native render pass"));
        }
        Operation::BeginComputePass => {
            if s.pass.is_some() || s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            s.native = NativeState::default();
            s.native.compute_pass = true;
        }
        Operation::EndComputePass => {
            if !s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            s.native = NativeState::default();
        }
        Operation::SetProgrammablePipeline(p) | Operation::SetFixedNativePipeline(p) => {
            if s.pass.is_none() || s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            validate_native_render_pipeline(p)?;
            s.native.fixed = matches!(op,Operation::SetFixedNativePipeline(_));
            s.native.render = Some(p.clone());
            s.pipeline = None;
        }
        Operation::SetComputePipeline(p) => {
            if !s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            validate_native_compute_pipeline(p)?;
            s.native.compute = Some(p.clone());
        }
        Operation::SetBindGroup { index, bindings } => {
            if (s.pass.is_none() && !s.native.compute_pass) || *index >= 4 || bindings.len() > 32 {
                return Err(CompileError::InvalidState);
            }
            for (i, b) in bindings.iter().enumerate() {
                if b.binding >= 32 || bindings[..i].iter().any(|x| x.binding == b.binding) {
                    return Err(CompileError::InvalidIdentity);
                }
            }
            s.native.groups.retain(|(i, _)| i != index);
            s.native.groups.push((*index, bindings.clone()));
        }
        Operation::SetPushConstants {
            stages,
            offset,
            data,
        } => {
            let layout = if s.native.compute_pass {
                &s.native
                    .compute
                    .as_ref()
                    .ok_or(CompileError::InvalidState)?
                    .descriptor
                    .layout()
            } else {
                &s.native
                    .render
                    .as_ref()
                    .ok_or(CompileError::InvalidState)?
                    .descriptor
                    .layout()
            };
            layout
                .validate_push_constants(*stages, *offset, data)
                .map_err(|_| CompileError::OutOfBounds)?;
            for (i, stage) in [
                ShaderStages::VERTEX,
                ShaderStages::FRAGMENT,
                ShaderStages::COMPUTE,
            ]
            .into_iter()
            .enumerate()
            {
                if stages.contains(stage) {
                    s.native.push[i][*offset as usize..*offset as usize + data.len()]
                        .copy_from_slice(data);
                }
            }
        }
        Operation::SetVertexBufferSlot {
            slot,
            buffer,
            offset,
        } => {
            if s.pass.is_none() || *slot >= 32 {
                return Err(CompileError::InvalidState);
            }
            let r = c::find_resource(input.resources, *buffer)?;
            c::require_buffer_usage(r, ir::BufferUsage::VERTEX)?;
            if *offset >= r.size {
                return Err(CompileError::OutOfBounds);
            }
            s.native.vertices.retain(|(i, _, _)| i != slot);
            s.native.vertices.push((*slot, *buffer, *offset));
        }
        Operation::SetTexture(id) if matches!(&c::find_resource(input.resources,*id)?.kind,
            ResourceKind::Image(i) if matches!(i.format,ir::TextureFormat::Rgba16Float|ir::TextureFormat::Rgba32Float)) => {
            if s.pass.is_none() {return Err(CompileError::InvalidState);}
            color::sampled_descriptor(c::find_resource(input.resources,*id)?,None,input.capabilities.max_linear_pitch)?;
            s.texture=Some(*id);
        }
        Operation::SetDrawIndex(index) => {
            if s.pass.is_none() {
                return Err(CompileError::InvalidState);
            }
            s.native.draw_index = *index;
        }
        Operation::Dispatch { x, y, z } => {
            if !s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            let p = s
                .native
                .compute
                .as_ref()
                .ok_or(CompileError::InvalidState)?;
            e.native_compute(input, &s.native, p, [*x, *y, *z])?;
        }
        Operation::DrawInstanced {
            vertex_count,
            instance_count,
            first_vertex,
            first_instance,
        } => {
            c::begin_draw_batch_if_needed(e, s)?;
            e.native_draw(
                input,
                s,
                *vertex_count,
                *instance_count,
                *first_vertex,
                *first_instance,
                None,
            )?;
        }
        Operation::DrawIndexedInstanced {
            index_count,
            instance_count,
            first_index,
            base_vertex,
            first_instance,
        } => {
            c::begin_draw_batch_if_needed(e, s)?;
            e.native_draw(
                input,
                s,
                *index_count,
                *instance_count,
                *first_index,
                *first_instance,
                Some(*base_vertex),
            )?;
        }
        Operation::Draw {
            vertex_count,
            first_vertex,
        } if s.native.render.is_some() => {
            c::begin_draw_batch_if_needed(e, s)?;
            e.native_draw(input, s, *vertex_count, 1, *first_vertex, 0, None)?;
        }
        Operation::DrawIndexed {
            index_count,
            first_index,
            base_vertex,
        } if s.native.render.is_some() => {
            c::begin_draw_batch_if_needed(e, s)?;
            e.native_draw(
                input,
                s,
                *index_count,
                1,
                *first_index,
                0,
                Some(*base_vertex),
            )?;
        }
        Operation::SetVertexBuffer { buffer, offset } => {
            s.native.vertices.retain(|(slot, _, _)| *slot != 0);
            s.native.vertices.push((0, *buffer, *offset));
            return Ok(false);
        }
        Operation::BeginRenderPass(_) | Operation::EndRenderPass | Operation::SetPipeline(_) => {
            if s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            if matches!(op, Operation::SetPipeline(_)) {
                s.native.render = None;
                s.native.fixed = false;
            } else {
                s.native = NativeState::default();
            }
            return Ok(false);
        }
        _ => {
            if s.native.compute_pass {
                return Err(CompileError::InvalidState);
            }
            return Ok(false);
        }
    }
    Ok(true)
}

struct Link {
    outputs: Vec<u32>,
    destinations: Vec<u32>,
    disable: [u32; 4],
    interp: [u32; 8],
    stride: u32,
    position: u32,
    psize: u32,
    count: u32,
    primitive: u32,
}
fn link(v: &Metadata, f: &Metadata) -> Result<Link, CompileError> {
    let primitive = builtin::primitive_location(f, Stage::Fragment)?;
    let mut entries = Vec::new();
    let mut mask = [0u32; 4];
    let mut interp = [0u32; 8];
    let mut maxloc = 0;
    for i in f
        .inputs
        .iter()
        .filter(|i| !i.sysval && i.inloc < f.total_in)
    {
        if i.compmask == 0 {
            continue;
        }
        let components = 32 - i.compmask.leading_zeros();
        if i.inloc + components > 128 {
            return Err(CompileError::OutOfBounds);
        }
        for n in 0..components {
            let loc = i.inloc + n;
            mask[(loc / 32) as usize] |= 1 << (loc % 32);
        }
        // IR3 packs enabled components consecutively for interpolation, even
        // when the producer's component mask has holes (Mesa emit_interp_state).
        if i.flat {
            for n in 0..i.compmask.count_ones() {
                let loc = i.inloc + n;
                interp[(loc / 16) as usize] |= 1 << ((loc % 16) * 2);
            }
        }
        if i.slot != builtin::PRIMITIVE_ID_SLOT {
            let output = v.outputs.iter().find(|o| o.slot == i.slot)
                .ok_or(unsupported("unmatched native varying"))?;
            entries.push((output.regid, i.compmask, i.inloc));
        }
        // Mesa ir3_link_shaders(pack_vs_out=true) reserves the VPC location
        // and varmask, but emits no dummy VS register for fixed-function IDs.
        maxloc = maxloc.max(i.inloc + components);
    }
    let position = maxloc;
    let pos = v
        .outputs
        .iter()
        .find(|o| o.slot == 0)
        .ok_or(CompileError::InvalidResource)?;
    entries.push((pos.regid, 15, maxloc));
    maxloc += 4;
    let psize = if let Some(p) = v.outputs.iter().find(|o| o.slot == 4) {
        let loc = maxloc;
        entries.push((p.regid, 1, loc));
        maxloc += 1;
        loc
    } else {
        255
    };
    if entries.len() > 32 || maxloc > 255 {
        return Err(CompileError::OutOfBounds);
    }
    let mut outputs = vec![0; entries.len().div_ceil(2)];
    let mut destinations = vec![0; entries.len().div_ceil(4)];
    for (n, (reg, comp, loc)) in entries.iter().enumerate() {
        outputs[n / 2] |= (reg | comp << 8) << ((n % 2) * 16);
        destinations[n / 4] |= loc << ((n % 4) * 8);
    }
    Ok(Link {
        outputs,
        destinations,
        disable: mask.map(|x| !x),
        interp,
        stride: maxloc,
        position,
        psize,
        count: entries.len() as u32,
        primitive,
    })
}

impl NativeState {
    fn binding(&self, group: u32, binding: u32) -> Result<&NativeBindingResource, CompileError> {
        self.groups
            .iter()
            .find(|(i, _)| *i == group)
            .and_then(|(_, b)| b.iter().find(|b| b.binding == binding))
            .map(|b| &b.resource)
            .ok_or(CompileError::InvalidState)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Descriptor {
    words: [u32; 16],
    address: Option<(ObjectRef, u64, u64, Access)>,
    secondary: Option<(ObjectRef,u64,u64,Access)>,
}

#[cfg(test)]
mod native_table_tests {
    use super::*;

    #[test]
    fn reuse_requires_all_words_and_exact_ordered_relocation_authority() {
        let mut emitter = Emitter::new(32768);
        let base = Descriptor {
            secondary: None,
            words: [0; 16],
            address: Some((ObjectRef::External(ObjectId::new(7)), 64, 128, Access::READ)),
        };
        let first = emitter.native_table(&[base]).unwrap();
        let mut variants = Vec::new();
        for index in 0..16 {
            let mut changed = base;
            changed.words[index] = 1;
            variants.push(changed);
        }
        for address in [
            None,
            Some((ObjectRef::External(ObjectId::new(8)), 64, 128, Access::READ)),
            Some((
                ObjectRef::Generated(GeneratedObjectId::new(7)),
                64,
                128,
                Access::READ,
            )),
            Some((
                ObjectRef::External(ObjectId::new(7)),
                128,
                128,
                Access::READ,
            )),
            Some((ObjectRef::External(ObjectId::new(7)), 64, 256, Access::READ)),
            Some((
                ObjectRef::External(ObjectId::new(7)),
                64,
                128,
                Access::READ | Access::WRITE,
            )),
        ] {
            variants.push(Descriptor { address, ..base });
        }
        for changed in variants {
            let id = emitter.native_table(&[changed]).unwrap();
            assert_ne!(id, first);
            assert_eq!(emitter.native_table(&[changed]).unwrap(), id);
        }
        let empty = Descriptor {
            secondary: None,
            words: [0; 16],
            address: None,
        };
        let pair = emitter.native_table(&[base, empty]).unwrap();
        assert_ne!(pair, first);
        assert_ne!(pair, emitter.native_table(&[empty, base]).unwrap());
        assert_eq!(pair, emitter.native_table(&[base, empty]).unwrap());
        let before = (
            emitter.texture_words.clone(),
            emitter.texture_fixups.clone(),
            emitter.generated_objects.clone(),
            emitter.accesses.clone(),
        );
        // Rebinding an older table must reuse it too, without another write,
        // wait, relocation or allocation. Its access record is already retained.
        assert_eq!(emitter.native_table(&[base]).unwrap(), first);
        assert_eq!(
            before,
            (
                emitter.texture_words,
                emitter.texture_fixups,
                emitter.generated_objects,
                emitter.accesses,
            )
        );
    }

    #[test]
    fn failed_table_emission_is_not_cached() {
        // One descriptor write fits, but its required memory-write wait does not.
        let mut emitter = Emitter::new(19);
        assert_eq!(
            emitter.native_table(&[Descriptor {
                secondary: None,
                words: [0; 16],
                address: None
            }]),
            Err(CompileError::CommandBudgetExceeded)
        );
        assert!(emitter.native_tables.is_empty());
    }
}

impl Emitter {
    fn native_load(
        &mut self,
        stage: Stage,
        block: u32,
        ty: u32,
        dst: u32,
        units: u32,
        object: ObjectRef,
        offset: u64,
        size: u64,
    ) -> Result<(), CompileError> {
        if units == 0 || units > 1023 || dst > 0x3fff {
            return Err(CompileError::InvalidResource);
        }
        self.push_word(type7(stage_load(stage).0, 3).map_err(|_| CompileError::InvalidPm4)?)?;
        self.push_word(dst | ty << 14 | 2 << 16 | block << 18 | units << 22)?;
        self.address_words(object, offset, size, Access::READ)
    }
    fn native_constants(
        &mut self,
        stage: Stage,
        dst: u32,
        words: &[u32],
    ) -> Result<(), CompileError> {
        if words.is_empty() {
            return Ok(());
        }
        if dst % 4 != 0 || dst as usize + words.len().next_multiple_of(4) > 1024 {
            return Err(CompileError::OutOfBounds);
        }
        let mut payload = vec![
            dst / 4 | 1 << 14 | stage_load(stage).1 << 18 | (words.len().div_ceil(4) as u32) << 22,
            0,
            0,
        ];
        payload.extend_from_slice(words);
        payload.resize(3 + words.len().next_multiple_of(4), 0);
        self.packet7(stage_load(stage).0, &payload)
    }
    fn native_program(
        &mut self,
        shader: &NativeShader,
        input: CompileInput<'_>,
    ) -> Result<(), CompileError> {
        let a = &shader.artifact;
        let m = &a.metadata;
        let r = c::find_resource(input.resources, shader.object)?;
        if !matches!(r.kind, ResourceKind::Buffer { .. }) || r.size < m.allocation_bytes as u64 {
            return Err(CompileError::OutOfBounds);
        }
        let (ctrl, config, instr, base, stack, constcfg) = match a.stage {
            Stage::Vertex => (0xa800, 0xa823, 0xa824, 0xa81b, 0xa825, 0xb800),
            Stage::Fragment => (0xa980, 0xab04, 0xab05, 0xa982, 0xa9a9, 0xbb10),
            Stage::Compute => (0xa9b0, 0xa9bb, 0xa9bc, 0xa9b3, 0xa9bd, 0xb987),
        };
        let common = ((m.max_half_reg + 1) as u32) << 1
            | ((m.max_reg + 1) as u32) << 7
            | m.branchstack_hw << 13;
        let control = common
            | match a.stage {
                Stage::Vertex => (m.mergedregs as u32) << 20,
                Stage::Fragment => {
                    (m.double_threadsize as u32) << 20
                        | ((m.total_in > 0) as u32) << 22
                        | (m.need_full_quad as u32) << 23
                        | 1 << 24
                        | (m.need_pixlod as u32) << 26
                        | (m.mergedregs as u32) << 31
                }
                Stage::Compute => {
                    1 << 20
                        | (m.compute.as_ref().unwrap().round_robin_mode as u32) << 22
                        | (m.mergedregs as u32) << 31
                }
            };
        self.packet4(ctrl, &[control])?;
        self.packet4(
            config,
            &[0x100 | (m.num_samp.min(16) as u32) << 9 | (m.num_samp as u32) << 17 | m.num_uavs << 22],
        )?;
        self.packet4(instr, &[m.instrlen])?;
        self.packet4(constcfg, &[0x100 | m.constlen.div_ceil(4)])?;
        self.push_word(type4(base, 7).map_err(|_| CompileError::InvalidPm4)?)?;
        self.push_word(0)?;
        self.address_words(
            ObjectRef::NativeShader(shader.object),
            0,
            m.allocation_bytes as u64,
            Access::READ,
        )?;
        let private_stack = self.native_private_memory(m, shader.private_memory.as_ref(), input)?;
        self.packet4(stack, &[private_stack])?;
        self.native_load(
            a.stage,
            stage_load(a.stage).1,
            0,
            0,
            // A618 has 64 instruction-cache units. The full program stays
            // addressable through SP_*_BASE, but preload must fit the cache.
            m.instrlen.min(64),
            ObjectRef::NativeShader(shader.object),
            0,
            m.allocation_bytes as u64,
        )
    }
    fn native_padded_buffer(
        &mut self,
        object: ObjectRef,
        offset: u64,
        size: u64,
    ) -> Result<(ObjectRef, u64, u64), CompileError> {
        if size == 0 || offset % 16 != 0 || size % 4 != 0 {
            return Err(CompileError::InvalidResource);
        }
        if size % 16 == 0 {
            return Ok((object, offset, size));
        }
        let rounded = size.checked_add(15).ok_or(CompileError::Overflow)? & !15;
        if rounded > 512 * 1024 {
            return Err(CompileError::OutOfBounds);
        }
        let id = self.add_generated(
            &vec![0; rounded as usize],
            64,
            Access::READ | Access::WRITE,
            GeneratedObjectKind::Upload,
        )?;
        let mut copied = 0;
        while copied < size {
            let bytes = (size - copied).min(512);
            self.push_word(type7(CP_MEMCPY, 5).map_err(|_| CompileError::InvalidPm4)?)?;
            self.push_word((bytes / 4) as u32)?;
            self.address_words(object, offset + copied, bytes, Access::READ)?;
            self.address_words(ObjectRef::Generated(id), copied, bytes, Access::WRITE)?;
            self.packet7(opcode::WAIT_MEM_WRITES, &[])?;
            copied += bytes;
        }
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])?;
        self.wait_for_idle()?;
        Ok((ObjectRef::Generated(id), 0, rounded))
    }
    fn native_ubo(
        &mut self,
        stage: Stage,
        slot: u32,
        object: ObjectRef,
        offset: u64,
        size: u64,
    ) -> Result<(), CompileError> {
        if slot >= 32 || size == 0 || size % 16 != 0 || offset % 16 != 0 || size / 16 > 0x7fff {
            return Err(CompileError::OutOfBounds);
        }
        self.push_word(type7(stage_load(stage).0, 5).map_err(|_| CompileError::InvalidPm4)?)?;
        self.extend_words(&[slot | 2 << 14 | stage_load(stage).1 << 18 | 1 << 22, 0, 0])?;
        let word_offset = self.words.len() as u32;
        self.extend_words(&[0, ((size / 16) as u32) << 17])?;
        self.fixups.push(SymbolicAddress {
            word_offset,
            object,
            object_offset: offset,
            required_size: size,
            access: Access::READ,
            encoding: AddressEncoding::GpuVa49UboDescriptor,
        });
        self.record_access(object, offset, size, Access::READ)
    }
    fn native_table(&mut self, entries: &[Descriptor]) -> Result<GeneratedObjectId, CompileError> {
        // Tables are immutable after their prologue writes. Reuse only exact
        // words AND relocation authority (object, offset, size, access), in
        // order, including empty slots. The owning Emitter is compile-local:
        // neither generated IDs nor symbolic resources escape into a cache
        // shared with a later compilation.
        if let Some((_, id)) = self.native_tables.iter().find(|(key, _)| key == entries) {
            return Ok(*id);
        }
        self.native_tables
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        let mut key = Vec::new();
        key.try_reserve_exact(entries.len())
            .map_err(|_| CompileError::OutOfMemory)?;
        key.extend_from_slice(entries);
        let id = self.add_generated(
            &vec![0; entries.len() * 64],
            64,
            Access::READ | Access::WRITE,
            GeneratedObjectKind::TextureState,
        )?;
        core::mem::swap(&mut self.words, &mut self.texture_words);
        core::mem::swap(&mut self.fixups, &mut self.texture_fixups);
        let result = (|| {
            for (i, d) in entries.iter().enumerate() {
                self.push_word(type7(CP_MEM_WRITE, 18).map_err(|_| CompileError::InvalidPm4)?)?;
                self.address_words(ObjectRef::Generated(id), (i * 64) as u64, 64, Access::WRITE)?;
                self.extend_words(&d.words[..4])?;
                if let Some((object, offset, size, access)) = d.address {
                    let word_offset = self.words.len() as u32;
                    self.extend_words(&d.words[4..6])?;
                    self.fixups.push(SymbolicAddress {
                        word_offset,
                        object,
                        object_offset: offset,
                        required_size: size,
                        access,
                        encoding: AddressEncoding::GpuVa49TexDescriptor,
                    });
                    self.record_access(object, offset, size, access)?;
                } else {
                    self.extend_words(&d.words[4..6])?;
                }
                self.push_word(d.words[6])?;
                if let Some((object,offset,size,access))=d.secondary {
                    // NV12 UV plane is a separate READ span, not a base+size
                    // widening over the gap between producer image planes.
                    if access!=Access::READ {return Err(CompileError::InvalidResource);}
                    self.address_words(object,offset,size,access)?;
                } else {self.extend_words(&d.words[7..9])?;}
                self.extend_words(&d.words[9..])?;
            }
            self.packet7(opcode::WAIT_MEM_WRITES, &[])
        })();
        core::mem::swap(&mut self.words, &mut self.texture_words);
        core::mem::swap(&mut self.fixups, &mut self.texture_fixups);
        result?;
        self.native_tables.push((key, id));
        Ok(id)
    }
    fn native_storage_descriptor(
        input: CompileInput<'_>,
        object: ObjectId,
        offset: u64,
        size: u64,
        write: bool,
        b: &sgfx_shader_ir3::Binding,
    ) -> Result<Descriptor, CompileError> {
        let r = c::find_resource(input.resources, object)?;
        c::require_buffer_usage(r, ir::BufferUsage::STORAGE)?;
        c::validate_range(r.size, offset, size)?;
        if b.writable && !write
            || size < b.minimum_size as u64
            || size == 0
            || size % 4 != 0
            || offset % 4 != 0
            || size / 4 >= 1 << 30
        {
            return Err(CompileError::InvalidResource);
        }
        let mut words = [0; 16];
        words[0] = 0x4b << 22 | 1 << 7 | 2 << 10 | 3 << 13;
        words[1] = (size / 4) as u32;
        // Mesa fdl6_buffer_view_init: base alignment is independent of the
        // logical R32 element offset. Keeping the original allocation avoids
        // breaking aliases and atomic/read-write visibility through staging.
        let prefix = offset & 63;
        words[2] = 4 << 29 | 1 << 4 | ((prefix / 4) as u32) << 16;
        let access = if b.writable {
            Access::READ | Access::WRITE
        } else {
            Access::READ
        };
        Ok(Descriptor {
            secondary: None,
            words,
            address: Some((
                ObjectRef::External(object),
                offset - prefix,
                size.checked_add(prefix).ok_or(CompileError::Overflow)?,
                access,
            )),
        })
    }
    fn native_resources(
        &mut self,
        shader: &NativeShader,
        state: &NativeState,
        input: CompileInput<'_>,
        driver: &[u32],
        partner: Option<&NativeShader>,
    ) -> Result<(), CompileError> {
        let a = &shader.artifact;
        let m = &a.metadata;
        let stage = a.stage;
        let nv12_profiles=m.nv12_profiles().map_err(|_|CompileError::InvalidResource)?.unwrap_or_default();
        let reconstruction=if nv12_profiles.is_empty() {None}else{m.nv12_reconstruction().map_err(|_|CompileError::InvalidResource)?};
        let (_, sb, tb) = stage_load(stage);
        let empty = Descriptor {
            secondary: None,
            words: [0; 16],
            address: None,
        };
        // The private sampler may use slot16; it is not a seventeenth texture.
        let mut textures = vec![empty; m.num_samp.min(16) as usize];
        let mut samplers = vec![0u32; m.num_samp as usize * 4];
        let mut uavs = vec![empty; m.num_uavs as usize];
        let mut ubos = Vec::new();
        for b in &a.bindings {
            let resource = state.binding(b.group, b.binding)?;
            match (b.kind, resource) {
                (
                    sgfx_shader_ir3::ResourceKind::UniformBuffer,
                    NativeBindingResource::Buffer {
                        object,
                        offset,
                        size,
                        write: false,
                    },
                ) => {
                    let r = c::find_resource(input.resources, *object)?;
                    c::require_buffer_usage(r, ir::BufferUsage::UNIFORM)?;
                    c::validate_range(r.size, *offset, *size)?;
                    if *size < b.minimum_size as u64 {
                        return Err(CompileError::OutOfBounds);
                    }
                    let (o, off, len) =
                        self.native_padded_buffer(ObjectRef::External(*object), *offset, *size)?;
                    self.native_ubo(stage, b.slot, o, off, len)?;
                    ubos.push((b.slot, o, off, len));
                }
                (
                    sgfx_shader_ir3::ResourceKind::StorageBuffer,
                    NativeBindingResource::Buffer {
                        object,
                        offset,
                        size,
                        write,
                    },
                ) => {
                    let d =
                        Self::native_storage_descriptor(input, *object, *offset, *size, *write, b)?;
                    if b.slot as usize >= uavs.len() {
                        return Err(CompileError::InvalidResource);
                    }
                    uavs[b.slot as usize] = d;
                    if let Some(&slot) = m.image_mapping.ssbo_to_tex.get(b.slot as usize) {
                        if slot != 255 && m.image_mapping.num_tex > 0 {
                            let texslot = m.image_mapping.tex_base as usize + slot as usize;
                            if texslot >= 32 {
                                return Err(CompileError::OutOfBounds);
                            }
                            if textures.len() <= texslot {
                                textures.resize(texslot + 1, empty);
                            }
                            let mut read = d;
                            // STARTOFFSETTEXELS already encodes the prefix.
                            // Keep the aligned base and expanded footprint in
                            // the texture alias; only narrow its permissions.
                            if let Some((_, _, _, access)) = read.address.as_mut() {
                                *access = Access::READ;
                            }
                            *textures.get_mut(texslot).ok_or(CompileError::InvalidResource)? = read;
                        }
                    }
                }
                (
                    sgfx_shader_ir3::ResourceKind::Texture,
                    NativeBindingResource::Texture { object, view },
                ) => {
                    if storage::is_image(b) {
                        let slot = storage::slot(b).unwrap() as usize;
                        let d = storage::descriptor(input, *object, *view)?;
                        *uavs.get_mut(slot).ok_or(CompileError::InvalidResource)? = d;
                        continue;
                    }
                    if nv12_profiles.iter().any(|p|p.profile.texture_slot==b.slot) {return Err(unsupported("NV12 converted shader requires its exact imported texture profile"));}
                    let r = c::find_resource(input.resources, *object)?;
                    if b.slot as usize >= textures.len() {
                        return Err(CompileError::InvalidResource);
                    }
                    textures[b.slot as usize] =
                        color::sampled_descriptor(r, *view, input.capabilities.max_linear_pitch)?;
                }
                (sgfx_shader_ir3::ResourceKind::Texture,NativeBindingResource::Nv12Texture(object))=>{
                    if storage::is_image(b)||b.writable||b.slot as usize>=textures.len() {return Err(CompileError::InvalidResource);}
                    let resource=c::find_resource(input.resources,*object)?;
                    if !state.fixed || !nv12_profiles.is_empty() {ycbcr::validate_bound(nv12_profiles.iter().find(|p|p.profile.texture_slot==b.slot),resource,b.slot)?;}
                    textures[b.slot as usize]=color::nv12_descriptor(resource,input.capabilities.max_linear_pitch)?;
                }
                (
                    sgfx_shader_ir3::ResourceKind::Sampler,
                    NativeBindingResource::Sampler(sampler),
                ) => {
                    if b.slot as usize * 4 + 4 > samplers.len() {
                        return Err(CompileError::InvalidResource);
                    }
                    if let Some(r)=&reconstruction {
                        if let Some(expected)=r.samplers.iter().find(|p|p.slot==b.slot) {
                            if *expected!=ycbcr::nv12_sampler_profile(b.slot,*sampler) {
                                return Err(unsupported("NV12 bound sampler does not match compiled reconstruction profile"));
                            }
                        } else if nv12_profiles.iter().any(|p|p.sample_instructions>0)
                            && [sampler.address_u(),sampler.address_v()].contains(&ir::AddressMode::ClampToBorder) {
                            return Err(unsupported("NV12 border requires the bound sampler profile"));
                        }
                    } else if nv12_profiles.iter().any(|p|p.sample_instructions>0)
                        && [sampler.address_u(),sampler.address_v()].contains(&ir::AddressMode::ClampToBorder) {
                        return Err(unsupported("NV12 border requires reconstruction metadata"));
                    }
                    // Texture-specific LOD limits are enforced by each texture descriptor.
                    let mut words = c::sampler_words(*sampler, 15);
                    if let Some(compare) = sampler.compare() {
                        let func = match compare {
                            ir::CompareFunction::Never => 0,
                            ir::CompareFunction::Less => 1,
                            ir::CompareFunction::Equal => 2,
                            ir::CompareFunction::LessEqual => 3,
                            ir::CompareFunction::Greater => 4,
                            ir::CompareFunction::NotEqual => 5,
                            ir::CompareFunction::GreaterEqual => 6,
                            ir::CompareFunction::Always => 7,
                        };
                        words[1] |= func << 1;
                    }
                    samplers[b.slot as usize * 4..b.slot as usize * 4 + 4].copy_from_slice(&words);
                }
                _ => return Err(CompileError::InvalidResource),
            }
        }
        if let Some(r)=&reconstruction {
            if let Some(slot)=r.nearest_sampler {
                if a.bindings.iter().any(|b|b.kind==sgfx_shader_ir3::ResourceKind::Sampler && b.slot==slot) {
                    return Err(CompileError::InvalidResource);
                }
                let words=c::sampler_words(ir::SamplerDesc::new(ir::FilterMode::Nearest,ir::FilterMode::Nearest,
                    ir::AddressMode::ClampToEdge,ir::AddressMode::ClampToEdge),0);
                samplers.get_mut(slot as usize*4..slot as usize*4+4)
                    .ok_or(CompileError::InvalidResource)?.copy_from_slice(&words);
            }
        }
        if let Some(ubo) = m.driver_ubos.first().filter(|u| u.idx >= 0) {
            if m.constant_data_bytes > 0 {
                let size = (m.constant_data_bytes as u64).next_multiple_of(16);
                c::validate_range(
                    m.allocation_bytes as u64,
                    m.constant_data_offset as u64,
                    size,
                )?;
                let o = ObjectRef::External(shader.object);
                let off = m.constant_data_offset as u64;
                self.native_ubo(stage, ubo.idx as u32, o, off, size)?;
                ubos.push((ubo.idx as u32, o, off, size));
            }
        }
        for r in &m.ubo_ranges {
            let &(_, object, offset, size) = ubos
                .iter()
                .find(|(slot, _, _, _)| *slot == r.block)
                .ok_or(CompileError::InvalidState)?;
            let total = u64::from(r.end - r.start);
            let available = size.saturating_sub(r.start as u64).min(total) & !15;
            if available > 0 {
                self.native_load(
                    stage,
                    sb,
                    1,
                    r.offset / 16,
                    (available / 16) as u32,
                    object,
                    offset + r.start as u64,
                    available,
                )?;
            }
            if total > available {
                self.native_constants(
                    stage,
                    (r.offset as u64 + available) as u32 / 4,
                    &vec![0; ((total - available) / 4) as usize],
                )?;
            }
        }
        self.native_constants(stage, m.immediate_offset_dwords, &m.imm_values)?;
        if a.push_constant_bytes > 0 {
            let words: Vec<_> = state.push[stage_index(stage)][..a.push_constant_bytes as usize]
                .chunks(4)
                .map(|b| {
                    let mut v = [0; 4];
                    v[..b.len()].copy_from_slice(b);
                    u32::from_le_bytes(v)
                })
                .collect();
            self.native_constants(stage, 0, &words)?;
        }
        if let Some(c) = m
            .const_allocations
            .iter()
            .find(|c| c.kind == 3 && c.size_vec4 > 0)
        {
            let len = (m.num_driver_params as usize).min(c.size_vec4 as usize * 4);
            let mut words = vec![0; len];
            let n = words.len().min(driver.len());
            words[..n].copy_from_slice(&driver[..n]);
            self.native_constants(stage, c.offset_vec4 * 4, &words)?;
        }
        self.packet4(
            match stage {
                Stage::Vertex => 0xa822,
                Stage::Fragment => SP_PS_TSIZE,
                Stage::Compute => 0xa9ba,
            },
            &[textures.len() as u32],
        )?;
        // A6xx graphics UAV base is SP_GFX_UAV_BASE (0xab1a). 0xa9f0
        // is CS bindless descriptor slot 4, not the graphics fallback table.
        let (sbase, tbase, ubase) = match stage {
            Stage::Vertex => (0xa8a0, 0xa8a8, 0xab1a),
            Stage::Fragment => (0xa9e0, 0xa9e4, 0xab1a),
            Stage::Compute => (0xa9e2, 0xa9e6, 0xa9f2),
        };
        if !textures.is_empty() {
            let table = self.native_table(&textures)?;
            let o = ObjectRef::Generated(table);
            let size = (textures.len() * 64) as u64;
            self.address_register(tbase, o, 0, size, Access::READ)?;
            self.native_load(stage, tb, 1, 0, textures.len() as u32, o, 0, size)?;
        }
        if !samplers.is_empty() {
            let bytes: Vec<_> = samplers.iter().flat_map(|w| w.to_le_bytes()).collect();
            let table =
                self.add_generated(&bytes, 16, Access::READ, GeneratedObjectKind::Upload)?;
            let o = ObjectRef::Generated(table);
            self.address_register(sbase, o, 0, bytes.len() as u64, Access::READ)?;
            self.native_load(
                stage,
                tb,
                0,
                0,
                (samplers.len() / 4) as u32,
                o,
                0,
                bytes.len() as u64,
            )?;
        }
        if let Some(partner) = partner {
            uavs.resize(
                uavs.len().max(partner.artifact.metadata.num_uavs as usize),
                empty,
            );
            for b in partner
                .artifact
                .bindings
                .iter()
                .filter(|b| storage::slot(b).is_some())
            {
                if storage::is_image(b) {
                    let NativeBindingResource::Texture { object, view } = state.binding(b.group,b.binding)? else { return Err(CompileError::InvalidResource); };
                    let slot = storage::slot(b).unwrap() as usize;
                    *uavs.get_mut(slot).ok_or(CompileError::InvalidResource)? = storage::descriptor(input,*object,*view)?;
                    continue;
                }
                let NativeBindingResource::Buffer {
                    object,
                    offset,
                    size,
                    write,
                } = state.binding(b.group, b.binding)?
                else {
                    return Err(CompileError::InvalidResource);
                };
                let mut d =
                    Self::native_storage_descriptor(input, *object, *offset, *size, *write, b)?;
                // Validate both reflected minimum sizes against the binding,
                // and preserve write authority required by either stage.
                if let (Some((_, _, _, old_access)), Some((_, _, _, access))) =
                    (uavs[b.slot as usize].address, d.address.as_mut())
                {
                    *access = *access | old_access;
                }
                uavs[b.slot as usize] = d;
            }
        }
        if !uavs.is_empty() {
            let table = self.native_table(&uavs)?;
            let o = ObjectRef::Generated(table);
            let size = (uavs.len() * 64) as u64;
            self.address_register(ubase, o, 0, size, Access::READ)?;
            if stage == Stage::Compute {
                self.native_load(stage, sb, 3, 0, uavs.len() as u32, o, 0, size)?;
            } else {
                // Graphics has one shared IBO table: Mesa fd6_emit_images uses
                // CP_LOAD_STATE6 / SB6_UAV / ST6_SHADER. ST6_UAV with a shader
                // block is the compute-only encoding, not a VS/FS preload.
                self.push_word(type7(0x36, 3).map_err(|_| CompileError::InvalidPm4)?)?;
                self.push_word(2 << 16 | 14 << 18 | (uavs.len() as u32) << 22)?;
                self.address_words(o, 0, size, Access::READ)?;
            }
        }
        Ok(())
    }
    fn native_compute(
        &mut self,
        input: CompileInput<'_>,
        state: &NativeState,
        p: &NativeComputePipeline,
        groups: [u32; 3],
    ) -> Result<(), CompileError> {
        validate_native_compute_pipeline(p)?;
        if groups.iter().any(|g| *g == 0 || *g > 65535) {
            return Err(CompileError::OutOfBounds);
        }
        self.invalidate_submission_caches()?;
        self.wait_for_idle()?;
        self.packet4(SP_UPDATE_CNTL, &[0xff])?;
        self.native_program(&p.shader, input)?;
        let m = &p.shader.artifact.metadata;
        let local = m.local_size;
        let cs = m.compute.as_ref().unwrap();
        let shared = cs.req_local_mem.max(m.shared_size).saturating_sub(1) / 1024;
        let mode = if m.constlen > 192 {
            2
        } else if m.constlen > 128 {
            1
        } else {
            0
        };
        self.packet4(0xa9b1, &[shared.max(1) | mode << 5])?;
        self.packet4(
            0xb997,
            &[
                packed4([cs.work_group_id, 252, 252, cs.local_invocation_id]),
                252 | 1 << 9,
            ],
        )?;
        self.packet4(SP_PS_WAVE_CNTL, &[m.double_threadsize as u32])?;
        let dim = if groups[2] * local[2] > 1 {
            3
        } else if groups[1] * local[1] > 1 {
            2
        } else {
            1
        };
        self.packet4(
            0xb990,
            &[
                dim | (local[0] - 1) << 2 | (local[1] - 1) << 12 | (local[2] - 1) << 22,
                groups[0] * local[0],
                0,
                groups[1] * local[1],
                0,
                groups[2] * local[2],
                0,
            ],
        )?;
        self.packet4(0xb999, &groups)?;
        let driver = [
            groups[0],
            groups[1],
            groups[2],
            dim,
            0,
            0,
            0,
            m.subgroup_size,
            local[0],
            local[1],
            local[2],
            m.subgroup_size.trailing_zeros(),
            0,
            0,
            0,
            0,
        ];
        self.native_resources(&p.shader, state, input, &driver, None)?;
        self.packet7(0x33, &[0, groups[0], groups[1], groups[2]])?;
        // UCHE storage writes must reach memory before the next dispatch/copy
        // invalidates its view; WFI alone does not write back dirty cache lines.
        self.addressed_cache_event(0x04)?; // Mesa FD_CACHE_CLEAN: CACHE_FLUSH_TS
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])?;
        self.wait_for_idle()?;
        self.last_draw = None;
        Ok(())
    }
    fn native_fragment(
        &mut self,
        m: &Metadata,
        targets: &[ir::ColorTargetState],
    ) -> Result<(), CompileError> {
        let ij = [
            sys(m, 59),
            sys(m, 60),
            sys(m, 61),
            sys(m, 62),
            sys(m, 63),
            sys(m, 64),
            sys(m, 65),
        ];
        let coord = sys(m, 19);
        let face = sys(m, 27);
        let sample = sys(m, 29);
        let mask = sys(m, 32);
        self.packet4(
            SP_REG_PROG_ID_0,
            &[
                packed4([face, sample, mask, ij[3]]),
                packed4([ij[0], ij[4], ij[2], ij[5]]),
                packed4([
                    ij[1],
                    ij[6],
                    coord,
                    if coord < 252 { coord + 2 } else { 252 },
                ]),
                0xfcfc,
            ],
        )?;
        self.packet4(SP_PS_INITIAL_TEX_LOAD_CNTL, &[((ij[0] == 252) as u32) << 3])?;
        self.packet4(SP_PS_TSIZE, &[m.num_samp.min(16) as u32])?;
        self.packet4(
            SP_PS_WAVE_CNTL,
            &[m.double_threadsize as u32 | ((m.total_in > 0) as u32) << 1],
        )?;
        self.packet4(SP_LB_PARAM_LIMIT, &[7])?;
        let mut flags = 0;
        for (bit, idx) in [0, 2, 1, 4, 5, 6].into_iter().enumerate() {
            if ij[idx] < 252 {
                flags |= 1 << bit;
            }
        }
        // A618 requires linear pixel coordinates to supply fragcoord/fragface size.
        if m.frag_face || m.fragcoord_compmask != 0 || ij[3] < 252 {
            flags |= 1 << 3;
        }
        flags |= m.fragcoord_compmask << 6;
        self.packet4(GRAS_CL_INTERP_CNTL, &[flags])?;
        self.packet4(RB_INTERP_CNTL, &[flags | ((m.total_in > 0) as u32) << 10])?;
        self.packet4(
            RB_PS_INPUT_CNTL,
            &[(mask < 252) as u32
                | (m.frag_face as u32) << 2
                | ((sample < 252) as u32) << 3
                | ((ij[3] < 252) as u32) << 6],
        )?;
        self.packet4(RB_PS_SAMPLEFREQ_CNTL, &[0])?;
        self.packet4(GRAS_LRZ_PS_INPUT_CNTL, &[0])?;
        self.packet4(GRAS_LRZ_PS_SAMPLEFREQ_CNTL, &[0])?;
        let depth = m
            .outputs
            .iter()
            .find(|o| o.slot == 0)
            .map_or(252, |o| o.regid);
        let sample_mask = m.outputs.iter().find(|o| o.slot == 3).map_or(252, |o| o.regid);
        // Mesa tu_shader.cc: SP sample-mask export register and RB coverage bit.
        // Emit the invalid reg and clear the bit again for a following plain FS.
        self.packet4(SP_PS_OUTPUT_CNTL, &[depth << 8 | sample_mask << 16 | 252 << 24])?;
        let mut regs = Vec::new();
        let mut components = 0;
        for i in 0..targets.len() {
            let output = m
                .outputs
                .iter()
                .find(|o| o.slot == 4 + i as u32 || (i == 0 && o.slot == 1));
            regs.push(output.map_or(252, |o| o.regid | (o.half as u32) << 8));
            if output.is_some() {
                components |= 15 << (4 * i);
            }
        }
        self.packet4(SP_PS_OUTPUT_REG, &regs)?;
        self.packet4(SP_PS_OUTPUT_MASK, &[components])?;
        self.packet4(RB_PS_OUTPUT_MASK, &[components])?;
        self.packet4(RB_PS_OUTPUT_CNTL, &[(m.writes_pos as u32) << 1 | (m.writes_smask as u32) << 2])?;
        if m.has_kill || m.writes_smask || m.no_earlyz || m.writes_pos || m.has_ssbo || m.num_uavs > 0 {
            // FRAG_WRITES_Z/KILL require late depth testing; disable early LRZ.
            self.packet4(RB_DEPTH_PLANE_CNTL, &[1])?;
            self.packet4(GRAS_SU_DEPTH_PLANE_CNTL, &[1])?;
        }
        Ok(())
    }
    fn native_draw(
        &mut self,
        input: CompileInput<'_>,
        state: &State<'_>,
        count: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
        base: Option<i32>,
    ) -> Result<(), CompileError> {
        let mut fixed_state;
        let native = if state.native.fixed {
            fixed_state=state.native.clone();
            let uniforms=state.uniforms.ok_or(CompileError::InvalidState)?;
            let mut words=[0u32;24];
            for (dst,value) in words[..16].iter_mut().zip(uniforms.transform().columns()) {*dst=value.to_bits();}
            for (dst,value) in words[16..20].iter_mut().zip(uniforms.color().components()) {*dst=value.to_bits();}
            let layout=&state.native.render.as_ref().ok_or(CompileError::InvalidState)?.descriptor;
            if !layout.layout().bind_groups().is_empty() {
                let texture=state.texture.ok_or(CompileError::InvalidState)?;
                let resource=c::find_resource(input.resources,texture)?;
                let ResourceKind::Image(image)=&resource.kind else {return Err(CompileError::InvalidResource);};
                if !image.usage.contains(ir::TextureUsage::SAMPLED) {return Err(CompileError::InvalidResource);}
                words[20]=u32::from(image.format==ir::TextureFormat::R8Unorm);
                let sampler=state.sampler.ok_or(CompileError::InvalidState)?;
                if sampler.compare().is_some() {return Err(unsupported("comparison sampler in fixed color program"));}
                fixed_state.groups=vec![(0,vec![
                    NativeBinding {binding:0,resource:if image.format==ir::TextureFormat::Nv12 {NativeBindingResource::Nv12Texture(texture)} else {NativeBindingResource::Texture {object:texture,view:None}}},
                    NativeBinding {binding:1,resource:NativeBindingResource::Sampler(sampler)},
                ])];
            }
            for stage in 0..2 {for (word,value) in words.iter().enumerate() {
                fixed_state.push[stage][word*4..word*4+4].copy_from_slice(&value.to_le_bytes());
            }}
            &fixed_state
        } else {&state.native};
        let p = native
            .render
            .as_ref()
            .ok_or(CompileError::InvalidState)?;
        validate_native_render_pipeline(p)?;
        let pass = state.pass.ok_or(CompileError::InvalidState)?;
        if count == 0 || instances == 0 {
            return Err(CompileError::InvalidResource);
        }
        first.checked_add(count).ok_or(CompileError::Overflow)?;
        first_instance
            .checked_add(instances)
            .ok_or(CompileError::Overflow)?;
        let topology = match p.descriptor.topology() {
            ir::PrimitiveTopology::TriangleList if count % 3 == 0 => 4,
            ir::PrimitiveTopology::TriangleStrip if count >= 3 => 6,
            ir::PrimitiveTopology::TriangleFan if count >= 3 => 5,
            ir::PrimitiveTopology::LineList if count % 2 == 0 => 2,
            ir::PrimitiveTopology::LineStrip if count >= 2 => 3,
            ir::PrimitiveTopology::PointList => 9,
            _ => return Err(CompileError::InvalidResource),
        };
        let attachments = native.colors.clone().unwrap_or_else(|| {
            vec![NativeColorAttachment {
                target: pass.target,
                load: ir::LoadOp::Load,
                store: ir::StoreOp::Store,
            }]
        });
        let color_states: Vec<_> = p.descriptor.color_targets().collect();
        if pass.depth.is_some_and(|id| p.vertex.artifact.bindings.iter().chain(&p.fragment.artifact.bindings).filter(|b|b.kind==sgfx_shader_ir3::ResourceKind::Texture).any(|b| matches!(native.binding(b.group,b.binding),Ok(NativeBindingResource::Texture{object,..}|NativeBindingResource::Nv12Texture(object)) if *object==id))) {
            return Err(unsupported("native depth attachment sampling feedback"));
        }
        if attachments.len() != color_states.len() {
            return Err(CompileError::InvalidResource);
        }
        let mut targets = Vec::new();
        for (a, desc) in attachments.iter().zip(&color_states) {
            let t = color::surface(
                c::find_resource(input.resources, a.target)?,
                ir::TextureUsage::RENDER_ATTACHMENT,
                input.capabilities.max_linear_pitch,
            )?;
            if t.logical != desc.format() {
                return Err(CompileError::InvalidResource);
            }
            if p.vertex.artifact.bindings.iter().chain(&p.fragment.artifact.bindings).filter(|b|b.kind==sgfx_shader_ir3::ResourceKind::Texture).any(|b| matches!(native.binding(b.group,b.binding),Ok(NativeBindingResource::Texture{object,..}|NativeBindingResource::Nv12Texture(object)) if *object==a.target)) {
                return Err(unsupported("native attachment sampling feedback"));
            }
            targets.push(t);
        }
        let target = targets[0].surface;
        let depth = match (pass.depth, p.descriptor.depth_state()) {
            (Some(id), Some(d)) => Some(DepthDrawState {
                target: c::require_depth_surface(
                    c::find_resource(input.resources, id)?,
                    target.width,
                    target.height,
                    input.capabilities.max_linear_pitch,
                )?,
                compare: d.compare(),
                write_enabled: d.write_enabled(),
            }),
            (None, Some(_)) => return Err(CompileError::InvalidState),
            _ => None,
        };
        let raster = p.descriptor.raster();
        let cull = match raster.cull_mode() {
            ir::CullMode::None => 0,
            ir::CullMode::Front => 1,
            ir::CullMode::Back => 2,
        } | if raster.front_face() == ir::FrontFace::Clockwise {
            4
        } else {
            0
        };
        let viewport = state.viewport.unwrap_or(
            ir::Viewport::new(
                0.0,
                0.0,
                target.width as f32,
                target.height as f32,
                0.0,
                1.0,
            )
            .map_err(|_| CompileError::InvalidResource)?,
        );
        self.wait_for_idle()?;
        let program_changed = !self
            .last_native_program
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, p));
        if program_changed {
            // Mesa program config invalidates shader loads. Keep its updates
            // paired with both complete program/private-memory bursts below;
            // never clear CONST_CONFIG while retaining only the shader loads.
            self.last_native_program = None;
            self.packet4(SP_UPDATE_CNTL, &[0xff])?;
            self.packet4(SP_VS_CONST_CONFIG, &[0, 0, 0, 0])?;
            self.packet4(SP_HS_CONFIG, &[0])?;
            self.packet4(SP_DS_CONFIG, &[0])?;
            self.packet4(SP_GS_CONFIG, &[0])?;
            self.packet4(
                SP_GFX_USIZE,
                &[p.vertex
                    .artifact
                    .metadata
                    .num_uavs
                    .max(p.fragment.artifact.metadata.num_uavs)],
            )?;
        }
        let raster_state = RenderState {
            target,
            area: pass.area,
            scissor: state.scissor.unwrap_or(pass.area),
            viewport,
            blend_enabled: p.descriptor.blend() != ir::BlendState::REPLACE,
            blend_control: c::blend_control(p.descriptor.blend()),
            cull,
            depth,
        };
        if native.colors.is_some()
            || color_states
                .iter()
                .any(|t| t.write_mask() != ir::ColorWriteMask::ALL)
        {
            self.native_color_state(raster_state, &targets, &color_states)?;
        } else {
            self.emit_render_state(raster_state)?;
        }
        // SGFX programmable positions use 0 <= Z <= W. The common fixed
        // state above uses a negative-one-to-one depth transform; inheriting
        // it turns a plain shader's Z/W=0.5 into 0.75, while frag_depth exports
        // bypass it. Match Mesa tu6_emit_viewport/tu6_emit_rs: select zero-to-one
        // clipping and map Z with minDepth + Z/W * (maxDepth - minDepth).
        let mut transform = viewport_transform(viewport);
        let [_, _, _, _, min_depth, max_depth] = viewport.components();
        transform[4] = min_depth.to_bits();
        transform[5] = (max_depth - min_depth).to_bits();
        self.packet4(GRAS_CL_VIEWPORT_XOFFSET, &transform)?;
        self.packet4(GRAS_CL_CNTL, &[0xc0])?; // ZERO_GB_SCALE_Z | VP_CLIP_CODE_IGNORE
        // Mesa fd6_rasterizer/tu6_emit_depth_bias use unscaled floating
        // constant/slope factors. The common state above resets all three
        // offset registers and POLY_OFFSET for every draw, including after a
        // biased pipeline. SGFX has no clamp, so the third register stays zero.
        let [constant, slope] = raster.depth_bias();
        if constant != 0.0 || slope != 0.0 {
            self.packet4(
                GRAS_SU_POLY_OFFSET_SCALE,
                &[slope.to_bits(), constant.to_bits(), 0],
            )?;
            self.packet4(GRAS_SU_CNTL, &[0x2010 | cull | (1 << 11)])?;
        }
        if program_changed {
            self.native_program(&p.vertex, input)?;
            self.native_program(&p.fragment, input)?;
        }
        let v = &p.vertex.artifact.metadata;
        let f = &p.fragment.artifact.metadata;
        let l = link(v, f)?;
        self.packet4(SP_VS_OUTPUT_CNTL, &[l.count])?;
        self.packet4(SP_VS_OUTPUT_REG, &l.outputs)?;
        self.packet4(SP_VS_VPC_DEST_REG, &l.destinations)?;
        self.packet4(VPC_VARYING_LM_TRANSFER_CNTL_DISABLE, &l.disable)?;
        self.packet4(VPC_VS_CNTL, &[l.stride | l.position << 8 | l.psize << 16])?;
        self.packet4(PC_VS_CNTL, &[l.stride | ((l.psize != 255) as u32) << 8])?;
        self.packet4(
            VPC_PS_CNTL,
            &[f.total_in | l.primitive << 8 | ((f.total_in > 0) as u32) << 16 | 255 << 24],
        )?;
        self.packet4(VPC_VS_CLIP_CULL_CNTL, &[0xffff00])?;
        self.packet4(VPC_VS_CLIP_CULL_CNTL_V2, &[0xffff00])?;
        self.packet4(GRAS_CL_VS_CLIP_CULL_DISTANCE, &[0])?;
        self.packet4(VPC_VS_SIV_CNTL, &[0xffff])?;
        self.packet4(VPC_VS_SIV_CNTL_V2, &[0xffff])?;
        self.packet4(GRAS_SU_VS_SIV_CNTL, &[0])?;
        self.packet4(PC_PS_CNTL, &[builtin::primitive_enabled(l.primitive)])?;
        self.packet4(VPC_VARYING_INTERP_MODE, &l.interp)?;
        self.packet4(VPC_VARYING_REPLACE_MODE, &[0; 8])?;
        self.native_fragment(f, &color_states)?;
        let attrs: Vec<_> = v.inputs.iter().filter(|i| !i.sysval).collect();
        self.packet4(
            VFD_CNTL_0,
            &[attrs.len() as u32 | (attrs.len() as u32) << 8],
        )?;
        self.packet4(
            VFD_CNTL_1,
            &[
                packed4([sys(v, 9), sys(v, 10), 252, 252]),
                0xfcfc,
                0xfcfcfcfc,
                252,
                0xfcfc,
                builtin::primitive_enabled(l.primitive),
            ],
        )?;
        self.packet4(
            VFD_INDEX_OFFSET,
            &[base.map_or(first, |b| b as u32), first_instance],
        )?;
        for (i, layout) in p.descriptor.vertex_buffers().iter().enumerate() {
            let &(_, id, offset) = native
                .vertices
                .iter()
                .find(|(slot, _, _)| *slot == i as u32)
                .ok_or(CompileError::InvalidState)?;
            let r = c::find_resource(input.resources, id)?;
            c::require_buffer_usage(r, ir::BufferUsage::VERTEX)?;
            if offset >= r.size {
                return Err(CompileError::OutOfBounds);
            }
            let instanced = layout.step_mode() == ir::VertexStepMode::Instance;
            let bytes = if instanced || base.is_none() {
                let last = if instanced {
                    first_instance + (instances - 1) / layout.instance_divisor()
                } else {
                    first + count - 1
                };
                let attribute_end = layout
                    .attributes()
                    .iter()
                    .map(|a| u64::from(a.offset()) + u64::from(a.format().byte_size()))
                    .max()
                    .unwrap_or(0);
                let required = u64::from(last) * u64::from(layout.stride()) + attribute_end;
                c::validate_range(r.size, offset, required)?;
                required
            } else {
                // Indexed vertex-rate fetches depend on GPU index contents,
                // including base_vertex. Keep the bound allocation suffix;
                // only non-indexed and instance-rate limits are known here.
                r.size - offset
            };
            self.address_register(
                VFD_VERTEX_BUFFER_BASE + i as u32 * 4,
                ObjectRef::External(id),
                offset,
                bytes,
                Access::READ,
            )?;
            self.packet4(
                VFD_VERTEX_BUFFER_SIZE + i as u32 * 4,
                &[
                    u32::try_from(bytes).map_err(|_| CompileError::Overflow)?,
                    layout.stride(),
                ],
            )?;
        }
        let mut fetch = Vec::new();
        let mut dest = Vec::new();
        for attr in attrs {
            let (slot, layout, a) = p
                .descriptor
                .vertex_buffers()
                .iter()
                .enumerate()
                .find_map(|(slot, b)| {
                    b.attributes()
                        .iter()
                        .find(|a| a.location() + 15 == attr.slot)
                        .map(|a| (slot, b, a))
                })
                .ok_or(CompileError::InvalidResource)?;
            let (format, float) = vertex_format(a.format());
            fetch.extend_from_slice(&[
                slot as u32
                    | a.offset() << 5
                    | ((layout.step_mode() == ir::VertexStepMode::Instance) as u32) << 17
                    | format << 20
                    | 1 << 30
                    | (float as u32) << 31,
                layout.instance_divisor(),
            ]);
            dest.push(attr.compmask | attr.regid << 4);
        }
        if !fetch.is_empty() {
            self.packet4(VFD_FETCH_INSTR, &fetch)?;
            self.packet4(VFD_DEST_CNTL, &dest)?;
        }
        let driver = [
            native.draw_index,
            base.map_or(first, |b| b as u32),
            first_instance,
            u32::MAX,
            if base.is_some() { u32::MAX } else { 0 },
        ];
        self.native_resources(&p.vertex, native, input, &driver, Some(&p.fragment))?;
        self.native_resources(
            &p.fragment,
            native,
            input,
            &[f.subgroup_size],
            Some(&p.vertex),
        )?;
        if base.is_some() {
            let b = state.index.ok_or(CompileError::InvalidState)?;
            let r = c::find_resource(input.resources, b.object)?;
            c::require_buffer_usage(r, ir::BufferUsage::INDEX)?;
            let bytes = u64::from(first + count) * b.format.byte_size();
            c::validate_range(r.size, b.offset, bytes)?;
            let kind = if b.format == ir::IndexFormat::Uint16 {
                0x400
            } else {
                0x800
            };
            self.push_word(type7(CP_DRAW_INDX_OFFSET, 7).map_err(|_| CompileError::InvalidPm4)?)?;
            self.extend_words(&[topology | kind, instances, count, first])?;
            self.address_words(ObjectRef::External(b.object), b.offset, bytes, Access::READ)?;
            self.push_word(first + count)?;
        } else {
            self.packet7(CP_DRAW_INDX_OFFSET, &[topology | 0x80, instances, count])?;
        }
        if v.has_ssbo || f.has_ssbo || v.num_uavs > 0 || f.num_uavs > 0 {
            self.addressed_cache_event(0x04)?;
            self.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])?;
            self.wait_for_idle()?;
        }
        // Commit only after the draw and all of its required barriers succeed.
        self.last_native_program = Some(p.clone());
        self.last_draw = None;
        Ok(())
    }
}
fn vertex_format(f: ir::VertexFormat) -> (u32, bool) {
    use ir::VertexFormat::*;
    match f {
        Float32 => (0x4a, true),
        Float32x2 => (0x67, true),
        Float32x3 => (0x74, true),
        Float32x4 => (0x82, true),
        Float16x2 => (0x45, true),
        Float16x4 => (0x62, true),
        Unorm8x4 => (0x30, true),
        Snorm8x4 => (0x32, true),
        Snorm10_10_10_2 => (0x39, true),
        Uint32 => (0x4b, false),
        Sint32 => (0x4c, false),
        Uint32x2 => (0x68, false),
        Sint32x2 => (0x69, false),
        Uint32x3 => (0x72, false),
        Sint32x3 => (0x73, false),
        Uint32x4 => (0x83, false),
        Sint32x4 => (0x84, false),
        Uint16x2 => (0x46, false),
        Sint16x2 => (0x47, false),
        Sint16x4 => (0x64, false),
    }
}

#[cfg(test)]
mod nv12_table_tests {
    use super::*;
    #[test]
    fn immutable_table_reuse_includes_secondary_plane_address_and_size() {
        let mut e=Emitter::new(65536);
        let object=ObjectRef::External(ObjectId::new(1));
        let mut d=Descriptor {words:[0;16],address:Some((object,0,1024,Access::READ)),secondary:Some((object,4096,512,Access::READ))};
        let original=e.native_table(&[d]).unwrap();
        assert_eq!(e.native_table(&[d]).unwrap(),original);
        let initial_words=e.texture_words.len();
        d.secondary=Some((object,8192,512,Access::READ));
        let moved=e.native_table(&[d]).unwrap();
        assert_ne!(original,moved);assert!(e.texture_words.len()>initial_words);
        assert!(e.texture_fixups.iter().any(|f|f.object==object&&f.object_offset==8192&&f.required_size==512));
        d.secondary=Some((object,8192,1024,Access::READ));
        assert_ne!(e.native_table(&[d]).unwrap(),moved);
    }
}
