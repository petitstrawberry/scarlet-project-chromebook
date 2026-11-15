#![cfg(feature = "programmable")]
mod native_support;
use adreno_a6xx_pm4::{Header, Packets};
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::{self, TextureFormat as F};
use std::sync::Arc;
fn reg(a: &RelocatablePm4, reg: u32) -> Vec<u32> {
    Packets::new(&a.words)
        .filter_map(|p| {
            let p = p.unwrap();
            match p.header {
                Header::Type4 { register, count }
                    if reg >= register && reg < register + u32::from(count) =>
                {
                    Some(p.payload[(reg - register) as usize])
                }
                _ => None,
            }
        })
        .collect()
}
#[test]
fn native_float_draws_preserve_formats_pitches_and_output_registers() {
    for (format, hw, bytes) in [(F::Rgba16Float, 0x62, 8), (F::Rgba32Float, 0x82, 16)] {
        let c = native_color_case(&[format], false);
        let a = c.compile().unwrap();
        assert_eq!(reg(&a, 0x8822), vec![hw]);
        assert_eq!(reg(&a, 0xa996), vec![hw]);
        assert_eq!(reg(&a, 0x8823), vec![16 * bytes / 64]);
        assert_eq!(reg(&a, 0x8824), vec![16 * 16 * bytes / 64]);
        assert_eq!(reg(&a, 0xa98d), vec![1]);
        assert!(
            a.fixups
                .iter()
                .any(|f| f.object == ObjectRef::External(TARGET)
                    && f.required_size == u64::from(256 * bytes)
                    && f.access == Access::WRITE)
        );
    }
}
#[test]
fn shader_free_float_clears_encode_per_component_half_or_full_float() {
    for (format, hw, ifmt, half, values) in [
        (F::Rgba16Float, 0x62, 3, 1, [0xc000, 0x4100, 1, 0x3800]),
        (
            F::Rgba32Float,
            0x82,
            4,
            0,
            [
                (-2.0f32).to_bits(),
                2.5f32.to_bits(),
                0x33800000,
                0.5f32.to_bits(),
            ],
        ),
    ] {
        let mut c = native_color_case(&[format], true);
        c.operations.retain(|o| {
            matches!(
                o,
                Operation::BeginNativeRenderPass(_) | Operation::EndRenderPass
            )
        });
        let a = c.compile().unwrap();
        assert!(
            a.fixups
                .iter()
                .all(|f| !matches!(f.object, ObjectRef::NativeShader(_)))
        );
        for (i, v) in values.iter().enumerate() {
            assert_eq!(reg(&a, 0x8c2c + i as u32), vec![*v]);
        }
        assert_eq!(*reg(&a, 0xacc0).last().unwrap(), hw << 3 | 15 << 12 | half);
        assert_eq!(reg(&a, 0x8c00)[0] >> 24 & 7, ifmt);
    }
}
#[test]
fn mrt_has_independent_formats_blending_masks_and_no_stale_extra_target() {
    let mut c = native_color_case(&[F::Rgba16Float, F::Bgra8Unorm], false);
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            p.descriptor = p
                .descriptor
                .clone()
                .with_color_targets(vec![
                    ir::ColorTargetState::new(
                        F::Rgba16Float,
                        ir::BlendState::REPLACE,
                        ir::ColorWriteMask::from_bits(5).unwrap(),
                    )
                    .unwrap(),
                    ir::ColorTargetState::new(
                        F::Bgra8Unorm,
                        ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                        ir::ColorWriteMask::ALL,
                    )
                    .unwrap(),
                ])
                .unwrap();
        }
    }
    let a = c.compile().unwrap();
    assert_eq!(reg(&a, 0x8822), vec![0x62]);
    assert_eq!(reg(&a, 0x882a), vec![0x30 | 1 << 13]);
    assert_eq!(reg(&a, 0x8820), vec![0x60 | 5 << 7]);
    assert_eq!(reg(&a, 0x8828), vec![0x7e3]);
    assert_eq!(reg(&a, 0x8830), vec![0]);
    assert_eq!(reg(&a, 0x8865), vec![0xffff0102]);
    assert_eq!(reg(&a, 0xa989), vec![0x102]);
    assert_eq!(reg(&a, 0xa98d), vec![2]);
    assert_eq!(reg(&a, 0xa98b), vec![0xff]);
    let shader = shader("mrt", sgfx_shader_ir3::Stage::Fragment, FS);
    for i in 0..2 {
        let o = shader
            .artifact
            .metadata
            .outputs
            .iter()
            .find(|o| o.slot == 4 + i)
            .unwrap();
        assert_eq!(reg(&a, 0xa98e + i), vec![o.regid | u32::from(o.half) << 8]);
    }
}
#[test]
fn native_attachment_count_format_extent_alias_and_output_mismatches_reject() {
    let mut c = native_color_case(&[F::Rgba16Float, F::Rgba32Float], false);
    if let Operation::BeginNativeRenderPass(p) = &mut c.operations[0] {
        p.colors.pop();
    }
    assert!(c.compile().is_err());
    let mut c = native_color_case(&[F::Rgba16Float], false);
    if let ResourceKind::Image(i) = &mut c
        .resources
        .iter_mut()
        .find(|r| r.id == TARGET)
        .unwrap()
        .kind
    {
        i.format = F::Rgba32Float;
    }
    assert!(c.compile().is_err());
    let mut c = native_color_case(&[F::Rgba16Float, F::Rgba32Float], false);
    if let Operation::BeginNativeRenderPass(p) = &mut c.operations[0] {
        p.colors[1].target = TARGET;
    }
    assert!(c.compile().is_err());
    let mut c = native_color_case(&[F::Rgba16Float, F::Rgba32Float], false);
    if let ResourceKind::Image(i) = &mut c
        .resources
        .iter_mut()
        .find(|r| r.id == ObjectId::new(1))
        .unwrap()
        .kind
    {
        i.extent = ir::Extent2D::new(15, 16).unwrap();
    }
    assert!(c.compile().is_err());
    let mut p = render_pipeline("fs");
    Arc::make_mut(&mut p.fragment.artifact).metadata.outputs[0].slot = 5;
    assert!(validate_native_render_pipeline(&p).is_err());
}
#[test]
fn float_upload_is_bit_exact_and_preserves_row_padding() {
    for (format, bpp) in [(F::Rgba16Float, 8), (F::Rgba32Float, 16)] {
        let resources = [native_image(TARGET, format, ir::TextureUsage::COPY_DST)];
        let data: Vec<u8> = (0..(3 * bpp * 2 + 16)).map(|n| n as u8).collect();
        let src_pitch = 3 * bpp + 16;
        let operations = [Operation::WriteTexture {
            destination: TARGET,
            area: ir::PixelRect::new(2, 3, 3, 2).unwrap(),
            bytes_per_row: src_pitch,
            data: &data,
        }];
        let a = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap();
        let expected = [
            &data[..3 * bpp as usize],
            &data[src_pitch as usize..src_pitch as usize + 3 * bpp as usize],
        ]
        .concat();
        assert!(a.generated_objects.iter().any(|o| o.bytes == expected));
        let offsets: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| f.object == ObjectRef::External(TARGET))
            .map(|f| (f.object_offset, f.required_size))
            .collect();
        assert_eq!(
            offsets,
            vec![
                (u64::from(3 * 16 * bpp + 2 * bpp), u64::from(3 * bpp)),
                (u64::from(4 * 16 * bpp + 2 * bpp), u64::from(3 * bpp))
            ]
        );
    }
}
#[test]
fn float_copy_preserves_nan_payloads_and_padding_with_bounded_ordered_chunks() {
    for (format, bpp) in [(F::Rgba16Float, 8), (F::Rgba32Float, 16)] {
        let c = native_float_copy_case(format);
        let a = c.compile().unwrap();
        let stride = 96 * bpp;
        let pattern = [0x7fc01234u32, 0xff800000, 0x80000000, 0x7e01fc00];
        let source: Vec<_> = (0..stride * 16 / 4)
            .flat_map(|i| pattern[i % pattern.len()].to_le_bytes())
            .collect();
        let mut destination = vec![0xa5; source.len()];
        let mut expected = destination.clone();
        for y in 0..2 {
            let src = (3 + y) * stride + 2 * bpp;
            let dst = (6 + y) * stride + 5 * bpp;
            expected[dst..dst + 73 * bpp].copy_from_slice(&source[src..src + 73 * bpp]);
        }
        let refs: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| {
                f.object == ObjectRef::External(SOURCE) || f.object == ObjectRef::External(TARGET)
            })
            .collect();
        for pair in refs.chunks_exact(2) {
            let (src, dst) = (pair[0], pair[1]);
            assert_eq!(src.object, ObjectRef::External(SOURCE));
            assert_eq!(src.access, Access::READ);
            assert_eq!(dst.object, ObjectRef::External(TARGET));
            assert_eq!(dst.access, Access::WRITE);
            assert_eq!(src.required_size, dst.required_size);
            assert!(src.required_size <= 512);
            let (s, d, n) = (
                src.object_offset as usize,
                dst.object_offset as usize,
                src.required_size as usize,
            );
            destination[d..d + n].copy_from_slice(&source[s..s + n]);
        }
        assert_eq!(destination, expected);
        let ps: Vec<_> = Packets::new(&a.words).map(|p| p.unwrap()).collect();
        let mut count = 0;
        for (i, p) in ps.iter().enumerate() {
            if matches!(p.header, Header::Type7 { opcode: 0x75, .. }) {
                count += 1;
                assert!((1..=128).contains(&p.payload[0]));
                assert!(matches!(
                    ps[i + 1].header,
                    Header::Type7 {
                        opcode: adreno_a6xx_pm4::opcode::WAIT_MEM_WRITES,
                        ..
                    }
                ));
            }
        }
        assert_eq!(count, 2 * (73 * bpp).div_ceil(512));
        assert!(
            reg(&a, 0x8c00).is_empty(),
            "raw copies must not convert through A2D"
        );
    }
}
#[test]
fn float_copy_rejects_alias_format_usage_extent_and_bounds_errors() {
    for bad in 0..6 {
        let mut c = native_float_copy_case(F::Rgba16Float);
        match bad {
            0 => {
                if let Operation::CopyTextureToTexture { destination, .. } = &mut c.operations[0] {
                    *destination = SOURCE;
                }
            }
            1 => c.resources[1] = native_image(TARGET, F::Rgba32Float, ir::TextureUsage::COPY_DST),
            2 => {
                if let ResourceKind::Image(i) = &mut c.resources[0].kind {
                    i.usage = ir::TextureUsage::SAMPLED;
                }
            }
            3 => {
                if let Operation::CopyTextureToTexture {
                    destination_rect, ..
                } = &mut c.operations[0]
                {
                    *destination_rect = ir::PixelRect::new(0, 0, 72, 2).unwrap();
                }
            }
            4 => {
                if let Operation::CopyTextureToTexture { source_rect, .. } = &mut c.operations[0] {
                    *source_rect = ir::PixelRect::new(24, 3, 73, 2).unwrap();
                }
            }
            5 => {
                if let ResourceKind::Image(i) = &mut c.resources[1].kind {
                    i.planes[0].stride = 64;
                }
            }
            _ => unreachable!(),
        }
        assert!(c.compile().is_err(), "bad case {bad}");
    }
}
#[test]
fn compiled_workgroup_uses_physical_shared_size_not_argument_hint() {
    let c = wide_shared_case();
    let a = c.compile().unwrap();
    let p = compute_pipeline("wide_shared");
    assert_eq!(p.shader.artifact.metadata.shared_size, 4096);
    assert_eq!(
        p.shader
            .artifact
            .metadata
            .compute
            .as_ref()
            .unwrap()
            .req_local_mem,
        0
    );
    assert_eq!(reg(&a, 0xa9b1)[0] & 31, 3);
    let mut p = p;
    Arc::make_mut(&mut p.shader.artifact).metadata.shared_size = 32769;
    assert!(validate_native_compute_pipeline(&p).is_err());
}
#[test]
fn float_sampling_uses_native_texel_format_and_byte_aware_mip_layout() {
    for (format, bpp, hw) in [(F::Rgba16Float, 8, 0x62), (F::Rgba32Float, 16, 0x82)] {
        let mut c = render_case("horizontal");
        let r = c.resources.iter_mut().find(|r| r.id == SOURCE).unwrap();
        *r = native_image(SOURCE, format, ir::TextureUsage::SAMPLED);
        let texture =
            adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(16, 16, 3, 1, bpp)
                .unwrap();
        r.size = texture.total_size;
        if let ResourceKind::Image(i) = &mut r.kind {
            i.texture = Some(texture);
            i.planes[0].stride = texture.row_pitch;
            i.planes[0].size = texture.total_size;
        }
        let a = c.compile().unwrap();
        let f = a
            .fixups
            .iter()
            .find(|f| {
                f.object == ObjectRef::External(SOURCE)
                    && f.encoding == AddressEncoding::GpuVa49TexDescriptor
            })
            .unwrap();
        let word = f.word_offset as usize;
        let descriptor = &a.words[word - 4..word];
        assert_eq!((descriptor[0] >> 22) & 255, hw);
        assert_eq!(descriptor[0] >> 30, 0);
        assert_eq!((descriptor[0] >> 16) & 15, 2);
        assert_eq!(descriptor[2] >> 7 & 0x3fffff, texture.row_pitch);
        assert_eq!(f.required_size, texture.total_size);
    }
}
#[test]
fn shrinking_mrt_pass_resets_counts_and_unused_write_enables() {
    let mut first = native_color_case(&[F::Rgba16Float, F::Rgba32Float], false);
    let mut second = native_color_case(&[F::Rgba16Float], false);
    let other_fs = ObjectId::new(50);
    for op in &mut second.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            Arc::make_mut(p).fragment.object = other_fs;
        }
    }
    let mut resource = second
        .resources
        .iter()
        .find(|r| r.id == FS)
        .unwrap()
        .clone();
    resource.id = other_fs;
    first.resources.push(resource);
    first.operations.extend(second.operations);
    let a = first.compile().unwrap();
    assert_eq!(reg(&a, 0xa98d), vec![2, 1]);
    assert_eq!(reg(&a, 0xa98b), vec![0xff, 0xf]);
    assert_eq!(reg(&a, 0x8828), vec![0x7e0, 0]);
}
#[test]
fn half_clear_saturates_finite_overflow_and_preserves_signed_zero() {
    let mut c = native_color_case(&[F::Rgba16Float], true);
    if let Operation::BeginNativeRenderPass(p) = &mut c.operations[0] {
        p.colors[0].load = ir::LoadOp::Clear(
            ir::Color::rgba(f32::MAX, -f32::MAX, -0.0, f32::from_bits(1)).unwrap(),
        );
    }
    c.operations.retain(|o| {
        matches!(
            o,
            Operation::BeginNativeRenderPass(_) | Operation::EndRenderPass
        )
    });
    let a = c.compile().unwrap();
    for (i, v) in [0x7bff, 0xfbff, 0x8000, 0].into_iter().enumerate() {
        assert_eq!(reg(&a, 0x8c2c + i as u32), vec![v]);
    }
}
#[test]
fn unimplemented_vertex_layer_or_clip_outputs_fail_before_admission() {
    for slot in [9, 10, 12, 13] {
        let mut p = render_pipeline("fs");
        let a = Arc::make_mut(&mut p.vertex.artifact);
        let mut output = a.metadata.outputs[0].clone();
        output.slot = slot;
        a.metadata.outputs.push(output);
        assert!(validate_native_render_pipeline(&p).is_err());
    }
}

