// SPDX-License-Identifier: GPL-2.0-only
//! Native ISA authority tests. These exercise the production wire decoder and
//! validator; execution/isolation hardware still needs CoachZ validation.
extern crate std;
use crate::submit::{ResolvedResource, validate_and_relocate};
use adreno_a6xx_pm4::{opcode, type4, type7};
use adreno_a6xx_submit_wire::{
    self as wire, ACCESS_READ, ACCESS_WRITE, AddressEncoding as Encoding, Relocation,
    RelocationSource as Source, Resource,
};
use std::{vec, vec::Vec};

fn reg(words: &mut Vec<u32>, register: u32, payload: &[u32]) {
    words.push(type4(register, payload.len() as u16).unwrap());
    words.extend_from_slice(payload);
}
fn packet(words: &mut Vec<u32>, opcode: u8, payload: &[u32]) {
    words.push(type7(opcode, payload.len() as u16).unwrap());
    words.extend_from_slice(payload);
}
fn relocation(word: u32, source: Source, size: u64, access: u32) -> Relocation {
    Relocation {
        pm4_word_offset: word,
        source,
        resource_offset: 0,
        required_size: size,
        access,
        encoding: Encoding::GpuVa64,
    }
}
fn compute() -> (Vec<u32>, Vec<Resource>, Vec<Relocation>) {
    let mut words = Vec::new();
    reg(&mut words, 0xa9b0, &[0x8040_0080]);
    reg(&mut words, 0xa9b1, &[0]);
    reg(&mut words, 0xa9bb, &[0x100]);
    reg(&mut words, 0xa9bc, &[1]);
    reg(&mut words, 0xb987, &[0x101]);
    reg(&mut words, 0xb990, &[1, 1, 0, 1, 0, 1, 0]);
    reg(&mut words, 0xb997, &[u32::MAX, 0x2fc]);
    reg(&mut words, 0xb980, &[0]);
    let base = words.len() as u32 + 2;
    reg(&mut words, 0xa9b3, &[0; 7]);
    // Match production emit: even zero-scratch programs explicitly reset the
    // stack offset instead of inheriting a preceding scratch-using program.
    reg(&mut words, 0xa9bd, &[0]);
    let preload = words.len() as u32 + 2;
    packet(
        &mut words,
        0x34,
        &[(2 << 16) | (13 << 18) | (1 << 22), 0, 0],
    );
    let pointer = words.len() as u32 + 4;
    packet(
        &mut words,
        0x34,
        &[(1 << 14) | (13 << 18) | (1 << 22), 0, 0, 0, 0, 0xabcdef, 0],
    );
    packet(&mut words, 0x33, &[0, 1, 1, 1]);
    (
        words,
        vec![
            Resource {
                attachment_token: 1,
                range_offset: 0,
                range_size: 128,
                access: ACCESS_READ,
            },
            Resource {
                attachment_token: 2,
                range_offset: 4096,
                range_size: 4096,
                access: ACCESS_READ | ACCESS_WRITE,
            },
        ],
        vec![
            relocation(base, Source::ShaderAttachment(0), 128, ACCESS_READ),
            relocation(preload, Source::ShaderAttachment(0), 128, ACCESS_READ),
            relocation(pointer, Source::Attachment(1), 4096, ACCESS_WRITE),
        ],
    )
}
fn encode(words: &[u32], resources: &[Resource], relocs: &[Relocation]) -> Vec<u8> {
    let submit = wire::Submit {
        pm4: words,
        resources,
        relocations: relocs,
    };
    let mut bytes = vec![0; wire::encoded_len(submit).unwrap()];
    wire::encode(submit, &mut bytes).unwrap();
    bytes
}
fn validate(bytes: &[u8]) -> Result<Vec<u32>, &'static str> {
    validate_and_relocate(
        bytes,
        |token| {
            Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000 + token * 0x10000,
                allocation_size: 8192,
                allowed_access: ACCESS_READ | ACCESS_WRITE,
                linear_image: None,
            })
        },
        |_| None,
    )
}
#[test]
fn native_compute_and_buffer_constant_pointer_relocate() {
    let (words, resources, relocs) = compute();
    let bytes = encode(&words, &resources, &relocs);
    assert!(wire::decode(&bytes).unwrap().requires_isolation());
    let result = validate(&bytes).unwrap();
    for (r, address) in relocs
        .iter()
        .zip([0x1_0001_0000u64, 0x1_0001_0000, 0x1_0002_1000])
    {
        assert_eq!(
            &result[r.pm4_word_offset as usize..r.pm4_word_offset as usize + 2],
            &[address as u32, (address >> 32) as u32]
        );
    }
    let stack = words
        .iter()
        .position(|&w| w == type4(0xa9bd, 1).unwrap())
        .unwrap();
    let mut bad = words.clone();
    bad[stack] = type4(0xa9be, 1).unwrap(); // omit the stack reset, keep relocation offsets
    assert_eq!(
        validate(&encode(&bad, &resources, &relocs)),
        Err("a618: private stack offset does not match active program")
    );
    bad[stack] = words[stack];
    bad[stack + 1] = 512; // legal scalar encoding, inconsistent with zero scratch
    assert_eq!(
        validate(&encode(&bad, &resources, &relocs)),
        Err("a618: private stack offset does not match active program")
    );
}
#[test]
fn attachment_cannot_become_executable_without_native_marker() {
    let (words, resources, mut relocs) = compute();
    relocs[0].source = Source::Attachment(0);
    relocs[1].source = Source::Attachment(0);
    let bytes = encode(&words, &resources, &relocs);
    assert!(!wire::decode(&bytes).unwrap().requires_isolation());
    assert!(validate(&bytes).is_err());
}
#[test]
fn shader_source_cannot_be_used_as_data_pointer() {
    let (words, mut resources, mut relocs) = compute();
    resources[1].access = ACCESS_READ;
    relocs[2].source = Source::ShaderAttachment(1);
    relocs[2].access = ACCESS_READ;
    assert!(validate(&encode(&words, &resources, &relocs)).is_err());
}
#[test]
fn native_marker_does_not_allow_privileged_packets_or_registers() {
    for (is_register, selector, payload) in [
        (true, 0x883u32, vec![0]),
        (true, 0x8c0, vec![1]),
        (false, 0x3f, vec![0, 0, 4]),
        (false, 0x46, vec![0x80000004, 0, 0, 1]),
    ] {
        let (mut words, resources, relocs) = compute();
        if is_register {
            reg(&mut words, selector, &payload);
        } else {
            packet(&mut words, selector as u8, &payload);
        }
        assert!(validate(&encode(&words, &resources, &relocs)).is_err());
    }
}
#[test]
fn compute_cannot_inherit_previous_submit_or_mismatch_preload() {
    let (mut words, resources, mut relocs) = compute();
    relocs[1].resource_offset = 128;
    let mut extended = resources.clone();
    extended[0].range_size = 256;
    assert!(validate(&encode(&words, &extended, &relocs)).is_err());
    relocs[1].resource_offset = 0;
    // Remove CS configuration without changing relocation positions.
    words[4] = type4(0xa9be, 1).unwrap();
    words[5] = 0;
    assert!(validate(&encode(&words, &resources, &relocs)).is_err());
}
#[test]
fn wire_native_is_versioned_read_only_and_instruction_aligned() {
    let (words, resources, relocs) = compute();
    let bytes = encode(&words, &resources, &relocs);
    for minor in [0u16, 1] {
        let mut old = bytes.clone();
        old[6..8].copy_from_slice(&minor.to_le_bytes());
        assert!(wire::decode(&old).is_err());
    }
    for (offset, size, access) in [
        (4, 128, ACCESS_READ),
        (0, 8, ACCESS_READ),
        (0, 128, ACCESS_WRITE),
    ] {
        let mut relocs = relocs.clone();
        relocs[0].resource_offset = offset;
        relocs[0].required_size = size;
        relocs[0].access = access;
        let submit = wire::Submit {
            pm4: &words,
            resources: &resources,
            relocations: &relocs,
        };
        assert!(wire::encode(submit, &mut vec![0; wire::encoded_len(submit).unwrap()]).is_err());
    }
}
#[test]
fn native_ubo_preserves_size_and_bounds_rodata() {
    let (mut words, resources, mut relocs) = compute();
    let first = words.len() as u32;
    packet(
        &mut words,
        0x34,
        &[(2 << 14) | (13 << 18) | (1 << 22), 0, 0, 0, 8 << 17],
    );
    let mut r = relocation(first + 4, Source::Attachment(0), 128, ACCESS_READ);
    r.encoding = Encoding::GpuVa49UboDescriptor;
    relocs.push(r);
    let bytes = encode(&words, &resources, &relocs);
    let result = validate(&bytes).unwrap();
    assert_eq!(result[first as usize + 5], (8 << 17) | 1);
    let mut old = bytes.clone();
    old[6..8].copy_from_slice(&1u16.to_le_bytes());
    assert!(wire::decode(&old).is_err());
    relocs.last_mut().unwrap().required_size = 64;
    assert!(validate(&encode(&words, &resources, &relocs)).is_err());
}

