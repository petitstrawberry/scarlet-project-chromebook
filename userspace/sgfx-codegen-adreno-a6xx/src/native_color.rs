//! Native linear float color and MRT state; Mesa 3f1b217b fd6_gmem/tu_clear_blit.
use super::*;
use crate::model::{DepthAttachment, ImageModifier, ResourceMeta};
use adreno_a6xx_layout::TILE_MODE_3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeColorAttachment {
    pub target: ObjectId,
    pub load: ir::LoadOp,
    pub store: ir::StoreOp,
}
#[derive(Clone, Debug, PartialEq)]
pub struct NativeRenderPass {
    pub colors: Vec<NativeColorAttachment>,
    pub area: ir::PixelRect,
    pub depth: Option<DepthAttachment>,
}
#[derive(Clone, Copy)]
pub(super) struct ColorSurface {
    pub surface: Surface,
    pub logical: ir::TextureFormat,
    pub format: u32,
    pub swap: u32,
}

pub(super) fn supported_format(f: ir::TextureFormat) -> bool {
    matches!(
        f,
        ir::TextureFormat::Bgra8Unorm
            | ir::TextureFormat::Rgba8Unorm
            | ir::TextureFormat::Bgra8UnormSrgb
            | ir::TextureFormat::Rgba8UnormSrgb
            | ir::TextureFormat::R8Unorm
            | ir::TextureFormat::Rg8Unorm
            | ir::TextureFormat::Rgba16Float
            | ir::TextureFormat::Rgba32Float
    )
}
pub(super) fn surface(
    r: &ResourceMeta,
    usage: ir::TextureUsage,
    max_pitch: u32,
) -> Result<ColorSurface, CompileError> {
    let ResourceKind::Image(image) = &r.kind else {
        return Err(CompileError::InvalidResource);
    };
    if (!supported_format(image.format)
        && !(usage == ir::TextureUsage::SAMPLED
            && matches!(
                image.format,
                ir::TextureFormat::R8Unorm
                    | ir::TextureFormat::Rg8Unorm
                    | ir::TextureFormat::Bgra8UnormSrgb
                    | ir::TextureFormat::Rgba8UnormSrgb
            )))
        || !image.usage.contains(usage)
    {
        return Err(CompileError::InvalidResource);
    }
    let (format, swap) = match (image.format, image.storage_format) {
        (
            ir::TextureFormat::Bgra8Unorm
            | ir::TextureFormat::Rgba8Unorm
            | ir::TextureFormat::R8Unorm
            | ir::TextureFormat::Rg8Unorm
            | ir::TextureFormat::Bgra8UnormSrgb
            | ir::TextureFormat::Rgba8UnormSrgb,
            ir::TextureFormat::Bgra8Unorm,
        ) => (0x30, 1),
        (
            ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::Rgba8UnormSrgb,
            ir::TextureFormat::Rgba8Unorm,
        ) => (0x30, 0),
        (ir::TextureFormat::Rgba16Float, ir::TextureFormat::Rgba16Float) => (0x62, 0),
        (ir::TextureFormat::Rgba32Float, ir::TextureFormat::Rgba32Float) => (0x82, 0),
        _ => return Err(CompileError::InvalidResource),
    };
    if image.modifier != ImageModifier::Linear {
        if !matches!(
            image.format,
            ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Bgra8UnormSrgb
        ) {
            return Err(unsupported("native tiled float/alternate color layout"));
        }
        return Ok(ColorSurface {
            surface: c::require_image_surface(r, usage, max_pitch)?,
            logical: image.format,
            format,
            swap: 0,
        });
    }
    c::validate_image_layout(r.size, image, max_pitch)?;
    let plane = image.planes[0];
    let tex = image.texture;
    Ok(ColorSurface {
        logical: image.format,
        format,
        swap,
        surface: Surface {
            nv12: None,
            mip_levels: tex.map_or(1, |t| t.mip_levels),
            array_pitch: tex.map_or(plane.size, |t| u64::from(t.layer_stride)),
            object: r.id,
            plane_offset: plane.offset,
            plane_size: plane.size,
            width: image.extent.width(),
            height: image.extent.height(),
            stride: plane.stride,
            tile_mode: 0,
            alpha_mask: image.format == ir::TextureFormat::R8Unorm,
            rg_mask: image.format == ir::TextureFormat::Rg8Unorm,
            srgb: color_encoding::srgb(image.format),
        },
    })
}