fn normalized_case(formats: &[F]) -> Case {
    let mut case = native_color_case(&vec![F::Bgra8Unorm; formats.len()], false);
    for (slot, format) in formats.iter().enumerate() {
        let ResourceKind::Image(image) = &mut case
            .resources
            .iter_mut()
            .find(|r| r.id == ObjectId::new(slot as u32))
            .unwrap()
            .kind
        else {
            panic!()
        };
        image.format = *format;
    }
    for op in &mut case.operations {
        if let Operation::BeginNativeRenderPass(pass) = op {
            for a in &mut pass.colors {
                a.load = ir::LoadOp::Clear(ir::Color::rgba(0.25, 0.5, 0.75, 0.5).unwrap());
            }
        }
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            p.descriptor = p
                .descriptor
                .clone()
                .with_color_targets(
                    formats
                        .iter()
                        .map(|f| {
                            ir::ColorTargetState::new(
                                *f,
                                ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                                ir::ColorWriteMask::ALL,
                            )
                            .unwrap()
                        })
                        .collect(),
                )
                .unwrap();
        }
    }
    case
}

#[test]
fn normalized_targets_encode_clear_and_select_srgb_and_red_storage() {
    for format in [F::Bgra8UnormSrgb, F::Rgba8UnormSrgb, F::R8Unorm] {
        let case = normalized_case(&[format]);
        let a = case.compile().unwrap();
        let red = format == F::R8Unorm;
        let srgb = u32::from(!red);
        assert_eq!(reg(&a, 0x880f), vec![srgb]);
        assert_eq!(reg(&a, 0xa98a), vec![srgb]);
        assert_eq!(
            reg(&a, 0x8822),
            vec![0x30 | if red { 3 << 13 } else { 1 << 13 }]
        );
        assert_eq!(reg(&a, 0x8820), vec![if red { 0xe3 } else { 0x7e3 }]);
        let expected = if red {
            [64, 0, 0, 255]
        } else {
            [137, 188, 225, 128]
        };
        for (i, v) in expected.into_iter().enumerate() {
            assert_eq!(reg(&a, 0x8c2c + i as u32), vec![v]);
        }
        assert_eq!(
            reg(&a, 0x8c17),
            vec![0x30 | if red { 3 << 10 } else { 1 << 10 }]
        );
    }
}

