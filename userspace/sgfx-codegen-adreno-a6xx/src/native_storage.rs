//! Public write-only 2D RGBA8 image descriptors. Mesa fd6_view storage_descriptor.
use super::*;
pub(super) fn is_image(b: &sgfx_shader_ir3::Binding) -> bool {
    b.kind == sgfx_shader_ir3::ResourceKind::Texture && b.writable
}
pub(super) fn slot(b: &sgfx_shader_ir3::Binding) -> Option<u32> {
    if is_image(b) {
        b.slot.checked_add(16)
    } else if b.kind == sgfx_shader_ir3::ResourceKind::StorageBuffer {
        Some(b.slot)
    } else {
        None
    }
}
pub(super) fn descriptor(
    input: CompileInput<'_>,
    object: ObjectId,
    view: Option<ir::TextureViewDesc>,
) -> Result<Descriptor, CompileError> {
    let r = c::find_resource(input.resources, object)?;
    let ResourceKind::Image(image) = &r.kind else {
        return Err(CompileError::InvalidResource);
    };
    if image.format != ir::TextureFormat::Rgba8Unorm
        || image.modifier != crate::model::ImageModifier::Linear
    {
        return Err(CompileError::InvalidResource);
    }
    let color = color::surface(
        r,
        ir::TextureUsage::STORAGE,
        input.capabilities.max_linear_pitch,
    )?;
    let mut t = color.surface;
    let (mip, layer) = if let Some(v) = view {
        if v.format() != image.format
            || v.dimension() != ir::TextureViewDimension::D2
            || v.mip_level_count() != 1
            || v.array_layer_count() != 1
        {
            return Err(CompileError::InvalidResource);
        }
        (v.base_mip_level(), v.base_array_layer())
    } else {
        if t.mip_levels != 1 || image.texture.is_some_and(|l| l.array_layers != 1) {
            return Err(CompileError::InvalidResource);
        }
        (0, 0)
    };
    if let Some(layout) = image.texture {
        let sub = layout
            .subresource(mip, layer)
            .ok_or(CompileError::OutOfBounds)?;
        t.plane_offset = t
            .plane_offset
            .checked_add(sub.offset)
            .ok_or(CompileError::Overflow)?;
        t.plane_size = sub.size;
        t.width = sub.width;
        t.height = sub.height;
        t.stride = sub.row_pitch;
    } else if mip != 0 || layer != 0 {
        return Err(CompileError::OutOfBounds);
    }
    c::validate_range(r.size, t.plane_offset, t.plane_size)?;
    if t.plane_offset % 64 != 0
        || t.stride % 64 != 0
        || t.width == 0
        || t.height == 0
        || t.width > 0x7fff
        || t.height > 0x7fff
    {
        return Err(CompileError::InvalidResource);
    }
    let mut words = [0; 16];
    // Unlike sampled-image swizzle, image stores use the render/color SWAP.
    // BGRA backing needs WXYZ=1, native RGBA backing WZYX=0.
    words[0] = (0x30 << 22) | (color.swap << 30) | 0x6880;
    words[1] = t.width | (t.height << 15);
    words[2] = (t.stride << 7) | (1 << 29);
    words[3] = u32::try_from(t.plane_size.div_ceil(4096)).map_err(|_| CompileError::Overflow)?;
    words[5] = 1 << 17;
    Ok(Descriptor {
        secondary: None,
        words,
        address: Some((
            ObjectRef::External(object),
            t.plane_offset,
            t.plane_size,
            Access::WRITE,
        )),
    })
}
