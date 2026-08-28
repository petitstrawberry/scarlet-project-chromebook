//! Versioned userspace/kernel submit wire format for Adreno A6xx.
//!
//! The outer Scarlet GPU queue submit remains backend-neutral and opaque. This
//! crate only encodes and structurally decodes the A6xx payload carried inside
//! that envelope. PM4 safety policy and resource authority checks remain in
//! the concrete kernel driver.

#![no_std]

use adreno_a6xx_shader_pack::{SHADER_SIZE, ShaderVariant};

/// Four-byte A6xx submit magic (`A6XS`) interpreted as little endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"A6XS");
/// Wire ABI major version.
pub const VERSION_MAJOR: u16 = 1;
/// Wire ABI minor version.
/// Minor 1 assigns relocation byte 30 to `RelocationSource`; minor 0 required
/// it to be zero and therefore supports attachment sources only.
pub const VERSION_MINOR: u16 = 1;
/// Fixed v1 header size.
pub const HEADER_SIZE: usize = 64;
/// Fixed v1 resource record size.
pub const RESOURCE_SIZE: usize = 32;
/// Fixed v1 relocation record size.
pub const RELOCATION_SIZE: usize = 32;
/// Maximum A618 payload accepted by Scarlet's opaque queue transport.
pub const MAX_SUBMIT_SIZE: usize = 256 * 1024;
/// Maximum resources referenced by one v1 submission.
pub const MAX_RESOURCES: usize = 1_024;
/// Maximum relocations referenced by one v1 submission.
pub const MAX_RELOCATIONS: usize = 4_096;

/// Resource is read by the GPU.
pub const ACCESS_READ: u32 = 1 << 0;
/// Resource is written by the GPU.
pub const ACCESS_WRITE: u32 = 1 << 1;
/// All access bits understood by v1.
pub const ACCESS_MASK: u32 = ACCESS_READ | ACCESS_WRITE;

/// Structural wire-format failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// An integer calculation overflowed.
    Overflow,
    /// The provided byte buffer is too small or larger than the v1 limit.
    InvalidSize,
    /// The magic or major ABI version is not supported.
    UnsupportedVersion,
    /// A field or record uses an unsupported value.
    InvalidField,
    /// Reserved v1 bytes or bits are non-zero.
    ReservedNotZero,
    /// Table offsets, sizes, ordering, or alignment are not canonical.
    InvalidTable,
    /// Relocations are not strictly ordered by PM4 word offset.
    RelocationsNotSorted,
    /// A relocation falls outside PM4 or its selected resource range.
    RelocationOutOfBounds,
    /// An address placeholder is not zero before kernel relocation.
    NonZeroPlaceholder,
}

/// Resource authority referenced by the A6xx command stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resource {
    /// Context-local, generation-checked attachment token.
    pub attachment_token: u64,
    /// First byte within the attached object made visible to this submit.
    pub range_offset: u64,
    /// Number of visible bytes starting at `range_offset`.
    pub range_size: u64,
    /// `ACCESS_READ`, `ACCESS_WRITE`, or both.
    pub access: u32,
}

/// Encoding used to patch one symbolic GPU address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum AddressEncoding {
    /// Two adjacent dwords containing a little-endian 64-bit GPU VA.
    GpuVa64 = 1,
    /// A6xx TEX_MEMOBJ address bits (word 0 bits 5..31 and word 1 bits
    /// 0..16), preserving non-address descriptor fields.
    GpuVa49TexDescriptor = 3,
}

impl AddressEncoding {
    const fn from_raw(value: u16) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::GpuVa64),
            3 => Ok(Self::GpuVa49TexDescriptor),
            _ => Err(Error::InvalidField),
        }
    }

    /// Number of PM4 dwords occupied by this address.
    pub const fn word_count(self) -> u32 {
        match self {
            Self::GpuVa64 | Self::GpuVa49TexDescriptor => 2,
        }
    }

    fn placeholder_is_valid(self, words: &[u32]) -> bool {
        match self {
            Self::GpuVa64 => words == [0, 0],
            Self::GpuVa49TexDescriptor => {
                words.len() == 2 && words[0] == 0 && words[1] & 0x1ffff == 0
            }
        }
    }
}

