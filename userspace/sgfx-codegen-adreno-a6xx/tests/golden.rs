use adreno_a6xx_pm4::{Header, Packets};
use adreno_a6xx_shader_pack::ShaderVariant;
use sgfx_codegen_adreno_a6xx::{
    Access, Capabilities, CompileError, CompileInput, ImageMeta, ImageModifier, ObjectId,
    ObjectRef, Operation, PipelineId, PipelineMeta, PlaneLayout, RenderPass, ResourceKind,
    ResourceMeta, compile,
};
use sgfx_core::ir::{
    AddressMode, BlendState, BufferUsage, Color, CullMode, DrawUniforms, Extent2D, FilterMode,
    FragmentProgram, FrontFace, LoadOp, PrimitiveTopology, RasterState, RenderPipelineDesc,
    SamplerDesc, StoreOp, TextureFormat, TextureSampleMode, TextureUsage, Transform,
    VertexAttribute, VertexBufferLayout, VertexFormat,
};

const TARGET: ObjectId = ObjectId::new(0);
const SOURCE: ObjectId = ObjectId::new(1);
const VERTICES: ObjectId = ObjectId::new(2);
const ALPHA: ObjectId = ObjectId::new(3);
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
    assert_eq!(artifact.fixups.len(), 1);
    assert_eq!(artifact.fixups[0].object, ObjectRef::External(TARGET));
    assert_eq!(artifact.fixups[0].access, Access::WRITE);
    assert_eq!(
        artifact.words.as_slice(),
        &[
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
            0x7026_8000,
        ]
    );
}

#[test]
fn copy_is_a_golden_two_relocation_stream() {
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
    assert_eq!(artifact.fixups.len(), 2);
    assert_eq!(artifact.fixups[0].object, ObjectRef::External(SOURCE));
    assert_eq!(artifact.fixups[0].access, Access::READ);
    assert_eq!(artifact.fixups[1].object, ObjectRef::External(TARGET));
    assert_eq!(artifact.fixups[1].access, Access::WRITE);
    assert_eq!(
        artifact.words.as_slice(),
        &[
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
            0x7026_8000,
        ]
    );
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
    assert_eq!(artifact.generated_objects.len(), 0);
    assert_eq!(artifact.fixups.len(), 6);
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
        (0x9306, &[1]),
        (0x8000, &[0x80]),
        (0x8006, &[0x0007_fdff]),
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
            CullMode::Back,
        ),
        (
            FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
            canvas_layout,
            CullMode::Back,
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
            let filter = if stride == 24 {
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
        if textured {
            let packets = Packets::new(&artifact.words)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let texture_state = packets
                .iter()
                .find(|packet| {
                    matches!(packet.header, Header::Type7 { opcode: 0x34, .. })
                        && packet.payload.len() == 19
                        && (packet.payload[0] >> 14) & 0x3 == 1
                        && (packet.payload[0] >> 18) & 0xf == 4
                        && (packet.payload[0] >> 22) & 0x3ff == 1
                })
                .expect("one 16-dword FS texture descriptor");
            assert_eq!(texture_state.payload.len(), 3 + 16);
            let sampler_state = packets
                .iter()
                .find(|packet| {
                    matches!(packet.header, Header::Type7 { opcode: 0x34, .. })
                        && packet.payload.len() == 7
                        && (packet.payload[0] >> 14) & 0x3 == 0
                        && (packet.payload[0] >> 18) & 0xf == 4
                        && (packet.payload[0] >> 22) & 0x3ff == 1
                })
                .expect("one four-dword FS sampler descriptor");
            let expected_sampler = if stride == 24 {
                [0x92a, 0x40, 0x20, 0]
            } else {
                [0x920, 0x40, 0, 0]
            };
            assert_eq!(&sampler_state.payload[3..], &expected_sampler);
        }
        assert_eq!(artifact.generated_objects.len(), 0);
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
