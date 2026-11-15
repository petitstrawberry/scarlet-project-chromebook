#![cfg(feature = "programmable")]
mod native_support;
use adreno_a6xx_pm4::{Header, Packets};
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::{self, TextureFormat as F};

fn reg(a: &RelocatablePm4, r: u32) -> Vec<u32> {
    Packets::new(&a.words)
        .filter_map(|p| {
            let p = p.unwrap();
            match p.header {
                Header::Type4 { register, count }
                    if r >= register && r < register + u32::from(count) =>
                {
                    Some(p.payload[(r - register) as usize])
                }
                _ => None,
            }
        })
        .collect()
}
#[test]
fn filtered_float_blits_preserve_format_precision_rotation_and_byte_pitch() {
    for (f, fmt, ifmt, bpp, half) in [
        (F::Rgba16Float, 0x62, 3, 8, 1),
        (F::Rgba32Float, 0x82, 4, 16, 0),
    ] {
        for filter in [ir::FilterMode::Nearest, ir::FilterMode::Linear] {
            for (flips, rotate) in [
                ([false, false], 0),
                ([true, false], 4),
                ([false, true], 5),
                ([true, true], 2),
            ] {
                let a = native_float_blit_case(f, filter, flips).compile().unwrap();
                assert_eq!(
                    *reg(&a, 0x8c00).last().unwrap() & 0x0700ff87,
                    ifmt << 24 | fmt << 8 | rotate
                );
                assert_eq!(*reg(&a, 0xacc0).last().unwrap(), fmt << 3 | 15 << 12 | half);
                assert_eq!(
                    reg(&a, 0xb4c0),
                    vec![
                        fmt | 1 << 20
                            | 1 << 22
                            | if filter == ir::FilterMode::Linear {
                                1 << 16
                            } else {
                                0
                            }
                    ]
                );
                assert_eq!(reg(&a, 0xb4c4), vec![(16 * bpp / 64) << 9]);
                for (id, access) in [(SOURCE, Access::READ), (TARGET, Access::WRITE)] {
                    assert!(a.fixups.iter().any(|p| p.object == ObjectRef::External(id)
                        && p.required_size == u64::from(256 * bpp)
                        && p.access == access));
                }
            }
        }
    }
}
#[test]
fn depth_upload_packs_linear_staging_and_targets_only_tiled_storage() {
    let a = native_depth_upload_case().compile().unwrap();
    let upload = a
        .generated_objects
        .iter()
        .find(|o| o.kind == GeneratedObjectKind::Upload)
        .unwrap();
    assert_eq!(upload.bytes.len(), 128);
    assert_eq!(upload.alignment, 64);
    assert_eq!(&upload.bytes[4..12], &[0, 0, 0, 63, 0, 0, 128, 63]);
    assert_eq!(
        &upload.bytes[64..76],
        &[0, 0, 128, 63, 0, 0, 0, 63, 0, 0, 0, 0]
    );
    assert!(
        upload.bytes[12..64]
            .iter()
            .chain(upload.bytes[76..].iter())
            .all(|b| *b == 0)
    );
    assert_eq!(reg(&a, 0xb4c0), vec![0x4a | 1 << 20 | 1 << 22]);
    assert_eq!(reg(&a, 0xb4c1), vec![3 | 2 << 15]);
    assert_eq!(reg(&a, 0xb4c4), vec![1 << 9]);
    assert!(
        a.fixups
            .iter()
            .any(|f| f.object == ObjectRef::Generated(upload.id)
                && f.required_size == 128
                && f.access == Access::READ)
    );
    assert!(
        a.fixups
            .iter()
            .any(|f| f.object == ObjectRef::External(TARGET)
                && f.required_size == 4096
                && f.access == Access::WRITE)
    );
}
#[test]
fn depth_copy_uses_tiled_source_and_no_filter_or_half_conversion() {
    let a = native_depth_copy_case().compile().unwrap();
    assert_eq!(reg(&a, 0xb4c0), vec![0x4a | 3 << 8 | 1 << 20 | 1 << 22]);
    assert_eq!(*reg(&a, 0xacc0).last().unwrap(), 0x4a << 3 | 15 << 12);
    assert_eq!(reg(&a, 0xb4c4), vec![4 << 9]);
    assert_eq!(
        *reg(&a, 0x8c00).last().unwrap() & 0x0700ff87,
        4 << 24 | 0x4a << 8
    );
    assert!(
        a.fixups
            .iter()
            .any(|f| f.object == ObjectRef::External(SOURCE) && f.required_size == 4096)
    );
    let mut c = native_depth_copy_case();
    if let ResourceKind::Image(i) = &mut c.resources[0].kind {
        i.modifier = ImageModifier::Linear;
    }
    assert!(c.compile().is_err());
}
#[test]
fn four_byte_storage_offsets_keep_logical_bounds_and_original_aliases() {
    for offset in [0, 4, 12, 60, 64, 68, 124] {
        let a = native_storage_offset_case(offset).compile().unwrap();
        let refs: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| f.encoding == AddressEncoding::GpuVa49TexDescriptor)
            .collect();
        assert_eq!(refs.len(), 3);
        for f in refs {
            let d = f.word_offset as usize - 4;
            assert_eq!(a.words[d + 1], 516 / 4);
            assert_eq!(
                a.words[d + 2],
                4 << 29 | 1 << 4 | (((offset & 63) / 4) as u32) << 16
            );
            assert_eq!(f.object_offset, offset & !63);
            assert_eq!(f.required_size, 516 + (offset & 63));
            assert!(matches!(f.object, ObjectRef::External(_)));
            // Effective first/last addresses stay exactly inside the binding.
            let start = f.object_offset + u64::from((a.words[d + 2] >> 16) & 63) * 4;
            assert_eq!(start, offset);
            assert_eq!(start + u64::from(a.words[d + 1]) * 4, offset + 516);
        }
    }
    for offset in [1, 2, 3, 63] {
        assert!(native_storage_offset_case(offset).compile().is_err());
    }
    let mut c = native_storage_offset_case(60);
    for r in &mut c.resources {
        if r.id == STORAGE {
            r.size -= 1;
        }
    }
    assert!(c.compile().is_err());
}