/// Texture coordinates address the selected mip/layer range directly; the
/// allocation identity remains stable for cached bindings and alias tracking.
pub(super) fn sampled_descriptor(
    r: &ResourceMeta,
    view: Option<ir::TextureViewDesc>,
    max_pitch: u32,
) -> Result<Descriptor, CompileError> {
    let ResourceKind::Image(i) = &r.kind else {
        return Err(CompileError::InvalidResource);
    };
    if i.format==ir::TextureFormat::Nv12 {
        return Err(unsupported("native NV12 YCbCr conversion not implemented"));
    }
    let color = if i.format == ir::TextureFormat::Depth32Float {
        depth_surface(r, ir::TextureUsage::SAMPLED, max_pitch)?
    } else {
        surface(r, ir::TextureUsage::SAMPLED, max_pitch)?
    };
    let mut t = color.surface;
    let (format, dimension, base_mip, levels, base_layer, layers) = view.map_or(
        (
            i.format,
            ir::TextureViewDimension::D2,
            0,
            t.mip_levels,
            0,
            1,
        ),
        |v| {
            (
                v.format(),
                v.dimension(),
                v.base_mip_level(),
                v.mip_level_count(),
                v.base_array_layer(),
                v.array_layer_count(),
            )
        },
    );
    let allocation_layers = i.texture.map_or(1, |l| l.array_layers);
    if !i.format.view_compatible(format)
        || levels == 0
        || layers == 0
        || base_mip
            .checked_add(levels)
            .is_none_or(|n| n > t.mip_levels)
        || base_layer
            .checked_add(layers)
            .is_none_or(|n| n > allocation_layers)
    {
        return Err(CompileError::InvalidResource);
    }
    let (kind, depth) = match dimension {
        ir::TextureViewDimension::D1 if i.extent.height() == 1 && layers == 1 => (0, 1),
        ir::TextureViewDimension::D1Array if i.extent.height() == 1 => (0, layers),
        ir::TextureViewDimension::D2 if layers == 1 => (1, 1),
        ir::TextureViewDimension::D2Array => (1, layers),
        ir::TextureViewDimension::Cube if layers == 6 && i.extent.width() == i.extent.height() => {
            (2, 1)
        }
        _ => return Err(CompileError::InvalidResource),
    };
    if let Some(layout) = i.texture {
        let sub = layout
            .subresource(base_mip, base_layer)
            .ok_or(CompileError::OutOfBounds)?;
        t.plane_offset = t
            .plane_offset
            .checked_add(sub.offset)
            .ok_or(CompileError::Overflow)?;
        // Retain the allocation suffix so hardware mip padding/prefetch stays
        // covered, without allowing any preceding mip/layer address.
        t.plane_size = t
            .plane_size
            .checked_sub(sub.offset)
            .ok_or(CompileError::OutOfBounds)?;
        t.width = sub.width;
        t.height = sub.height;
        t.stride = sub.row_pitch;
    }
    t.mip_levels = levels;
    t.alpha_mask = false;
    let mut words = [0; 16];
    words[..4].copy_from_slice(&Emitter::texture_descriptor_prefix(t)?);
    words[0] = (words[0] & !((255 << 22) | (3 << 30) | (0xfff << 4) | (1 << 2)))
        | color.format << 22
        | color.swap << 30;
    // R8 is physically expanded into alpha by the upload ABI. Native WGSL
    // still observes logical (r,0,0,1), unlike the fixed alpha-mask shader.
    let swizzle = match format {
        ir::TextureFormat::R8Unorm => [3, 4, 4, 5],
        ir::TextureFormat::Rg8Unorm => [0, 1, 4, 5],
        ir::TextureFormat::Depth32Float => [0, 0, 0, 5],
        _ => [0, 1, 2, 3],
    };
    for (n, c) in swizzle.into_iter().enumerate() {
        words[0] |= c << (4 + n * 3);
    }
    if matches!(
        format,
        ir::TextureFormat::Bgra8UnormSrgb | ir::TextureFormat::Rgba8UnormSrgb
    ) {
        words[0] |= 1 << 2;
    }
    words[2] = (words[2] & !(7 << 29)) | kind << 29;
    words[5] = depth << 17;
    Ok(Descriptor {
        secondary: None,
        words,
        address: Some((
            ObjectRef::External(r.id),
            t.plane_offset,
            t.plane_size,
            Access::READ,
        )),
    })
}

