use adreno_a6xx_pm4::{Header, Packets, opcode};
use adreno_a6xx_shader_pack::ShaderVariant;
use sgfx_codegen_adreno_a6xx::{
    Access, Capabilities, CompileError, CompileInput, DepthAttachment, GeneratedObjectKind,
    ImageMeta, ImageModifier, ObjectId, ObjectRef, Operation, PipelineId, PipelineMeta,
    PlaneLayout, RenderPass, ResourceKind, ResourceMeta, compile,
};
use sgfx_core::ir::{
    AddressMode, BlendState, BufferUsage, Color, CompareFunction, CullMode, DepthLoadOp,
    DepthState, DrawUniforms, Extent2D, FilterMode, FragmentProgram, FrontFace, IndexFormat,
    LoadOp, PrimitiveTopology, RasterState, RenderPipelineDesc, SamplerDesc, StoreOp,
    TextureFormat, TextureSampleMode, TextureUsage, Transform, VertexAttribute, VertexBufferLayout,
    VertexFormat,
};

const TARGET: ObjectId = ObjectId::new(0);
const SOURCE: ObjectId = ObjectId::new(1);
const VERTICES: ObjectId = ObjectId::new(2);
const ALPHA: ObjectId = ObjectId::new(3);
const INDICES: ObjectId = ObjectId::new(4);
const DEPTH: ObjectId = ObjectId::new(5);
const PIPELINE: PipelineId = PipelineId::new(0);

fn rect(x: u32, y: u32, width: u32, height: u32) -> sgfx_core::ir::PixelRect {
    sgfx_core::ir::PixelRect::new(x, y, width, height).unwrap()
}

fn image(id: ObjectId, usage: TextureUsage) -> ResourceMeta {
    ResourceMeta {
        id,
        size: 16 * 64,
        kind: ResourceKind::Image(ImageMeta {
            format: TextureFormat::Bgra8Unorm,
            storage_format: TextureFormat::Bgra8Unorm,
            extent: Extent2D::new(16, 16).unwrap(),
            usage,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 64,
                size: 16 * 64,
            }],
        }),
    }
}

fn resources() -> Vec<ResourceMeta> {
    vec![
        image(
            TARGET,
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_DST | TextureUsage::COPY_SRC,
        ),
        image(SOURCE, TextureUsage::COPY_SRC | TextureUsage::SAMPLED),
        ResourceMeta {
            id: VERTICES,
            size: 120,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX,
            },
        },
        ResourceMeta {
            id: ALPHA,
            size: 16 * 64,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::R8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(16, 16).unwrap(),
                usage: TextureUsage::SAMPLED,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 64,
                    size: 16 * 64,
                }],
            }),
        },
    ]
}

fn pipeline() -> PipelineMeta {
    let layout = VertexBufferLayout::new(
        40,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x4, 16),
        ],
    )
    .unwrap();
    let descriptor = RenderPipelineDesc::new(
        TextureFormat::Bgra8Unorm,
        PrimitiveTopology::TriangleList,
        layout,
        FragmentProgram::VertexColor,
        BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
        RasterState::new(CullMode::Back, FrontFace::CounterClockwise),
    )
    .unwrap();
    PipelineMeta {
        id: PIPELINE,
        descriptor,
    }
}

fn showcase_color_pipeline() -> PipelineMeta {
    let layout = VertexBufferLayout::new(
        32,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x4, 16),
        ],
    )
    .unwrap();
    PipelineMeta {
        id: PIPELINE,
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            layout,
            FragmentProgram::VertexColor,
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
    }
}

fn assert_well_formed(artifact: &sgfx_codegen_adreno_a6xx::RelocatablePm4) {
    assert!(!artifact.words.is_empty());
    let packets: Result<Vec<_>, _> = Packets::new(&artifact.words).collect();
    assert!(!packets.unwrap().is_empty());
    let mut previous_end = None;
    for fixup in &artifact.fixups {
        let start = fixup.word_offset as usize;
        let end = start + fixup.encoding.word_count() as usize;
        assert!(previous_end.is_none_or(|offset| offset <= start));
        match fixup.encoding {
            sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa64 => {
                assert!(artifact.words[start..end].iter().all(|word| *word == 0));
            }
            sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49TexDescriptor => {
                assert_eq!(artifact.words[start], 0);
                assert_eq!(artifact.words[start + 1] & 0x1ffff, 0);
            }
        }
        previous_end = Some(end);
    }
}