#[test]
fn native_graphics_accepts_dynamic_linkage_but_bounds_every_shader_fetch() {
    let mut words = Vec::new();
    let mut relocs = Vec::new();
    for (source, ctrl, config, size_reg, program, load, block, units) in [
        (0, 0xa800, 0xa823, 0xa824, 0xa81b, 0x32, 8, 2),
        (1, 0xa980, 0xab04, 0xab05, 0xa982, 0x34, 12, 3),
    ] {
        reg(&mut words, ctrl, &[0x1a80]);
        reg(&mut words, config, &[0x100]);
        reg(&mut words, size_reg, &[units]);
        let base = words.len() as u32 + 2;
        reg(&mut words, program, &[0; 7]);
        reg(&mut words, if source == 0 { 0xa825 } else { 0xa9a9 }, &[0]);
        relocs.push(relocation(
            base,
            Source::ShaderAttachment(source),
            512,
            ACCESS_READ,
        ));
        let preload = words.len() as u32 + 2;
        packet(
            &mut words,
            load,
            &[(2 << 16) | (block << 18) | (units << 22), 0, 0],
        );
        relocs.push(relocation(
            preload,
            Source::ShaderAttachment(source),
            512,
            ACCESS_READ,
        ));
    }
    // Dynamic linkage exceeds the canonical two-register burst.
    reg(&mut words, 0xa803, &[0x100, 0x201, 0x302, 0x403]);
    reg(&mut words, 0xa813, &[0x03020100, 0x07060504, 0x0b0a0908]);
    reg(
        &mut words,
        0xa001,
        &[0xfcfcfcfc, 0xfcfc, 0xfcfcfcfc, 252, 0xfcfc, 0],
    );
    reg(&mut words, 0x9200, &[0x55555555; 8]);
    // Instanced point draw with sysval-only vertex shader (no VBO).
    reg(&mut words, 0x880c, &[0]);
    reg(&mut words, 0xa98d, &[0]);
    let draw = words.len();
    packet(&mut words, 0x38, &[0x89, 4, 1]);
    for event in [0x1d, 0x1c] {
        let address = words.len() as u32 + 2;
        packet(
            &mut words,
            opcode::EVENT_WRITE,
            &[event | (1 << 30), 0, 0, 1],
        );
        relocs.push(relocation(address, Source::Attachment(2), 4, ACCESS_WRITE));
    }
    packet(&mut words, opcode::EVENT_WRITE, &[0x19]);
    packet(&mut words, opcode::EVENT_WRITE, &[0x18]);
    packet(&mut words, opcode::WAIT_FOR_IDLE, &[]);
    let resources = [
        Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 512,
            access: ACCESS_READ,
        },
        Resource {
            attachment_token: 2,
            range_offset: 0,
            range_size: 512,
            access: ACCESS_READ,
        },
        Resource {
            attachment_token: 3,
            range_offset: 0,
            range_size: 4,
            access: ACCESS_WRITE,
        },
    ];
    assert!(
        validate(&encode(&words, &resources, &relocs)).is_ok(),
        "{:?}",
        validate(&encode(&words, &resources, &relocs))
    );
    let mut bad = relocs.clone();
    bad[0].required_size = 128;
    assert!(validate(&encode(&words, &resources, &bad)).is_err());
    let mut bad = relocs.clone();
    bad[0].source = Source::Attachment(0);
    assert!(validate(&encode(&words, &resources, &bad)).is_err());
    let mut bad_words = words.clone();
    bad_words[draw + 1] = 0x80; // DI_PT_NONE is not a point draw.
    assert!(validate(&encode(&bad_words, &resources, &relocs)).is_err());
    for register in [0xa825, 0xa9a9] {
        let stack = words
            .iter()
            .position(|&w| w == type4(register, 1).unwrap())
            .unwrap();
        let mut bad = words.clone();
        bad[stack] = type4(0xa9be, 1).unwrap();
        assert_eq!(
            validate(&encode(&bad, &resources, &relocs)),
            Err("a618: private stack offset does not match active program")
        );
        bad[stack] = words[stack];
        bad[stack + 1] = 512;
        assert_eq!(
            validate(&encode(&bad, &resources, &relocs)),
            Err("a618: private stack offset does not match active program")
        );
    }
}