pub(super) fn begin(
    e: &mut Emitter,
    s: &mut State<'_>,
    input: CompileInput<'_>,
    pass: &NativeRenderPass,
) -> Result<(), CompileError> {
    if s.pass.is_some() || s.native.compute_pass {
        return Err(CompileError::InvalidState);
    }
    if pass.colors.is_empty() || pass.colors.len() > 8 {
        return Err(CompileError::InvalidResource);
    }
    let mut targets = Vec::new();
    for (i, a) in pass.colors.iter().enumerate() {
        if pass.colors[..i].iter().any(|b| a.target == b.target) {
            return Err(unsupported("native attachment alias"));
        }
        let t = surface(
            c::find_resource(input.resources, a.target)?,
            ir::TextureUsage::RENDER_ATTACHMENT,
            input.capabilities.max_linear_pitch,
        )?;
        c::validate_rect(pass.area, t.surface)?;
        if targets.first().is_some_and(|first: &ColorSurface| {
            first.surface.width != t.surface.width || first.surface.height != t.surface.height
        }) {
            return Err(CompileError::InvalidResource);
        }
        targets.push(t);
    }
    let primary = targets[0].surface;
    let depth = if let Some(a) = pass.depth {
        if pass.colors.iter().any(|c| c.target == a.target) {
            return Err(CompileError::InvalidResource);
        }
        let t = c::require_depth_surface(
            c::find_resource(input.resources, a.target)?,
            primary.width,
            primary.height,
            input.capabilities.max_linear_pitch,
        )?;
        if let ir::DepthLoadOp::Clear(value) = a.load {
            e.clear_depth(t, pass.area, value)?;
        }
        Some(a.target)
    } else {
        None
    };
    for (a, t) in pass.colors.iter().zip(targets) {
        if let ir::LoadOp::Clear(value) = a.load {
            e.native_clear_color(t, pass.area, value)?;
        }
    }
    s.native = NativeState::default();
    s.native.colors = Some(pass.colors.clone());
    s.pass = Some(c::PassState {
        target: pass.colors[0].target,
        area: pass.area,
        depth,
    });
    s.draw_batch_started = false;
    s.pipeline = None;
    s.vertex = None;
    s.index = None;
    s.texture = None;
    s.sampler = None;
    s.uniforms = None;
    s.scissor = None;
    s.viewport = None;
    Ok(())
}