fn assert_ccu_clean_before_every_color_invalidate(
    artifact: &sgfx_codegen_adreno_a6xx::RelocatablePm4,
) {
    const EVENT_WRITE_4: u32 = 0x7046_0004;
    const CCU_CLEAN_COLOR: u32 = 0x1d;
    const CCU_CLEAN_DEPTH: u32 = 0x1c;
    const EVENT_WRITE_TIMESTAMP: u32 = 1 << 30;
    const CCU_INVALIDATE_COLOR: u32 = 0x19;
    const CCU_INVALIDATE_DEPTH: u32 = 0x18;

    let sequence_objects = artifact
        .generated_objects
        .iter()
        .filter(|object| object.kind == GeneratedObjectKind::CcuSequence)
        .collect::<Vec<_>>();
    assert_eq!(sequence_objects.len(), 1);
    let sequence = sequence_objects[0];
    assert_eq!(sequence.alignment, 64);
    assert_eq!(sequence.bytes, [0; 4]);
    assert_eq!(sequence.access, Access::WRITE);

    let sequence_positions = artifact
        .words
        .windows(5)
        .enumerate()
        .filter_map(|(position, words)| {
            (words[0] == EVENT_WRITE_4
                && matches!(
                    words[1],
                    value if value == CCU_CLEAN_COLOR | EVENT_WRITE_TIMESTAMP
                        || value == CCU_CLEAN_DEPTH | EVENT_WRITE_TIMESTAMP
                )
                && words[2] == 0
                && words[3] == 0
                && words[4] != 0)
                .then_some(position)
        })
        .collect::<Vec<_>>();
    assert!(!sequence_positions.is_empty());

    for position in sequence_positions {
        let fixup = artifact
            .fixups
            .iter()
            .find(|fixup| fixup.word_offset as usize == position + 2)
            .expect("addressed CCU clean has a sequence relocation");
        assert_eq!(fixup.object, ObjectRef::Generated(sequence.id));
        assert_eq!(fixup.required_size, 4);
        assert_eq!(fixup.access, Access::WRITE);
    }

    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (index, packet) in packets.iter().enumerate() {
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: 0x46,
                count: 1
            }
        ) {
            continue;
        }
        if packet.payload == [CCU_INVALIDATE_COLOR] {
            let immediately_clean = packets.get(index.wrapping_sub(1)).is_some_and(|previous| {
                previous.payload.first() == Some(&(CCU_CLEAN_COLOR | EVENT_WRITE_TIMESTAMP))
            });
            let clean_before_depth = index >= 2
                && packets[index - 2].payload.first()
                    == Some(&(CCU_CLEAN_COLOR | EVENT_WRITE_TIMESTAMP))
                && packets[index - 1].payload.first()
                    == Some(&(CCU_CLEAN_DEPTH | EVENT_WRITE_TIMESTAMP));
            assert!(immediately_clean || clean_before_depth);
        } else if packet.payload == [CCU_INVALIDATE_DEPTH] {
            assert!(index >= 2);
            assert_eq!(packets[index - 2].payload.len(), 4);
            assert_eq!(
                packets[index - 2].payload.first(),
                Some(&(CCU_CLEAN_DEPTH | EVENT_WRITE_TIMESTAMP))
            );
            assert_eq!(packets[index - 1].payload, [CCU_INVALIDATE_COLOR]);
        }
    }
}

#[test]
fn clear_is_a_golden_address_free_stream() {
    let resources = resources();
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, 16, 16),
            load: LoadOp::Clear(Color::rgba(1.0, 0.5, 0.0, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::EndRenderPass,
    ];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &[],
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(artifact.fixups.len(), 4);
    assert_eq!(artifact.fixups[1].object, ObjectRef::External(TARGET));
    assert_eq!(artifact.fixups[1].access, Access::WRITE);
    assert_eq!(
        artifact.words.as_slice(),
        &[
            0x7046_0004,
            0x4000_001d,
            0,
            0,
            1,
            0x7046_0001,
            0x19,
            0x7046_0001,
            0x31,
            0x7026_8000,
            0x408e_0701,
            0x0800_0000,
            0x408e_0401,
            0x0410_0000,
            0x70e5_0001,
            0x0c,
            0x7026_8000,
            0x4884_0502,
            0,
            0x000f_000f,
            0x4884_0a02,
            0,
            0x000f_000f,
            0x488c_2c04,
            0xff,
            0x80,
            0,
            0xff,
            0x408c_0001,
            0x00f1_3080,
            0x4884_0001,
            0x00f1_3080,
            0x48ac_c001,
            0x0000_f180,
            0x488c_0101,
            0,
            0x408c_1701,
            0x430,
            0x408c_1802,
            0,
            0,
            0x488c_1a01,
            1,
            0x7026_8000,
            0x702c_0001,
            3,
            0x7026_8000,
            0x7046_0004,
            0x4000_001d,
            0,
            0,
            2,
            0x7046_0004,
            0x4000_001c,
            0,
            0,
            3,
            0x7046_0001,
            0x19,
            0x7046_0001,
            0x18,
            0x7026_8000,
        ]
    );
}

#[test]
fn depth32_clear_test_and_write_are_a_relocatable_sysmem_stream() {
    let mut resources = resources();
    resources.push(ResourceMeta {
        id: DEPTH,
        size: 4096,
        kind: ResourceKind::Image(ImageMeta {
            format: TextureFormat::Depth32Float,
            storage_format: TextureFormat::Depth32Float,
            extent: Extent2D::new(16, 16).unwrap(),
            usage: TextureUsage::RENDER_ATTACHMENT,
            modifier: ImageModifier::A6xxTile6_3Depth,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 256,
                size: 4096,
            }],
        }),
    });
    let mut pipeline = pipeline();
    pipeline.descriptor = pipeline
        .descriptor
        .with_depth_stencil(DepthState::new(
            TextureFormat::Depth32Float,
            CompareFunction::Less,
            true,
        ))
        .unwrap();
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, 16, 16),
            load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: Some(DepthAttachment {
                target: DEPTH,
                load: DepthLoadOp::Clear(1.0),
                store: StoreOp::Store,
            }),
        }),
        Operation::SetPipeline(PIPELINE),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &[pipeline],
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8c00,
                count: 1
            }
        ) && packet.payload == [0x04f1_4a80]
    }));
    let depth_layout = packets
        .iter()
        .find(|packet| {
            matches!(
                packet.header,
                Header::Type4 {
                    register: 0x8872,
                    count: 6
                }
            ) && packet.payload[0] == 4
        })
        .unwrap();
    assert_eq!(depth_layout.payload, [4, 4, 64, 0, 0, 0]);
    assert!(artifact.fixups.iter().any(|fixup| {
        fixup.word_offset == depth_layout.word_offset + 4
            && fixup.object == ObjectRef::External(DEPTH)
            && fixup.required_size == 4096
            && fixup.access == Access::READ | Access::WRITE
    }));
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8c17,
                count: 1
            }
        ) && packet.payload == [0x34a]
    }));
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8871,
                count: 1
            }
        ) && packet.payload == [0x47]
    }));
}