/// One kernel-applied symbolic address relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    /// First address-placeholder dword in the inline PM4 table.
    pub pm4_word_offset: u32,
    /// Strictly typed source of the relocated address.
    pub source: RelocationSource,
    /// Byte offset relative to the resource record's visible range.
    pub resource_offset: u64,
    /// Minimum valid byte range beginning at `resource_offset`.
    pub required_size: u64,
    /// Access required by the PM4 operation.
    pub access: u32,
    /// Address representation expected by the PM4 field.
    pub encoding: AddressEncoding,
}

/// Authority from which the kernel resolves one relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocationSource {
    /// An attachment authorized by one resource table record.
    Attachment(u32),
    /// An immutable program in the kernel-owned canonical shader pack.
    CanonicalShader(ShaderVariant),
}

/// Borrowed data to encode as one canonical v1 payload.
#[derive(Clone, Copy, Debug)]
pub struct Submit<'a> {
    /// Inline, address-free PM4 dwords.
    pub pm4: &'a [u32],
    /// Context-local resources used by the PM4 stream.
    pub resources: &'a [Resource],
    /// Symbolic addresses patched by the kernel.
    pub relocations: &'a [Relocation],
}

#[derive(Clone, Copy)]
struct Layout {
    total: usize,
    pm4_offset: usize,
    resources_offset: usize,
    relocations_offset: usize,
}

const fn align_up(value: usize, alignment: usize) -> Result<usize, Error> {
    match value.checked_add(alignment - 1) {
        Some(value) => Ok(value & !(alignment - 1)),
        None => Err(Error::Overflow),
    }
}

fn layout(
    pm4_count: usize,
    resource_count: usize,
    relocation_count: usize,
) -> Result<Layout, Error> {
    if pm4_count == 0 || resource_count > MAX_RESOURCES || relocation_count > MAX_RELOCATIONS {
        return Err(Error::InvalidSize);
    }
    let pm4_size = pm4_count.checked_mul(4).ok_or(Error::Overflow)?;
    let resources_size = resource_count
        .checked_mul(RESOURCE_SIZE)
        .ok_or(Error::Overflow)?;
    let relocations_size = relocation_count
        .checked_mul(RELOCATION_SIZE)
        .ok_or(Error::Overflow)?;
    let pm4_offset = HEADER_SIZE;
    let resources_offset = align_up(pm4_offset.checked_add(pm4_size).ok_or(Error::Overflow)?, 8)?;
    let relocations_offset = resources_offset
        .checked_add(resources_size)
        .ok_or(Error::Overflow)?;
    let total = relocations_offset
        .checked_add(relocations_size)
        .ok_or(Error::Overflow)?;
    if total > MAX_SUBMIT_SIZE || total > u32::MAX as usize {
        return Err(Error::InvalidSize);
    }
    Ok(Layout {
        total,
        pm4_offset,
        resources_offset,
        relocations_offset,
    })
}

/// Return the exact number of bytes needed to encode `submit`.
pub fn encoded_len(submit: Submit<'_>) -> Result<usize, Error> {
    Ok(layout(
        submit.pm4.len(),
        submit.resources.len(),
        submit.relocations.len(),
    )?
    .total)
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

fn get_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

fn get_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ])
}

fn validate_access(access: u32) -> Result<(), Error> {
    if access == 0 || access & !ACCESS_MASK != 0 {
        Err(Error::InvalidField)
    } else {
        Ok(())
    }
}

fn validate_resource(resource: Resource) -> Result<(), Error> {
    validate_access(resource.access)?;
    if resource.attachment_token == 0
        || resource.range_size == 0
        || resource
            .range_offset
            .checked_add(resource.range_size)
            .is_none()
    {
        return Err(Error::InvalidField);
    }
    Ok(())
}

