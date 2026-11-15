// SPDX-License-Identifier: GPL-2.0-only
//! Real Mesa IR3 metadata -> production emitter -> wire -> production validator.
extern crate std;
use crate::submit::{
    ImageModifier, LinearImage, LinearImageFormat, ResolvedResource, validate_and_relocate,
};
use adreno_a6xx_submit_wire::{self as wire, AddressEncoding, RelocationSource, Resource};
use sgfx_codegen_adreno_a6xx::{ObjectRef, RelocatablePm4};
use std::vec::Vec;

#[path = "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native_support/mod.rs"]
mod fixtures;

#[path = "vertex_authority.rs"]
mod vertex_authority;

fn token(object: ObjectRef) -> u64 {
    match object {
        ObjectRef::External(id) | ObjectRef::NativeShader(id) => u64::from(id.raw()) + 1,
        ObjectRef::Generated(id) => 0x1000 + u64::from(id.raw()),
        ObjectRef::CanonicalShader(_) => panic!("canonical code has kernel authority"),
    }
}

fn case_images(case: &fixtures::Case) -> Vec<(u64, LinearImage)> {
    use sgfx_codegen_adreno_a6xx::{ImageModifier as Modifier, ResourceKind};
    use sgfx_core::ir::TextureFormat;
    case.resources
        .iter()
        .filter_map(|r| {
            let ResourceKind::Image(meta) = &r.kind else {
                return None;
            };
            Some((
                token(ObjectRef::External(r.id)),
                LinearImage {
                    texture: meta.texture,
                    format: match meta.storage_format {
                        TextureFormat::Rgba16Float => LinearImageFormat::Rgba16Float,
                        TextureFormat::Rgba32Float => LinearImageFormat::Rgba32Float,
                        TextureFormat::Depth32Float => LinearImageFormat::Depth32Float,
                        _ => LinearImageFormat::Bgra8Unorm,
                    },
                    modifier: match meta.modifier {
                        Modifier::Linear => ImageModifier::Linear,
                        Modifier::A6xxTile6_3Depth => ImageModifier::Tile6_3Depth,
                        Modifier::A6xxTile6_3Color => ImageModifier::Tile6_3Color,
                    },
                    width: meta.extent.width(),
                    height: meta.extent.height(),
                    row_pitch: meta.planes[0].stride,
                    visible_size: meta.planes[0].size,
                },
            ))
        })
        .collect()
}

fn mutate_register(artifact: &mut RelocatablePm4, register: u32, index: usize, value: u32) {
    use adreno_a6xx_pm4::{Header, Packets};
    let word = Packets::new(&artifact.words)
        .filter_map(|packet| {
            let packet = packet.unwrap();
            matches!(packet.header, Header::Type4 { register: r, .. } if r == register)
                .then_some(packet.word_offset as usize + 1 + index)
        })
        .last()
        .unwrap();
    artifact.words[word] = value;
}
fn validate_result(artifact: &RelocatablePm4) -> Result<Vec<u32>, &'static str> {
    validate_with_images(
        artifact,
        &[(
            token(ObjectRef::External(fixtures::TARGET)),
            LinearImage {
                texture: None,
                format: LinearImageFormat::Bgra8Unorm,
                modifier: ImageModifier::Linear,
                width: 16,
                height: 16,
                row_pitch: 64,
                visible_size: 1024,
            },
        )],
    )
}

fn validate_with_images(
    artifact: &RelocatablePm4,
    images: &[(u64, LinearImage)],
) -> Result<Vec<u32>, &'static str> {
    validate_with_permissions(artifact,images, |_|wire::ACCESS_READ|wire::ACCESS_WRITE)
}
fn validate_with_permissions(
    artifact:&RelocatablePm4,
    images:&[(u64,LinearImage)],
    allowed:impl Fn(u64)->u32,
)->Result<Vec<u32>,&'static str> {
    let resources: Vec<_> = artifact
        .accesses
        .iter()
        .map(|a| Resource {
            attachment_token: token(a.object),
            range_offset: a.offset,
            range_size: a.size,
            access: u32::from(a.access.bits()),
        })
        .collect();
    let relocations: Vec<_> = artifact
        .fixups
        .iter()
        .map(|fixup| {
            let source = if let ObjectRef::CanonicalShader(variant) = fixup.object {
                RelocationSource::CanonicalShader(variant)
            } else {
                let index = resources
                    .iter()
                    .position(|r| {
                        r.attachment_token == token(fixup.object)
                            && r.access & u32::from(fixup.access.bits())
                                == u32::from(fixup.access.bits())
                            && fixup.object_offset >= r.range_offset
                            && fixup.object_offset + fixup.required_size
                                <= r.range_offset + r.range_size
                    })
                    .unwrap();
                if matches!(fixup.object, ObjectRef::NativeShader(_)) {
                    RelocationSource::ShaderAttachment(index as u32)
                } else {
                    RelocationSource::Attachment(index as u32)
                }
            };
            let offset = match source {
                RelocationSource::Attachment(i) | RelocationSource::ShaderAttachment(i) => {
                    fixup.object_offset - resources[i as usize].range_offset
                }
                RelocationSource::CanonicalShader(_) => 0,
            };
            wire::Relocation {
                pm4_word_offset: fixup.word_offset,
                source,
                resource_offset: offset,
                required_size: fixup.required_size,
                access: u32::from(fixup.access.bits()),
                encoding: match fixup.encoding {
                    sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa64 => AddressEncoding::GpuVa64,
                    sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49TexDescriptor => {
                        AddressEncoding::GpuVa49TexDescriptor
                    }
                    sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49UboDescriptor => {
                        AddressEncoding::GpuVa49UboDescriptor
                    }
                },
            }
        })
        .collect();
    let submit = wire::Submit {
        pm4: &artifact.words,
        resources: &resources,
        relocations: &relocations,
    };
    let mut bytes = std::vec![0; wire::encoded_len(submit).unwrap()];
    wire::encode(submit, &mut bytes).unwrap();
    assert_eq!(
        wire::decode(&bytes).unwrap().requires_isolation(),
        artifact
            .fixups
            .iter()
            .any(|f| matches!(f.object, ObjectRef::NativeShader(_)))
    );
    let result = validate_and_relocate(
        &bytes,
        |token| {
            let size = resources
                .iter()
                .filter(|r| r.attachment_token == token)
                .map(|r| r.range_offset + r.range_size)
                .max()?;
            Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000 + token * 0x100000,
                allocation_size: size,
                allowed_access: allowed(token),
                linear_image: images
                    .iter()
                    .find(|(id, _)| *id == token)
                    .map(|(_, image)| *image),
            })
        },
        |variant| {
            Some(ResolvedResource {
                attachment_token: 0,
                gpu_va: 0x2_0000_0000 + variant.offset() as u64,
                allocation_size: variant.size() as u64,
                allowed_access: wire::ACCESS_READ,
                linear_image: None,
            })
        },
    );
    if let Err(error) = &result {
        std::eprintln!(
            "validator error: {error}; packet: {:?}",
            crate::submit::diagnose_rejected_packet(&bytes)
        );
    }
    result
}

fn validate(artifact: &RelocatablePm4) {
    assert!(validate_result(artifact).is_ok());
}