#[test]
fn copy_is_a_golden_stream_with_ccu_sequence_and_two_surfaces() {
    let resources = resources();
    let operations = [Operation::CopyTextureToTexture {
        source: SOURCE,
        source_rect: rect(1, 2, 8, 6),
        destination: TARGET,
        destination_rect: rect(3, 4, 8, 6),
    }];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &[],
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(artifact.fixups.len(), 5);
    assert_eq!(artifact.fixups[1].object, ObjectRef::External(SOURCE));
    assert_eq!(artifact.fixups[1].access, Access::READ);
    assert_eq!(artifact.fixups[2].object, ObjectRef::External(TARGET));
    assert_eq!(artifact.fixups[2].access, Access::WRITE);
    assert_eq!(
        artifact.words.as_slice(),
        &[
            0x7046_0004,
            0x4000_001d,
            0,
            0,
            1,
            0x7046_0001,
            0x19,
            0x7046_0001,
            0x31,
            0x7026_8000,
            0x408e_0701,
            0x0800_0000,
            0x408e_0401,
            0x0410_0000,
            0x70e5_0001,
            0x0c,
            0x7026_8000,
            0x4084_0104,
            0x100,
            0x800,
            0x200,
            0x700,
            0x4884_0502,
            0x0004_0003,
            0x0009_000a,
            0x4884_0a02,
            0x0004_0003,
            0x0009_000a,
            0x408c_0001,
            0x00f1_3000,
            0x4884_0001,
            0x00f1_3000,
            0x48ac_c001,
            0x0000_f180,
            0x488c_0101,
            0,
            0x48b4_c001,
            0x0050_0430,
            0x40b4_c101,
            0x0008_0010,
            0x40b4_c202,
            0,
            0,
            0x40b4_c401,
            0x200,
            0x408c_1701,
            0x430,
            0x408c_1802,
            0,
            0,
            0x488c_1a01,
            1,
            0x7026_8000,
            0x702c_0001,
            3,
            0x7026_8000,
            0x7046_0004,
            0x4000_001d,
            0,
            0,
            2,
            0x7046_0004,
            0x4000_001c,
            0,
            0,
            3,
            0x7046_0001,
            0x19,
            0x7046_0001,
            0x18,
            0x7026_8000,
        ]
    );
}

#[test]
fn partial_render_area_keeps_full_target_viewport_and_uses_damage_scissors() {
    let resources = resources();
    let pipelines = [pipeline()];
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(4, 5, 6, 7),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PIPELINE),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        Operation::SetScissor(None),
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &pipelines,
        operations: &operations,
    })
    .unwrap();
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let register = |wanted| {
        packets.iter().find_map(|packet| {
            matches!(
                packet.header,
                Header::Type4 {
                    register,
                    ..
                } if register == wanted
            )
            .then_some(packet.payload)
        })
    };

    assert_eq!(
        register(0x8010),
        Some(
            &[
                8.0_f32.to_bits(),
                8.0_f32.to_bits(),
                8.0_f32.to_bits(),
                (-8.0_f32).to_bits(),
                0.5_f32.to_bits(),
                0.5_f32.to_bits(),
            ][..]
        )
    );
    assert_eq!(register(0x80b0), Some(&[0x0005_0004, 0x000b_0009][..]));
    assert_eq!(register(0x80d0), Some(&[0, 0x000f_000f][..]));
    assert_eq!(register(0x80f0), Some(&[0x0005_0004, 0x000b_0009][..]));
}

#[test]
fn vertex_color_draw_uses_only_canonical_shader_relocations() {
    let resources = resources();
    let pipelines = [pipeline()];
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, 16, 16),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PIPELINE),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &pipelines,
        operations: &operations,
    })
    .unwrap();
    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(artifact.generated_objects.len(), 1);
    assert_eq!(artifact.fixups.len(), 9);
    let canonical: Vec<_> = artifact
        .fixups
        .iter()
        .filter_map(|fixup| match fixup.object {
            ObjectRef::CanonicalShader(variant) => Some(variant),
            _ => None,
        })
        .collect();
    assert_eq!(canonical.len(), 4);
    assert_eq!(
        canonical
            .iter()
            .filter(|variant| **variant == ShaderVariant::VsStride40Pos4Color4)
            .count(),
        2
    );
    assert_eq!(
        canonical
            .iter()
            .filter(|variant| **variant == ShaderVariant::FsVertexColor)
            .count(),
        2
    );
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(packets.iter().any(|packet| {
        matches!(packet.header, Header::Type7 { opcode: 0x65, .. }) && packet.payload == [1]
    }));
    assert!(!packets.iter().any(|packet| {
        matches!(packet.header, Header::Type7 { opcode: 0x65, .. }) && packet.payload == [12]
    }));
    assert!(packets.iter().any(|packet| {
        matches!(packet.header, Header::Type7 { opcode: 0x6d, .. })
            && packet.payload == [2, 0x8801, 0x10]
    }));
    for (opcode, expected) in [(0x1d, 0), (0x23, 1)] {
        assert!(
            packets.iter().any(|packet| {
                matches!(packet.header, Header::Type7 { opcode: actual, .. } if actual == opcode)
                    && packet.payload == [expected]
            }),
            "missing canonical A618 IB2 policy opcode {opcode:#x}"
        );
    }
    let shader_preloads: Vec<_> = packets
        .iter()
        .filter(|packet| {
            matches!(
                packet.header,
                Header::Type7 {
                    opcode: 0x32 | 0x34,
                    ..
                }
            ) && packet.payload.len() == 3
                && (packet.payload[0] >> 14) & 3 == 0
                && (packet.payload[0] >> 16) & 3 == 2
        })
        .collect();
    assert_eq!(shader_preloads.len(), 2);
    assert!(shader_preloads.iter().any(|packet| {
        matches!(packet.header, Header::Type7 { opcode: 0x32, .. })
            && (packet.payload[0] >> 18) & 0xf == 8
    }));
    assert!(shader_preloads.iter().any(|packet| {
        matches!(packet.header, Header::Type7 { opcode: 0x34, .. })
            && (packet.payload[0] >> 18) & 0xf == 12
    }));
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8822,
                ..
            }
        ) && packet.payload == [0x2030]
    }));
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8823,
                ..
            }
        ) && packet.payload == [1, 16]
    }));
    for (register, expected) in [
        (0x8827, &[0][..]),
        (0x8903, &[0, 0, 0]),
        (0x8102, &[0x30]),
        (0x880f, &[0]),
        (0xa98a, &[0]),
        (0x8820, &[0x7e3, 0x0701_0706]),
        (0x8865, &[0xffff_0001]),
        (0xa989, &[1]),
        (0x8870, &[0]),
        (0x8872, &[0, 0, 0, 0, 0, 0]),
        (0x8094, &[0]),
        (0xb300, &[0, 4]),
        (0x80a2, &[0, 4, 0]),
        (0x8802, &[0, 4, 0]),
        (0x9200, &[0, 0, 0, 0, 0, 0, 0, 0]),
        (0x9208, &[0, 0, 0, 0, 0, 0, 0, 0]),
        (0x9306, &[0]),
        (0x8000, &[0x80]),
        (0x8006, &[0x0007_fdff]),
        (0x8090, &[0x2012]),
        (0x9108, &[3]),
        (0x9b00, &[0]),
    ] {
        assert!(
            packets.iter().any(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: actual,
                        ..
                    } if actual == register
                ) && packet.payload == expected
            }),
            "missing canonical A618 register {register:#x}"
        );
    }
}

