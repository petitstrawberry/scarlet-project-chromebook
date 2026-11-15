//! Actual IR3 image stores through production codegen and hostile-input validator.
use super::*;
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::*;
use sgfx_shader_ir3::{BindingAssignment, BindingMap, ResourceKind as K, Source, Stage};
use std::sync::Arc;
const CS: &str = "@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>; @group(0) @binding(1) var<storage,read> buf:array<u32>; @compute @workgroup_size(4,4) fn main(@builtin(global_invocation_id) p:vec3<u32>){textureStore(image,vec2<i32>(p.xy),vec4<f32>(f32(buf[0])/255.0,0.4,0.6,1.0));}";
fn map() -> BindingMap {
    BindingMap::new(std::vec![
        BindingAssignment {
            group: 0,
            binding: 0,
            kind: K::Texture,
            slot: 0
        },
        BindingAssignment {
            group: 0,
            binding: 1,
            kind: K::StorageBuffer,
            slot: 7
        }
    ])
    .unwrap()
}
fn native(source: &str, stage: Stage, id: ObjectId) -> NativeShader {
    NativeShader {
        object: id,
        private_memory: None,
        artifact: Arc::new(
            sgfx_shader_ir3::compile_with_bindings(Source::Wgsl(source), stage, "main", &map())
                .unwrap(),
        ),
    }
}
fn layout(stages: ShaderStages) -> PipelineLayoutDesc {
    PipelineLayoutDesc::new(std::vec![
        BindGroupLayoutDesc::new(std::vec![
            BindGroupLayoutEntry::new(
                0,
                stages,
                BindingType::StorageTexture {
                    format: TextureFormat::Rgba8Unorm,
                    access: StorageTextureAccess::WriteOnly
                }
            ),
            BindGroupLayoutEntry::new(1, stages, BindingType::StorageBuffer { read_only: true })
        ])
        .unwrap()
    ])
    .unwrap()
}
fn image() -> ResourceMeta {
    let mut r = fixtures::image(fixtures::SOURCE, TextureUsage::STORAGE);
    if let ResourceKind::Image(ref mut m) = r.kind {
        m.format = TextureFormat::Rgba8Unorm;
    }
    r
}
fn bindings() -> Operation<'static> {
    Operation::SetBindGroup {
        index: 0,
        bindings: std::vec![
            NativeBinding {
                binding: 0,
                resource: NativeBindingResource::Texture {
                    object: fixtures::SOURCE,
                    view: None
                }
            },
            NativeBinding {
                binding: 1,
                resource: NativeBindingResource::Buffer {
                    object: fixtures::STORAGE,
                    offset: 0,
                    size: 64,
                    write: false
                }
            }
        ],
    }
}
fn compute_case() -> fixtures::Case {
    let t = ResourceTable::new();
    let m = t
        .define_shader_module(ShaderModuleDesc::wgsl(CS.into()).unwrap())
        .unwrap();
    let shader = native(CS, Stage::Compute, fixtures::CS);
    let descriptor = ComputePipelineDesc::new(
        ShaderEntryPoint::new(m, ShaderStage::Compute, "main".into()).unwrap(),
        layout(ShaderStages::COMPUTE),
    )
    .unwrap();
    fixtures::Case {
        resources: std::vec![
            fixtures::shader_resource(&shader),
            image(),
            fixtures::buffer(fixtures::STORAGE, 64, BufferUsage::STORAGE)
        ],
        operations: std::vec![
            Operation::BeginComputePass,
            Operation::SetComputePipeline(Arc::new(NativeComputePipeline { descriptor, shader })),
            bindings(),
            Operation::Dispatch { x: 4, y: 4, z: 1 },
            Operation::EndComputePass
        ],
    }
}
#[test]
fn actual_storage_image_ibo_and_kernel_layout_authority() {
    let c = compute_case();
    let a = c.compile().unwrap();
    let images = case_images(&c);
    validate_with_images(&a, &images).unwrap();
    let fix = a
        .fixups
        .iter()
        .find(|f| {
            f.object == ObjectRef::External(fixtures::SOURCE)
                && f.encoding == sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49TexDescriptor
        })
        .unwrap();
    assert_eq!(fix.access, Access::WRITE);
    let start = fix.word_offset as usize - 4;
    assert_eq!(a.words[start], 0x4c006880);
    for (offset, bits) in [(0, 1 << 30), (1, 1), (2, 64 << 7), (5, 1 << 17), (8, 1)] {
        let mut bad = a.clone();
        bad.words[start + offset] ^= bits;
        assert!(validate_with_images(&bad, &images).is_err());
    }
    let mut read_only = c;
    if let ResourceKind::Image(ref mut i) = read_only.resources[1].kind {
        i.usage = TextureUsage::SAMPLED;
    }
    assert!(read_only.compile().is_err());
}
#[test]
fn actual_colorless_fragment_and_shared_image_slots_are_admitted() {
    const VS: &str = "@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>; @vertex fn main(@builtin(vertex_index) id:u32)->@builtin(position) vec4<f32>{textureStore(image,vec2<i32>(i32(id),0),vec4<f32>(1.0));return vec4<f32>(f32(id&1u),f32(id>>1u),0.5,1.0);}";
    for fs in [
        "@fragment fn main(){discard;}",
        "@fragment fn main()->@builtin(frag_depth) f32{return 0.5;}",
        "@group(0) @binding(0) var image:texture_storage_2d<rgba8unorm,write>; @group(0) @binding(1) var<storage,read> buf:array<u32>; @fragment fn main(@builtin(position) p:vec4<f32>){textureStore(image,vec2<i32>(p.xy),vec4<f32>(f32(buf[0])/255.0));}",
    ] {
        let t = ResourceTable::new();
        let vr = t
            .define_shader_module(ShaderModuleDesc::wgsl(VS.into()).unwrap())
            .unwrap();
        let fr = t
            .define_shader_module(ShaderModuleDesc::wgsl(fs.into()).unwrap())
            .unwrap();
        let vertex = native(VS, Stage::Vertex, fixtures::VS);
        let fragment = native(fs, Stage::Fragment, fixtures::FS);
        assert_eq!(vertex.artifact.metadata.num_ssbos, 16);
        if fragment
            .artifact
            .bindings
            .iter()
            .any(|b| b.kind == K::Texture)
        {
            assert_eq!(fragment.artifact.metadata.num_ssbos, 16);
        }
        let descriptor = ProgrammableRenderPipelineDesc::new(
            ShaderEntryPoint::new(vr, ShaderStage::Vertex, "main".into()).unwrap(),
            ShaderEntryPoint::new(fr, ShaderStage::Fragment, "main".into()).unwrap(),
            layout(ShaderStages::VERTEX | ShaderStages::FRAGMENT),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap();
        let c = fixtures::Case {
            resources: std::vec![
                fixtures::shader_resource(&vertex),
                fixtures::shader_resource(&fragment),
                image(),
                fixtures::image(fixtures::TARGET, TextureUsage::RENDER_ATTACHMENT),
                fixtures::buffer(fixtures::STORAGE, 64, BufferUsage::STORAGE)
            ],
            operations: std::vec![
                Operation::BeginRenderPass(RenderPass {
                    target: fixtures::TARGET,
                    area: PixelRect::new(0, 0, 16, 16).unwrap(),
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                    depth: None
                }),
                Operation::SetProgrammablePipeline(Arc::new(NativeRenderPipeline {
                    descriptor,
                    vertex,
                    fragment
                })),
                bindings(),
                Operation::Draw {
                    vertex_count: 3,
                    first_vertex: 0
                },
                Operation::EndRenderPass
            ],
        };
        let a = c.compile().unwrap();
        validate_with_images(&a, &case_images(&c)).unwrap();
    }
}