fn validate_relocation(
    relocation: Relocation,
    previous_end: Option<u32>,
    pm4: &[u32],
    resources: &[Resource],
) -> Result<(), Error> {
    validate_access(relocation.access)?;
    if previous_end.is_some_and(|previous| previous > relocation.pm4_word_offset) {
        return Err(Error::RelocationsNotSorted);
    }
    match relocation.source {
        RelocationSource::Attachment(resource_index) => {
            let resource = resources
                .get(resource_index as usize)
                .ok_or(Error::RelocationOutOfBounds)?;
            if relocation.access & !resource.access != 0 || relocation.required_size == 0 {
                return Err(Error::RelocationOutOfBounds);
            }
            let range_end = relocation
                .resource_offset
                .checked_add(relocation.required_size)
                .ok_or(Error::Overflow)?;
            if range_end > resource.range_size {
                return Err(Error::RelocationOutOfBounds);
            }
        }
        RelocationSource::CanonicalShader(_) => {
            if relocation.resource_offset != 0
                || relocation.required_size != SHADER_SIZE as u64
                || relocation.access != ACCESS_READ
                || relocation.encoding != AddressEncoding::GpuVa64
            {
                return Err(Error::RelocationOutOfBounds);
            }
        }
    }
    let word_end = relocation
        .pm4_word_offset
        .checked_add(relocation.encoding.word_count())
        .ok_or(Error::Overflow)?;
    if word_end as usize > pm4.len() {
        return Err(Error::RelocationOutOfBounds);
    }
    let start = relocation.pm4_word_offset as usize;
    if !relocation
        .encoding
        .placeholder_is_valid(&pm4[start..word_end as usize])
    {
        return Err(Error::NonZeroPlaceholder);
    }
    Ok(())
}

/// Encode one canonical v1 payload into caller-owned storage.
///
/// The destination length must exactly equal [`encoded_len`].
pub fn encode(submit: Submit<'_>, output: &mut [u8]) -> Result<(), Error> {
    let layout = layout(
        submit.pm4.len(),
        submit.resources.len(),
        submit.relocations.len(),
    )?;
    if output.len() != layout.total {
        return Err(Error::InvalidSize);
    }
    for resource in submit.resources {
        validate_resource(*resource)?;
    }
    let mut previous_end = None;
    for relocation in submit.relocations {
        validate_relocation(*relocation, previous_end, submit.pm4, submit.resources)?;
        previous_end = relocation
            .pm4_word_offset
            .checked_add(relocation.encoding.word_count());
    }

    output.fill(0);
    put_u32(output, 0, MAGIC);
    put_u16(output, 4, VERSION_MAJOR);
    put_u16(output, 6, VERSION_MINOR);
    put_u16(output, 8, HEADER_SIZE as u16);
    put_u16(output, 10, 0);
    put_u32(output, 12, layout.total as u32);
    put_u32(output, 16, layout.pm4_offset as u32);
    put_u32(output, 20, submit.pm4.len() as u32);
    put_u32(output, 24, layout.resources_offset as u32);
    put_u32(output, 28, submit.resources.len() as u32);
    put_u32(output, 32, layout.relocations_offset as u32);
    put_u32(output, 36, submit.relocations.len() as u32);

    for (index, word) in submit.pm4.iter().enumerate() {
        put_u32(output, layout.pm4_offset + index * 4, *word);
    }
    for (index, resource) in submit.resources.iter().enumerate() {
        let offset = layout.resources_offset + index * RESOURCE_SIZE;
        put_u64(output, offset, resource.attachment_token);
        put_u64(output, offset + 8, resource.range_offset);
        put_u64(output, offset + 16, resource.range_size);
        put_u32(output, offset + 24, resource.access);
    }
    for (index, relocation) in submit.relocations.iter().enumerate() {
        let offset = layout.relocations_offset + index * RELOCATION_SIZE;
        let (source_kind, source_index) = match relocation.source {
            RelocationSource::Attachment(index) => (0_u16, index),
            RelocationSource::CanonicalShader(variant) => (1, u32::from(variant.raw())),
        };
        put_u32(output, offset, relocation.pm4_word_offset);
        put_u32(output, offset + 4, source_index);
        put_u64(output, offset + 8, relocation.resource_offset);
        put_u64(output, offset + 16, relocation.required_size);
        put_u32(output, offset + 24, relocation.access);
        put_u16(output, offset + 28, relocation.encoding as u16);
        put_u16(output, offset + 30, source_kind);
    }
    Ok(())
}