#[test]
fn indexed_draws_use_the_bounded_a6xx_dma_packet() {
    for (format, index_bytes, initiator) in [
        (IndexFormat::Uint16, 16_u64, 0x404_u32),
        (IndexFormat::Uint32, 32_u64, 0x804_u32),
    ] {
        let mut resources = resources();
        resources[2].size = 128;
        resources.push(ResourceMeta {
            id: INDICES,
            size: index_bytes,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::INDEX,
            },
        });
        let pipelines = [showcase_color_pipeline()];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: rect(0, 0, 16, 16),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(PIPELINE),
            Operation::SetVertexBuffer {
                buffer: VERTICES,
                offset: 0,
            },
            Operation::SetIndexBuffer {
                buffer: INDICES,
                offset: 0,
                format,
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::DrawIndexed {
                index_count: 6,
                first_index: 2,
                base_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();
        assert_well_formed(&artifact);
        let packet = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: 0x38,
                        count: 7,
                    }
                )
            })
            .unwrap();
        assert_eq!(packet.payload, [initiator, 1, 6, 2, 0, 0, 8]);
        let fixup = artifact
            .fixups
            .iter()
            .find(|fixup| fixup.word_offset == packet.word_offset + 5)
            .unwrap();
        assert_eq!(fixup.object, ObjectRef::External(INDICES));
        assert_eq!(fixup.object_offset, 0);
        assert_eq!(fixup.required_size, index_bytes);
        assert_eq!(fixup.access, Access::READ);
    }
}

#[test]
fn indexed_draw_rejects_unaligned_binding_and_negative_base_vertex() {
    let mut resources = resources();
    resources[2].size = 128;
    resources.push(ResourceMeta {
        id: INDICES,
        size: 16,
        kind: ResourceKind::Buffer {
            usage: BufferUsage::INDEX,
        },
    });
    let pipelines = [showcase_color_pipeline()];
    let make_operations = |offset, base_vertex| {
        vec![
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: rect(0, 0, 16, 16),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(PIPELINE),
            Operation::SetVertexBuffer {
                buffer: VERTICES,
                offset: 0,
            },
            Operation::SetIndexBuffer {
                buffer: INDICES,
                offset,
                format: IndexFormat::Uint16,
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::DrawIndexed {
                index_count: 6,
                first_index: 0,
                base_vertex,
            },
            Operation::EndRenderPass,
        ]
    };
    for (operations, expected) in [
        (make_operations(1, 0), CompileError::OutOfBounds),
        (make_operations(0, -1), CompileError::UnsupportedFeature),
    ] {
        assert_eq!(
            compile(CompileInput {
                capabilities: Capabilities::a618(512 * 1024, 4096),
                resources: &resources,
                pipelines: &pipelines,
                operations: &operations,
            }),
            Err(expected)
        );
    }
}

#[test]
fn aligned_upload_becomes_generated_object_and_symbolic_memcpy() {
    let resources = [ResourceMeta {
        id: VERTICES,
        size: 64,
        kind: ResourceKind::Buffer {
            usage: BufferUsage::COPY_DST,
        },
    }];
    let data = [1, 2, 3, 4, 5, 6, 7, 8];
    let operations = [Operation::WriteBuffer {
        destination: VERTICES,
        offset: 16,
        data: &data,
    }];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &[],
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_eq!(artifact.generated_objects.len(), 1);
    assert_eq!(artifact.generated_objects[0].bytes, data);
    assert_eq!(artifact.fixups.len(), 2);
    assert!(matches!(artifact.fixups[0].object, ObjectRef::Generated(_)));
    assert_eq!(artifact.fixups[1].object, ObjectRef::External(VERTICES));

    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let memcpy = packets
        .iter()
        .position(|packet| matches!(packet.header, Header::Type7 { opcode: 0x75, .. }))
        .unwrap();
    assert!(matches!(
        packets.get(memcpy + 1),
        Some(packet)
            if packet.header
                == (Header::Type7 {
                    opcode: opcode::WAIT_MEM_WRITES,
                    count: 0,
                })
                && packet.payload.is_empty()
    ));
}

#[test]
fn large_upload_is_split_into_bounded_ordered_memcpy_chunks() {
    const UPLOAD_DWORDS: usize = 252;
    let resources = [ResourceMeta {
        id: VERTICES,
        size: (UPLOAD_DWORDS * 4) as u64,
        kind: ResourceKind::Buffer {
            usage: BufferUsage::COPY_DST,
        },
    }];
    let data = [0x5a; UPLOAD_DWORDS * 4];
    let operations = [Operation::WriteBuffer {
        destination: VERTICES,
        offset: 0,
        data: &data,
    }];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &[],
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_eq!(artifact.generated_objects.len(), 1);
    assert_eq!(artifact.fixups.len(), 4);
    assert_eq!(artifact.accesses.len(), 2);
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let memcpy_indices: Vec<_> = packets
        .iter()
        .enumerate()
        .filter_map(|(index, packet)| {
            matches!(packet.header, Header::Type7 { opcode: 0x75, .. }).then_some(index)
        })
        .collect();
    assert_eq!(memcpy_indices.len(), 2);
    assert_eq!(
        memcpy_indices
            .iter()
            .map(|index| packets[*index].payload[0])
            .collect::<Vec<_>>(),
        [128, 124],
    );
    for index in memcpy_indices {
        assert!(matches!(
            packets.get(index + 1),
            Some(packet)
                if packet.header
                    == (Header::Type7 {
                        opcode: opcode::WAIT_MEM_WRITES,
                        count: 0,
                    })
                    && packet.payload.is_empty()
        ));
    }
}

#[test]
fn logical_sample_uploads_are_converted_to_physical_bgra_rows() {
    for (logical_format, source, expected) in [
        (
            TextureFormat::Rgba8Unorm,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            vec![3, 2, 1, 4, 7, 6, 5, 8],
        ),
        (
            TextureFormat::R8Unorm,
            vec![9, 10],
            vec![0, 0, 0, 9, 0, 0, 0, 10],
        ),
    ] {
        let resources = [ResourceMeta {
            id: SOURCE,
            size: 64,
            kind: ResourceKind::Image(ImageMeta {
                format: logical_format,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(2, 1).unwrap(),
                usage: TextureUsage::COPY_DST | TextureUsage::SAMPLED,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 64,
                    size: 64,
                }],
            }),
        }];
        let operations = [Operation::WriteTexture {
            destination: SOURCE,
            area: rect(0, 0, 2, 1),
            bytes_per_row: source.len() as u32,
            data: &source,
        }];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap();

        assert_eq!(artifact.generated_objects.len(), 1);
        assert_eq!(artifact.generated_objects[0].bytes, expected);
        assert_eq!(artifact.fixups[1].object, ObjectRef::External(SOURCE));
        assert_eq!(artifact.fixups[1].required_size, 8);
    }
}