impl Emitter {
    pub(super) fn native_color_state(
        &mut self,
        draw: RenderState,
        targets: &[ColorSurface],
        states: &[ir::ColorTargetState],
    ) -> Result<(), CompileError> {
        self.packet7(CP_REG_WRITE, &[2, RB_RENDER_CNTL, 0x10])?;
        let mut blend = 0;
        let mut srgb_mask = 0;
        for (i, (target, state)) in targets.iter().zip(states).enumerate() {
            let t = target.surface;
            let i = i as u32;
            let enabled = state.blend() != ir::BlendState::REPLACE;
            srgb_mask |= u32::from(t.srgb) << i;
            let mask = u32::from(state.write_mask().bits()) & if t.alpha_mask { 1 } else if t.rg_mask { 3 } else { 15 };
            if enabled {
                blend |= 1 << i;
            }
            self.packet4(
                RB_MRT_CONTROL + i * 8,
                &[
                    0x60 | mask << 7 | if enabled { 3 } else { 0 },
                    color_encoding::target_blend(c::blend_control(state.blend()), t.alpha_mask || t.rg_mask),
                ],
            )?;
            self.packet4(
                RB_MRT_BUF_INFO + i * 8,
                &[target.format
                    | t.tile_mode << 8
                    | if t.alpha_mask { 3 << 13 } else { target.swap << 13 }],
            )?;
            self.packet4(
                RB_MRT_PITCH + i * 8,
                &[
                    t.stride >> 6,
                    u32::try_from(t.array_pitch >> 6).map_err(|_| CompileError::Overflow)?,
                ],
            )?;
            self.address_register(
                RB_MRT_BASE + i * 8,
                ObjectRef::External(t.object),
                t.plane_offset,
                t.plane_size,
                Access::WRITE,
            )?;
            self.packet4(RB_MRT_BASE_GMEM + i * 8, &[0])?;
            self.packet4(RB_COLOR_FLAG_BUFFER_ADDR + i * 3, &[0; 3])?;
            self.packet4(SP_PS_MRT_REG + i, &[target.format])?;
        }
        // Count/masks gate all consumers, and inactive write enables are reset
        // so shrinking an MRT pass never retains an older target's writes.
        for i in targets.len() as u32..8 {
            self.packet4(RB_MRT_CONTROL + i * 8, &[0])?;
        }
        self.packet4(GRAS_LRZ_MRT_BUFFER_INFO_0, &[targets[0].format])?;
        self.packet4(RB_SRGB_CNTL, &[srgb_mask])?;
        self.packet4(SP_SRGB_CNTL, &[srgb_mask])?;
        self.packet4(GRAS_CL_ARRAY_SIZE, &[0])?;
        let independent = u32::from(targets.len() > 1) << 8;
        self.packet4(RB_BLEND_CNTL, &[0xffff0000 | independent | blend])?;
        self.packet4(SP_BLEND_CNTL, &[independent | blend])?;
        self.packet4(RB_DITHER_CNTL, &[0])?;
        self.packet4(SP_PS_MRT_CNTL, &[targets.len() as u32])?;
        self.packet4(RB_PS_MRT_CNTL, &[targets.len() as u32])?;
        self.emit_render_common_state(draw)
    }
    fn native_clear_color(
        &mut self,
        t: ColorSurface,
        area: ir::PixelRect,
        color: ir::Color,
    ) -> Result<(), CompileError> {
        if t.format == 0x30 && t.swap == 1 {
            return self.clear(t.surface, area, color);
        }
        self.submission_begin_2d()?;
        self.wait_for_idle()?;
        self.emit_coordinates(area, None)?;
        let (ifmt, values) = match t.format {
            0x30 => (0, normalized_clear_words(t.surface, color)),
            0x62 => (3, color.components().map(half_rtz)),
            0x82 => (4, color.components().map(f32::to_bits)),
            _ => return Err(CompileError::InvalidResource),
        };
        self.packet4(RB_A2D_CLEAR_COLOR_DW0, &values)?;
        self.emit_a2d_common(t.format, ifmt, true, true)?;
        if ifmt == 3 {
            self.packet4(SP_A2D_OUTPUT_INFO, &[t.format << 3 | 15 << 12 | 1])?;
        }
        self.emit_destination(t.surface, t.format, t.swap << 10)?;
        self.emit_blit()?;
        self.submission_end()
    }
}

// Mesa _mesa_float_to_float16_rtz: deterministic truncation, including
// subnormals and finite overflow saturation. Color already rejects NaN/Inf.
pub(super) fn half_rtz(value: f32) -> u32 {
    let bits = value.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exp = (bits >> 23) & 255;
    let mant = bits & 0x7fffff;
    sign | if exp > 142 {
        0x7bff
    } else if exp >= 113 {
        ((exp - 112) << 10) | (mant >> 13)
    } else if exp >= 103 {
        (mant | 0x800000) >> (126 - exp)
    } else {
        0
    }
}