/// Structurally validated, borrowed v1 payload.
#[derive(Clone, Copy)]
pub struct DecodedSubmit<'a> {
    bytes: &'a [u8],
    layout: Layout,
    pm4_count: usize,
    resource_count: usize,
    relocation_count: usize,
}

impl<'a> DecodedSubmit<'a> {
    /// Number of inline PM4 dwords.
    pub const fn pm4_len(&self) -> usize {
        self.pm4_count
    }

    /// Read one inline PM4 dword.
    pub fn pm4_word(&self, index: usize) -> Option<u32> {
        (index < self.pm4_count).then(|| get_u32(self.bytes, self.layout.pm4_offset + index * 4))
    }

    /// Number of resource records.
    pub const fn resource_len(&self) -> usize {
        self.resource_count
    }

    /// Decode one resource record.
    pub fn resource(&self, index: usize) -> Option<Resource> {
        if index >= self.resource_count {
            return None;
        }
        let offset = self.layout.resources_offset + index * RESOURCE_SIZE;
        Some(Resource {
            attachment_token: get_u64(self.bytes, offset),
            range_offset: get_u64(self.bytes, offset + 8),
            range_size: get_u64(self.bytes, offset + 16),
            access: get_u32(self.bytes, offset + 24),
        })
    }

    /// Number of relocation records.
    pub const fn relocation_len(&self) -> usize {
        self.relocation_count
    }

    /// Decode one relocation record.
    pub fn relocation(&self, index: usize) -> Option<Relocation> {
        if index >= self.relocation_count {
            return None;
        }
        let offset = self.layout.relocations_offset + index * RELOCATION_SIZE;
        let source = match get_u16(self.bytes, offset + 30) {
            0 => RelocationSource::Attachment(get_u32(self.bytes, offset + 4)),
            1 if get_u16(self.bytes, 6) >= 1 => RelocationSource::CanonicalShader(
                ShaderVariant::from_raw(u16::try_from(get_u32(self.bytes, offset + 4)).ok()?)?,
            ),
            _ => return None,
        };
        Some(Relocation {
            pm4_word_offset: get_u32(self.bytes, offset),
            source,
            resource_offset: get_u64(self.bytes, offset + 8),
            required_size: get_u64(self.bytes, offset + 16),
            access: get_u32(self.bytes, offset + 24),
            encoding: AddressEncoding::from_raw(get_u16(self.bytes, offset + 28)).ok()?,
        })
    }
}

