//! Shared real-IR3 fixtures for codegen and kernel-validator integration tests.
#![allow(dead_code)]
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::*;
use sgfx_shader_ir3::{CompiledShader, Stage};
use std::{path::PathBuf, sync::Arc};

pub const TARGET: ObjectId = ObjectId::new(0);
pub const SOURCE: ObjectId = ObjectId::new(1);
pub const UBO: ObjectId = ObjectId::new(2);
pub const STORAGE: ObjectId = ObjectId::new(3);
pub const VERTICES: ObjectId = ObjectId::new(4);
pub const VS: ObjectId = ObjectId::new(10);
pub const FS: ObjectId = ObjectId::new(11);
pub const CS: ObjectId = ObjectId::new(12);

pub fn shader(name: &str, stage: Stage, object: ObjectId) -> NativeShader {
    // file!(), unlike CARGO_MANIFEST_DIR, also works when this helper is included
    // by the independent production-kernel validator harness.
    let dir = PathBuf::from(file!())
        .parent()
        .unwrap()
        .join("../native-fixtures");
    let read = |ext: &str| std::fs::read(dir.join(format!("{name}.{ext}"))).unwrap();
    NativeShader {
        private_memory: None,
        object,
        artifact: Arc::new(CompiledShader {
            stage,
            entry: if name.starts_with("draw_") {
                "main"
            } else if matches!(name, "push" | "storage") {
                "cs"
            } else if name == "mrt" {
                "fs"
            } else {
                name
            }
            .into(),
            allocation: read("bin"),
            metadata: serde_json::from_slice(&read("json")).unwrap(),
            bindings: serde_json::from_slice(&read("bindings.json")).unwrap(),
            push_constant_bytes: if name == "push" { 4 } else { 0 },
            spirv: read("spv")
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect(),
        }),
    }
}
pub fn buffer(id: ObjectId, size: u64, usage: BufferUsage) -> ResourceMeta {
    ResourceMeta {
        id,
        size,
        kind: ResourceKind::Buffer { usage },
    }
}
pub fn image(id: ObjectId, usage: TextureUsage) -> ResourceMeta {
    ResourceMeta {
        id,
        size: 1024,
        kind: ResourceKind::Image(ImageMeta {
            conversion: None,
            texture: None,
            format: TextureFormat::Bgra8Unorm,
            storage_format: TextureFormat::Bgra8Unorm,
            extent: Extent2D::new(16, 16).unwrap(),
            usage,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: 64,
                size: 1024,
            }],
        }),
    }
}
pub fn shader_resource(s: &NativeShader) -> ResourceMeta {
    buffer(
        s.object,
        s.artifact.allocation.len() as u64,
        BufferUsage::COPY_DST,
    )
}
fn layout(shaders: &[&NativeShader]) -> PipelineLayoutDesc {
    let mut groups: Vec<Vec<BindGroupLayoutEntry>> = Vec::new();
    let mut ranges = Vec::new();
    for shader in shaders {
        let stage = match shader.artifact.stage {
            Stage::Vertex => ShaderStages::VERTEX,
            Stage::Fragment => ShaderStages::FRAGMENT,
            Stage::Compute => ShaderStages::COMPUTE,
        };
        for b in &shader.artifact.bindings {
            while groups.len() <= b.group as usize {
                groups.push(Vec::new());
            }
            let entries = &mut groups[b.group as usize];
            let ty = match b.kind {
                sgfx_shader_ir3::ResourceKind::UniformBuffer => BindingType::UniformBuffer,
                sgfx_shader_ir3::ResourceKind::StorageBuffer => BindingType::StorageBuffer {
                    read_only: !b.writable,
                },
                sgfx_shader_ir3::ResourceKind::Texture => BindingType::SampledTexture,
                sgfx_shader_ir3::ResourceKind::Sampler => BindingType::Sampler,
            };
            if let Some(e) = entries.iter_mut().find(|e| e.binding() == b.binding) {
                *e = BindGroupLayoutEntry::new(b.binding, e.visibility() | stage, ty);
            } else {
                entries.push(BindGroupLayoutEntry::new(b.binding, stage, ty));
            }
        }
        if shader.artifact.push_constant_bytes > 0 {
            ranges.push(
                PushConstantRange::new(
                    stage,
                    0,
                    shader.artifact.push_constant_bytes.next_multiple_of(4),
                )
                .unwrap(),
            );
        }
    }
    PipelineLayoutDesc::new(
        groups
            .into_iter()
            .map(|e| BindGroupLayoutDesc::new(e).unwrap())
            .collect(),
    )
    .unwrap()
    .with_push_constant_ranges(ranges)
    .unwrap()
}
pub fn render_pipeline(fragment: &str) -> NativeRenderPipeline {
    let is_glass = fragment != "fs";
    let vertex_name = if matches!(fragment, "glass" | "erase") {
        "glass_vertex"
    } else if is_glass {
        "blur_vertex"
    } else {
        "vs"
    };
    let vertex = shader(vertex_name, Stage::Vertex, VS);
    let fragment_shader = shader(fragment, Stage::Fragment, FS);
    let source = if is_glass {
        include_str!("../native-fixtures/glass.wgsl")
    } else {
        include_str!("../native-fixtures/procedural.wgsl")
    };
    let table = ResourceTable::new();
    let module = table
        .define_shader_module(ShaderModuleDesc::wgsl(source.into()).unwrap())
        .unwrap();
    let vertex_entry =
        ShaderEntryPoint::new(module, ShaderStage::Vertex, vertex_name.into()).unwrap();
    let fragment_entry =
        ShaderEntryPoint::new(module, ShaderStage::Fragment, fragment.into()).unwrap();
    let vb = if vertex_name == "glass_vertex" {
        Some(
            VertexBufferLayout::new(
                16,
                vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)],
            )
            .unwrap(),
        )
    } else {
        None
    };
    NativeRenderPipeline {
        descriptor: ProgrammableRenderPipelineDesc::new(
            vertex_entry,
            fragment_entry,
            layout(&[&vertex, &fragment_shader]),
            TextureFormat::Bgra8Unorm,
            vb,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
        vertex,
        fragment: fragment_shader,
    }
}
pub fn compute_pipeline(name: &str) -> NativeComputePipeline {
    let shader = shader(name, Stage::Compute, CS);
    let source = match name {
        "push" => include_str!("../native-fixtures/push.wgsl"),
        "storage" => include_str!("../native-fixtures/storage.wgsl"),
        "wide_shared" => include_str!("../native-fixtures/workgroup.wgsl"),
        _ => include_str!("../native-fixtures/compute.wgsl"),
    };
    let table = ResourceTable::new();
    let module = table
        .define_shader_module(ShaderModuleDesc::wgsl(source.into()).unwrap())
        .unwrap();
    let entry =
        ShaderEntryPoint::new(module, ShaderStage::Compute, shader.artifact.entry.clone()).unwrap();
    NativeComputePipeline {
        descriptor: ComputePipelineDesc::new(entry, layout(&[&shader])).unwrap(),
        shader,
    }
}
pub struct Case {
    pub resources: Vec<ResourceMeta>,
    pub operations: Vec<Operation<'static>>,
}
impl Case {
    pub fn compile(&self) -> std::result::Result<RelocatablePm4, CompileError> {
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 32768),
            resources: &self.resources,
            pipelines: &[],
            operations: &self.operations,
        })
    }
}
pub fn sampler() -> SamplerDesc {
    SamplerDesc::new(
        FilterMode::Linear,
        FilterMode::Linear,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge,
    )
}
pub fn render_case(fragment: &str) -> Case {
    let p = render_pipeline(fragment);
    let mut resources = vec![
        image(TARGET, TextureUsage::RENDER_ATTACHMENT),
        image(SOURCE, TextureUsage::SAMPLED),
        buffer(UBO, 1120, BufferUsage::UNIFORM),
        buffer(VERTICES, 96, BufferUsage::VERTEX),
        shader_resource(&p.vertex),
        shader_resource(&p.fragment),
    ];
    let mut operations = vec![
        Operation::BeginRenderPass(RenderPass {
            target: TARGET,
            area: PixelRect::new(0, 0, 16, 16).unwrap(),
            load: LoadOp::Load,
            store: StoreOp::Store,
            depth: None,
        }),
        Operation::SetProgrammablePipeline(Arc::new(p)),
        Operation::SetBindGroup {
            index: 0,
            bindings: vec![
                NativeBinding {
                    binding: 0,
                    resource: NativeBindingResource::Buffer {
                        object: UBO,
                        offset: 0,
                        size: 1120,
                        write: false,
                    },
                },
                NativeBinding {
                    binding: 1,
                    resource: NativeBindingResource::Texture {
                        object: SOURCE,
                        view: None,
                    },
                },
                NativeBinding {
                    binding: 2,
                    resource: NativeBindingResource::Sampler(sampler()),
                },
            ],
        },
    ];
    operations.push(Operation::SetVertexBufferSlot {
        slot: 0,
        buffer: VERTICES,
        offset: 0,
    });
    operations.push(Operation::Draw {
        vertex_count: if fragment == "fs" { 3 } else { 6 },
        first_vertex: 0,
    });
    operations.push(Operation::EndRenderPass);
    // Keep deterministic identities/order, without requiring them to be dense.
    resources.sort_by_key(|r| r.id);
    Case {
        resources,
        operations,
    }
}
/// Repeated immutable sampled bindings, for construction-cost and validator tests.
pub fn native_repeated_sampled_case(draws: usize) -> Case {
    assert!(draws > 0);
    let mut c = render_case("horizontal");
    c.operations.pop();
    for _ in 1..draws {
        c.operations.push(Operation::Draw {
            vertex_count: 6,
            first_vertex: 0,
        });
    }
    c.operations.push(Operation::EndRenderPass);
    c
}
/// Exact four-IBO, 256KiB/offset4 interface from the installed graphics oracle.
/// Vertex reads indices32768/65535; fragment uses resinfo and dynamic loads.
pub fn native_large_readonly_graphics_case() -> Case {
    let mut vertex = shader("large_storage_vs", Stage::Vertex, VS);
    let mut fragment = shader("large_storage_fs", Stage::Fragment, FS);
    Arc::make_mut(&mut vertex.artifact).entry = "main".into();
    Arc::make_mut(&mut fragment.artifact).entry = "main".into();
    let table = ResourceTable::new();
    let vs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(include_str!("../native-fixtures/large_storage_vs.wgsl").into())
                .unwrap(),
        )
        .unwrap();
    let fs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(include_str!("../native-fixtures/large_storage_fs.wgsl").into())
                .unwrap(),
        )
        .unwrap();
    let descriptor = ProgrammableRenderPipelineDesc::new(
        ShaderEntryPoint::new(vs, ShaderStage::Vertex, "main".into()).unwrap(),
        ShaderEntryPoint::new(fs, ShaderStage::Fragment, "main".into()).unwrap(),
        layout(&[&vertex, &fragment]),
        TextureFormat::Bgra8Unorm,
        None,
        PrimitiveTopology::TriangleList,
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise),
    )
    .unwrap();
    let mut resources = vec![
        image(TARGET, TextureUsage::RENDER_ATTACHMENT),
        shader_resource(&vertex),
        shader_resource(&fragment),
    ];
    let bindings = (0..4)
        .map(|binding| {
            let object = ObjectId::new(40 + binding);
            resources.push(buffer(
                object,
                262152,
                BufferUsage::STORAGE | BufferUsage::COPY_DST | BufferUsage::COPY_SRC,
            ));
            NativeBinding {
                binding,
                resource: NativeBindingResource::Buffer {
                    object,
                    offset: 4,
                    size: 262144,
                    write: false,
                },
            }
        })
        .collect();
    Case {
        resources,
        operations: vec![
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 4, 4).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetProgrammablePipeline(Arc::new(NativeRenderPipeline {
                descriptor,
                vertex,
                fragment,
            })),
            Operation::SetBindGroup { index: 0, bindings },
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ],
    }
}
/// Real DrawParameters VS/FS, followed by three draws with distinct parameters.
/// The source SPIR-V uses actual builtins; its WGSL template is not shader input.
pub fn native_draw_parameters_case(extension: bool) -> Case {
    let vertex = shader(
        if extension {
            "draw_vs_khr"
        } else {
            "draw_vs_core"
        },
        Stage::Vertex,
        VS,
    );
    let fragment = shader("draw_fs", Stage::Fragment, FS);
    let table = ResourceTable::new();
    let vs = table
        .define_shader_module(ShaderModuleDesc::spirv(vertex.artifact.spirv.clone()).unwrap())
        .unwrap();
    let fs = table
        .define_shader_module(ShaderModuleDesc::spirv(fragment.artifact.spirv.clone()).unwrap())
        .unwrap();
    let descriptor = ProgrammableRenderPipelineDesc::new(
        ShaderEntryPoint::new(vs, ShaderStage::Vertex, "main".into()).unwrap(),
        ShaderEntryPoint::new(fs, ShaderStage::Fragment, "main".into()).unwrap(),
        layout(&[&vertex, &fragment]),
        TextureFormat::Bgra8Unorm,
        None,
        PrimitiveTopology::TriangleList,
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise),
    )
    .unwrap();
    let index = ObjectId::new(30);
    Case {
        resources: vec![
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            shader_resource(&vertex),
            shader_resource(&fragment),
            buffer(index, 12, BufferUsage::INDEX | BufferUsage::COPY_DST),
        ],
        operations: vec![
            Operation::WriteBuffer {
                destination: index,
                offset: 0,
                data: &[5, 0, 0, 0, 6, 0, 0, 0, 7, 0, 0, 0],
            },
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetProgrammablePipeline(Arc::new(NativeRenderPipeline {
                descriptor,
                vertex,
                fragment,
            })),
            Operation::SetDrawIndex(13),
            Operation::DrawInstanced {
                vertex_count: 3,
                instance_count: 2,
                first_vertex: 7,
                first_instance: 11,
            },
            Operation::SetIndexBuffer {
                buffer: index,
                offset: 0,
                format: IndexFormat::Uint32,
            },
            Operation::SetDrawIndex(19),
            Operation::DrawIndexedInstanced {
                index_count: 3,
                instance_count: 2,
                first_index: 0,
                base_vertex: -5,
                first_instance: 17,
            },
            Operation::SetDrawIndex(5),
            Operation::DrawInstanced {
                vertex_count: 3,
                instance_count: 1,
                first_vertex: 2,
                first_instance: 3,
            },
            Operation::EndRenderPass,
        ],
    }
}
pub fn compute_case(name: &str) -> Case {
    let p = compute_pipeline(name);
    let mut resources = vec![
        shader_resource(&p.shader),
        buffer(STORAGE, 512, BufferUsage::STORAGE),
        buffer(UBO, 16, BufferUsage::UNIFORM),
    ];
    let mut groups: Vec<(u32, Vec<NativeBinding>)> = Vec::new();
    for b in &p.shader.artifact.bindings {
        let object = if b.kind == sgfx_shader_ir3::ResourceKind::UniformBuffer {
            UBO
        } else if b.slot == 0 {
            STORAGE
        } else {
            ObjectId::new(20 + b.slot)
        };
        let size = if object == UBO { 16 } else { 512 };
        if !resources.iter().any(|r| r.id == object) {
            resources.push(buffer(object, size, BufferUsage::STORAGE));
        }
        if !groups.iter().any(|(g, _)| *g == b.group) {
            groups.push((b.group, Vec::new()));
        }
        groups
            .iter_mut()
            .find(|(g, _)| *g == b.group)
            .unwrap()
            .1
            .push(NativeBinding {
                binding: b.binding,
                resource: NativeBindingResource::Buffer {
                    object,
                    offset: 0,
                    size,
                    write: b.writable,
                },
            });
    }
    let mut operations = vec![
        Operation::BeginComputePass,
        Operation::SetComputePipeline(Arc::new(p)),
    ];
    for (index, bindings) in groups {
        operations.push(Operation::SetBindGroup { index, bindings });
    }
    if name == "push" {
        operations.push(Operation::SetPushConstants {
            stages: ShaderStages::COMPUTE,
            offset: 0,
            data: &[7, 0, 0, 0],
        });
    }
    operations.push(Operation::Dispatch { x: 2, y: 1, z: 1 });
    operations.push(Operation::EndComputePass);
    resources.sort_by_key(|r| r.id);
    Case {
        resources,
        operations,
    }
}
pub fn render_fixture(name: &str) -> RelocatablePm4 {
    render_case(name).compile().unwrap()
}
pub fn compute_fixture() -> RelocatablePm4 {
    compute_case("cs").compile().unwrap()
}