#[test]
fn normalized_mrt_masks_and_r8_destination_alpha_are_independent() {
    let mut c = normalized_case(&[F::Rgba8UnormSrgb, F::R8Unorm]);
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            let blend = ir::BlendState::new(
                ir::BlendComponent::new(
                    ir::BlendFactor::DestinationAlpha,
                    ir::BlendFactor::OneMinusDestinationAlpha,
                    ir::BlendOp::Add,
                ),
                ir::BlendComponent::new(
                    ir::BlendFactor::One,
                    ir::BlendFactor::Zero,
                    ir::BlendOp::Add,
                ),
            );
            p.descriptor = p
                .descriptor
                .clone()
                .with_color_targets(vec![
                    ir::ColorTargetState::new(
                        F::Rgba8UnormSrgb,
                        ir::BlendState::REPLACE,
                        ir::ColorWriteMask::ALL,
                    )
                    .unwrap(),
                    ir::ColorTargetState::new(
                        F::R8Unorm,
                        blend,
                        ir::ColorWriteMask::from_bits(1).unwrap(),
                    )
                    .unwrap(),
                ])
                .unwrap();
        }
    }
    let a = c.compile().unwrap();
    assert_eq!(reg(&a, 0x880f), vec![1]);
    assert_eq!(reg(&a, 0xa98a), vec![1]);
    assert_eq!(reg(&a, 0x8829), vec![0x00010001]);
    assert_eq!(reg(&a, 0x8828), vec![0xe3]);
    // A mask containing only non-existent G/B/A must not write physical R.
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            p.descriptor = p
                .descriptor
                .clone()
                .with_color_targets(vec![
                    ir::ColorTargetState::new(
                        F::Rgba8UnormSrgb,
                        ir::BlendState::REPLACE,
                        ir::ColorWriteMask::ALL,
                    )
                    .unwrap(),
                    ir::ColorTargetState::new(
                        F::R8Unorm,
                        ir::BlendState::REPLACE,
                        ir::ColorWriteMask::from_bits(14).unwrap(),
                    )
                    .unwrap(),
                ])
                .unwrap();
        }
    }
    assert_eq!(reg(&c.compile().unwrap(), 0x8828), vec![0x60]);
}