#[test]
fn ui_pipeline_matrix_is_accepted_without_external_shader_objects() {
    let paint_layout = VertexBufferLayout::new(
        16,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x2, 0),
            VertexAttribute::new(1, VertexFormat::Float32x2, 8),
        ],
    )
    .unwrap();
    let canvas_layout = VertexBufferLayout::new(
        40,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x4, 16),
            VertexAttribute::new(2, VertexFormat::Float32x2, 32),
        ],
    )
    .unwrap();
    let variants = [
        (FragmentProgram::Solid, paint_layout.clone(), CullMode::None),
        (
            FragmentProgram::Texture(TextureSampleMode::Rgba),
            paint_layout.clone(),
            CullMode::None,
        ),
        (
            FragmentProgram::Texture(TextureSampleMode::AlphaMask),
            paint_layout,
            CullMode::None,
        ),
        (
            FragmentProgram::VertexColor,
            canvas_layout.clone(),
            CullMode::None,
        ),
        (
            FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
            canvas_layout.clone(),
            CullMode::None,
        ),
        (
            FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask),
            canvas_layout,
            CullMode::None,
        ),
    ];
    let mut pipelines = variants
        .into_iter()
        .enumerate()
        .map(|(index, (fragment, layout, cull))| PipelineMeta {
            id: PipelineId::new(index as u32),
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                layout,
                fragment,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(cull, FrontFace::CounterClockwise),
            )
            .unwrap(),
        })
        .collect::<Vec<_>>();
    let composition_layout = VertexBufferLayout::new(
        24,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x2, 16),
        ],
    )
    .unwrap();
    for fragment in [
        FragmentProgram::Solid,
        FragmentProgram::Texture(TextureSampleMode::Rgba),
        FragmentProgram::Texture(TextureSampleMode::RgbIgnoreAlpha),
        FragmentProgram::Texture(TextureSampleMode::AlphaMask),
    ] {
        pipelines.push(PipelineMeta {
            id: PipelineId::new(pipelines.len() as u32),
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                composition_layout.clone(),
                fragment,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        });
    }
    pipelines.push(PipelineMeta {
        id: PipelineId::new(pipelines.len() as u32),
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            VertexBufferLayout::new(
                28,
                vec![
                    VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                    VertexAttribute::new(1, VertexFormat::Float32x3, 16),
                ],
            )
            .unwrap(),
            FragmentProgram::VertexColor,
            BlendState::REPLACE,
            RasterState::new(CullMode::Front, FrontFace::Clockwise),
        )
        .unwrap(),
    });

    let resources = resources();
    for pipeline in &pipelines {
        let textured = matches!(
            pipeline.descriptor.fragment(),
            FragmentProgram::Texture(_) | FragmentProgram::TextureVertexColor(_)
        );
        let alpha = matches!(
            pipeline.descriptor.fragment(),
            FragmentProgram::Texture(TextureSampleMode::AlphaMask)
                | FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask)
        );
        let stride = pipeline.descriptor.vertex_buffer().stride();
        let mut operations = vec![
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: rect(0, 0, 16, 16),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(pipeline.id),
            Operation::SetVertexBuffer {
                buffer: VERTICES,
                offset: 0,
            },
        ];
        if textured {
            operations.push(Operation::SetTexture(if alpha { ALPHA } else { SOURCE }));
            let filter = if matches!(stride, 16 | 24) {
                FilterMode::Linear
            } else {
                FilterMode::Nearest
            };
            operations.push(Operation::SetSampler(SamplerDesc::new(
                filter,
                filter,
                AddressMode::ClampToEdge,
                AddressMode::ClampToEdge,
            )));
        }
        operations.push(Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )));
        operations.push(Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        });
        operations.push(Operation::EndRenderPass);
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: core::slice::from_ref(pipeline),
            operations: &operations,
        })
        .unwrap();
        assert_well_formed(&artifact);
        assert_ccu_clean_before_every_color_invalidate(&artifact);
        if textured {
            let packets = Packets::new(&artifact.words)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let state_object = artifact
                .generated_objects
                .iter()
                .find(|object| object.kind == GeneratedObjectKind::TextureState)
                .expect("one generated FS texture-state object");
            assert_eq!(state_object.alignment, 64);
            assert_eq!(state_object.bytes, [0; 80]);
            assert_eq!(state_object.access, Access::READ | Access::WRITE);

            let expected_descriptor = if alpha { 0x4c00_76d0 } else { 0x4c00_6880 };
            let state_write = packets
                .iter()
                .find(|packet| {
                    matches!(packet.header, Header::Type7 { opcode: 0x3d, .. })
                        && packet.payload.len() == 22
                        && packet.payload[2] == expected_descriptor
                })
                .expect("one CP_MEM_WRITE texture-state materialization");
            let expected_sampler = if matches!(stride, 16 | 24) {
                [0x92a, 0x40, 0x20, 0]
            } else {
                [0x920, 0x40, 0, 0]
            };
            assert_eq!(&state_write.payload[18..], &expected_sampler);
            assert!(artifact.fixups.iter().any(|fixup| {
                fixup.word_offset == state_write.word_offset + 1
                    && fixup.object == ObjectRef::Generated(state_object.id)
                    && fixup.required_size == 80
                    && fixup.access == Access::WRITE
            }));

            for (state_type, offset, size) in [(0, 64, 16), (1, 0, 64)] {
                let load = packets
                    .iter()
                    .find(|packet| {
                        matches!(packet.header, Header::Type7 { opcode: 0x34, .. })
                            && packet.payload.len() == 3
                            && (packet.payload[0] >> 14) & 0x3 == state_type
                            && (packet.payload[0] >> 16) & 0x3 == 2
                            && (packet.payload[0] >> 18) & 0xf == 4
                            && (packet.payload[0] >> 22) & 0x3ff == 1
                    })
                    .expect("one indirect FS texture-state load");
                assert!(artifact.fixups.iter().any(|fixup| {
                    fixup.word_offset == load.word_offset + 2
                        && fixup.object == ObjectRef::Generated(state_object.id)
                        && fixup.object_offset == offset
                        && fixup.required_size == size
                        && fixup.access == Access::READ
                }));
            }
        }
        assert_eq!(
            artifact.generated_objects.len(),
            if textured { 2 } else { 1 }
        );
        assert_eq!(
            artifact
                .fixups
                .iter()
                .filter(|fixup| matches!(fixup.object, ObjectRef::CanonicalShader(_)))
                .count(),
            4
        );
    }
}