pub fn native_image(id: ObjectId, format: TextureFormat, usage: TextureUsage) -> ResourceMeta {
    let bpp = format.bytes_per_pixel().unwrap();
    let pitch = (16 * bpp).next_multiple_of(64);
    let size = u64::from(pitch) * 16;
    ResourceMeta {
        id,
        size,
        kind: ResourceKind::Image(ImageMeta {
            conversion: None,
            texture: None,
            format,
            storage_format: format,
            extent: Extent2D::new(16, 16).unwrap(),
            usage,
            modifier: ImageModifier::Linear,
            planes: vec![PlaneLayout {
                offset: 0,
                stride: pitch,
                size,
            }],
        }),
    }
}
pub fn native_float_blit_case(format: TextureFormat, filter: FilterMode, flips: [bool; 2]) -> Case {
    Case {
        resources: vec![
            native_image(SOURCE, format, TextureUsage::COPY_SRC),
            native_image(TARGET, format, TextureUsage::COPY_DST),
        ],
        operations: vec![Operation::BlitTexture {
            source: SOURCE,
            source_rect: PixelRect::new(1, 2, 12, 8).unwrap(),
            destination: TARGET,
            destination_rect: PixelRect::new(3, 4, 6, 4).unwrap(),
            filter,
            flips,
        }],
    }
}
pub fn native_depth_image(id: ObjectId, usage: TextureUsage) -> ResourceMeta {
    let mut r = native_image(id, TextureFormat::Depth32Float, usage);
    let ResourceKind::Image(i) = &mut r.kind else {
        unreachable!()
    };
    i.modifier = ImageModifier::A6xxTile6_3Depth;
    i.planes[0].stride = 256;
    i.planes[0].size = 4096;
    r.size = 4096;
    r
}
/// Native D32 draw with public raster depth-bias state, for validator reuse.
pub fn native_depth_bias_case(constant: f32, slope: f32) -> Case {
    let mut c = render_case("fs");
    let depth = ObjectId::new(40);
    c.resources
        .push(native_depth_image(depth, TextureUsage::RENDER_ATTACHMENT));
    for op in &mut c.operations {
        match op {
            Operation::BeginRenderPass(pass) => {
                pass.depth = Some(sgfx_codegen_adreno_a6xx::DepthAttachment {
                    target: depth,
                    load: DepthLoadOp::Load,
                    store: StoreOp::Store,
                })
            }
            Operation::SetProgrammablePipeline(p) => {
                let p = Arc::make_mut(p);
                p.descriptor = p
                    .descriptor
                    .clone()
                    .with_raster_state(
                        RasterState::new(CullMode::Back, FrontFace::Clockwise)
                            .with_depth_bias(constant, slope)
                            .unwrap(),
                    )
                    .with_depth_state(DepthState::new(
                        TextureFormat::Depth32Float,
                        CompareFunction::LessEqual,
                        true,
                    ))
                    .unwrap();
            }
            _ => {}
        }
    }
    c
}
pub fn native_depth_copy_case() -> Case {
    Case {
        resources: vec![
            native_depth_image(SOURCE, TextureUsage::COPY_SRC),
            native_depth_image(TARGET, TextureUsage::COPY_DST),
        ],
        operations: vec![Operation::CopyTextureToTexture {
            source: SOURCE,
            source_rect: PixelRect::new(1, 2, 7, 5).unwrap(),
            destination: TARGET,
            destination_rect: PixelRect::new(3, 4, 7, 5).unwrap(),
        }],
    }
}
pub fn native_depth_upload_case() -> Case {
    // Three packed pixels + source padding; the final row has no trailing pad.
    static DATA: [u8; 28] = [
        0, 0, 0, 0, 0, 0, 0, 63, 0, 0, 128, 63, 0xab, 0xcd, 0xef, 0xff, 0, 0, 128, 63, 0, 0, 0, 63,
        0, 0, 0, 0,
    ];
    Case {
        resources: vec![native_depth_image(TARGET, TextureUsage::COPY_DST)],
        operations: vec![Operation::WriteTexture {
            destination: TARGET,
            area: PixelRect::new(2, 3, 3, 2).unwrap(),
            bytes_per_row: 16,
            data: &DATA,
        }],
    }
}
pub fn native_storage_offset_case(offset: u64) -> Case {
    let mut c = compute_case("storage");
    for op in &mut c.operations {
        if let Operation::SetBindGroup { bindings, .. } = op {
            for b in bindings {
                if let NativeBindingResource::Buffer {
                    offset: o, size, ..
                } = &mut b.resource
                {
                    *o = offset;
                    *size = 516;
                }
            }
        }
    }
    for r in &mut c.resources {
        if matches!(r.kind,ResourceKind::Buffer{usage} if usage.contains(BufferUsage::STORAGE)) {
            r.size = offset + 516;
        }
    }
    c
}