pub(super) fn upload(
    e: &mut Emitter,
    input: CompileInput<'_>,
    destination: ObjectId,
    area: ir::PixelRect,
    bytes_per_row: u32,
    data: &[u8],
) -> Result<(), CompileError> {
    if matches!(&c::find_resource(input.resources, destination)?.kind, ResourceKind::Image(i) if i.format == ir::TextureFormat::Depth32Float)
    {
        return upload_depth(e, input, destination, area, bytes_per_row, data);
    }
    let target = surface(
        c::find_resource(input.resources, destination)?,
        ir::TextureUsage::COPY_DST,
        input.capabilities.max_linear_pitch,
    )?;
    let t = target.surface;
    if t.tile_mode != 0 {
        return Err(unsupported("native tiled color upload"));
    }
    c::validate_rect(area, t)?;
    let bpp = target
        .logical
        .bytes_per_pixel()
        .ok_or(CompileError::InvalidResource)?;
    let row = area
        .width()
        .checked_mul(bpp)
        .ok_or(CompileError::Overflow)?;
    let required = u64::from(bytes_per_row) * u64::from(area.height() - 1) + u64::from(row);
    if bytes_per_row < row || required > data.len() as u64 {
        return Err(CompileError::OutOfBounds);
    }
    let len = (row as usize)
        .checked_mul(area.height() as usize)
        .ok_or(CompileError::Overflow)?;
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(len)
        .map_err(|_| CompileError::OutOfMemory)?;
    for y in 0..area.height() as usize {
        let offset = y * bytes_per_row as usize;
        packed.extend_from_slice(&data[offset..offset + row as usize]);
    }
    let offset = t
        .plane_offset
        .checked_add(u64::from(area.y()) * u64::from(t.stride))
        .and_then(|o| o.checked_add(u64::from(area.x()) * u64::from(bpp)))
        .ok_or(CompileError::Overflow)?;
    // Earlier rendering may still own dirty CCU lines for this image. Flush
    // before the CP writes, then invalidate consumers after the row-copy waits.
    e.resource_barrier()?;
    e.upload_rows(destination, offset, t.stride, row as usize, packed)?;
    e.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])
}

/// Equal-size linear float copies preserve all bits, including NaN payloads,
/// signed zero and infinities, without any A2D floating-point conversion.
pub(super) fn copy(
    e: &mut Emitter,
    input: CompileInput<'_>,
    source: ObjectId,
    source_rect: ir::PixelRect,
    destination: ObjectId,
    destination_rect: ir::PixelRect,
) -> Result<(), CompileError> {
    if source == destination || !source_rect.same_extent(destination_rect) {
        return Err(CompileError::InvalidResource);
    }
    if matches!(&c::find_resource(input.resources, source)?.kind, ResourceKind::Image(i) if i.format == ir::TextureFormat::Depth32Float)
    {
        let src = depth_surface(
            c::find_resource(input.resources, source)?,
            ir::TextureUsage::COPY_SRC,
            input.capabilities.max_linear_pitch,
        )?;
        let dst = depth_surface(
            c::find_resource(input.resources, destination)?,
            ir::TextureUsage::COPY_DST,
            input.capabilities.max_linear_pitch,
        )?;
        c::validate_rect(source_rect, src.surface)?;
        c::validate_rect(destination_rect, dst.surface)?;
        return e.native_blit(
            src,
            ObjectRef::External(source),
            source_rect,
            dst,
            destination_rect,
            ir::FilterMode::Nearest,
            [false; 2],
        );
    }
    let src = surface(
        c::find_resource(input.resources, source)?,
        ir::TextureUsage::COPY_SRC,
        input.capabilities.max_linear_pitch,
    )?;
    let dst = surface(
        c::find_resource(input.resources, destination)?,
        ir::TextureUsage::COPY_DST,
        input.capabilities.max_linear_pitch,
    )?;
    if src.logical != dst.logical || src.surface.tile_mode != 0 || dst.surface.tile_mode != 0 {
        return Err(CompileError::InvalidResource);
    }
    c::validate_rect(source_rect, src.surface)?;
    c::validate_rect(destination_rect, dst.surface)?;
    let bpp = src
        .logical
        .bytes_per_pixel()
        .ok_or(CompileError::InvalidResource)?;
    let row_bytes = usize::try_from(u64::from(source_rect.width()) * u64::from(bpp))
        .map_err(|_| CompileError::Overflow)?;
    let offset = |s: Surface, r: ir::PixelRect, row: u32| {
        s.plane_offset
            .checked_add(u64::from(r.y() + row) * u64::from(s.stride))
            .and_then(|o| o.checked_add(u64::from(r.x()) * u64::from(bpp)))
            .ok_or(CompileError::Overflow)
    };
    e.resource_barrier()?;
    for y in 0..source_rect.height() {
        e.copy_memory(
            ObjectRef::External(source),
            offset(src.surface, source_rect, y)?,
            row_bytes,
            destination,
            offset(dst.surface, destination_rect, y)?,
        )?;
    }
    e.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])
}