#[test]
fn coachz_four_quad_texture_composition_is_a_well_formed_stream() {
    const TARGET_SIZE: u64 = 2_160 * 1_440 * 4;
    const TEXTURE_SIZE: u64 = 256 * 256 * 4;
    const QUAD_BYTES: u64 = 6 * 24;

    let resources = [
        ResourceMeta {
            id: TARGET,
            size: TARGET_SIZE,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(2_160, 1_440).unwrap(),
                usage: TextureUsage::RENDER_ATTACHMENT,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 2_160 * 4,
                    size: TARGET_SIZE,
                }],
            }),
        },
        ResourceMeta {
            id: SOURCE,
            size: TEXTURE_SIZE,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(256, 256).unwrap(),
                usage: TextureUsage::SAMPLED | TextureUsage::COPY_DST,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 256 * 4,
                    size: TEXTURE_SIZE,
                }],
            }),
        },
        ResourceMeta {
            id: VERTICES,
            size: QUAD_BYTES * 4,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
            },
        },
    ];
    let layout = VertexBufferLayout::new(
        24,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x2, 16),
        ],
    )
    .unwrap();
    let pipelines = [
        FragmentProgram::Solid,
        FragmentProgram::Texture(TextureSampleMode::Rgba),
        FragmentProgram::Texture(TextureSampleMode::RgbIgnoreAlpha),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, fragment)| PipelineMeta {
        id: PipelineId::new(index as u32),
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            layout.clone(),
            fragment,
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
    })
    .collect::<Vec<_>>();
    let vertices = vec![0; (QUAD_BYTES * 4) as usize];
    let mut operations = vec![
        Operation::WriteBuffer {
            destination: VERTICES,
            offset: 0,
            data: &vertices,
        },
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, 2_160, 1_440),
            load: LoadOp::Clear(Color::rgba(0.08, 0.1, 0.14, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: None,
        }),
    ];
    // Keep two adjacent solid draws so the golden also exercises retained
    // fixed state and pipelined draw submission before switching programs.
    for (index, pipeline) in [0_u32, 0, 1, 2].into_iter().enumerate() {
        operations.push(Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: index as u64 * QUAD_BYTES,
        });
        operations.push(Operation::SetPipeline(PipelineId::new(pipeline)));
        if pipeline != 0 {
            operations.push(Operation::SetTexture(SOURCE));
            operations.push(Operation::SetSampler(SamplerDesc::new(
                FilterMode::Linear,
                FilterMode::Linear,
                AddressMode::ClampToEdge,
                AddressMode::ClampToEdge,
            )));
        }
        operations.push(Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )));
        operations.push(Operation::SetScissor(None));
        operations.push(Operation::Draw {
            vertex_count: 6,
            first_vertex: 0,
        });
    }
    operations.push(Operation::EndRenderPass);

    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &pipelines,
        operations: &operations,
    })
    .unwrap();
    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(artifact.words.len(), 1_331);
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let draws = packets
        .iter()
        .enumerate()
        .filter_map(|(index, packet)| {
            matches!(packet.header, Header::Type7 { opcode: 0x38, .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(draws.len(), 4);
    assert!(
        packets[draws[0] + 1..draws[1]].iter().all(|packet| {
            !matches!(
                packet.header,
                Header::Type4 {
                    register: 0x8823 | 0x8825 | 0x8000 | 0x8080 | 0x8082,
                    ..
                }
            )
        }),
        "a compatible draw must inherit immutable target and viewport state",
    );
    assert!(
        !matches!(
            packets[draws[1] - 1].header,
            Header::Type7 {
                opcode: opcode::WAIT_FOR_IDLE,
                ..
            }
        ),
        "compatible draws must remain pipelined",
    );
    assert_eq!(
        packets
            .iter()
            .filter(|packet| {
                matches!(packet.header, Header::Type7 { opcode: 0x65, .. }) && packet.payload == [1]
            })
            .count(),
        1,
        "one render pass must establish the A6xx 3D baseline once",
    );
    assert_eq!(
        packets
            .iter()
            .filter(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: 0x46,
                        count: 4
                    }
                ) && packet.payload.first() == Some(&0x4000_001c)
            })
            .count(),
        2,
        "the A2D clear and the complete 3D draw batch retire depth once each",
    );
}