/// Real VS/FS readonly storage artifacts. `texture_lowering` explicitly
/// exercises the supported metadata route: the current compiler emits UAV
/// loads for these shaders, so this override is not an ISA/hardware proof.
pub fn native_graphics_storage_offset_case(offset: u64, texture_lowering: bool) -> Case {
    let mut vertex = shader("readonly_vs", Stage::Vertex, VS);
    let mut fragment = shader("readonly_fs", Stage::Fragment, FS);
    if texture_lowering {
        for shader in [&mut vertex, &mut fragment] {
            let m = &mut Arc::make_mut(&mut shader.artifact).metadata;
            m.image_mapping.num_tex = 1;
            m.image_mapping.tex_base = 2;
            m.image_mapping.ssbo_to_tex[0] = 0;
            m.num_samp = 3;
        }
    }
    let table = ResourceTable::new();
    let module = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(
                include_str!("../native-fixtures/readonly_graphics.wgsl").into(),
            )
            .unwrap(),
        )
        .unwrap();
    let p = NativeRenderPipeline {
        descriptor: ProgrammableRenderPipelineDesc::new(
            ShaderEntryPoint::new(module, ShaderStage::Vertex, "readonly_vs".into()).unwrap(),
            ShaderEntryPoint::new(module, ShaderStage::Fragment, "readonly_fs".into()).unwrap(),
            layout(&[&vertex, &fragment]),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
        vertex,
        fragment,
    };
    let mut c = render_case("fs");
    c.resources
        .retain(|r| r.id != VS && r.id != FS && r.id != STORAGE);
    c.resources.extend([
        shader_resource(&p.vertex),
        shader_resource(&p.fragment),
        buffer(STORAGE, offset + 516, BufferUsage::STORAGE),
    ]);
    for op in &mut c.operations {
        match op {
            Operation::SetProgrammablePipeline(pipeline) => *pipeline = Arc::new(p.clone()),
            Operation::SetBindGroup { bindings, .. } => {
                *bindings = vec![NativeBinding {
                    binding: 0,
                    resource: NativeBindingResource::Buffer {
                        object: STORAGE,
                        offset,
                        size: 516,
                        write: false,
                    },
                }]
            }
            _ => {}
        }
    }
    c
}