#[test]
fn graphics_uav_preload_authorizes_only_its_bounded_shared_table() {
    use adreno_a6xx_pm4::{Header, Packets, type7};
    let case = fixtures::native_large_readonly_graphics_case();
    let artifact = case.compile().unwrap();
    validate_with_images(&artifact, &case_images(&case)).unwrap();
    let load = Packets::new(&artifact.words)
        .map(|p| p.unwrap())
        .find(|p| matches!(p.header, Header::Type7 { opcode: 0x36, .. }))
        .unwrap();
    let control = load.word_offset as usize + 1;
    assert_eq!(artifact.words[control], 0x013a_0000);
    // The indirect preload does not replace the SP fallback descriptor base.
    // A6xx XML assigns 0xa9f0 to CS bindless slot 4, not graphics UAVs.
    let bases: Vec<_> = Packets::new(&artifact.words)
        .map(Result::unwrap)
        .filter(|p| matches!(p.header, Header::Type4 { register: 0xab1a, count: 2 }))
        .map(|p| p.word_offset as usize)
        .collect();
    assert_eq!(bases.len(), 2); // both stages establish the shared table
    for base in bases {
        let mut wrong_bank = artifact.clone();
        wrong_bank.words[base] = adreno_a6xx_pm4::type4(0xa9f0, 2).unwrap();
        assert!(validate_with_images(&wrong_bank, &case_images(&case)).is_err());
    }
    for (opcode, block) in [(0x32, 8), (0x34, 12)] {
        let mut invalid = artifact.clone();
        invalid.words[control - 1] = type7(opcode, 3).unwrap();
        invalid.words[control] =
            (artifact.words[control] & !(15 << 18)) | (block << 18) | (3 << 14);
        assert_eq!(
            validate_with_images(&invalid, &case_images(&case)),
            Err("a618: graphics UAV load must use the shared UAV block")
        );
    }
    // Changing either state kind or table size must not turn an authorized
    // table pointer into a load of unrelated memory/state.
    for value in [
        artifact.words[control] | (3 << 14),
        (artifact.words[control] & !(15 << 18)) | (8 << 18),
        artifact.words[control] + (1 << 22),
    ] {
        let mut invalid = artifact.clone();
        invalid.words[control] = value;
        assert!(validate_with_images(&invalid, &case_images(&case)).is_err());
    }
}

#[test]
fn real_core_and_khr_draw_parameters_reach_production_validator() {
    for extension in [false, true] {
        let case = fixtures::native_draw_parameters_case(extension);
        let artifact = case.compile().unwrap();
        let relocated = validate_with_images(&artifact, &case_images(&case))
            .expect("real DrawParameters must pass the production validator");
        let draws = adreno_a6xx_pm4::Packets::new(&relocated)
            .filter(|packet| {
                matches!(
                    packet.as_ref().unwrap().header,
                    adreno_a6xx_pm4::Header::Type7 { opcode: 0x38, .. }
                )
            })
            .count();
        assert_eq!(draws, 3);
    }
}

#[test]
fn repeated_native_glass_with_interned_descriptors_reaches_validator() {
    for private in [false, true] {
        let mut case = fixtures::native_repeated_sampled_case(32);
        if private {
            use sgfx_codegen_adreno_a6xx::{
                NativePrivateMemory, ObjectId, Operation, PrivateMemoryLayout,
            };
            use sgfx_core::ir::BufferUsage;
            let object = ObjectId::new(991);
            let layout = PrivateMemoryLayout::new(512, false).unwrap().unwrap();
            case.resources.push(fixtures::buffer(
                object,
                layout.allocation_bytes,
                BufferUsage::STORAGE,
            ));
            for op in &mut case.operations {
                if let Operation::SetProgrammablePipeline(p) = op {
                    let p = std::sync::Arc::make_mut(p);
                    std::sync::Arc::make_mut(&mut p.fragment.artifact)
                        .metadata
                        .pvtmem_size = 512;
                    p.fragment.private_memory = Some(NativePrivateMemory { object });
                }
            }
        }
        let artifact = case.compile().unwrap();
        for object in [fixtures::VS, fixtures::FS] {
            assert_eq!(
                artifact
                    .fixups
                    .iter()
                    .filter(|f| f.object == ObjectRef::NativeShader(object))
                    .count(),
                2
            );
        }
        let relocated = validate_with_images(&artifact, &case_images(&case))
            .expect("interned descriptors must retain typed relocation authority");
        let draws = adreno_a6xx_pm4::Packets::new(&relocated)
            .filter(|packet| {
                matches!(
                    packet.as_ref().unwrap().header,
                    adreno_a6xx_pm4::Header::Type7 { opcode: 0x38, .. }
                )
            })
            .count();
        assert_eq!(draws, 32);
    }
}

#[test]
fn retained_native_program_requires_complete_initialization_and_no_partial_transition() {
    use adreno_a6xx_pm4::{Header, Packets, type4};
    let case = fixtures::native_repeated_sampled_case(2);
    let a = case.compile().unwrap();
    let mut partial = a.clone();
    let last_draw = Packets::new(&a.words)
        .map(Result::unwrap)
        .filter(|p| matches!(p.header, Header::Type7 { opcode: 0x38, .. }))
        .last()
        .unwrap()
        .word_offset;
    partial.words.splice(
        last_draw as usize..last_draw as usize,
        [type4(0xa824, 1).unwrap(), 1],
    );
    for f in &mut partial.fixups {
        if f.word_offset >= last_draw {
            f.word_offset += 2;
        }
    }
    assert_eq!(
        validate_with_images(&partial, &case_images(&case)),
        Err("a618: incomplete native graphics program transition")
    );

    let mut omitted = a.clone();
    let mut mapping = vec![None; a.words.len()];
    omitted.words.clear();
    for p in Packets::new(&a.words).map(Result::unwrap) {
        // Keep typed shader preloads so this is still decoded as native, but
        // omit initial program bursts: no previous draw can supply them.
        let shader = matches!(
            p.header,
            Header::Type4 {
                register: 0xa81b | 0xa982,
                ..
            }
        );
        if shader {
            continue;
        }
        for i in p.word_offset as usize..p.word_offset as usize + p.payload.len() + 1 {
            mapping[i] = Some(omitted.words.len() as u32);
            omitted.words.push(a.words[i]);
        }
    }
    omitted.fixups = a
        .fixups
        .iter()
        .filter_map(|f| {
            let mut f = *f;
            f.word_offset = mapping[f.word_offset as usize]?;
            Some(f)
        })
        .collect();
    assert_eq!(
        validate_with_images(&omitted, &case_images(&case)),
        Err("a618: incomplete native graphics program transition")
    );
}

#[test]
fn emitted_native_d32_depth_bias_and_following_zero_state_reach_validator() {
    use adreno_a6xx_pm4::{Header, Packets};
    for (constant, slope) in [(2.0f32, 1.25f32), (-2.0, -1.5), (0.0, -0.5), (0.0, 0.0)] {
        let mut case = fixtures::native_depth_bias_case(constant, slope);
        let zero = fixtures::native_depth_bias_case(0.0, 0.0);
        case.operations.extend(zero.operations);
        let artifact = case.compile().unwrap();
        assert!(validate_with_images(&artifact, &case_images(&case)).is_ok());
        let mut bias = None;
        let mut raster = 0;
        let mut depth_format = 0;
        let mut draws = 0;
        for packet in Packets::new(&artifact.words) {
            let packet = packet.unwrap();
            match packet.header {
                Header::Type4 {
                    register: 0x8095, ..
                } => bias = Some(packet.payload),
                Header::Type4 {
                    register: 0x8090, ..
                } => raster = packet.payload[0],
                Header::Type4 {
                    register: 0x8098, ..
                } => depth_format = packet.payload[0],
                Header::Type7 { opcode: 0x38, .. } => {
                    let (constant, slope) = if draws == 0 {
                        (constant, slope)
                    } else {
                        (0.0, 0.0)
                    };
                    assert_eq!(
                        bias,
                        Some([slope.to_bits(), constant.to_bits(), 0].as_slice())
                    );
                    assert_eq!(raster & (1 << 11) != 0, constant != 0.0 || slope != 0.0);
                    assert_eq!(depth_format, 4);
                    draws += 1;
                }
                _ => {}
            }
        }
        assert_eq!(draws, 2);
    }
}

#[test]
fn native_depth_bias_rejects_nonfinite_reserved_and_uninitialized_state() {
    use adreno_a6xx_pm4::{Header, Packets, opcode, type7};
    let mut artifact = fixtures::render_case("fs").compile().unwrap();
    mutate_register(&mut artifact, 0x8090, 0, 0x2810);
    for (index, value) in [1.25f32, -2.0, 0.0].into_iter().enumerate() {
        mutate_register(&mut artifact, 0x8095, index, value.to_bits());
    }
    assert!(validate_result(&artifact).is_ok());
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for index in 0..3 {
            let mut invalid = artifact.clone();
            mutate_register(&mut invalid, 0x8095, index, value.to_bits());
            assert_eq!(
                validate_result(&invalid),
                Err("a618: native depth bias must contain three finite values")
            );
        }
    }
    let mut invalid = artifact.clone();
    mutate_register(&mut invalid, 0x8090, 0, 0x2810 | (1 << 17));
    assert_eq!(
        validate_result(&invalid),
        Err("a618: unsupported native raster control")
    );
    let mut missing = artifact.clone();
    for packet in Packets::new(&artifact.words) {
        let packet = packet.unwrap();
        if matches!(
            packet.header,
            Header::Type4 {
                register: 0x8095,
                ..
            }
        ) {
            let start = packet.word_offset as usize;
            missing.words[start..start + 4].fill(type7(opcode::WAIT_FOR_IDLE, 0).unwrap());
        }
    }
    assert_eq!(
        validate_result(&missing),
        Err("a618: native depth bias is not initialized in this submission")
    );
}