/// Decode and structurally validate a complete canonical v1 payload.
pub fn decode(bytes: &[u8]) -> Result<DecodedSubmit<'_>, Error> {
    if bytes.len() < HEADER_SIZE || bytes.len() > MAX_SUBMIT_SIZE {
        return Err(Error::InvalidSize);
    }
    if get_u32(bytes, 0) != MAGIC || get_u16(bytes, 4) != VERSION_MAJOR {
        return Err(Error::UnsupportedVersion);
    }
    if get_u16(bytes, 6) > VERSION_MINOR || get_u16(bytes, 8) as usize != HEADER_SIZE {
        return Err(Error::UnsupportedVersion);
    }
    if get_u16(bytes, 10) != 0 || bytes[40..HEADER_SIZE].iter().any(|byte| *byte != 0) {
        return Err(Error::ReservedNotZero);
    }
    if get_u32(bytes, 12) as usize != bytes.len() {
        return Err(Error::InvalidSize);
    }
    let pm4_offset = get_u32(bytes, 16) as usize;
    let pm4_count = get_u32(bytes, 20) as usize;
    let resources_offset = get_u32(bytes, 24) as usize;
    let resource_count = get_u32(bytes, 28) as usize;
    let relocations_offset = get_u32(bytes, 32) as usize;
    let relocation_count = get_u32(bytes, 36) as usize;
    let expected = layout(pm4_count, resource_count, relocation_count)?;
    if pm4_offset != expected.pm4_offset
        || resources_offset != expected.resources_offset
        || relocations_offset != expected.relocations_offset
        || expected.total != bytes.len()
    {
        return Err(Error::InvalidTable);
    }
    let pm4_padding_start = expected
        .pm4_offset
        .checked_add(pm4_count.checked_mul(4).ok_or(Error::Overflow)?)
        .ok_or(Error::Overflow)?;
    if bytes[pm4_padding_start..expected.resources_offset]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(Error::ReservedNotZero);
    }
    let decoded = DecodedSubmit {
        bytes,
        layout: expected,
        pm4_count,
        resource_count,
        relocation_count,
    };
    for index in 0..resource_count {
        let offset = expected.resources_offset + index * RESOURCE_SIZE;
        if get_u32(bytes, offset + 28) != 0 {
            return Err(Error::ReservedNotZero);
        }
        validate_resource(decoded.resource(index).ok_or(Error::InvalidTable)?)?;
    }
    let mut previous_end = None;
    for index in 0..relocation_count {
        let relocation = decoded.relocation(index).ok_or(Error::InvalidField)?;
        validate_access(relocation.access)?;
        if previous_end.is_some_and(|value| value > relocation.pm4_word_offset) {
            return Err(Error::RelocationsNotSorted);
        }
        match relocation.source {
            RelocationSource::Attachment(resource_index) => {
                let resource = decoded
                    .resource(resource_index as usize)
                    .ok_or(Error::RelocationOutOfBounds)?;
                if relocation.access & !resource.access != 0 || relocation.required_size == 0 {
                    return Err(Error::RelocationOutOfBounds);
                }
                let range_end = relocation
                    .resource_offset
                    .checked_add(relocation.required_size)
                    .ok_or(Error::Overflow)?;
                if range_end > resource.range_size {
                    return Err(Error::RelocationOutOfBounds);
                }
            }
            RelocationSource::CanonicalShader(_) => {
                if relocation.resource_offset != 0
                    || relocation.required_size != SHADER_SIZE as u64
                    || relocation.access != ACCESS_READ
                    || relocation.encoding != AddressEncoding::GpuVa64
                {
                    return Err(Error::RelocationOutOfBounds);
                }
            }
        }
        let word_end = relocation
            .pm4_word_offset
            .checked_add(relocation.encoding.word_count())
            .ok_or(Error::Overflow)?;
        if word_end as usize > pm4_count {
            return Err(Error::RelocationOutOfBounds);
        }
        let mut placeholder = [0_u32; 2];
        for (destination, word) in placeholder
            .iter_mut()
            .zip(relocation.pm4_word_offset..word_end)
        {
            *destination = decoded
                .pm4_word(word as usize)
                .ok_or(Error::RelocationOutOfBounds)?;
        }
        if !relocation
            .encoding
            .placeholder_is_valid(&placeholder[..relocation.encoding.word_count() as usize])
        {
            return Err(Error::NonZeroPlaceholder);
        }
        previous_end = Some(word_end);
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use adreno_a6xx_shader_pack::ShaderVariant;
    use std::vec;

    use super::{
        ACCESS_READ, ACCESS_WRITE, AddressEncoding, Error, Relocation, RelocationSource, Resource,
        Submit, decode, encode, encoded_len,
    };

    fn sample<'a>(
        pm4: &'a [u32],
        resources: &'a [Resource],
        relocs: &'a [Relocation],
    ) -> Submit<'a> {
        Submit {
            pm4,
            resources,
            relocations: relocs,
        }
    }

    #[test]
    fn canonical_payload_round_trips() {
        let pm4 = [0x7000_8026, 0, 0];
        let resources = [Resource {
            attachment_token: 9,
            range_offset: 4096,
            range_size: 8192,
            access: ACCESS_READ | ACCESS_WRITE,
        }];
        let relocs = [Relocation {
            pm4_word_offset: 1,
            source: RelocationSource::Attachment(0),
            resource_offset: 128,
            required_size: 256,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        }];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.pm4_len(), 3);
        assert_eq!(decoded.resource(0), Some(resources[0]));
        assert_eq!(decoded.relocation(0), Some(relocs[0]));
    }

    #[test]
    fn v1_0_attachment_is_compatible_but_reserved_source_kind_is_rejected() {
        let pm4 = [0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 8,
            access: ACCESS_READ,
        }];
        let relocs = [Relocation {
            pm4_word_offset: 0,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 8,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        }];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        bytes[6..8].copy_from_slice(&0_u16.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap().relocation(0), Some(relocs[0]));
        let relocation_offset = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        bytes[relocation_offset + 30..relocation_offset + 32].copy_from_slice(&1_u16.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(Error::InvalidField)));
    }

    #[test]
    fn v1_1_canonical_shader_roundtrip_and_unknown_id_mutation() {
        let pm4 = [0, 0];
        let reloc = Relocation {
            pm4_word_offset: 0,
            source: RelocationSource::CanonicalShader(ShaderVariant::FsSolid),
            resource_offset: 0,
            required_size: adreno_a6xx_shader_pack::SHADER_SIZE as u64,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        };
        let submit = sample(&pm4, &[], core::slice::from_ref(&reloc));
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert_eq!(decode(&bytes).unwrap().relocation(0), Some(reloc));
        let relocation_offset = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        bytes[relocation_offset + 4..relocation_offset + 8]
            .copy_from_slice(&0xffff_u32.to_le_bytes());
        assert!(matches!(decode(&bytes), Err(Error::InvalidField)));
    }

    #[test]
    fn rejects_nonzero_address_placeholder() {
        let pm4 = [0x7000_8026, 1, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [Relocation {
            pm4_word_offset: 1,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 4,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        }];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::NonZeroPlaceholder));
    }

    #[test]
    fn texture_descriptor_relocation_preserves_non_address_high_bits() {
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [Relocation {
            pm4_word_offset: 1,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 64,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa49TexDescriptor,
        }];
        let pm4 = [0, 0, 1 << 17];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert_eq!(decode(&bytes).unwrap().pm4_word(2), Some(1 << 17));

        let invalid = [0, 1 << 5, 1 << 17];
        let submit = sample(&invalid, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::NonZeroPlaceholder));
    }

    #[test]
    fn rejects_unsorted_relocations() {
        let pm4 = [0, 0, 0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 4,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
            Relocation {
                pm4_word_offset: 1,
                source: RelocationSource::Attachment(0),
                resource_offset: 4,
                required_size: 4,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
        ];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::RelocationsNotSorted));
    }

    #[test]
    fn rejects_overlapping_relocations() {
        let pm4 = [0, 0, 0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [
            Relocation {
                pm4_word_offset: 1,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 8,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 8,
                required_size: 4,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
        ];
        let submit = sample(&pm4, &resources, &relocs);
        let mut bytes = vec![0; encoded_len(submit).unwrap()];
        assert_eq!(encode(submit, &mut bytes), Err(Error::RelocationsNotSorted));
    }

    #[test]
    fn decoder_is_total_for_single_byte_corruption() {
        let pm4 = [0x7000_8026, 0, 0];
        let resources = [Resource {
            attachment_token: 3,
            range_offset: 0,
            range_size: 4096,
            access: ACCESS_READ,
        }];
        let relocs = [Relocation {
            pm4_word_offset: 1,
            source: RelocationSource::Attachment(0),
            resource_offset: 0,
            required_size: 16,
            access: ACCESS_READ,
            encoding: AddressEncoding::GpuVa64,
        }];
        let submit = sample(&pm4, &resources, &relocs);
        let mut canonical = vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut canonical).unwrap();

        for index in 0..canonical.len() {
            for replacement in 0..=u8::MAX {
                let mut corrupted = canonical.clone();
                corrupted[index] = replacement;
                let _ = decode(&corrupted);
            }
        }
    }
}