/// Real Mesa shaders for array/cube sampling and depth load + comparison.
pub fn native_sampled_case(name: &str) -> Case {
    let vertex = shader("vs", Stage::Vertex, VS);
    let fragment = shader(name, Stage::Fragment, FS);
    let is_depth = name == "depth_sample";
    let dimension = match name {
        "depth_sample" => TextureViewDimension::D2,
        "array_sample" => TextureViewDimension::D2Array,
        "cube_sample" => TextureViewDimension::Cube,
        _ => panic!("unknown sampled fixture"),
    };
    let format = if is_depth {
        TextureFormat::Depth32Float
    } else {
        TextureFormat::Rgba32Float
    };
    let texture = TextureDesc::new(
        format,
        Extent2D::new(16, 16).unwrap(),
        TextureUsage::SAMPLED,
    )
    .unwrap();
    let texture = if is_depth {
        texture
    } else {
        texture
            .with_mip_level_count(3)
            .unwrap()
            .with_array_layer_count(8)
            .unwrap()
    };
    let view = TextureViewDesc::new(
        texture,
        format,
        dimension,
        if is_depth { 0 } else { 1 },
        if is_depth { 1 } else { 2 },
        if is_depth { 0 } else { 1 },
        if is_depth {
            1
        } else if dimension == TextureViewDimension::Cube {
            6
        } else {
            3
        },
    )
    .unwrap();
    let entries = fragment
        .artifact
        .bindings
        .iter()
        .map(|b| {
            BindGroupLayoutEntry::new(
                b.binding,
                ShaderStages::FRAGMENT,
                match b.kind {
                    sgfx_shader_ir3::ResourceKind::Texture => BindingType::SampledTextureView {
                        dimension,
                        depth: is_depth,
                    },
                    sgfx_shader_ir3::ResourceKind::Sampler if is_depth => {
                        BindingType::ComparisonSampler
                    }
                    sgfx_shader_ir3::ResourceKind::Sampler => BindingType::Sampler,
                    _ => panic!("unexpected resource"),
                },
            )
        })
        .collect();
    let pipeline_layout =
        PipelineLayoutDesc::new(vec![BindGroupLayoutDesc::new(entries).unwrap()]).unwrap();
    let table = ResourceTable::new();
    let vs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(include_str!("../native-fixtures/procedural.wgsl").into())
                .unwrap(),
        )
        .unwrap();
    let fs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(include_str!("../native-fixtures/sampled_views.wgsl").into())
                .unwrap(),
        )
        .unwrap();
    let p = NativeRenderPipeline {
        descriptor: ProgrammableRenderPipelineDesc::new(
            ShaderEntryPoint::new(vs, ShaderStage::Vertex, "vs".into()).unwrap(),
            ShaderEntryPoint::new(fs, ShaderStage::Fragment, name.into()).unwrap(),
            pipeline_layout,
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap(),
        vertex,
        fragment,
    };
    let mut source = if is_depth {
        native_depth_image(SOURCE, TextureUsage::SAMPLED)
    } else {
        native_image(SOURCE, format, TextureUsage::SAMPLED)
    };
    if !is_depth {
        let l = adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(16, 16, 3, 8, 16)
            .unwrap();
        let ResourceKind::Image(i) = &mut source.kind else {
            unreachable!()
        };
        i.texture = Some(l);
        i.planes[0].stride = l.row_pitch;
        i.planes[0].size = l.total_size;
        source.size = l.total_size;
    }
    let bindings: Vec<NativeBinding> = p
        .fragment
        .artifact
        .bindings
        .iter()
        .map(|b| NativeBinding {
            binding: b.binding,
            resource: match b.kind {
                sgfx_shader_ir3::ResourceKind::Texture => NativeBindingResource::Texture {
                    object: SOURCE,
                    view: Some(view),
                },
                _ => NativeBindingResource::Sampler(sampler().with_compare(if is_depth {
                    Some(CompareFunction::LessEqual)
                } else {
                    None
                })),
            },
        })
        .collect();
    let mut c = render_case("fs");
    c.resources.retain(|r| r.id != FS && r.id != SOURCE);
    c.resources.push(shader_resource(&p.fragment));
    c.resources.push(source);
    for op in &mut c.operations {
        match op {
            Operation::SetProgrammablePipeline(pipeline) => *pipeline = Arc::new(p.clone()),
            Operation::SetBindGroup { bindings: b, .. } => *b = bindings.clone(),
            _ => {}
        }
    }
    c
}
/// Wide partial copies exercise the 128-dword CP_MEMCPY limit and row padding.
pub fn native_float_copy_case(format: TextureFormat) -> Case {
    let resources = [
        (SOURCE, TextureUsage::COPY_SRC),
        (TARGET, TextureUsage::COPY_DST),
    ]
    .into_iter()
    .map(|(id, usage)| {
        let mut r = native_image(id, format, usage);
        let ResourceKind::Image(i) = &mut r.kind else {
            unreachable!()
        };
        i.extent = Extent2D::new(96, 16).unwrap();
        i.planes[0].stride = 96 * format.bytes_per_pixel().unwrap();
        i.planes[0].size = u64::from(i.planes[0].stride) * 16;
        r.size = i.planes[0].size;
        r
    })
    .collect();
    Case {
        resources,
        operations: vec![Operation::CopyTextureToTexture {
            source: SOURCE,
            source_rect: PixelRect::new(2, 3, 73, 2).unwrap(),
            destination: TARGET,
            destination_rect: PixelRect::new(5, 6, 73, 2).unwrap(),
        }],
    }
}
/// Independent real shader artifacts, also used by the production validator harness.
pub fn native_color_case(formats: &[TextureFormat], clear: bool) -> Case {
    let vertex = shader("vs", Stage::Vertex, VS);
    let fragment = shader(
        if formats.len() > 1 { "mrt" } else { "fs" },
        Stage::Fragment,
        FS,
    );
    let table = ResourceTable::new();
    let vs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(include_str!("../native-fixtures/procedural.wgsl").into())
                .unwrap(),
        )
        .unwrap();
    let fs = table
        .define_shader_module(
            ShaderModuleDesc::wgsl(
                if formats.len() > 1 {
                    include_str!("../native-fixtures/mrt.wgsl")
                } else {
                    include_str!("../native-fixtures/procedural.wgsl")
                }
                .into(),
            )
            .unwrap(),
        )
        .unwrap();
    let descriptor = ProgrammableRenderPipelineDesc::new(
        ShaderEntryPoint::new(vs, ShaderStage::Vertex, "vs".into()).unwrap(),
        ShaderEntryPoint::new(fs, ShaderStage::Fragment, "fs".into()).unwrap(),
        PipelineLayoutDesc::new(vec![]).unwrap(),
        formats[0],
        None,
        PrimitiveTopology::TriangleList,
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise),
    )
    .unwrap()
    .with_color_targets(
        formats
            .iter()
            .map(|f| ColorTargetState::new(*f, BlendState::REPLACE, ColorWriteMask::ALL).unwrap())
            .collect(),
    )
    .unwrap();
    let p = NativeRenderPipeline {
        descriptor,
        vertex,
        fragment,
    };
    let mut resources = vec![shader_resource(&p.vertex), shader_resource(&p.fragment)];
    let mut colors = Vec::new();
    for (i, format) in formats.iter().enumerate() {
        let id = ObjectId::new(i as u32);
        resources.push(native_image(
            id,
            *format,
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_DST | TextureUsage::SAMPLED,
        ));
        colors.push(NativeColorAttachment {
            target: id,
            load: if clear {
                LoadOp::Clear(Color::rgba(-2.0, 2.5, f32::from_bits(0x33800000), 0.5).unwrap())
            } else {
                LoadOp::Load
            },
            store: StoreOp::Store,
        });
    }
    Case {
        resources,
        operations: vec![
            Operation::BeginNativeRenderPass(NativeRenderPass {
                colors,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                depth: None,
            }),
            Operation::SetProgrammablePipeline(Arc::new(p)),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ],
    }
}
pub fn wide_shared_case() -> Case {
    let mut c = compute_case("wide_shared");
    for resource in &mut c.resources {
        if matches!(resource.kind,ResourceKind::Buffer{usage} if usage==BufferUsage::STORAGE) {
            resource.size = 8192;
        }
    }
    for operation in &mut c.operations {
        if let Operation::SetBindGroup { bindings, .. } = operation {
            for binding in bindings {
                if let NativeBindingResource::Buffer { size, .. } = &mut binding.resource {
                    *size = 8192;
                }
            }
        }
    }
    c
}

