//! Validate write-only image IBO relocations against canonical attached subresources.
use super::*;
pub(super) fn validate(
    words: &[u32],
    relocation: &Relocation,
    resource: &ResolvedResource,
    attached_image: bool,
) -> Result<(), &'static str> {
    if relocation.encoding != AddressEncoding::GpuVa49TexDescriptor
        || relocation.access & ACCESS_WRITE == 0
        || !attached_image
    {
        return Ok(());
    }
    let image = resource
        .linear_image
        .and_then(|i| image_range(i, relocation.resource_offset, relocation.required_size))
        .ok_or("a618: storage image requires a complete canonical subresource")?;
    if relocation.access != ACCESS_WRITE
        || image.modifier != ImageModifier::Linear
        || image.format != LinearImageFormat::Bgra8Unorm
        || resource
            .gpu_va
            .checked_add(relocation.resource_offset)
            .is_none_or(|a| a & 63 != 0)
    {
        return Err("a618: invalid storage image format/access/alignment");
    }
    let p = Packets::new(words)
        .filter_map(Result::ok)
        .find(|p| p.word_offset + 7 == relocation.pm4_word_offset)
        .ok_or("a618: storage image descriptor lacks canonical table write")?;
    if !matches!(
        p.header,
        Header::Type7 {
            opcode: CP_MEM_WRITE,
            ..
        }
    ) || p.payload.len() != 18
    {
        return Err("a618: storage image descriptor must be one complete table entry");
    }
    let d = &p.payload[2..];
    let expected = [
        0x4c006880,
        image.width | (image.height << 15),
        (image.row_pitch << 7) | (1 << 29),
        u32::try_from(relocation.required_size.div_ceil(4096))
            .map_err(|_| "a618: storage image size overflow")?,
        0,
        1 << 17,
    ];
    if d[..6] != expected || d[6..].iter().any(|w| *w != 0) {
        return Err("a618: storage image descriptor disagrees with attached layout");
    }
    Ok(())
}