#[test]
fn native_ssbo_tables_require_destination_and_embedded_pointer_authority() {
    let (mut words, mut resources, mut relocs) = compute();
    resources.push(Resource {
        attachment_token: 3,
        range_offset: 0,
        range_size: 64,
        access: ACCESS_READ | ACCESS_WRITE,
    });
    let start = words.len() as u32;
    let mut table = vec![0; 18]; // destination addr64 and sixteen descriptor words
    table[2] = 0x4b << 22; // buffer descriptor data; never interpreted as PM4
    packet(&mut words, 0x3d, &table);
    packet(&mut words, opcode::WAIT_MEM_WRITES, &[]);
    relocs.push(relocation(
        start + 1,
        Source::Attachment(2),
        64,
        ACCESS_WRITE,
    ));
    let mut pointer = relocation(
        start + 7,
        Source::Attachment(1),
        4096,
        ACCESS_READ | ACCESS_WRITE,
    );
    pointer.encoding = Encoding::GpuVa49TexDescriptor;
    relocs.push(pointer);
    let base = words.len() as u32;
    reg(&mut words, 0xa9f2, &[0, 0]);
    relocs.push(relocation(base + 1, Source::Attachment(2), 64, ACCESS_READ));
    let load = words.len() as u32;
    packet(
        &mut words,
        0x34,
        &[(3 << 14) | (2 << 16) | (13 << 18) | (1 << 22), 0, 0],
    );
    relocs.push(relocation(load + 2, Source::Attachment(2), 64, ACCESS_READ));
    assert!(
        validate(&encode(&words, &resources, &relocs)).is_ok(),
        "{:?}",
        validate(&encode(&words, &resources, &relocs))
    );
    let mut bad = relocs.clone();
    bad[3].access = ACCESS_READ;
    assert!(validate(&encode(&words, &resources, &bad)).is_err());
    let mut bad = relocs.clone();
    bad[3].required_size = 32;
    assert!(validate(&encode(&words, &resources, &bad)).is_err());
}