/// A real FS primitive input, with no geometry shader or fabricated VS ID.
pub fn native_primitive_id_case(reset_to_plain: bool) -> Case {
    let mut case = render_case("fs");
    let mut pipeline = render_pipeline("fs");
    pipeline.fragment = shader("primitive_id", Stage::Fragment, FS);
    case.resources.retain(|r| r.id != FS);
    case.resources.push(shader_resource(&pipeline.fragment));
    case.operations[1] = Operation::SetProgrammablePipeline(Arc::new(pipeline));
    if reset_to_plain {
        let mut plain = render_pipeline("fs");
        plain.fragment.object = ObjectId::new(13);
        case.resources.push(shader_resource(&plain.fragment));
        case.operations.pop();
        case.operations
            .push(Operation::SetProgrammablePipeline(Arc::new(plain)));
        case.operations.push(Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        });
        case.operations.push(Operation::EndRenderPass);
    }
    case
}

/// Actual samples==1 Mesa output, shared with the bounded device diagnostic.
pub fn native_single_sample_case(center: bool, derivatives: bool) -> Case {
    let mut case = render_case("fs");
    let mut pipeline = render_pipeline("fs");
    pipeline.vertex = shader("single_sample_vs", Stage::Vertex, VS);
    let name = match (center, derivatives) {
        (false, false) => "single_sample_sample",
        (true, false) => "single_sample_center",
        (false, true) => "single_sample_sample-derivatives",
        (true, true) => "single_sample_center-derivatives",
    };
    pipeline.fragment = shader(name, Stage::Fragment, FS);
    Arc::make_mut(&mut pipeline.vertex.artifact).entry = "vs".into();
    Arc::make_mut(&mut pipeline.fragment.artifact).entry =
        if derivatives { "derivatives" } else { "fs" }.into();
    case.resources.retain(|r| r.id != VS && r.id != FS);
    case.resources.push(shader_resource(&pipeline.vertex));
    case.resources.push(shader_resource(&pipeline.fragment));
    case.operations[1] = Operation::SetProgrammablePipeline(Arc::new(pipeline));
    case
}