#[test]
fn validation_rejects_duplicate_ids_and_unaligned_image_layouts() {
    let mut duplicate_resources = resources();
    duplicate_resources[1].id = TARGET;
    assert_eq!(
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &duplicate_resources,
            pipelines: &[],
            operations: &[],
        }),
        Err(CompileError::InvalidIdentity)
    );

    let mut invalid_layout_resources = resources();
    let ResourceKind::Image(image) = &mut invalid_layout_resources[0].kind else {
        unreachable!("target fixture is an image");
    };
    image.planes[0].stride = 60;
    assert_eq!(
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &invalid_layout_resources,
            pipelines: &[],
            operations: &[],
        }),
        Err(CompileError::InvalidResource)
    );
}

#[test]
fn vertex_draw_range_must_fit_the_bound_buffer() {
    let resources = resources();
    let pipelines = [pipeline()];
    let operations = [
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, 16, 16),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PIPELINE),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 6,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    assert_eq!(
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        }),
        Err(CompileError::OutOfBounds)
    );
}

#[test]
fn coachz_cube_first_frame_uses_upstream_shader_program_bursts() {
    const WIDTH: u32 = 2160;
    const HEIGHT: u32 = 1440;
    const STRIDE: u32 = WIDTH * 4;
    const VERTEX_BYTES: usize = 36 * 28;

    let resources = [
        ResourceMeta {
            id: TARGET,
            size: u64::from(STRIDE) * u64::from(HEIGHT),
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(WIDTH, HEIGHT).unwrap(),
                usage: TextureUsage::RENDER_ATTACHMENT
                    | TextureUsage::COPY_DST
                    | TextureUsage::COPY_SRC,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: STRIDE,
                    size: u64::from(STRIDE) * u64::from(HEIGHT),
                }],
            }),
        },
        ResourceMeta {
            id: VERTICES,
            size: VERTEX_BYTES as u64,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
            },
        },
    ];
    let descriptor = RenderPipelineDesc::new(
        TextureFormat::Bgra8Unorm,
        PrimitiveTopology::TriangleList,
        VertexBufferLayout::new(
            28,
            vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x3, 16),
            ],
        )
        .unwrap(),
        FragmentProgram::VertexColor,
        BlendState::REPLACE,
        RasterState::new(CullMode::Back, FrontFace::CounterClockwise),
    )
    .unwrap();
    let pipelines = [PipelineMeta {
        id: PIPELINE,
        descriptor,
    }];
    let upload = [0_u8; VERTEX_BYTES];
    let operations = [
        Operation::WriteBuffer {
            destination: VERTICES,
            offset: 0,
            data: &upload,
        },
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: rect(0, 0, WIDTH, HEIGHT),
            load: LoadOp::Clear(Color::rgba(0.45, 0.45, 0.45, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PIPELINE),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        Operation::Draw {
            vertex_count: 36,
            first_vertex: 0,
        },
        Operation::EndRenderPass,
    ];
    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 4096),
        resources: &resources,
        pipelines: &pipelines,
        operations: &operations,
    })
    .unwrap();

    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(artifact.words.len(), 496);
    assert_eq!(artifact.generated_objects.len(), 2);
    let packets = Packets::new(&artifact.words)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(packets.iter().any(|packet| {
        matches!(
            packet.header,
            Header::Type4 {
                register: 0x8090,
                count: 1,
            }
        ) && packet.payload == [0x2012]
    }));
    for (register, variant) in [
        (0xa81b, ShaderVariant::VsStride28Pos4Color3),
        (0xa982, ShaderVariant::FsVertexColor),
    ] {
        let layouts: Vec<_> = packets
            .iter()
            .filter(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: actual,
                        count: 7,
                    } if actual == register
                )
            })
            .collect();
        assert_eq!(layouts.len(), 1);
        let layout = layouts[0];
        assert_eq!(layout.payload.len(), 7);
        assert_eq!(layout.payload[0], 0);
        assert_eq!(&layout.payload[3..], &[0; 4]);
        assert!(artifact.fixups.iter().any(|fixup| {
            fixup.word_offset == layout.word_offset + 2
                && fixup.object == ObjectRef::CanonicalShader(variant)
        }));
    }
}