#[test]
fn filtered_float_blits_reach_validator_with_typed_source_and_destination() {
    use sgfx_core::ir::{FilterMode, TextureFormat};
    for format in [TextureFormat::Rgba16Float, TextureFormat::Rgba32Float] {
        for filter in [FilterMode::Nearest, FilterMode::Linear] {
            for flips in [[false, false], [true, false], [false, true], [true, true]] {
                let case = fixtures::native_float_blit_case(format, filter, flips);
                let images = case_images(&case);
                let artifact = case.compile().unwrap();
                assert!(validate_with_images(&artifact, &images).is_ok());
                let mut invalid = artifact.clone();
                mutate_register(
                    &mut invalid,
                    0xb4c0,
                    0,
                    0x30 | (1 << 10) | (1 << 20) | (1 << 22),
                );
                assert_eq!(
                    validate_with_images(&invalid, &images),
                    Err("a618: A2D source and destination formats differ")
                );
                assert!(validate_with_images(&artifact, &[]).is_err());
            }
        }
    }
}

#[test]
fn d32_upload_and_copy_require_bounded_staging_and_typed_tiled_images() {
    use sgfx_codegen_adreno_a6xx::GeneratedObjectKind;
    for case in [
        fixtures::native_depth_copy_case(),
        fixtures::native_depth_upload_case(),
    ] {
        let images = case_images(&case);
        let artifact = case.compile().unwrap();
        assert!(validate_with_images(&artifact, &images).is_ok());
        let mut linear_target = images.clone();
        linear_target
            .iter_mut()
            .find(|(id, _)| *id == token(ObjectRef::External(fixtures::TARGET)))
            .unwrap()
            .1
            .modifier = ImageModifier::Linear;
        assert!(validate_with_images(&artifact, &linear_target).is_err());
        let mut invalid = artifact.clone();
        let tiled = case.resources.iter().any(|r| r.id == fixtures::SOURCE);
        mutate_register(
            &mut invalid,
            0xb4c0,
            0,
            0x4a | (if tiled { 3 << 8 } else { 0 }) | (1 << 16) | (1 << 20) | (1 << 22),
        );
        assert_eq!(
            validate_with_images(&invalid, &images),
            Err("a618: D32 transfer must be an unfiltered same-size copy")
        );
        // Even a physically bounded image must not authorize scaling or
        // fractional source coordinates for a depth bit-preserving transfer.
        let mut scaled = artifact.clone();
        mutate_register(&mut scaled, 0x8401, 1, 1 << 8);
        assert!(validate_with_images(&scaled, &images).is_err());
        let mut fractional = artifact.clone();
        mutate_register(&mut fractional, 0x8401, 0, 1);
        assert!(validate_with_images(&fractional, &images).is_err());
        let mut oversized_pitch = artifact.clone();
        mutate_register(&mut oversized_pitch, 0xb4c4, 0, 0x7fff << 9);
        assert!(validate_with_images(&oversized_pitch, &images).is_err());
        for upload in artifact
            .generated_objects
            .iter()
            .filter(|o| o.kind == GeneratedObjectKind::Upload)
        {
            let object = ObjectRef::Generated(upload.id);
            let mut truncated = artifact.clone();
            for fixup in &mut truncated.fixups {
                if fixup.object == object {
                    fixup.required_size -= 4;
                }
            }
            assert_eq!(
                validate_with_images(&truncated, &images),
                Err("a618: invalid D32 linear staging authority")
            );
            let mut reinterpreted = images.clone();
            reinterpreted.push((
                token(object),
                LinearImage {
                    texture: None,
                    format: LinearImageFormat::Depth32Float,
                    modifier: ImageModifier::Tile6_3Depth,
                    width: 16,
                    height: 16,
                    row_pitch: 256,
                    visible_size: 4096,
                },
            ));
            assert_eq!(
                validate_with_images(&artifact, &reinterpreted),
                Err("a618: invalid D32 linear staging authority")
            );
        }
    }
}

#[test]
fn compiled_depth_array_and_cube_views_reach_production_validator() {
    for name in ["depth_sample", "array_sample", "cube_sample"] {
        let case = fixtures::native_sampled_case(name);
        let images = case_images(&case);
        let artifact = case.compile().unwrap();
        assert!(
            validate_with_images(&artifact, &images).is_ok(),
            "{name} descriptor and view authority"
        );
    }
}

#[test]
fn four_byte_storage_offsets_reach_validator_with_expanded_authority() {
    for offset in [4, 12, 60, 68] {
        let artifact = fixtures::native_storage_offset_case(offset)
            .compile()
            .unwrap();
        assert!(validate_with_images(&artifact, &[]).is_ok());
        let storage: Vec<_> = artifact
            .fixups
            .iter()
            .filter(|f| f.required_size == 516 + (offset & 63))
            .collect();
        assert_eq!(storage.len(), 3);
        assert!(storage.iter().all(|f| f.object_offset & 63 == 0));
    }
}

#[test]
fn readonly_graphics_storage_texture_alias_keeps_expanded_authority() {
    for texture_lowering in [false, true] {
        for offset in [4, 12, 60, 68] {
            // The true case explicitly selects the alternate compiler metadata
            // route; it verifies relocation handling, not execution of that ISA.
            let case = fixtures::native_graphics_storage_offset_case(offset, texture_lowering);
            let artifact = case.compile().unwrap();
            let fixups: Vec<_> = artifact
                .fixups
                .iter()
                .filter(|f| f.required_size == 516 + (offset & 63))
                .collect();
            // Identical VS/FS tables share a relocation. The alternate texture
            // lowering additionally has a distinct (sparse) texture table.
            assert_eq!(fixups.len(), if texture_lowering { 2 } else { 1 });
            assert!(fixups.iter().all(|f| f.object_offset == offset & !63));
            assert!(validate_with_images(&artifact, &case_images(&case)).is_ok());
        }
    }
}

#[test]
fn float_partial_row_copies_reach_validator_with_required_chunk_waits() {
    use adreno_a6xx_pm4::{Header, Packets, opcode, type7};
    use sgfx_core::ir::TextureFormat;
    for format in [TextureFormat::Rgba16Float, TextureFormat::Rgba32Float] {
        let artifact = fixtures::native_float_copy_case(format).compile().unwrap();
        assert!(validate_with_images(&artifact, &[]).is_ok());
        let packets = Packets::new(&artifact.words)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut copies = 0;
        for (i, packet) in packets.iter().enumerate() {
            if !matches!(packet.header, Header::Type7 { opcode: 0x75, .. }) {
                continue;
            }
            copies += 1;
            assert!(packet.payload[0] <= 128);
            let wait = &packets[i + 1];
            assert!(matches!(
                wait.header,
                Header::Type7 {
                    opcode: opcode::WAIT_MEM_WRITES,
                    count: 0
                }
            ));
            let mut invalid = artifact.clone();
            invalid.words[wait.word_offset as usize] = type7(opcode::WAIT_FOR_IDLE, 0).unwrap();
            assert_eq!(
                validate_with_images(&invalid, &[]),
                Err("qcom-adreno-a618: CP memory write must be followed by CP_WAIT_MEM_WRITES")
            );
        }
        assert!(copies >= 4);
    }
}

#[test]
fn real_ir3_compute_reaches_kernel_validator() {
    for name in ["cs", "push", "storage"] {
        std::eprintln!("native compute fixture: {name}");
        validate(&fixtures::compute_case(name).compile().unwrap());
    }
    validate(&fixtures::wide_shared_case().compile().unwrap());
}