/// RG8 uses the same expanded BGRA backing as production logical narrow images.
pub fn native_rg8_case() -> Case {
    let mut c = native_color_case(&[TextureFormat::Rg8Unorm],true);
    for r in &mut c.resources {
        if let ResourceKind::Image(i) = &mut r.kind {
            i.storage_format = TextureFormat::Bgra8Unorm;
        }
    }
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            p.descriptor = p.descriptor.clone().with_color_targets(vec![ColorTargetState::new(
                TextureFormat::Rg8Unorm,BlendState::new(
                    BlendComponent::new(BlendFactor::DestinationAlpha,BlendFactor::OneMinusDestinationAlpha,BlendOp::Add),
                    BlendComponent::new(BlendFactor::One,BlendFactor::Zero,BlendOp::Add)),ColorWriteMask::ALL).unwrap()]).unwrap();
        }
    }
    c
}

/// Backend-generated fixed semantics using real source-built IR3, general VFD,
/// native float source/target, and ordinary fixed state commands.
pub fn fixed_native_case(indexed: bool) -> Case {
    let mut vertex=shader("fixed_native_vs",Stage::Vertex,VS);
    let mut fragment=shader("fixed_native_fs",Stage::Fragment,FS);
    Arc::make_mut(&mut vertex.artifact).entry="vs".into();
    Arc::make_mut(&mut fragment.artifact).entry="fs".into();
    Arc::make_mut(&mut vertex.artifact).push_constant_bytes=96;
    Arc::make_mut(&mut fragment.artifact).push_constant_bytes=96;
    let table=ResourceTable::new();
    let module=table.define_shader_module(ShaderModuleDesc::wgsl(include_str!("../native-fixtures/fixed_native.wgsl").into()).unwrap()).unwrap();
    let group=BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0,ShaderStages::FRAGMENT,BindingType::SampledTexture),
        BindGroupLayoutEntry::new(1,ShaderStages::FRAGMENT,BindingType::Sampler),
    ]).unwrap();
    let layout=PipelineLayoutDesc::new(vec![group]).unwrap().with_push_constant_ranges(vec![PushConstantRange::new(ShaderStages::VERTEX|ShaderStages::FRAGMENT,0,96).unwrap()]).unwrap();
    let descriptor=ProgrammableRenderPipelineDesc::new(
        ShaderEntryPoint::new(module,ShaderStage::Vertex,"vs".into()).unwrap(),
        ShaderEntryPoint::new(module,ShaderStage::Fragment,"fs".into()).unwrap(),layout,
        TextureFormat::Rgba16Float,None,PrimitiveTopology::TriangleStrip,BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
        RasterState::new(CullMode::Front,FrontFace::Clockwise)).unwrap().with_vertex_buffers(vec![
            VertexBufferLayout::new(80,vec![VertexAttribute::new(0,VertexFormat::Float32x3,8),
                VertexAttribute::new(1,VertexFormat::Unorm8x4,32),VertexAttribute::new(2,VertexFormat::Float32x2,56)]).unwrap()]).unwrap();
    let pipeline=Arc::new(NativeRenderPipeline {descriptor,vertex,fragment});
    let index=ObjectId::new(123);
    let mut resources=vec![shader_resource(&pipeline.vertex),shader_resource(&pipeline.fragment),
        native_image(TARGET,TextureFormat::Rgba16Float,TextureUsage::RENDER_ATTACHMENT),
        native_image(SOURCE,TextureFormat::Rgba32Float,TextureUsage::SAMPLED),
        buffer(VERTICES,320,BufferUsage::VERTEX)];
    let mut operations=vec![Operation::BeginNativeRenderPass(NativeRenderPass {colors:vec![NativeColorAttachment {target:TARGET,load:LoadOp::Load,store:StoreOp::Store}],area:PixelRect::new(0,0,16,16).unwrap(),depth:None}),
        Operation::SetFixedNativePipeline(pipeline),Operation::SetVertexBuffer {buffer:VERTICES,offset:0},
        Operation::SetUniforms(DrawUniforms::new(Transform::identity(),Color::rgba(0.25,0.5,0.75,0.5).unwrap())),
        Operation::SetTexture(SOURCE),Operation::SetSampler(SamplerDesc::new(FilterMode::Nearest,FilterMode::Nearest,AddressMode::ClampToEdge,AddressMode::ClampToEdge))];
    if indexed {resources.push(buffer(index,8,BufferUsage::INDEX));operations.push(Operation::SetIndexBuffer {buffer:index,offset:0,format:IndexFormat::Uint16});operations.push(Operation::DrawIndexed {index_count:4,first_index:0,base_vertex:-1});}
    else {operations.push(Operation::Draw {vertex_count:4,first_vertex:0});}
    operations.push(Operation::EndRenderPass);Case {resources,operations}
}

