#![cfg(feature = "programmable")]
mod native_support;
use adreno_a6xx_pm4::{Header, Packets};
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use std::sync::Arc;

fn assert_programs(a: &RelocatablePm4, expected: usize) {
    for (register, opcode, block) in [(0xa81b, 0x32, 8), (0xa982, 0x34, 12)] {
        let packets: Vec<_> = Packets::new(&a.words).map(Result::unwrap).collect();
        assert_eq!(
            packets
                .iter()
                .filter(|p| matches!(p.header,
            Header::Type4 { register: r, count: 7 } if r == register))
                .count(),
            expected
        );
        assert_eq!(
            packets
                .iter()
                .filter(|p| matches!(p.header,
            Header::Type7 { opcode: op, .. } if op == opcode)
                    && (p.payload[0] >> 14) & 3 == 0
                    && (p.payload[0] >> 18) & 15 == block)
                .count(),
            expected
        );
    }
}

fn pipeline(c: &Case) -> Arc<NativeRenderPipeline> {
    c.operations
        .iter()
        .find_map(|op| match op {
            Operation::SetProgrammablePipeline(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap()
}

fn draw() -> Operation<'static> {
    Operation::Draw {
        vertex_count: 3,
        first_vertex: 0,
    }
}

fn packet_count(a: &RelocatablePm4, op: u8) -> usize {
    Packets::new(&a.words)
        .filter(|p| matches!(p.as_ref().unwrap().header,Header::Type7{opcode,..} if opcode==op))
        .count()
}
#[test]
fn repeated_sampled_draw_construction_cost() {
    let c = native_repeated_sampled_case(32);
    let a = c.compile().unwrap();
    assert_eq!(packet_count(&a, 0x38), 32);
    let single = native_repeated_sampled_case(1).compile().unwrap();
    assert_eq!(packet_count(&a, 0x3d), packet_count(&single, 0x3d));
    assert_eq!(packet_count(&a, 0x12), packet_count(&single, 0x12));
    assert_eq!(descriptor_tables(&a), descriptor_tables(&single));
    assert_programs(&a, 1);
    assert_eq!(packet_count(&a, 0x26), packet_count(&single, 0x26) + 31);
    // Every compile owns complete allocations, prologue writes and fixups;
    // a second compile cannot refer to objects from the first one.
    assert_eq!(c.compile().unwrap(), a);
    println!(
        "NATIVE_REUSE draws=32 words={} fixups={} objects={} object_bytes={} descriptor_writes={} memory_waits={} idle_waits={}",
        a.words.len(),
        a.fixups.len(),
        a.generated_objects.len(),
        a.generated_objects
            .iter()
            .map(|g| g.bytes.len())
            .sum::<usize>(),
        packet_count(&a, 0x3d),
        packet_count(&a, 0x12),
        packet_count(&a, 0x26)
    );
    let start = std::time::Instant::now();
    for _ in 0..200 {
        std::hint::black_box(c.compile().unwrap());
    }
    println!(
        "NATIVE_REUSE host_compile_200_us={}",
        start.elapsed().as_micros()
    );
}

fn descriptor_tables(a: &RelocatablePm4) -> usize {
    a.generated_objects
        .iter()
        .filter(|g| g.kind == GeneratedObjectKind::TextureState)
        .count()
}

#[test]
fn repeated_compute_reuses_descriptors_without_dropping_write_authority() {
    let mut c = compute_case("storage");
    let single = c.compile().unwrap();
    c.operations.pop();
    c.operations.push(Operation::Dispatch { x: 2, y: 1, z: 1 });
    c.operations.push(Operation::EndComputePass);
    let a = c.compile().unwrap();
    assert_eq!(packet_count(&a, 0x33), 2);
    assert_eq!(descriptor_tables(&a), descriptor_tables(&single));
    assert_eq!(packet_count(&a, 0x3d), packet_count(&single, 0x3d));
    assert_eq!(packet_count(&a, 0x12), packet_count(&single, 0x12));
    // Access merging may reorder records when the shader is loaded again.
    assert_eq!(a.accesses.len(), single.accesses.len());
    assert!(single.accesses.iter().all(|r| a.accesses.contains(r)));
    assert!(
        a.accesses
            .iter()
            .any(|r| matches!(r.object, ObjectRef::External(_))
                && r.access == Access::READ | Access::WRITE)
    );
}

#[test]
fn repeated_graphics_ibo_tables_share_across_stages_and_draws() {
    let mut c = native_graphics_storage_offset_case(4, false);
    let single = c.compile().unwrap();
    assert_eq!(descriptor_tables(&single), 1);
    c.operations.pop();
    for _ in 1..32 {
        c.operations.push(Operation::Draw {
            vertex_count: 3,
            first_vertex: 0,
        });
    }
    c.operations.push(Operation::EndRenderPass);
    let a = c.compile().unwrap();
    assert_eq!(packet_count(&a, 0x38), 32);
    assert_eq!(descriptor_tables(&a), 1);
    assert_eq!(packet_count(&a, 0x3d), packet_count(&single, 0x3d));
    assert_eq!(packet_count(&a, 0x12), packet_count(&single, 0x12));
    let resource = a
        .accesses
        .iter()
        .find(|r| r.object == ObjectRef::External(STORAGE))
        .unwrap();
    assert_eq!(resource.access, Access::READ);
    assert_eq!((resource.offset, resource.size), (0, 520));
}

#[test]
fn sampled_resource_rebind_and_return_preserve_both_tables_and_accesses() {
    let mut c = native_repeated_sampled_case(1);
    let other = ObjectId::new(999);
    let mut source = c.resources.iter().find(|r| r.id == SOURCE).unwrap().clone();
    source.id = other;
    c.resources.push(source);
    let original = c
        .operations
        .iter()
        .find_map(|op| match op {
            Operation::SetBindGroup { bindings, .. } => Some(bindings.clone()),
            _ => None,
        })
        .unwrap();
    let mut changed = original.clone();
    for b in &mut changed {
        if let NativeBindingResource::Texture { object, .. } = &mut b.resource {
            *object = other;
        }
    }
    c.operations.pop();
    for bindings in [changed, original] {
        c.operations
            .push(Operation::SetBindGroup { index: 0, bindings });
        c.operations.push(Operation::Draw {
            vertex_count: 6,
            first_vertex: 0,
        });
    }
    c.operations.push(Operation::EndRenderPass);
    let a = c.compile().unwrap();
    assert_eq!(packet_count(&a, 0x38), 3);
    assert_programs(&a, 1);
    assert_eq!(descriptor_tables(&a), 2);
    assert_eq!(packet_count(&a, 0x12), 2);
    for object in [SOURCE, other] {
        assert!(
            a.fixups
                .iter()
                .any(|f| f.object == ObjectRef::External(object)
                    && f.encoding == AddressEncoding::GpuVa49TexDescriptor
                    && f.access == Access::READ)
        );
        assert!(
            a.accesses
                .iter()
                .any(|r| r.object == ObjectRef::External(object) && r.access == Access::READ)
        );
    }
}

#[test]
fn pipeline_switch_invalidates_even_without_an_intervening_draw() {
    let mut c = render_case("fs");
    let a = pipeline(&c);
    // Same resource IDs and metadata, but a different immutable pipeline identity.
    let b = Arc::new((*a).clone());
    c.operations.pop();
    c.operations.extend([
        Operation::SetProgrammablePipeline(a.clone()),
        draw(),
        Operation::SetProgrammablePipeline(b.clone()),
        draw(),
        Operation::SetProgrammablePipeline(a.clone()),
        draw(),
        Operation::SetProgrammablePipeline(b),
        Operation::SetProgrammablePipeline(a),
        draw(),
        Operation::EndRenderPass,
    ]);
    let a = c.compile().unwrap();
    assert_eq!(packet_count(&a, 0x38), 5);
    assert_programs(&a, 4);
}

#[test]
fn pass_and_compute_boundaries_restore_the_same_pipeline() {
    for compute_between in [false, true] {
        let mut c = render_case("fs");
        let second = c.operations.clone();
        if compute_between {
            let compute = compute_case("storage");
            for r in compute.resources {
                if !c.resources.iter().any(|existing| existing.id == r.id) {
                    c.resources.push(r);
                }
            }
            c.operations.extend(compute.operations);
        }
        c.operations.extend(second);
        let a = c.compile().unwrap();
        assert_programs(&a, 2);
        assert_eq!(packet_count(&a, 0x38), 2);
        assert_eq!(packet_count(&a, 0x33), usize::from(compute_between));
    }
}

#[test]
fn scratch_identity_and_layout_changes_restore_complete_programs() {
    use sgfx_core::ir::BufferUsage;
    let mut c = render_case("fs");
    c.operations.pop();
    let mut p = (*pipeline(&c)).clone();
    for (object, bytes) in [(ObjectId::new(90), 512), (ObjectId::new(91), 1024)] {
        let layout = PrivateMemoryLayout::new(bytes, false).unwrap().unwrap();
        c.resources.push(buffer(
            object,
            layout.allocation_bytes,
            BufferUsage::STORAGE,
        ));
    }
    for (object, bytes) in [
        (ObjectId::new(90), 512),
        (ObjectId::new(91), 512),
        (ObjectId::new(91), 1024),
    ] {
        Arc::make_mut(&mut p.fragment.artifact).metadata.pvtmem_size = bytes;
        p.fragment.private_memory = Some(NativePrivateMemory { object });
        c.operations
            .push(Operation::SetProgrammablePipeline(Arc::new(p.clone())));
        c.operations.extend([draw(), draw()]);
    }
    c.operations.push(Operation::EndRenderPass);
    let a = c.compile().unwrap();
    assert_programs(&a, 4); // zero scratch, object change, then layout change
    assert_eq!(packet_count(&a, 0x38), 7);
    for (object, sizes) in [
        (ObjectId::new(90), vec![512]),
        (ObjectId::new(91), vec![512, 1024]),
    ] {
        let refs: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| f.object == ObjectRef::External(object))
            .collect();
        assert_eq!(refs.len(), sizes.len());
        for (r, bytes) in refs.iter().zip(sizes) {
            assert_eq!(r.access, Access::READ | Access::WRITE);
            assert_eq!(
                r.required_size,
                PrivateMemoryLayout::new(bytes, false)
                    .unwrap()
                    .unwrap()
                    .allocation_bytes
            );
        }
    }
}

#[test]
fn retained_program_keeps_graphics_push_constant_updates() {
    use sgfx_core::ir::{ProgrammableRenderPipelineDesc, PushConstantRange, ShaderStages};
    let mut c = render_case("fs");
    // Exercise graphics push upload policy with reflected metadata; this is
    // a packet test, not a claim that this fixture's ISA reads these constants.
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            let shader = Arc::make_mut(&mut p.fragment.artifact);
            shader.push_constant_bytes = 4;
            shader.metadata.constlen = shader.metadata.constlen.max(1);
            let d = &p.descriptor;
            p.descriptor = ProgrammableRenderPipelineDesc::new(
                d.vertex().clone(),
                d.fragment().clone(),
                d.layout()
                    .clone()
                    .with_push_constant_ranges(vec![
                        PushConstantRange::new(ShaderStages::FRAGMENT, 0, 4).unwrap(),
                    ])
                    .unwrap(),
                d.target_format(),
                d.vertex_buffer().cloned(),
                d.topology(),
                d.blend(),
                d.raster(),
            )
            .unwrap();
        }
    }
    c.operations.pop();
    c.operations.pop();
    for data in [&[0x17, 0xab, 0, 0][..], &[0x29, 0xcd, 0, 0][..]] {
        c.operations.push(Operation::SetPushConstants {
            stages: ShaderStages::FRAGMENT,
            offset: 0,
            data,
        });
        c.operations.push(draw());
    }
    c.operations.push(Operation::EndRenderPass);
    let a = c.compile().unwrap();
    assert_programs(&a, 1);
    let mut current = None;
    let mut values = Vec::new();
    for p in Packets::new(&a.words).map(Result::unwrap) {
        if matches!(p.header, Header::Type7 { opcode: 0x34, .. })
            && p.payload.len() == 7
            && p.payload[0] == (1 << 14 | 12 << 18 | 1 << 22)
        {
            current = Some(p.payload[3]);
        }
        if matches!(p.header, Header::Type7 { opcode: 0x38, .. }) {
            values.push(current.unwrap());
        }
    }
    assert_eq!(values, [0xab17, 0xcd29]);
}