#[test]
fn real_mrt_fixture_and_clear_reach_validator() {
    use sgfx_codegen_adreno_a6xx::{Operation, ResourceKind};
    use sgfx_core::ir::TextureFormat;
    let mut case = fixtures::native_color_case(
        &[TextureFormat::Rgba16Float, TextureFormat::Rgba32Float],
        true,
    );
    let images: Vec<_> = case
        .resources
        .iter()
        .filter_map(|r| {
            let ResourceKind::Image(meta) = &r.kind else {
                return None;
            };
            Some((
                token(ObjectRef::External(r.id)),
                LinearImage {
                    texture: meta.texture,
                    format: match meta.storage_format {
                        TextureFormat::Rgba16Float => LinearImageFormat::Rgba16Float,
                        TextureFormat::Rgba32Float => LinearImageFormat::Rgba32Float,
                        _ => LinearImageFormat::Bgra8Unorm,
                    },
                    modifier: ImageModifier::Linear,
                    width: meta.extent.width(),
                    height: meta.extent.height(),
                    row_pitch: meta.planes[0].stride,
                    visible_size: meta.planes[0].size,
                },
            ))
        })
        .collect();
    assert!(validate_with_images(&case.compile().unwrap(), &images).is_ok());
    case.operations.retain(|op| {
        matches!(
            op,
            Operation::BeginNativeRenderPass(_) | Operation::EndRenderPass
        )
    });
    assert!(validate_with_images(&case.compile().unwrap(), &images).is_ok());
}

#[test]
fn capped_preload_retains_full_program_authority_at_kernel_boundary() {
    use adreno_a6xx_pm4::{Header, Packets};
    use sgfx_codegen_adreno_a6xx::Operation;
    use std::sync::Arc;

    // Synthetic metadata exercises the cache boundary; this is not an ISA
    // execution test. The production emitter and validator process the wire.
    let mut case = fixtures::compute_case("storage");
    for op in &mut case.operations {
        if let Operation::SetComputePipeline(pipeline) = op {
            let shader = Arc::make_mut(&mut Arc::make_mut(pipeline).shader.artifact);
            shader.allocation.resize(65 * 128, 0);
            shader.metadata.allocation_bytes = 65 * 128;
            shader.metadata.instrlen = 65;
            shader.metadata.sizedwords = 65 * 32;
        }
    }
    case.resources
        .iter_mut()
        .find(|r| r.id == fixtures::CS)
        .unwrap()
        .size = 65 * 128;
    let artifact = case.compile().unwrap();
    assert!(Packets::new(&artifact.words).any(|packet| {
        let packet = packet.unwrap();
        matches!(packet.header, Header::Type7 { opcode: 0x34, .. })
            && packet.payload.len() == 3
            && packet.payload[0] == (2 << 16 | 13 << 18 | 64 << 22)
    }));
    validate(&artifact);

    // A cache-sized authorization must not authorize the longer program.
    let mut truncated = artifact.clone();
    for fixup in &mut truncated.fixups {
        if fixup.object == ObjectRef::NativeShader(fixtures::CS) {
            assert_eq!(fixup.required_size, 65 * 128);
            fixup.required_size = 64 * 128;
        }
    }
    assert_eq!(
        validate_result(&truncated),
        Err("a618: compute instruction size exceeds shader")
    );
}
#[test]
fn real_ir3_render_reaches_kernel_validator() {
    for name in ["fs", "glass", "horizontal", "vertical", "erase"] {
        std::eprintln!("native fixture: {name}");
        validate(&fixtures::render_fixture(name));
    }
}

#[test]
fn float_targets_require_matching_typed_image_footprints() {
    use adreno_a6xx_pm4::{Header, Packets};
    for (format, hardware, bytes) in [
        (LinearImageFormat::Rgba16Float, 0x62, 8),
        (LinearImageFormat::Rgba32Float, 0x82, 16),
    ] {
        let mut artifact = fixtures::render_fixture("fs");
        let image = LinearImage {
            texture: None,
            format,
            modifier: ImageModifier::Linear,
            width: 16,
            height: 16,
            row_pitch: 16 * bytes,
            visible_size: u64::from(256 * bytes),
        };
        let target = ObjectRef::External(fixtures::TARGET);
        for access in &mut artifact.accesses {
            if access.object == target {
                access.size = image.visible_size;
            }
        }
        for fixup in &mut artifact.fixups {
            if fixup.object == target {
                fixup.required_size = image.visible_size;
            }
        }
        let mut format_word = 0;
        let packets = Packets::new(&artifact.words)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let edits: Vec<_> = packets
            .iter()
            .filter_map(|p| {
                let Header::Type4 { register, .. } = p.header else {
                    return None;
                };
                match register {
                    0x8822 | 0xa996 | 0x8102 => {
                        Some((p.word_offset as usize + 1, std::vec![hardware]))
                    }
                    0x8823 => Some((
                        p.word_offset as usize + 1,
                        std::vec![image.row_pitch / 64, image.visible_size as u32 / 64],
                    )),
                    _ => None,
                }
            })
            .collect();
        for p in &packets {
            if matches!(
                p.header,
                Header::Type4 {
                    register: 0xa996,
                    ..
                }
            ) {
                format_word = p.word_offset as usize + 1;
            }
        }
        drop(packets);
        for (word, data) in edits {
            artifact.words[word..word + data.len()].copy_from_slice(&data);
        }
        let id = token(target);
        assert!(validate_with_images(&artifact, &[(id, image)]).is_ok());
        assert!(validate_with_images(&artifact, &[]).is_err());
        for malformed in [
            LinearImage {
                format: LinearImageFormat::Bgra8Unorm,
                ..image
            },
            LinearImage {
                row_pitch: image.row_pitch / 2,
                ..image
            },
            LinearImage {
                visible_size: image.visible_size - 64,
                ..image
            },
            LinearImage {
                modifier: ImageModifier::Tile6_3Color,
                ..image
            },
            LinearImage {
                width: image.width + 1,
                ..image
            },
        ] {
            assert!(validate_with_images(&artifact, &[(id, malformed)]).is_err());
        }
        let mut wrong_output = artifact.clone();
        wrong_output.words[format_word] = 0x30;
        assert_eq!(
            validate_with_images(&wrong_output, &[(id, image)]),
            Err("a618: native SP/RB MRT formats disagree")
        );
    }
}

#[test]
fn native_depth_target_requires_typed_tiled_d32_authority() {
    use sgfx_codegen_adreno_a6xx::{DepthAttachment, ObjectId, Operation, ResourceKind};
    use sgfx_core::ir::{
        CompareFunction, DepthLoadOp, DepthState, StoreOp, TextureFormat, TextureUsage,
    };
    use std::sync::Arc;
    let mut case = fixtures::render_case("fs");
    let id = ObjectId::new(40);
    let mut resource = fixtures::image(id, TextureUsage::RENDER_ATTACHMENT);
    resource.size = 4096;
    let ResourceKind::Image(meta) = &mut resource.kind else {
        panic!()
    };
    meta.format = TextureFormat::Depth32Float;
    meta.storage_format = TextureFormat::Depth32Float;
    meta.modifier = sgfx_codegen_adreno_a6xx::ImageModifier::A6xxTile6_3Depth;
    meta.planes[0].stride = 256;
    meta.planes[0].size = 4096;
    case.resources.push(resource);
    for operation in &mut case.operations {
        match operation {
            Operation::BeginRenderPass(pass) => {
                pass.depth = Some(DepthAttachment {
                    target: id,
                    load: DepthLoadOp::Load,
                    store: StoreOp::Store,
                })
            }
            Operation::SetProgrammablePipeline(p) => {
                let p = Arc::make_mut(p);
                p.descriptor = p
                    .descriptor
                    .clone()
                    .with_depth_state(DepthState::new(
                        TextureFormat::Depth32Float,
                        CompareFunction::Less,
                        true,
                    ))
                    .unwrap();
            }
            _ => {}
        }
    }
    let mut images = [
        (
            token(ObjectRef::External(fixtures::TARGET)),
            LinearImage {
                texture: None,
                format: LinearImageFormat::Bgra8Unorm,
                modifier: ImageModifier::Linear,
                width: 16,
                height: 16,
                row_pitch: 64,
                visible_size: 1024,
            },
        ),
        (
            token(ObjectRef::External(id)),
            LinearImage {
                texture: None,
                format: LinearImageFormat::Depth32Float,
                modifier: ImageModifier::Tile6_3Depth,
                width: 16,
                height: 16,
                row_pitch: 256,
                visible_size: 4096,
            },
        ),
    ];
    let artifact = case.compile().unwrap();
    assert!(validate_with_images(&artifact, &images).is_ok());
    images[1].1.modifier = ImageModifier::Linear;
    assert!(validate_with_images(&artifact, &images).is_err());
    images[1].1.modifier = ImageModifier::Tile6_3Depth;
    images[1].1.format = LinearImageFormat::Rgba32Float;
    assert!(validate_with_images(&artifact, &images).is_err());
}