#[test]
fn showcase_first_frame_compiles_as_one_complete_multipass_stream() {
    const SCREEN: ObjectId = ObjectId::new(20);
    const OFFSCREEN: ObjectId = ObjectId::new(21);
    const COPIED: ObjectId = ObjectId::new(22);
    const MASK: ObjectId = ObjectId::new(23);
    const COLOR_VERTICES: ObjectId = ObjectId::new(24);
    const TEXTURE_VERTICES: ObjectId = ObjectId::new(25);
    const SHOWCASE_INDICES: ObjectId = ObjectId::new(26);

    let image =
        |id: ObjectId, format: TextureFormat, width: u32, height: u32, usage: TextureUsage| {
            let stride = (width * 4_u32).next_multiple_of(64);
            ResourceMeta {
                id,
                size: u64::from(stride) * u64::from(height),
                kind: ResourceKind::Image(ImageMeta {
                    format,
                    storage_format: TextureFormat::Bgra8Unorm,
                    extent: Extent2D::new(width, height).unwrap(),
                    usage,
                    modifier: ImageModifier::Linear,
                    planes: vec![PlaneLayout {
                        offset: 0,
                        stride,
                        size: u64::from(stride) * u64::from(height),
                    }],
                }),
            }
        };
    let resources = vec![
        image(
            SCREEN,
            TextureFormat::Bgra8Unorm,
            64,
            48,
            TextureUsage::RENDER_ATTACHMENT,
        ),
        image(
            OFFSCREEN,
            TextureFormat::Bgra8Unorm,
            32,
            24,
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::SAMPLED | TextureUsage::COPY_SRC,
        ),
        image(
            COPIED,
            TextureFormat::Bgra8Unorm,
            32,
            24,
            TextureUsage::SAMPLED | TextureUsage::COPY_DST,
        ),
        image(
            MASK,
            TextureFormat::R8Unorm,
            4,
            4,
            TextureUsage::SAMPLED | TextureUsage::COPY_DST,
        ),
        ResourceMeta {
            id: COLOR_VERTICES,
            size: 128,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
            },
        },
        ResourceMeta {
            id: TEXTURE_VERTICES,
            size: 96,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
            },
        },
        ResourceMeta {
            id: SHOWCASE_INDICES,
            size: 12,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::INDEX | BufferUsage::COPY_DST,
            },
        },
    ];
    let color_layout = VertexBufferLayout::new(
        32,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x4, 16),
        ],
    )
    .unwrap();
    let texture_layout = VertexBufferLayout::new(
        24,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x2, 16),
        ],
    )
    .unwrap();
    let raster = RasterState::new(CullMode::None, FrontFace::CounterClockwise);
    let pipeline = |id, layout, fragment| PipelineMeta {
        id: PipelineId::new(id),
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            layout,
            fragment,
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            raster,
        )
        .unwrap(),
    };
    let pipelines = vec![
        pipeline(0, color_layout.clone(), FragmentProgram::VertexColor),
        pipeline(1, color_layout, FragmentProgram::Solid),
        pipeline(
            2,
            texture_layout.clone(),
            FragmentProgram::Texture(TextureSampleMode::Rgba),
        ),
        pipeline(
            3,
            texture_layout,
            FragmentProgram::Texture(TextureSampleMode::AlphaMask),
        ),
    ];
    let color_bytes = [0_u8; 128];
    let texture_bytes = [0_u8; 96];
    let index_bytes = [0_u8, 0, 1, 0, 2, 0, 0, 0, 2, 0, 3, 0];
    let mask_bytes = [0xff_u8; 16];
    let uniforms = DrawUniforms::new(
        Transform::identity(),
        Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
    );
    let sampler = SamplerDesc::new(
        FilterMode::Linear,
        FilterMode::Linear,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge,
    );
    let mut operations = vec![
        Operation::WriteBuffer {
            destination: COLOR_VERTICES,
            offset: 0,
            data: &color_bytes,
        },
        Operation::WriteBuffer {
            destination: TEXTURE_VERTICES,
            offset: 0,
            data: &texture_bytes,
        },
        Operation::WriteBuffer {
            destination: SHOWCASE_INDICES,
            offset: 0,
            data: &index_bytes,
        },
        Operation::WriteTexture {
            destination: MASK,
            area: rect(0, 0, 4, 4),
            bytes_per_row: 4,
            data: &mask_bytes,
        },
        Operation::BeginRenderPass(RenderPass {
            target: OFFSCREEN,
            area: rect(0, 0, 32, 24),
            load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PipelineId::new(0)),
        Operation::SetVertexBuffer {
            buffer: COLOR_VERTICES,
            offset: 0,
        },
        Operation::SetIndexBuffer {
            buffer: SHOWCASE_INDICES,
            offset: 0,
            format: IndexFormat::Uint16,
        },
        Operation::SetScissor(Some(rect(1, 1, 30, 22))),
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::EndRenderPass,
        Operation::CopyTextureToTexture {
            source: OFFSCREEN,
            source_rect: rect(0, 0, 32, 24),
            destination: COPIED,
            destination_rect: rect(0, 0, 32, 24),
        },
        Operation::BeginRenderPass(RenderPass {
            target: SCREEN,
            area: rect(0, 0, 64, 48),
            load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetPipeline(PipelineId::new(2)),
        Operation::SetVertexBuffer {
            buffer: TEXTURE_VERTICES,
            offset: 0,
        },
        Operation::SetIndexBuffer {
            buffer: SHOWCASE_INDICES,
            offset: 0,
            format: IndexFormat::Uint16,
        },
        Operation::SetTexture(COPIED),
        Operation::SetSampler(sampler),
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::SetTexture(OFFSCREEN),
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::SetPipeline(PipelineId::new(3)),
        Operation::SetTexture(MASK),
        Operation::SetScissor(None),
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::SetPipeline(PipelineId::new(1)),
        Operation::SetVertexBuffer {
            buffer: COLOR_VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(uniforms),
        Operation::DrawIndexed {
            index_count: 6,
            first_index: 0,
            base_vertex: 0,
        },
        Operation::EndRenderPass,
    ];

    let artifact = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 16 * 1024),
        resources: &resources,
        pipelines: &pipelines,
        operations: &operations,
    })
    .unwrap();
    assert_well_formed(&artifact);
    assert_ccu_clean_before_every_color_invalidate(&artifact);
    assert_eq!(
        Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .filter(|packet| matches!(
                packet.header,
                Header::Type7 {
                    opcode: 0x38,
                    count: 7,
                }
            ))
            .count(),
        6
    );
    assert!(artifact.fixups.iter().any(|fixup| {
        fixup.object == ObjectRef::CanonicalShader(ShaderVariant::FsTextureAlphaMask)
    }));
    operations.clear();
}