/// Real generated conversion shader; both readonly producer planes retain one
/// attachment identity and disjoint ranges. Used by codegen and kernel tests.
pub fn fixed_nv12_case(conversion:YcbcrConversion)->Case {
    let mut vertex=shader("fixed_nv12_vs",Stage::Vertex,VS);
    let mut fragment=shader("fixed_nv12_fs",Stage::Fragment,FS);
    for (s,name) in [(&mut vertex,"vs"),(&mut fragment,"fs")] {
        let a=Arc::make_mut(&mut s.artifact);a.entry=name.into();a.push_constant_bytes=96;
    }
    let table=ResourceTable::new();
    let module=table.define_shader_module(ShaderModuleDesc::wgsl(include_str!("../native-fixtures/fixed_nv12.wgsl").into()).unwrap()).unwrap();
    let group=BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0,ShaderStages::FRAGMENT,BindingType::SampledTexture),
        BindGroupLayoutEntry::new(1,ShaderStages::FRAGMENT,BindingType::Sampler)]).unwrap();
    let layout=PipelineLayoutDesc::new(vec![group]).unwrap().with_push_constant_ranges(vec![PushConstantRange::new(ShaderStages::VERTEX|ShaderStages::FRAGMENT,0,96).unwrap()]).unwrap();
    let descriptor=ProgrammableRenderPipelineDesc::new(
        ShaderEntryPoint::new(module,ShaderStage::Vertex,"vs".into()).unwrap(),
        ShaderEntryPoint::new(module,ShaderStage::Fragment,"fs".into()).unwrap(),layout,
        TextureFormat::Bgra8Unorm,None,PrimitiveTopology::TriangleList,BlendState::REPLACE,
        RasterState::new(CullMode::None,FrontFace::CounterClockwise)).unwrap().with_vertex_buffers(vec![
            VertexBufferLayout::new(24,vec![VertexAttribute::new(0,VertexFormat::Float32x4,0),
                VertexAttribute::new(1,VertexFormat::Float32x2,16)]).unwrap()]).unwrap();
    let pipeline=Arc::new(NativeRenderPipeline {descriptor,vertex,fragment});
    let resources=vec![shader_resource(&pipeline.vertex),shader_resource(&pipeline.fragment),image(TARGET,TextureUsage::RENDER_ATTACHMENT),
        ResourceMeta {id:SOURCE,size:8192,kind:ResourceKind::Image(ImageMeta {format:TextureFormat::Nv12,storage_format:TextureFormat::Nv12,
            extent:Extent2D::new(16,16).unwrap(),usage:TextureUsage::SAMPLED,modifier:ImageModifier::Linear,texture:None,conversion:Some(conversion),
            planes:vec![PlaneLayout {offset:0,stride:64,size:1024},PlaneLayout {offset:4096,stride:64,size:512}]})},
        buffer(VERTICES,72,BufferUsage::VERTEX)];
    let operations=vec![Operation::BeginRenderPass(RenderPass {target:TARGET,area:PixelRect::new(0,0,16,16).unwrap(),load:LoadOp::Load,store:StoreOp::Store,depth:None}),
        Operation::SetFixedNativePipeline(pipeline),Operation::SetVertexBuffer {buffer:VERTICES,offset:0},
        Operation::SetUniforms(DrawUniforms::new(Transform::identity(),Color::rgba(1.0,1.0,1.0,1.0).unwrap())),
        Operation::SetTexture(SOURCE),Operation::SetSampler(sampler()),Operation::Draw {vertex_count:3,first_vertex:0},Operation::EndRenderPass];
    Case {resources,operations}
}