pub(super) fn depth_surface(
    r: &ResourceMeta,
    usage: ir::TextureUsage,
    max_pitch: u32,
) -> Result<ColorSurface, CompileError> {
    let ResourceKind::Image(i) = &r.kind else {
        return Err(CompileError::InvalidResource);
    };
    if i.format != ir::TextureFormat::Depth32Float
        || i.storage_format != i.format
        || i.modifier != ImageModifier::A6xxTile6_3Depth
        || !i.usage.contains(usage)
    {
        return Err(CompileError::InvalidResource);
    }
    c::validate_image_layout(r.size, i, max_pitch)?;
    let p = i.planes[0];
    Ok(ColorSurface {
        logical: i.format,
        format: 0x4a,
        swap: 0,
        surface: Surface {
            object: r.id,
            plane_offset: p.offset,
            plane_size: p.size,
            width: i.extent.width(),
            height: i.extent.height(),
            stride: p.stride,
            tile_mode: TILE_MODE_3,
            alpha_mask: false,
        rg_mask: false,
            srgb: false,
            nv12: None,
            mip_levels: 1,
            array_pitch: p.size,
        },
    })
}

fn upload_depth(
    e: &mut Emitter,
    input: CompileInput<'_>,
    destination: ObjectId,
    area: ir::PixelRect,
    bytes_per_row: u32,
    data: &[u8],
) -> Result<(), CompileError> {
    let dst = depth_surface(
        c::find_resource(input.resources, destination)?,
        ir::TextureUsage::COPY_DST,
        input.capabilities.max_linear_pitch,
    )?;
    c::validate_rect(area, dst.surface)?;
    let row = area.width().checked_mul(4).ok_or(CompileError::Overflow)?;
    let required = u64::from(bytes_per_row) * u64::from(area.height() - 1) + u64::from(row);
    if bytes_per_row < row || required > data.len() as u64 {
        return Err(CompileError::OutOfBounds);
    }
    let stride = row.checked_add(63).ok_or(CompileError::Overflow)? & !63;
    if stride > input.capabilities.max_linear_pitch {
        return Err(CompileError::InvalidResource);
    }
    let len = usize::try_from(u64::from(stride) * u64::from(area.height()))
        .map_err(|_| CompileError::Overflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| CompileError::OutOfMemory)?;
    bytes.resize(len, 0);
    for y in 0..area.height() as usize {
        bytes[y * stride as usize..y * stride as usize + row as usize].copy_from_slice(
            &data[y * bytes_per_row as usize..y * bytes_per_row as usize + row as usize],
        );
    }
    let id = e.add_generated(&bytes, 64, Access::READ, GeneratedObjectKind::Upload)?;
    let src = ColorSurface {
        surface: Surface {
            object: destination,
            plane_offset: 0,
            plane_size: len as u64,
            width: area.width(),
            height: area.height(),
            stride,
            tile_mode: 0,
            alpha_mask: false,
        rg_mask: false,
            srgb: false,
            nv12: None,
            mip_levels: 1,
            array_pitch: len as u64,
        },
        logical: ir::TextureFormat::Depth32Float,
        format: 0x4a,
        swap: 0,
    };
    let rect = ir::PixelRect::new(0, 0, area.width(), area.height())
        .map_err(|_| CompileError::InvalidResource)?;
    e.native_blit(
        src,
        ObjectRef::Generated(id),
        rect,
        dst,
        area,
        ir::FilterMode::Nearest,
        [false; 2],
    )
}

pub(super) fn blit(
    e: &mut Emitter,
    input: CompileInput<'_>,
    source: ObjectId,
    source_rect: ir::PixelRect,
    destination: ObjectId,
    destination_rect: ir::PixelRect,
    filter: ir::FilterMode,
    flips: [bool; 2],
) -> Result<(), CompileError> {
    if source == destination {
        return Err(CompileError::InvalidResource);
    }
    let src = surface(
        c::find_resource(input.resources, source)?,
        ir::TextureUsage::COPY_SRC,
        input.capabilities.max_linear_pitch,
    )?;
    let dst = surface(
        c::find_resource(input.resources, destination)?,
        ir::TextureUsage::COPY_DST,
        input.capabilities.max_linear_pitch,
    )?;
    if src.logical != dst.logical || src.surface.tile_mode != 0 || dst.surface.tile_mode != 0 {
        return Err(CompileError::InvalidResource);
    }
    c::validate_rect(source_rect, src.surface)?;
    c::validate_rect(destination_rect, dst.surface)?;
    e.native_blit(
        src,
        ObjectRef::External(source),
        source_rect,
        dst,
        destination_rect,
        filter,
        flips,
    )
}