#[test]
fn production_float_render_and_shader_free_clear_reach_validator() {
    use sgfx_codegen_adreno_a6xx::{
        NativeColorAttachment, NativeRenderPass, Operation, ResourceKind,
    };
    use sgfx_core::ir::{
        BlendState, Color, ColorTargetState, ColorWriteMask, LoadOp, StoreOp, TextureFormat,
    };
    use std::sync::Arc;
    for (format, kernel_format, bytes) in [
        (
            TextureFormat::Rgba16Float,
            LinearImageFormat::Rgba16Float,
            8,
        ),
        (
            TextureFormat::Rgba32Float,
            LinearImageFormat::Rgba32Float,
            16,
        ),
    ] {
        let mut case = fixtures::render_case("fs");
        let image = LinearImage {
            texture: None,
            format: kernel_format,
            modifier: ImageModifier::Linear,
            width: 16,
            height: 16,
            row_pitch: 16 * bytes,
            visible_size: u64::from(256 * bytes),
        };
        let target = case
            .resources
            .iter_mut()
            .find(|r| r.id == fixtures::TARGET)
            .unwrap();
        target.size = image.visible_size;
        let ResourceKind::Image(meta) = &mut target.kind else {
            panic!()
        };
        meta.format = format;
        meta.storage_format = format;
        meta.planes[0].stride = image.row_pitch;
        meta.planes[0].size = image.visible_size;
        for op in &mut case.operations {
            match op {
                Operation::BeginRenderPass(pass) => {
                    *op = Operation::BeginNativeRenderPass(NativeRenderPass {
                        colors: std::vec![NativeColorAttachment {
                            target: pass.target,
                            load: LoadOp::Clear(Color::rgba(-0.0, -0.5, 2.0, 0.00001).unwrap()),
                            store: StoreOp::Store
                        }],
                        area: pass.area,
                        depth: pass.depth,
                    });
                }
                Operation::SetProgrammablePipeline(p) => {
                    let p = Arc::make_mut(p);
                    p.descriptor = p
                        .descriptor
                        .clone()
                        .with_color_targets(std::vec![
                            ColorTargetState::new(
                                format,
                                BlendState::REPLACE,
                                ColorWriteMask::from_bits(5).unwrap()
                            )
                            .unwrap()
                        ])
                        .unwrap();
                }
                _ => {}
            }
        }
        let authority = [(token(ObjectRef::External(fixtures::TARGET)), image)];
        let render = case.compile().unwrap();
        assert!(validate_with_images(&render, &authority).is_ok());
        case.operations.retain(|op| {
            matches!(
                op,
                Operation::BeginNativeRenderPass(_) | Operation::EndRenderPass
            )
        });
        let clear = case.compile().unwrap();
        assert!(
            !clear
                .fixups
                .iter()
                .any(|f| matches!(f.object, ObjectRef::NativeShader(_)))
        );
        assert!(validate_with_images(&clear, &authority).is_ok());
        assert!(
            validate_with_images(
                &clear,
                &[(
                    authority[0].0,
                    LinearImage {
                        format: LinearImageFormat::Bgra8Unorm,
                        ..image
                    }
                )]
            )
            .is_err()
        );
    }
}

#[cfg(feature = "live-compiler")]
fn replace_compute_shader(case: &mut fixtures::Case, shader: &sgfx_shader_ir3::CompiledShader) {
    use sgfx_codegen_adreno_a6xx::Operation;
    use std::sync::Arc;
    let mut replaced = false;
    for op in &mut case.operations {
        if let Operation::SetComputePipeline(pipeline) = op {
            let pipeline = Arc::make_mut(pipeline);
            assert_eq!(pipeline.shader.artifact.entry, shader.entry);
            pipeline.shader.artifact = Arc::new(shader.clone());
            *case
                .resources
                .iter_mut()
                .find(|r| r.id == pipeline.shader.object)
                .unwrap() = fixtures::shader_resource(&pipeline.shader);
            replaced = true;
        }
    }
    assert!(replaced);
}

#[cfg(feature = "live-compiler")]
#[test]
fn freshly_compiled_shared_128_thread_group_reaches_validator() {
    use adreno_a6xx_pm4::{Header, Packets};
    use sgfx_shader_ir3::{Source, Stage, compile};
    let shader = compile(
        Source::Wgsl(include_str!(
            "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native-fixtures/workgroup.wgsl"
        )),
        Stage::Compute,
        "wide_shared",
    )
    .unwrap();
    let metadata = &shader.metadata;
    assert_eq!(metadata.local_size, [128, 1, 1]);
    assert!(metadata.has_barrier);
    let shared = metadata
        .shared_size
        .max(metadata.compute.as_ref().unwrap().req_local_mem);
    assert!((4096..=32768).contains(&shared));
    let mut case = fixtures::wide_shared_case();
    replace_compute_shader(&mut case, &shader);
    let artifact = case.compile().unwrap();
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let register = |reg| {
        packets
            .iter()
            .find_map(|p| {
                matches!(p.header, Header::Type4 { register, .. } if register == reg)
                    .then_some(p.payload)
            })
            .unwrap()
    };
    assert_eq!(register(0xa9b1)[0] & 31, ((shared - 1) / 1024).max(1));
    assert_eq!((register(0xb990)[0] >> 2) & 1023, 127);
    assert!(validate_with_images(&artifact, &[]).is_ok());
    // Reserved shared/constant mode must remain forbidden even for native ISA.
    let mut reserved = artifact.clone();
    mutate_register(&mut reserved, 0xa9b1, 0, 3 << 5);
    assert!(validate_with_images(&reserved, &[]).is_err());
    // Local dimensions and dispatched global dimensions must agree.
    let mut wrong_global = artifact.clone();
    mutate_register(&mut wrong_global, 0xb990, 1, register(0xb990)[1] + 1);
    assert_eq!(
        validate_with_images(&wrong_global, &[]),
        Err("a618: compute dispatch/ND range mismatch")
    );
}

#[cfg(feature = "live-compiler")]
#[test]
fn freshly_compiled_ssbo_offsets_preserve_logical_extent_and_physical_authority() {
    use sgfx_codegen_adreno_a6xx::AddressEncoding;
    use sgfx_shader_ir3::{Source, Stage, compile};
    let shader = compile(
        Source::Wgsl(include_str!(
            "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native-fixtures/storage.wgsl"
        )),
        Stage::Compute,
        "cs",
    )
    .unwrap();
    assert_eq!(shader.metadata.local_size, [128, 1, 1]);
    for offset in [4, 12, 60, 68] {
        let mut case = fixtures::native_storage_offset_case(offset);
        replace_compute_shader(&mut case, &shader);
        let artifact = case.compile().unwrap();
        let descriptors: Vec<_> = artifact
            .fixups
            .iter()
            .filter(|fixup| {
                fixup.encoding == AddressEncoding::GpuVa49TexDescriptor
                    && fixup.required_size == 516 + (offset & 63)
            })
            .collect();
        assert_eq!(descriptors.len(), 3);
        for fixup in descriptors {
            let base = fixup.word_offset as usize;
            assert_eq!(artifact.words[base - 3], 516 / 4);
            assert_eq!(
                artifact.words[base - 2],
                4 << 29 | 1 << 4 | (((offset & 63) / 4) as u32) << 16
            );
            assert_eq!(fixup.object_offset, offset & !63);
            assert_eq!(fixup.object_offset + fixup.required_size, offset + 516);
        }
        assert!(validate_with_images(&artifact, &[]).is_ok());
    }
}