#[test]
fn large_readonly_graphics_uses_shared_uav_load_with_exact_buffer_bounds() {
    let a = native_large_readonly_graphics_case().compile().unwrap();
    let packets: Vec<_> = Packets::new(&a.words).map(|p| p.unwrap()).collect();
    let loads: Vec<_> = packets
        .iter()
        .filter(|p| matches!(p.header, Header::Type7 { opcode: 0x36, .. }))
        .collect();
    assert_eq!(loads.len(), 2); // both stage bindings populate the shared table
    for load in loads {
        assert_eq!(load.payload, [2 << 16 | 14 << 18 | 4 << 22, 0, 0]);
        let f = a
            .fixups
            .iter()
            .find(|f| f.word_offset == load.word_offset + 2)
            .unwrap();
        assert_eq!(f.required_size, 4 * 64);
        assert_eq!(f.access, Access::READ);
        assert_eq!(f.encoding, AddressEncoding::GpuVa64);
        assert!(matches!(f.object, ObjectRef::Generated(_)));
    }
    assert!(!packets.iter().any(|p| matches!(
        p.header,
        Header::Type7 {
            opcode: 0x32 | 0x34,
            ..
        }
    ) && (p.payload[0] >> 14) & 3 == 3));
    for binding in 0..4 {
        let refs: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| {
                f.object == ObjectRef::External(ObjectId::new(40 + binding))
                    && f.encoding == AddressEncoding::GpuVa49TexDescriptor
            })
            .collect();
        assert_eq!(refs.len(), 1); // both stages reuse the same immutable table
        for f in refs {
            let d = f.word_offset as usize - 4;
            assert_eq!(a.words[d + 1], 0x10000); // 65536 R32 elements; WIDTH15|HEIGHT15
            assert_eq!(a.words[d + 2], 4 << 29 | 1 << 4 | 1 << 16);
            assert_eq!(f.object_offset, 0);
            assert_eq!(f.required_size, 262148); // includes alignment prefix exactly once
            assert_eq!(f.access, Access::READ);
        }
    }
    let cs = compute_case("storage").compile().unwrap();
    assert!(Packets::new(&cs.words).any(|p| {
        let p = p.unwrap();
        matches!(p.header, Header::Type7 { opcode: 0x34, .. })
            && (p.payload[0] >> 14) & 3 == 3
            && (p.payload[0] >> 18) & 15 == 13
    }));
    assert!(
        !Packets::new(&cs.words)
            .any(|p| matches!(p.unwrap().header, Header::Type7 { opcode: 0x36, .. }))
    );
}
#[test]
fn graphics_readonly_texture_lowering_preserves_aligned_storage_address() {
    for lowered in [false, true] {
        for offset in [0, 4, 12, 60, 64, 68, 124] {
            let a = native_graphics_storage_offset_case(offset, lowered)
                .compile()
                .unwrap();
            let refs: Vec<_> = a
                .fixups
                .iter()
                .filter(|f| {
                    f.object == ObjectRef::External(STORAGE)
                        && f.encoding == AddressEncoding::GpuVa49TexDescriptor
                })
                .collect();
            assert_eq!(refs.len(), if lowered { 2 } else { 1 }); // shared UAV and optional texture table
            for f in refs {
                let d = f.word_offset as usize - 4;
                assert_eq!(a.words[d + 1], 129);
                let prefix = u64::from((a.words[d + 2] >> 16) & 63) * 4;
                assert_eq!(prefix, offset & 63);
                assert_eq!(
                    f.object_offset,
                    offset & !63,
                    "offset {offset}, lowered {lowered}"
                );
                assert_eq!(f.required_size, 516 + prefix);
                assert_eq!(f.access, Access::READ);
                assert_eq!(f.object_offset + prefix, offset);
                assert_eq!(f.object_offset + f.required_size, offset + 516);
            }
        }
    }
}

