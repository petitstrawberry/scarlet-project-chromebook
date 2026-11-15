//! Actual single-sample coverage exports and reset into an ordinary shader.
use super::*;
use sgfx_codegen_adreno_a6xx::*;
use sgfx_core::ir::*;
use sgfx_shader_ir3::{Source, Stage};
use std::sync::Arc;
const VS: &str = include_str!(
    "../../../userspace/sgfx-adreno-check/src/programmable/storage-fixtures/plain_vertex.wgsl"
);
const FS: &str = include_str!(
    "../../../userspace/sgfx-adreno-check/src/programmable/storage-fixtures/sample_mask.wgsl"
);
const PLAIN: &str = include_str!(
    "../../../userspace/sgfx-adreno-check/src/programmable/storage-fixtures/sample_plain.wgsl"
);
fn pipeline(fs: &str, id: u32) -> NativeRenderPipeline {
    let t = ResourceTable::new();
    let v = t
        .define_shader_module(ShaderModuleDesc::wgsl(VS.into()).unwrap())
        .unwrap();
    let f = t
        .define_shader_module(ShaderModuleDesc::wgsl(fs.into()).unwrap())
        .unwrap();
    let shader = |source, stage, object| NativeShader {
        object,
        private_memory: None,
        artifact: Arc::new(sgfx_shader_ir3::compile(Source::Wgsl(source), stage, "main").unwrap()),
    };
    NativeRenderPipeline {
        vertex: shader(VS, Stage::Vertex, fixtures::VS),
        fragment: shader(fs, Stage::Fragment, ObjectId::new(id)),
        descriptor: ProgrammableRenderPipelineDesc::new(
            ShaderEntryPoint::new(v, ShaderStage::Vertex, "main".into()).unwrap(),
            ShaderEntryPoint::new(f, ShaderStage::Fragment, "main".into()).unwrap(),
            PipelineLayoutDesc::new(std::vec![]).unwrap(),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
        .unwrap()
        .with_depth_state(DepthState::new(
            TextureFormat::Depth32Float,
            CompareFunction::Always,
            true,
        ))
        .unwrap(),
    }
}
#[test]
fn real_sample_mask_exports_reset_and_malformed_metadata_is_rejected() {
    let p = pipeline(FS, 11);
    let plain = pipeline(PLAIN, 12);
    assert!(p.fragment.artifact.metadata.writes_smask);
    assert!(!p.fragment.artifact.metadata.sample_shading);
    assert!(p.fragment.artifact.metadata.writes_pos);
    assert!(!plain.fragment.artifact.metadata.writes_pos);
    let reg = p
        .fragment
        .artifact
        .metadata
        .outputs
        .iter()
        .find(|o| o.slot == 3)
        .unwrap()
        .regid;
    let depth = fixtures::native_depth_image(ObjectId::new(2), TextureUsage::RENDER_ATTACHMENT);
    let c = fixtures::Case {
        resources: std::vec![
            fixtures::image(fixtures::TARGET, TextureUsage::RENDER_ATTACHMENT),
            depth,
            fixtures::shader_resource(&p.vertex),
            fixtures::shader_resource(&p.fragment),
            fixtures::shader_resource(&plain.fragment)
        ],
        operations: std::vec![
            Operation::BeginRenderPass(RenderPass {
                target: fixtures::TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: Some(sgfx_codegen_adreno_a6xx::DepthAttachment {
                    target: ObjectId::new(2),
                    load: DepthLoadOp::Load,
                    store: StoreOp::Store
                })
            }),
            Operation::SetProgrammablePipeline(Arc::new(p.clone())),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0
            },
            Operation::SetProgrammablePipeline(Arc::new(plain)),
            Operation::SetScissor(Some(PixelRect::new(0, 0, 16, 1).unwrap())),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0
            },
            Operation::EndRenderPass
        ],
    };
    let a = c.compile().unwrap();
    // Check state at the draws, rather than merely finding an earlier write.
    // The plain shader uses interpolated vertex Z; the masked shader exports Z.
    let mut state = std::collections::BTreeMap::new();
    let mut draws = Vec::new();
    for packet in adreno_a6xx_pm4::Packets::new(&a.words) {
        let packet = packet.unwrap();
        match packet.header {
            adreno_a6xx_pm4::Header::Type4 { register, .. } => {
                for (i, value) in packet.payload.iter().enumerate() {
                    state.insert(register + i as u32, *value);
                }
            }
            adreno_a6xx_pm4::Header::Type7 { opcode: 0x38, .. } => {
                draws.push(state.clone());
            }
            _ => {}
        }
    }
    assert_eq!(draws.len(), 2);
    for (i, state) in draws.iter().enumerate() {
        assert_eq!(state[&0x8000], 0xc0); // ZERO_GB_SCALE_Z, zero-to-one clip Z
        let offset = f32::from_bits(state[&0x8014]);
        let scale = f32::from_bits(state[&0x8015]);
        // SGFX's pinned rendering contract: 0 <= z <= w. In particular,
        // plain_vertex.wgsl emits Z/W=0.5 and must store 0.5, not 0.75.
        assert_eq!([offset, scale], [0.0, 1.0]);
        assert_eq!(offset + scale * 0.5, 0.5);
        assert_eq!(state[&0x8871], 31); // Always, depth test + write enabled
        assert_eq!(state[&0x8114], 1); // GRAS depth test enabled
        assert_eq!(state[&0x8870], u32::from(i == 0)); // RB late -> early Z
        assert_eq!(state[&0x8094], u32::from(i == 0)); // GRAS agrees
        assert_eq!(state[&0x880b], if i == 0 { 6 } else { 0 });
        if i == 1 {
            assert_eq!(state[&0xa98c], 0xfcfcfc00); // depth/mask/stencil absent
            assert_eq!(state[&0x80f1], 15); // plain row scissor bottom-right
        }
    }
    let values = |register| {
        adreno_a6xx_pm4::Packets::new(&a.words)
            .map(|p| p.unwrap())
            .filter_map(|p| match p.header {
                adreno_a6xx_pm4::Header::Type4 { register: r, .. } if r == register => {
                    Some(p.payload[0])
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let sp = values(0xa98c);
    assert!(sp.iter().any(|v| (v >> 16) & 255 == reg));
    assert_eq!((sp.last().unwrap() >> 16) & 255, 252);
    let rb = values(0x880b);
    assert!(rb.contains(&6));
    assert_eq!(*rb.last().unwrap(), 0);
    assert!(values(0x8094).contains(&1)); // GRAS_SU_DEPTH_PLANE_CNTL: late depth
    let mut invalid = p.clone();
    Arc::make_mut(&mut invalid.fragment.artifact)
        .metadata
        .writes_smask = false;
    assert!(validate_native_render_pipeline(&invalid).is_err());
    let mut invalid = p;
    Arc::make_mut(&mut invalid.fragment.artifact)
        .metadata
        .outputs
        .iter_mut()
        .find(|o| o.slot == 3)
        .unwrap()
        .half = true;
    assert!(validate_native_render_pipeline(&invalid).is_err());
    validate_with_images(&a, &case_images(&c)).unwrap();

    // Map both endpoints correctly for forward and reversed depth ranges.
    let mut c = c;
    c.operations.insert(
        1,
        Operation::SetViewport(Viewport::new(0.0, 0.0, 16.0, 16.0, 0.0, 1.0).unwrap()),
    );
    for [near, far] in [[0.125, 0.625], [1.0, 0.0]] {
        c.operations[1] =
            Operation::SetViewport(Viewport::new(0.0, 0.0, 16.0, 16.0, near, far).unwrap());
        let a = c.compile().unwrap();
        validate_with_images(&a, &case_images(&c)).unwrap();
        let viewport = adreno_a6xx_pm4::Packets::new(&a.words)
            .map(|p| p.unwrap())
            .filter(|p| {
                matches!(
                    p.header,
                    adreno_a6xx_pm4::Header::Type4 {
                        register: 0x8010,
                        ..
                    }
                )
            })
            .last()
            .unwrap();
        assert_eq!(f32::from_bits(viewport.payload[4]), near);
        assert_eq!(f32::from_bits(viewport.payload[5]), far - near);
        // The new native state must still reject clipping-disable/reserved
        // bits. Existing typed image authority and private-domain isolation apply.
        let clip = adreno_a6xx_pm4::Packets::new(&a.words)
            .map(|p| p.unwrap())
            .filter(|p| {
                matches!(
                    p.header,
                    adreno_a6xx_pm4::Header::Type4 {
                        register: 0x8000,
                        ..
                    }
                )
            })
            .last()
            .unwrap()
            .word_offset as usize
            + 1;
        for (word, value) in [(clip, 0xc1), (clip, 0x1c0)] {
            let mut invalid = a.clone();
            invalid.words[word] = value;
            assert!(
                validate_with_images(&invalid, &case_images(&c)).is_err(),
                "accepted word {word} value {value:#x}"
            );
        }
    }
}