#[cfg(feature = "live-compiler")]
#[test]
fn freshly_compiled_mixed_float_mrt_reaches_validator() {
    use sgfx_codegen_adreno_a6xx::{
        NativeColorAttachment, NativeRenderPass, ObjectId, Operation, ResourceKind,
    };
    use sgfx_core::ir::{
        BlendState, ColorTargetState, ColorWriteMask, LoadOp, StoreOp, TextureFormat,
    };
    use sgfx_shader_ir3::{Source, Stage, compile};
    use std::sync::Arc;
    let source = include_str!(
        "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native-fixtures/procedural.wgsl"
    );
    for count in [2, 8] {
        let mut wgsl = source.split("@fragment").next().unwrap().to_owned();
        wgsl.push_str("struct Outputs {\n");
        for i in 0..count {
            wgsl.push_str(&std::format!("@location({i}) color{i}: vec4<f32>,\n"));
        }
        wgsl.push_str("}\n@fragment fn fs(input: Varyings) -> Outputs { var out: Outputs;\n");
        for i in 0..count {
            wgsl.push_str(&std::format!(
                "out.color{i} = vec4(input.color + vec3({i}.25), 0.5);\n"
            ));
        }
        wgsl.push_str("return out; }");
        let vs = compile(Source::Wgsl(&wgsl), Stage::Vertex, "vs").unwrap();
        let fs = compile(Source::Wgsl(&wgsl), Stage::Fragment, "fs").unwrap();
        let mut case = fixtures::render_case("fs");
        let mut colors = Vec::new();
        let mut states = Vec::new();
        let mut images = Vec::new();
        for i in 0..count {
            let id = ObjectId::new(20 + i);
            let (format, kernel_format, bytes) = if i % 2 == 0 {
                (
                    TextureFormat::Rgba16Float,
                    LinearImageFormat::Rgba16Float,
                    8,
                )
            } else {
                (
                    TextureFormat::Rgba32Float,
                    LinearImageFormat::Rgba32Float,
                    16,
                )
            };
            let mut resource = fixtures::image(id, sgfx_core::ir::TextureUsage::RENDER_ATTACHMENT);
            resource.size = 256 * bytes;
            let ResourceKind::Image(meta) = &mut resource.kind else {
                panic!()
            };
            meta.format = format;
            meta.storage_format = format;
            meta.planes[0].stride = 16 * bytes as u32;
            meta.planes[0].size = resource.size;
            images.push((
                token(ObjectRef::External(id)),
                LinearImage {
                    texture: None,
                    format: kernel_format,
                    modifier: ImageModifier::Linear,
                    width: 16,
                    height: 16,
                    row_pitch: meta.planes[0].stride,
                    visible_size: resource.size,
                },
            ));
            case.resources.push(resource);
            colors.push(NativeColorAttachment {
                target: id,
                load: LoadOp::Load,
                store: StoreOp::Store,
            });
            states.push(
                ColorTargetState::new(
                    format,
                    BlendState::REPLACE,
                    ColorWriteMask::from_bits(if i % 2 == 0 { 5 } else { 10 }).unwrap(),
                )
                .unwrap(),
            );
        }
        for op in &mut case.operations {
            match op {
                Operation::BeginRenderPass(pass) => {
                    *op = Operation::BeginNativeRenderPass(NativeRenderPass {
                        colors: colors.clone(),
                        area: pass.area,
                        depth: pass.depth,
                    })
                }
                Operation::SetProgrammablePipeline(p) => {
                    let p = Arc::make_mut(p);
                    p.vertex.artifact = Arc::new(vs.clone());
                    p.fragment.artifact = Arc::new(fs.clone());
                    p.descriptor = p
                        .descriptor
                        .clone()
                        .with_color_targets(states.clone())
                        .unwrap();
                    for shader in [&p.vertex, &p.fragment] {
                        *case
                            .resources
                            .iter_mut()
                            .find(|r| r.id == shader.object)
                            .unwrap() = fixtures::shader_resource(shader);
                    }
                }
                _ => {}
            }
        }
        let artifact = case.compile().unwrap();
        assert!(validate_with_images(&artifact, &images).is_ok());
        // Every target, including MRT7, must independently match authority.
        for i in 0..images.len() {
            let mut wrong = images.clone();
            wrong[i].1.format = LinearImageFormat::Bgra8Unorm;
            assert!(validate_with_images(&artifact, &wrong).is_err());
        }
    }
}

#[cfg(feature = "live-compiler")]
#[test]
fn freshly_compiled_arbitrary_wgsl_graphics_and_compute_reach_validator() {
    use sgfx_codegen_adreno_a6xx::Operation;
    use sgfx_shader_ir3::{Source, Stage, compile};
    use std::sync::Arc;
    let source = include_str!(
        "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native-fixtures/procedural.wgsl"
    );
    let modified = source.replace(
        "input.color * input.color + vec3(0.05)",
        "input.color * input.color * input.color + vec3(0.17)",
    );
    assert_ne!(source, modified);
    let mut previous = None;
    for source in [source, modified.as_str()] {
        let vertex = compile(Source::Wgsl(source), Stage::Vertex, "vs").unwrap();
        let fragment = compile(Source::Wgsl(source), Stage::Fragment, "fs").unwrap();
        if let Some(previous) = previous {
            assert_ne!(previous, fragment.allocation);
        }
        previous = Some(fragment.allocation.clone());
        let mut case = fixtures::render_case("fs");
        for op in &mut case.operations {
            if let Operation::SetProgrammablePipeline(pipeline) = op {
                let pipeline = Arc::make_mut(pipeline);
                pipeline.vertex.artifact = Arc::new(vertex.clone());
                pipeline.fragment.artifact = Arc::new(fragment.clone());
                for shader in [&pipeline.vertex, &pipeline.fragment] {
                    *case
                        .resources
                        .iter_mut()
                        .find(|r| r.id == shader.object)
                        .unwrap() = fixtures::shader_resource(shader);
                }
            }
        }
        validate(&case.compile().unwrap());
    }
    let source = include_str!(
        "../../../userspace/sgfx-codegen-adreno-a6xx/tests/native-fixtures/compute.wgsl"
    );
    let modified = source.replace("id.x * id.x", "id.x * 3u + 19u");
    assert_ne!(source, modified);
    let mut previous = None;
    for source in [source, modified.as_str()] {
        let shader = compile(Source::Wgsl(source), Stage::Compute, "cs").unwrap();
        if let Some(previous) = previous {
            assert_ne!(previous, shader.allocation);
        }
        previous = Some(shader.allocation.clone());
        let mut case = fixtures::compute_case("cs");
        for op in &mut case.operations {
            if let Operation::SetComputePipeline(pipeline) = op {
                let pipeline = Arc::make_mut(pipeline);
                pipeline.shader.artifact = Arc::new(shader.clone());
                *case
                    .resources
                    .iter_mut()
                    .find(|r| r.id == pipeline.shader.object)
                    .unwrap() = fixtures::shader_resource(&pipeline.shader);
            }
        }
        validate(&case.compile().unwrap());
    }
}