impl Emitter {
    /// Mesa r2d_setup_common/r2d_src. Used for filtered float color and exact
    /// geometry depth transfers; raw linear color copies use CP_MEMCPY instead.
    fn native_blit(
        &mut self,
        source: ColorSurface,
        source_object: ObjectRef,
        source_rect: ir::PixelRect,
        destination: ColorSurface,
        destination_rect: ir::PixelRect,
        filter: ir::FilterMode,
        flips: [bool; 2],
    ) -> Result<(), CompileError> {
        let ifmt = match destination.format {
            0x62 => 3,
            0x82 | 0x4a => 4,
            _ => return Err(CompileError::InvalidResource),
        };
        self.resource_barrier()?;
        self.submission_begin_2d()?;
        self.wait_for_idle()?;
        self.packet4(
            GRAS_A2D_SRC_XMIN,
            &[
                source_rect.x() << 8,
                (source_rect.x() + source_rect.width() - 1) << 8,
                source_rect.y() << 8,
                (source_rect.y() + source_rect.height() - 1) << 8,
            ],
        )?;
        self.emit_coordinates(destination_rect, None)?;
        self.emit_a2d_common(destination.format, ifmt, false, true)?;
        if source.format == 0x62 {
            self.packet4(
                SP_A2D_OUTPUT_INFO,
                &[destination.format << 3 | 15 << 12 | 1],
            )?;
        }
        let rotation = match flips {
            [false, false] => 0,
            [true, false] => 4,
            [false, true] => 5,
            [true, true] => 2,
        };
        if rotation != 0 {
            let control =
                destination.format << 8 | BLIT_CHANNEL_MASK | BLIT_SCISSOR | ifmt << 24 | rotation;
            self.packet4(RB_A2D_BLT_CNTL, &[control])?;
            self.packet4(GRAS_A2D_BLT_CNTL, &[control])?;
        }
        let src = source.surface;
        self.packet4(
            TPL1_A2D_SRC_TEXTURE_INFO,
            &[source.format
                | src.tile_mode << 8
                | source.swap << 10
                | SOURCE_TEXTURE_REQUIRED
                | if filter == ir::FilterMode::Linear {
                    1 << 16
                } else {
                    0
                }],
        )?;
        self.packet4(TPL1_A2D_SRC_TEXTURE_SIZE, &[src.width | src.height << 15])?;
        self.address_register(
            TPL1_A2D_SRC_TEXTURE_BASE,
            source_object,
            src.plane_offset,
            src.plane_size,
            Access::READ,
        )?;
        self.packet4(TPL1_A2D_SRC_TEXTURE_PITCH, &[(src.stride >> 6) << 9])?;
        self.emit_destination(
            destination.surface,
            destination.format,
            destination.swap << 10,
        )?;
        self.emit_blit()?;
        self.submission_end()
    }
}

/// The exact combined A618 NV12 descriptor already used by canonical shaders.
/// Matrix conversion is shader arithmetic; chroma siting is descriptor state.
pub(super) fn nv12_descriptor(r:&ResourceMeta,max_pitch:u32)->Result<Descriptor,CompileError> {
    let ResourceKind::Image(image)=&r.kind else {return Err(CompileError::InvalidResource);};
    if image.format!=ir::TextureFormat::Nv12 {return Err(CompileError::InvalidResource);}
    let t=c::require_image_surface(r,ir::TextureUsage::SAMPLED,max_pitch)?;
    let uv=t.nv12.ok_or(CompileError::InvalidResource)?;
    let mut words=[0u32;16];
    words[..4].copy_from_slice(&Emitter::texture_descriptor_prefix(t)?);
    words[5]=1<<17;
    words[6]=t.stride.checked_shl(8).ok_or(CompileError::Overflow)?;
    Ok(Descriptor {words,
        address:Some((ObjectRef::External(t.object),t.plane_offset,t.plane_size,Access::READ)),
        secondary:Some((ObjectRef::External(t.object),uv.uv_offset,uv.uv_size,Access::READ))})
}