#[test]
fn native_descriptor_write_batch_requires_wait_before_any_consumer() {
    let (mut words, resources, mut relocs) = compute();
    for value in [1, 2] {
        let start = words.len() as u32;
        packet(&mut words, 0x3d, &[0, 0, value]);
        relocs.push(relocation(
            start + 1,
            Source::Attachment(1),
            4,
            ACCESS_WRITE,
        ));
    }
    assert!(validate(&encode(&words, &resources, &relocs)).is_err());
    packet(&mut words, opcode::WAIT_MEM_WRITES, &[]);
    assert!(validate(&encode(&words, &resources, &relocs)).is_ok());
    let mut bad = words.clone();
    *bad.last_mut().unwrap() = type7(opcode::WAIT_FOR_IDLE, 0).unwrap();
    assert!(validate(&encode(&bad, &resources, &relocs)).is_err());
}

#[test]
fn native_uche_clean_cannot_request_kernel_completion_irq() {
    let (mut words, resources, mut relocs) = compute();
    let start = words.len() as u32;
    packet(&mut words, opcode::EVENT_WRITE, &[0x40000004, 0, 0, 1]);
    relocs.push(relocation(
        start + 2,
        Source::Attachment(1),
        4,
        ACCESS_WRITE,
    ));
    assert!(validate(&encode(&words, &resources, &relocs)).is_ok());
    words[start as usize + 1] |= 1 << 31;
    assert!(validate(&encode(&words, &resources, &relocs)).is_err());
}