#[test]
fn padded_ubo_copy_reaches_validator_and_requires_each_chunk_wait() {
    use adreno_a6xx_pm4::{Header, Packets, opcode, type7};
    use sgfx_codegen_adreno_a6xx::{NativeBindingResource, Operation};
    let mut case = fixtures::compute_case("cs");
    case.resources
        .iter_mut()
        .find(|r| r.id == fixtures::UBO)
        .unwrap()
        .size = 516;
    for operation in &mut case.operations {
        if let Operation::SetBindGroup { bindings, .. } = operation {
            for binding in bindings {
                if let NativeBindingResource::Buffer { object, size, .. } = &mut binding.resource {
                    if *object == fixtures::UBO {
                        *size = 516;
                    }
                }
            }
        }
    }
    let artifact = case.compile().unwrap();
    let packets: Vec<_> = Packets::new(&artifact.words)
        .collect::<Result<_, _>>()
        .unwrap();
    let copies: Vec<_> = packets
        .iter()
        .enumerate()
        .filter(|(_, packet)| matches!(packet.header, Header::Type7 { opcode: 0x75, .. }))
        .collect();
    assert_eq!(
        copies
            .iter()
            .map(|(_, packet)| packet.payload[0])
            .collect::<Vec<_>>(),
        [128, 1]
    );
    validate(&artifact);
    for (index, _) in copies {
        let wait = &packets[index + 1];
        assert!(matches!(
            wait.header,
            Header::Type7 {
                opcode: opcode::WAIT_MEM_WRITES,
                count: 0
            }
        ));
        // Keep packet lengths and every relocation offset unchanged while
        // replacing the required memory-write ordering with an insufficient WFI.
        let mut missing_wait = artifact.clone();
        missing_wait.words[wait.word_offset as usize] = type7(opcode::WAIT_FOR_IDLE, 0).unwrap();
        assert_eq!(
            validate_result(&missing_wait),
            Err("qcom-adreno-a618: CP memory write must be followed by CP_WAIT_MEM_WRITES")
        );
    }
}

#[test]
fn normalized_render_targets_validate_clear_and_draw() {
    use sgfx_codegen_adreno_a6xx::{Operation, ResourceKind};
    use sgfx_core::ir::{
        BlendState, Color, ColorTargetState, ColorWriteMask, LoadOp, TextureFormat as F,
    };
    use std::sync::Arc;
    for format in [F::Bgra8UnormSrgb, F::Rgba8UnormSrgb, F::R8Unorm] {
        let mut case = fixtures::native_color_case(&[F::Bgra8Unorm], false);
        for r in &mut case.resources {
            if let ResourceKind::Image(meta) = &mut r.kind {
                meta.format = format;
            }
        }
        for op in &mut case.operations {
            match op {
                Operation::BeginNativeRenderPass(pass) => {
                    pass.colors[0].load = LoadOp::Clear(Color::rgba(0.25, 0.5, 0.75, 0.5).unwrap())
                }
                Operation::SetProgrammablePipeline(p) => {
                    let p = Arc::make_mut(p);
                    p.descriptor = p
                        .descriptor
                        .clone()
                        .with_color_targets(std::vec![
                            ColorTargetState::new(
                                format,
                                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                                ColorWriteMask::ALL
                            )
                            .unwrap()
                        ])
                        .unwrap();
                }
                _ => {}
            }
        }
        let artifact = case.compile().unwrap();
        let images = case_images(&case);
        validate_with_images(&artifact, &images).unwrap();
        let mut invalid = artifact.clone();
        mutate_register(&mut invalid, 0x880f, 0, 0x100);
        assert!(validate_with_images(&invalid, &images).is_err());
        case.operations.retain(|o| {
            matches!(
                o,
                Operation::BeginNativeRenderPass(_) | Operation::EndRenderPass
            )
        });
        validate_with_images(&case.compile().unwrap(), &images).unwrap();
    }
}

#[cfg(feature = "live-compiler")]
#[path = "storage_image.rs"]
mod storage_image;

#[test]
fn fixed_normalized_targets_reach_validator_with_retained_draws() {
    use sgfx_codegen_adreno_a6xx::{self as cg, Operation, ResourceKind};
    use sgfx_core::ir::*;
    for format in [
        TextureFormat::Bgra8UnormSrgb,
        TextureFormat::Rgba8UnormSrgb,
        TextureFormat::R8Unorm,
    ] {
        let mut resources = std::vec![
            fixtures::image(fixtures::TARGET, TextureUsage::RENDER_ATTACHMENT),
            fixtures::buffer(fixtures::VERTICES, 192, BufferUsage::VERTEX)
        ];
        let ResourceKind::Image(meta) = &mut resources[0].kind else {
            panic!()
        };
        meta.format = format;
        let pipeline = cg::PipelineMeta {
            id: cg::PipelineId::new(0),
            descriptor: RenderPipelineDesc::new(
                format,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    32,
                    std::vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16)
                    ],
                )
                .unwrap(),
                FragmentProgram::VertexColor,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        };
        let operations = std::vec![
            Operation::BeginRenderPass(cg::RenderPass {
                target: fixtures::TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.25, 0.5, 0.75, 0.5).unwrap()),
                store: StoreOp::Store,
                depth: None
            }),
            Operation::SetPipeline(pipeline.id),
            Operation::SetVertexBuffer {
                buffer: fixtures::VERTICES,
                offset: 0
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap()
            )),
            Operation::Draw {
                vertex_count: 6,
                first_vertex: 0
            },
            Operation::Draw {
                vertex_count: 6,
                first_vertex: 0
            },
            Operation::EndRenderPass
        ];
        let a = cg::compile(cg::CompileInput {
            capabilities: cg::Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &[pipeline],
            operations: &operations,
        })
        .unwrap();
        validate_result(&a).unwrap();
        let mut invalid = a.clone();
        mutate_register(&mut invalid, 0x880f, 0, 2);
        assert!(validate_result(&invalid).is_err());
    }
}

#[cfg(feature = "live-compiler")]
#[path = "sample_mask.rs"]
mod sample_mask;

#[test]
fn native_primitive_id_routes_pass_strict_boundary_and_reserved_pc_bits_fail() {
    let case = fixtures::native_primitive_id_case(true);
    let artifact = case.compile().unwrap();
    validate(&artifact);
    for value in [2, 3, 1 << 31, u32::MAX] {
        let mut invalid = artifact.clone();
        mutate_register(&mut invalid, 0x9806, 0, value);
        assert!(validate_result(&invalid).is_err(), "PC_PS_CNTL={value:#x}");
    }
    // Zero is still the canonical/native default; bit zero is the ONLY new
    // documented scalar. A plain native program emits zero without mutation.
    let plain = fixtures::render_case("fs").compile().unwrap();
    validate(&plain);
    let mut enabled = plain.clone();
    mutate_register(&mut enabled, 0x9806, 0, 1);
    validate(&enabled);
}

#[test]
fn native_single_sample_builtin_interpolation_and_derivatives_pass_existing_strict_boundary() {
    for center in [false,true] {
        for derivatives in [false,true] {
            let case = fixtures::native_single_sample_case(center,derivatives);
            let artifact = case.compile().unwrap();
            validate(&artifact);
        }
    }
}

#[test]
fn native_rg8_logical_render_target_passes_strict_boundary() {
    validate(&fixtures::native_rg8_case().compile().unwrap());
}

#[test]
fn generated_fixed_general_layout_float_sampling_and_negative_base_pass_existing_native_boundary() {
    for indexed in [false,true] {let case=fixtures::fixed_native_case(indexed);assert!(validate_with_images(&case.compile().unwrap(),&case_images(&case)).is_ok());}
}

#[test]
fn native_fixed_nv12_two_plane_relocations_pass_with_readonly_import_and_reject_write_authority() {
    use sgfx_core::ir::*;
    use sgfx_codegen_adreno_a6xx::Access;
    let c=fixtures::fixed_nv12_case(YcbcrConversion {matrix:YcbcrMatrix::Bt709,range:YcbcrRange::Limited,chroma_x:ChromaLocation::Midpoint,chroma_y:ChromaLocation::Midpoint});
    let source=token(ObjectRef::External(fixtures::SOURCE));
    let mut images=case_images(&c);
    let image=&mut images.iter_mut().find(|(id,_)|*id==source).unwrap().1;
    image.format=LinearImageFormat::Nv12 {uv_offset:4096,uv_size:512};
    image.visible_size=4608;
    let a=c.compile().unwrap();
    let permissions=|id|if id==source {wire::ACCESS_READ}else{wire::ACCESS_READ|wire::ACCESS_WRITE};
    validate_with_permissions(&a,&images,permissions).unwrap();
    let mut wrong=a.clone();
    for f in &mut wrong.fixups {if f.object==ObjectRef::External(fixtures::SOURCE)&&f.object_offset==4096 {f.access=Access::WRITE;}}
    for r in &mut wrong.accesses {if r.object==ObjectRef::External(fixtures::SOURCE)&&r.offset==4096 {r.access=Access::WRITE;}}
    assert!(validate_with_permissions(&wrong,&images,permissions).is_err());
}
#[test]
fn generic_nv12_shader_real_conversion_and_queries_pass_unchanged_native_kernel_policy() {
    let case=fixtures::generic_nv12_case();let a=case.compile().unwrap();let source=token(ObjectRef::External(fixtures::SOURCE));
    let mut images=case_images(&case);let image=&mut images.iter_mut().find(|(id,_)|*id==source).unwrap().1;
    image.format=LinearImageFormat::Nv12 {uv_offset:4096,uv_size:512};image.visible_size=4608;
    validate_with_permissions(&a,&images,|id|if id==source {wire::ACCESS_READ}else{wire::ACCESS_READ|wire::ACCESS_WRITE}).unwrap();
}