fn sampled_words(a: &RelocatablePm4) -> (&SymbolicAddress, &[u32]) {
    let f = a
        .fixups
        .iter()
        .find(|f| {
            f.object == ObjectRef::External(SOURCE)
                && f.encoding == AddressEncoding::GpuVa49TexDescriptor
        })
        .unwrap();
    (
        f,
        &a.words[f.word_offset as usize - 4..f.word_offset as usize + 12],
    )
}
#[test]
fn real_array_and_cube_shaders_use_selected_mips_layers_and_physical_pitch() {
    let l =
        adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(16, 16, 3, 8, 16).unwrap();
    let sub = l.subresource(1, 1).unwrap();
    for (name, kind, depth) in [("array_sample", 1, 3), ("cube_sample", 2, 1)] {
        let a = native_sampled_case(name).compile().unwrap();
        let (f, w) = sampled_words(&a);
        assert_eq!(f.object_offset, sub.offset);
        assert_eq!(f.required_size, l.total_size - sub.offset);
        assert_eq!((w[0] >> 22) & 255, 0x82);
        assert_eq!((w[0] >> 16) & 15, 1);
        assert_eq!(w[1], 8 | 8 << 15);
        assert_eq!(w[2] >> 29, kind);
        assert_eq!((w[2] >> 7) & 0x3fffff, sub.row_pitch);
        assert_eq!(w[3], l.layer_stride >> 12);
        assert_eq!(w[5] >> 17, depth);
    }
}
#[test]
fn real_depth_shader_has_tiled_float_descriptor_and_comparison_sampler() {
    let a = native_sampled_case("depth_sample").compile().unwrap();
    let (f, w) = sampled_words(&a);
    assert_eq!(f.object_offset, 0);
    assert_eq!(f.required_size, 4096);
    assert_eq!((w[0] >> 22) & 255, 0x4a);
    assert_eq!(w[0] & 3, 3);
    assert_eq!(w[0] & 0xfff0, 5 << 13); // R,R,R,1
    assert_eq!(w[2], 1 << 29 | 256 << 7 | 2);
    let p = shader("depth_sample", sgfx_shader_ir3::Stage::Fragment, FS);
    let slot = p
        .artifact
        .bindings
        .iter()
        .find(|b| b.kind == sgfx_shader_ir3::ResourceKind::Sampler)
        .unwrap()
        .slot as usize;
    let size = p.artifact.metadata.num_samp as usize * 16;
    let table = a
        .generated_objects
        .iter()
        .find(|o| {
            o.kind == GeneratedObjectKind::Upload && o.alignment == 16 && o.bytes.len() == size
        })
        .unwrap();
    let compare = u32::from_le_bytes(
        table.bytes[slot * 16 + 4..slot * 16 + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!((compare >> 1) & 7, 3); // LessEqual
}
#[test]
fn srgb_view_and_native_narrow_formats_preserve_logical_sampling() {
    for (logical, view_format, swizzle, srgb) in [
        (F::Bgra8Unorm, F::Bgra8UnormSrgb, 0x6880, 4),
        (F::Bgra8UnormSrgb, F::Bgra8Unorm, 0x6880, 0),
        (
            F::R8Unorm,
            F::R8Unorm,
            3 << 4 | 4 << 7 | 4 << 10 | 5 << 13,
            0,
        ),
        (F::Rg8Unorm, F::Rg8Unorm, 1 << 7 | 4 << 10 | 5 << 13, 0),
    ] {
        let mut c = render_case("horizontal");
        let r = c.resources.iter_mut().find(|r| r.id == SOURCE).unwrap();
        if let ResourceKind::Image(i) = &mut r.kind {
            i.format = logical;
        }
        let desc = ir::TextureDesc::new(
            logical,
            ir::Extent2D::new(16, 16).unwrap(),
            ir::TextureUsage::SAMPLED,
        )
        .unwrap();
        let view =
            ir::TextureViewDesc::new(desc, view_format, ir::TextureViewDimension::D2, 0, 1, 0, 1)
                .unwrap();
        for op in &mut c.operations {
            if let Operation::SetBindGroup { bindings, .. } = op {
                for b in bindings {
                    if let NativeBindingResource::Texture { view: v, .. } = &mut b.resource {
                        *v = Some(view);
                    }
                }
            }
        }
        let a = c.compile().unwrap();
        let (_, w) = sampled_words(&a);
        assert_eq!(w[0] & 0xfff0, swizzle);
        assert_eq!(w[0] & 4, srgb);
    }
}
#[test]
fn sampled_view_rejects_forged_allocation_range_or_format_and_depth_feedback() {
    for bad in 0..3 {
        let mut c = native_sampled_case("array_sample");
        let r = c.resources.iter_mut().find(|r| r.id == SOURCE).unwrap();
        match bad {
            0 => {
                if let ResourceKind::Image(i) = &mut r.kind {
                    i.texture.as_mut().unwrap().array_layers = 2;
                }
            }
            1 => {
                if let ResourceKind::Image(i) = &mut r.kind {
                    i.format = F::Rgba16Float;
                }
            }
            2 => r.size -= 1,
            _ => unreachable!(),
        }
        assert!(c.compile().is_err());
    }
    let mut c = native_sampled_case("depth_sample");
    for r in &mut c.resources {
        if r.id == SOURCE {
            if let ResourceKind::Image(i) = &mut r.kind {
                i.usage = i.usage | ir::TextureUsage::RENDER_ATTACHMENT;
            }
        }
    }
    for op in &mut c.operations {
        if let Operation::BeginRenderPass(p) = op {
            p.depth = Some(DepthAttachment {
                target: SOURCE,
                load: ir::DepthLoadOp::Load,
                store: ir::StoreOp::Store,
            });
        }
    }
    assert_eq!(
        c.compile().unwrap_err(),
        CompileError::UnsupportedFeatureReason("native depth attachment sampling feedback")
    );
}