pub fn generic_nv12_case()->Case {
    let conversion=YcbcrConversion {matrix:YcbcrMatrix::Bt709,range:YcbcrRange::Limited,chroma_x:ChromaLocation::Midpoint,chroma_y:ChromaLocation::Midpoint};
    let mut c=fixed_nv12_case(conversion);
    let mut vertex=shader("generic_nv12_vs",Stage::Vertex,VS);let mut fragment=shader("generic_nv12_fs",Stage::Fragment,FS);
    Arc::make_mut(&mut vertex.artifact).entry="vs".into();Arc::make_mut(&mut fragment.artifact).entry="fs".into();
    let table=ResourceTable::new();let module=table.define_shader_module(ShaderModuleDesc::wgsl(include_str!("../native-fixtures/generic_nv12.wgsl").into()).unwrap()).unwrap();
    let layout=PipelineLayoutDesc::new(vec![BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0,ShaderStages::FRAGMENT,BindingType::SampledTexture),BindGroupLayoutEntry::new(1,ShaderStages::FRAGMENT,BindingType::Sampler)]).unwrap()]).unwrap();
    let descriptor=ProgrammableRenderPipelineDesc::new(ShaderEntryPoint::new(module,ShaderStage::Vertex,"vs".into()).unwrap(),
        ShaderEntryPoint::new(module,ShaderStage::Fragment,"fs".into()).unwrap(),layout,TextureFormat::Bgra8Unorm,None,PrimitiveTopology::TriangleList,
        BlendState::REPLACE,RasterState::new(CullMode::None,FrontFace::CounterClockwise)).unwrap();
    let pipeline=Arc::new(NativeRenderPipeline {descriptor,vertex,fragment});
    c.resources.retain(|r|r.id!=VS&&r.id!=FS&&r.id!=VERTICES);
    c.resources.push(shader_resource(&pipeline.vertex));c.resources.push(shader_resource(&pipeline.fragment));
    c.operations=vec![Operation::BeginRenderPass(RenderPass {target:TARGET,area:PixelRect::new(0,0,16,16).unwrap(),load:LoadOp::Load,store:StoreOp::Store,depth:None}),
        Operation::SetProgrammablePipeline(pipeline),Operation::SetBindGroup {index:0,bindings:vec![
            NativeBinding {binding:0,resource:NativeBindingResource::Nv12Texture(SOURCE)},NativeBinding {binding:1,resource:NativeBindingResource::Sampler(sampler())}]},
        Operation::Draw {vertex_count:3,first_vertex:0},Operation::EndRenderPass];c
}
