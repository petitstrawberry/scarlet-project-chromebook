// SPDX-License-Identifier: GPL-2.0-only
//! Exact A618 private-memory layout admission. Child module of submit.rs.
//! No arbitrary register intervals or unrelocated scratch addresses are admitted.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PrivateLayout {
    pub allocation_bytes: u64,
    pub stack_word: u32,
}

pub(super) fn decode_layout(
    param: u32,
    size: u32,
    compute: bool,
) -> Result<Option<PrivateLayout>, &'static str> {
    if param == 0 && size == 0 {
        return Ok(None);
    }
    // No hardware call stack; powers of two in 512-byte units match Mesa's
    // backing allocation policy. Bit31 is only emitted for compute shaders.
    if param == 0
        || param > 128
        || !param.is_power_of_two()
        || size & !0x8003ffff != 0
        || (!compute && size & 0x80000000 != 0)
    {
        return Err("a618: invalid private-memory layout");
    }
    let per_fiber = param * 512;
    let per_sp = per_fiber * 2048;
    if size & 0x3ffff != per_sp >> 12 {
        return Err("a618: private-memory allocation stride mismatch");
    }
    Ok(Some(PrivateLayout {
        allocation_bytes: u64::from(per_sp),
        stack_word: per_sp >> 11,
    }))
}

fn registers(register: u32) -> Option<(ShaderStage, u32)> {
    match register {
        0xa81b => Some((ShaderStage::Vertex, 0xa825)),
        0xa982 => Some((ShaderStage::Fragment, 0xa9a9)),
        0xa9b3 => Some((ShaderStage::Compute, 0xa9bd)),
        _ => None,
    }
}

/// Retained-program draws may omit the program burst. A standalone stack
/// update must still agree with the retained layout before the next draw.
pub(super) fn validate_sequences(packets: &[Packet<'_>]) -> Result<(), &'static str> {
    let mut layouts: [Option<Option<PrivateLayout>>; 3] = [None; 3];
    let mut stacks = [None; 3];
    for packet in packets {
        match packet.header {
            Header::Type4 { register, .. } => {
                if let Some((stage, _)) = registers(register) {
                    let i = match stage {
                        ShaderStage::Vertex => 0,
                        ShaderStage::Fragment => 1,
                        ShaderStage::Compute => 2,
                    };
                    let p = packet.payload;
                    if p.len() != 7 {
                        return Err("a618: incomplete private program burst");
                    }
                    layouts[i] = Some(decode_layout(p[3], p[6], i == 2)?);
                } else if let Some(i) = [0xa825, 0xa9a9, 0xa9bd].iter().position(|r| *r == register)
                {
                    if packet.payload.len() != 1 {
                        return Err("a618: invalid private stack burst");
                    }
                    stacks[i] = Some(packet.payload[0]);
                }
            }
            Header::Type7 { opcode, .. }
                if opcode == CP_DRAW_INDX_OFFSET || opcode == CP_EXEC_CS =>
            {
                let stages: &[usize] = if opcode == CP_EXEC_CS { &[2] } else { &[0, 1] };
                for &i in stages {
                    if let Some(layout) = layouts[i] {
                        if stacks[i] != Some(layout.map_or(0, |l| l.stack_word)) {
                            return Err("a618: private stack offset does not match active program");
                        }
                    }
                }
            }
            _ => (),
        }
    }
    Ok(())
}

/// Call before the native/fixed type4 allowlists, only for native submissions.
pub(super) fn validate_type4(
    register: u32,
    payload: &[u32],
    word: u32,
    addresses: &mut Vec<AddressField>,
) -> Option<Result<(), &'static str>> {
    if matches!(register, 0xa825 | 0xa9a9 | 0xa9bd) {
        return Some(
            if payload.len() == 1
                && (payload[0] == 0
                    || (512..=65536).contains(&payload[0]) && payload[0].is_power_of_two())
            {
                Ok(())
            } else {
                Err("a618: invalid private-memory stack offset")
            },
        );
    }
    let (stage, _) = registers(register)?;
    Some((|| {
        if payload.len() != 7 || payload[0] != 0 || payload[4..6] != [0; 2] {
            return Err("a618: malformed native private-memory program burst");
        }
        let layout = decode_layout(payload[3], payload[6], stage == ShaderStage::Compute)?;
        canonical_address_field(addresses, word + 2, stage)?;
        if let Some(layout) = layout {
            address_field(
                addresses,
                word + 5,
                ACCESS_READ | ACCESS_WRITE,
                false,
                Some(layout.allocation_bytes),
            )?;
            addresses.last_mut().unwrap().private_memory = true;
        }
        Ok(())
    })())
}

/// Native sequence validators must use this instead of the canonical-only
/// segment_shader_program_layout, which correctly still requires zero scratch.
pub(super) fn program_layout(
    segment: &SegmentIndex<'_, '_, '_>,
    register: u32,
) -> Option<(u32, u32)> {
    let (stage, stack_reg) = registers(register)?;
    let (word, payload) = segment.reg(register)?;
    if payload.len() != 7 || payload[0] != 0 || payload[4..6] != [0; 2] {
        return None;
    }
    let layout = decode_layout(payload[3], payload[6], stage == ShaderStage::Compute).ok()?;
    let (_, stack) = segment.reg(stack_reg)?;
    if stack != [layout.map_or(0, |l| l.stack_word)] {
        return None;
    }
    Some((word, word + 2))
}

/// Called for the private address during resolved relocation validation. Check
/// final VA, not merely relative resource_offset: forged wire ranges can shift
/// an otherwise page-aligned allocation. Existing general validation checks
/// authority, allocation bounds, exact required_size and address placeholder.
pub(super) fn validate_resolved(
    relocation: &Relocation,
    resource: &ResolvedResource,
    attached_image: bool,
) -> Result<(), &'static str> {
    if !matches!(relocation.source, RelocationSource::Attachment(_))
        || relocation.access != (ACCESS_READ | ACCESS_WRITE)
        || relocation.encoding != AddressEncoding::GpuVa64
        || attached_image
        || resource
            .gpu_va
            .checked_add(relocation.resource_offset)
            .is_none_or(|va| va & 4095 != 0)
    {
        return Err("a618: private memory requires aligned read-write buffer attachment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_layout_rejects_mismatched_sizes_and_reserved_bits() {
        assert_eq!(
            decode_layout(2, 512, false).unwrap(),
            Some(PrivateLayout {
                allocation_bytes: 2097152,
                stack_word: 1024
            })
        );
        assert_eq!(
            decode_layout(2, 0x80000200, true).unwrap(),
            decode_layout(2, 512, false).unwrap()
        );
        assert_eq!(decode_layout(0, 0, false).unwrap(), None);
        for (param, size, compute) in [
            (0, 512, false),
            (2, 0, false),
            (3, 768, false),
            (256, 65536, false),
            (2, 513, false),
            (2, 0x40000200, false),
            (2, 0x80000200, false),
            (0x1000002, 512, true),
        ] {
            assert!(decode_layout(param, size, compute).is_err());
        }
        assert_eq!(
            decode_layout(128, 32768, false)
                .unwrap()
                .unwrap()
                .allocation_bytes,
            134217728
        );
    }
}