#[cfg(feature = "live-compiler")]
#[test]
fn canonical_nv12_and_programmable_draws_share_one_valid_submission() {
    use sgfx_codegen_adreno_a6xx::*;
    use sgfx_core::ir::*;
    use std::sync::Arc;
    let mut case=fixtures::generic_nv12_case();
    let Operation::SetProgrammablePipeline(converted)=&case.operations[1] else {panic!()};let converted=converted.clone();
    let map=sgfx_shader_ir3::BindingMap::new(std::vec![
        sgfx_shader_ir3::BindingAssignment {group:0,binding:0,kind:sgfx_shader_ir3::ResourceKind::Texture,slot:0},
        sgfx_shader_ir3::BindingAssignment {group:0,binding:1,kind:sgfx_shader_ir3::ResourceKind::Sampler,slot:0}]).unwrap();
    let source=include_str!("../../../userspace/sgfx-shader-ir3/tests/shaders/nv12_programmable.wgsl");
    let ordinary=Arc::new(NativeRenderPipeline {descriptor:converted.descriptor.clone(),vertex:converted.vertex.clone(),fragment:NativeShader {private_memory:None,object:ObjectId::new(13),artifact:Arc::new(sgfx_shader_ir3::compile_with_bindings(sgfx_shader_ir3::Source::Wgsl(source),sgfx_shader_ir3::Stage::Fragment,"fs",&map).unwrap())}});
    case.resources.push(fixtures::shader_resource(&ordinary.fragment));
    let regular=ObjectId::new(6);let target=ObjectId::new(7);
    let mut image=fixtures::image(regular,TextureUsage::SAMPLED|TextureUsage::COPY_DST);
    image.size=65536;if let ResourceKind::Image(i)=&mut image.kind {i.extent=Extent2D::new(128,128).unwrap();i.planes[0].stride=512;i.planes[0].size=65536;}
    case.resources.push(image);case.resources.push(fixtures::image(target,TextureUsage::RENDER_ATTACHMENT));
    case.resources.push(fixtures::buffer(fixtures::VERTICES,144,BufferUsage::VERTEX|BufferUsage::COPY_DST));
    let r=case.resources.iter_mut().find(|r|r.id==fixtures::SOURCE).unwrap();r.size=24576;
    if let ResourceKind::Image(i)=&mut r.kind {i.extent=Extent2D::new(128,128).unwrap();i.planes=std::vec![PlaneLayout {offset:0,stride:128,size:16384},PlaneLayout {offset:16384,stride:128,size:8192}];}
    let pipeline=PipelineMeta {id:PipelineId::new(99),descriptor:RenderPipelineDesc::new(TextureFormat::Bgra8Unorm,PrimitiveTopology::TriangleList,
        VertexBufferLayout::new(24,std::vec![VertexAttribute::new(0,VertexFormat::Float32x4,0),VertexAttribute::new(1,VertexFormat::Float32x2,16)]).unwrap(),
        FragmentProgram::Texture(TextureSampleMode::Rgba),BlendState::REPLACE,RasterState::new(CullMode::None,FrontFace::CounterClockwise)).unwrap()};
    for uploads in [false,true] {
        let mut ops=std::vec![];
        if uploads {ops.push(Operation::WriteBuffer {destination:fixtures::VERTICES,offset:0,data:&[0;144]});ops.push(Operation::WriteTexture {destination:regular,area:PixelRect::new(0,0,128,128).unwrap(),bytes_per_row:512,data:&[0;65536]});}
        ops.extend([
            Operation::BeginRenderPass(RenderPass {target:fixtures::TARGET,area:PixelRect::new(0,0,16,16).unwrap(),load:LoadOp::Load,store:StoreOp::Store,depth:None}),
            Operation::SetPipeline(pipeline.id),Operation::SetVertexBuffer {buffer:fixtures::VERTICES,offset:0},Operation::SetTexture(fixtures::SOURCE),Operation::SetSampler(fixtures::sampler()),
            Operation::SetUniforms(DrawUniforms::new(Transform::identity(),Color::rgba(1.0,1.0,1.0,1.0).unwrap())),Operation::Draw {vertex_count:6,first_vertex:0},Operation::EndRenderPass,
            Operation::BeginRenderPass(RenderPass {target,area:PixelRect::new(0,0,16,16).unwrap(),load:LoadOp::Load,store:StoreOp::Store,depth:None}),
            Operation::SetProgrammablePipeline(converted.clone()),Operation::SetBindGroup {index:0,bindings:std::vec![NativeBinding {binding:0,resource:NativeBindingResource::Nv12Texture(fixtures::SOURCE)},NativeBinding {binding:1,resource:NativeBindingResource::Sampler(fixtures::sampler())}]},Operation::Draw {vertex_count:3,first_vertex:0},
            Operation::SetProgrammablePipeline(ordinary.clone()),Operation::SetBindGroup {index:0,bindings:std::vec![NativeBinding {binding:0,resource:NativeBindingResource::Texture {object:regular,view:None}},NativeBinding {binding:1,resource:NativeBindingResource::Sampler(fixtures::sampler())}]},Operation::Draw {vertex_count:3,first_vertex:0},Operation::EndRenderPass]);
        let a=compile(CompileInput {capabilities:Capabilities::a618(512*1024,131072),resources:&case.resources,pipelines:&[pipeline.clone()],operations:&ops}).unwrap();
        let mut images=case_images(&case);let source=token(ObjectRef::External(fixtures::SOURCE));
        let i=&mut images.iter_mut().find(|(id,_)|*id==source).unwrap().1;i.format=LinearImageFormat::Nv12 {uv_offset:16384,uv_size:8192};i.visible_size=24576;
        let result=validate_with_permissions(&a,&images,|id|if id==source {wire::ACCESS_READ}else{wire::ACCESS_READ|wire::ACCESS_WRITE});
        std::println!("actual mixed NV12 uploads={uploads} words={} accesses={} fixups={} result={:?}",a.words.len(),a.accesses.len(),a.fixups.len(),result.as_ref().map(|v|v.len()));
        result.unwrap();
        // Keep the full canonical allocation in the wire relocation.
        // A prefetch exceeding that allocation must still be rejected.
        let preload = adreno_a6xx_pm4::Packets::new(&a.words)
            .map(|packet| packet.unwrap())
            .find_map(|packet| {
                if !matches!(packet.header, adreno_a6xx_pm4::Header::Type7 {opcode:0x34,..})
                    || packet.payload.len()!=3 || (packet.payload[0]>>18)&15!=12
                    || (packet.payload[0]>>14)&3!=0 {return None;}
                a.fixups.iter().position(|fixup| fixup.word_offset==packet.word_offset+2
                    && fixup.object==ObjectRef::CanonicalShader(adreno_a6xx_shader_pack::ShaderVariant::FsNv12))
            }).expect("canonical NV12 preload");
        assert_eq!(a.fixups[preload].required_size,256);
        let mut oversized=a.clone();
        let control=a.fixups[preload].word_offset as usize-1;
        oversized.words[control]=(oversized.words[control]&((1<<22)-1))|(3<<22);
        assert_eq!(validate_with_permissions(&oversized,&images,|id|if id==source {wire::ACCESS_READ}else{wire::ACCESS_READ|wire::ACCESS_WRITE}),
            Err("qcom-adreno-a618: relocation size does not match PM4 operation"));
    }
}