#[test]
fn retained_program_still_emits_driver_parameters_and_validates_changed_bindings() {
    for extension in [false, true] {
        let c = native_draw_parameters_case(extension);
        let a = c.compile().unwrap();
        assert_programs(&a, 1);
        assert_eq!(packet_count(&a, 0x38), 3);
    }
    let mut c = native_repeated_sampled_case(1);
    c.operations.pop();
    c.operations.extend([
        Operation::SetVertexBufferSlot {
            slot: 0,
            buffer: VERTICES,
            offset: 96,
        },
        draw(),
        Operation::EndRenderPass,
    ]);
    assert_eq!(c.compile(), Err(CompileError::OutOfBounds));
}

#[test]
fn fixed_draw_between_native_draws_invalidates_program() {
    use sgfx_core::ir::*;
    let mut c = render_case("fs");
    let native = pipeline(&c);
    let fixed = PipelineMeta {
        id: PipelineId::new(0),
        descriptor: RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            VertexBufferLayout::new(
                40,
                vec![
                    VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                    VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                ],
            )
            .unwrap(),
            FragmentProgram::VertexColor,
            BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
            RasterState::new(CullMode::Back, FrontFace::CounterClockwise),
        )
        .unwrap(),
    };
    c.resources
        .iter_mut()
        .find(|r| r.id == VERTICES)
        .unwrap()
        .size = 256;
    c.operations.pop();
    c.operations.extend([
        Operation::SetPipeline(fixed.id),
        Operation::SetVertexBuffer {
            buffer: VERTICES,
            offset: 0,
        },
        Operation::SetUniforms(DrawUniforms::new(
            Transform::identity(),
            Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
        )),
        draw(),
        Operation::SetProgrammablePipeline(native),
        draw(),
        Operation::EndRenderPass,
    ]);
    let a = compile(CompileInput {
        capabilities: Capabilities::a618(512 * 1024, 32768),
        resources: &c.resources,
        pipelines: &[fixed],
        operations: &c.operations,
    })
    .unwrap();
    assert_programs(&a, 3);
    assert_eq!(
        a.fixups
            .iter()
            .filter(|f| matches!(f.object, ObjectRef::NativeShader(_)))
            .count(),
        8
    );
}