#[test]
fn normalized_pass_switch_clears_srgb_state_before_r8_draw() {
    let mut first = normalized_case(&[F::Rgba8UnormSrgb]);
    let mut second = normalized_case(&[F::R8Unorm]);
    let id = ObjectId::new(99);
    let mut target = second.resources.iter().find(|r| r.id == TARGET).unwrap().clone();
    target.id = id;
    first.resources.push(target);
    for op in &mut second.operations {
        if let Operation::BeginNativeRenderPass(pass) = op {pass.colors[0].target = id;}
    }
    first.operations.extend(second.operations);
    let a = first.compile().unwrap();
    assert_eq!(reg(&a, 0x880f), vec![1, 0]);
    assert_eq!(reg(&a, 0xa98a), vec![1, 0]);
    assert_eq!(reg(&a, 0x8822), vec![0x2030, 0x6030]);
}

#[test]
fn rg8_draw_masks_missing_channels_and_uses_logical_destination_alpha_one() {
    let a = native_rg8_case().compile().unwrap();
    assert_eq!(reg(&a,0x8820).last().unwrap() >> 7 & 15,3);
    let blend = *reg(&a,0x8821).last().unwrap();
    assert_eq!(blend & 31,1);
    assert_eq!(blend >> 8 & 31,0);
    assert_eq!(reg(&a,0x8c2c),[0]);
    assert_eq!(reg(&a,0x8c2d),[255]);
    assert_eq!(reg(&a,0x8c2e),[0]);
    assert_eq!(reg(&a,0x8c2f),[255]);
}
