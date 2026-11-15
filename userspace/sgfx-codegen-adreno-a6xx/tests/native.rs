#![cfg(feature = "programmable")]
mod native_support;
use adreno_a6xx_pm4::{Header, Packets};
use native_support::*;
use sgfx_codegen_adreno_a6xx::*;
use std::sync::Arc;

fn register(a: &RelocatablePm4, reg: u32) -> Vec<u32> {
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
fn packets(a: &RelocatablePm4, op: u8) -> Vec<Vec<u32>> {
    Packets::new(&a.words)
        .filter_map(|p| {
            let p = p.unwrap();
            match p.header {
                Header::Type7 { opcode, .. } if opcode == op => Some(p.payload.to_vec()),
                _ => None,
            }
        })
        .collect()
}
#[test]
fn real_draw_parameters_update_driver_constants_for_each_draw() {
    for extension in [false, true] {
        let c = native_draw_parameters_case(extension);
        let pipeline = c
            .operations
            .iter()
            .find_map(|o| match o {
                Operation::SetProgrammablePipeline(p) => Some(p),
                _ => None,
            })
            .unwrap();
        let m = &pipeline.vertex.artifact.metadata;
        let allocation = m.const_allocations.iter().find(|c| c.kind == 3).unwrap();
        assert!(m.need_driver_params);
        assert_eq!(allocation.offset_vec4, 4);
        assert_eq!(m.num_driver_params, 4); // this real shader does not read is_indexed_draw
        let a = c.compile().unwrap();
        let loads: Vec<_> = packets(&a, 0x32)
            .into_iter()
            .filter(|p| {
                p[0] & 0x3fff == allocation.offset_vec4
                    && (p[0] >> 14) & 3 == 1
                    && (p[0] >> 16) & 3 == 0
                    && (p[0] >> 18) & 15 == 8
            })
            .map(|p| p[3..].to_vec())
            .collect();
        assert_eq!(
            loads,
            vec![
                vec![13, 7, 11, u32::MAX],
                vec![19, (-5i32) as u32, 17, u32::MAX],
                vec![5, 2, 3, u32::MAX],
            ]
        );
        assert_eq!(register(&a, 0xa00e), [7, (-5i32) as u32, 2]);
        assert_eq!(register(&a, 0xa00f), [11, 17, 3]);
        let draws = packets(&a, 0x38);
        assert_eq!(draws.len(), 3);
        assert_eq!(draws[0], [0x84, 2, 3]);
        assert_eq!(draws[1][..4], [0x804, 2, 3, 0]);
        assert_eq!(draws[2], [0x84, 1, 3]);
    }
}
#[test]
fn native_depth_bias_encodes_finite_factors_and_resets_for_the_next_draw() {
    for (constant, slope) in [(1.0, 2.0), (-2.5, 0.75), (0.0, -1.0), (3.0, 0.0)] {
        let c = native_depth_bias_case(constant, slope);
        let a = c.compile().unwrap();
        assert_eq!(*register(&a, 0x8095).last().unwrap(), slope.to_bits());
        assert_eq!(*register(&a, 0x8096).last().unwrap(), constant.to_bits());
        assert_eq!(*register(&a, 0x8097).last().unwrap(), 0);
        assert_eq!(*register(&a, 0x8090).last().unwrap(), 0x2010 | 6 | 1 << 11);
        assert_eq!(*register(&a, 0x8098).last().unwrap(), 4); // existing D32 mode
    }
    let mut c = native_depth_bias_case(2.0, -0.5);
    let mut unbiased = c
        .operations
        .iter()
        .find_map(|o| match o {
            Operation::SetProgrammablePipeline(p) => Some((**p).clone()),
            _ => None,
        })
        .unwrap();
    unbiased.descriptor = unbiased.descriptor.clone().with_raster_state(
        unbiased
            .descriptor
            .raster()
            .with_depth_bias(0.0, 0.0)
            .unwrap(),
    );
    c.operations.pop(); // EndRenderPass
    c.operations
        .push(Operation::SetProgrammablePipeline(Arc::new(unbiased)));
    c.operations.push(Operation::Draw {
        vertex_count: 3,
        first_vertex: 0,
    });
    c.operations.push(Operation::EndRenderPass);
    let a = c.compile().unwrap();
    assert_eq!(register(&a, 0x8095), [0, (-0.5f32).to_bits(), 0]);
    assert_eq!(register(&a, 0x8096), [0, 2.0f32.to_bits(), 0]);
    assert_eq!(*register(&a, 0x8090).last().unwrap() & (1 << 11), 0);
    assert!(
        sgfx_core::ir::RasterState::new(
            sgfx_core::ir::CullMode::None,
            sgfx_core::ir::FrontFace::CounterClockwise
        )
        .with_depth_bias(f32::NAN, 0.0)
        .is_err()
    );
}
#[test]
fn real_render_fixtures_use_external_cached_code_and_dynamic_linkage() {
    for name in ["fs", "horizontal", "vertical", "glass", "erase"] {
        let case = render_case(name);
        let a = case.compile().unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(
            a.fixups
                .iter()
                .all(|f| !matches!(f.object, ObjectRef::CanonicalShader(_)))
        );
        for id in [VS, FS] {
            let size = case.resources.iter().find(|r| r.id == id).unwrap().size;
            let code: Vec<_> = a
                .fixups
                .iter()
                .filter(|f| f.object == ObjectRef::NativeShader(id))
                .collect();
            assert_eq!(code.len(), 2);
            assert!(code.iter().all(|f| f.access == Access::READ
                && f.required_size == size
                && f.object_offset == 0));
            assert!(
                a.accesses
                    .iter()
                    .any(|f| f.object == ObjectRef::External(id))
            );
            assert!(
                a.accesses
                    .iter()
                    .all(|f| !matches!(f.object, ObjectRef::NativeShader(_)))
            );
        }
        assert_eq!(
            packets(&a, 0x38).last().unwrap(),
            &[0x84, 1, if name == "fs" { 3 } else { 6 }]
        );
    }
    let a = render_fixture("fs");
    let p = render_pipeline("fs");
    let f = &p.fragment.artifact.metadata;
    let v = &p.vertex.artifact.metadata;
    let input = f.inputs.iter().find(|i| !i.sysval).unwrap();
    let output = v.outputs.iter().find(|o| o.slot == input.slot).unwrap();
    assert_eq!(
        register(&a, 0xa803)[0] & 0xffff,
        output.regid | input.compmask << 8
    );
}
#[test]
fn glass_keeps_texture_slot_one_and_sampler_slot_two_independent() {
    let a = render_fixture("horizontal");
    let texture = a
        .fixups
        .iter()
        .find(|f| {
            f.object == ObjectRef::External(SOURCE)
                && f.encoding == AddressEncoding::GpuVa49TexDescriptor
        })
        .unwrap();
    let word = texture.word_offset as usize;
    // A CP_MEM_WRITE puts address at descriptor dword4, following destination pair.
    let table_dest = a
        .fixups
        .iter()
        .find(|f| f.word_offset + 6 == texture.word_offset)
        .unwrap();
    assert_eq!(table_dest.object_offset, 64);
    assert_eq!(texture.required_size, 1024);
    assert_eq!(a.words[word + 1] >> 17, 1);
    let sampler_fix = a
        .fixups
        .iter()
        .find(|f| {
            f.required_size == 48
                && matches!(f.object, ObjectRef::Generated(_))
                && f.access == Access::READ
        })
        .unwrap();
    let ObjectRef::Generated(id) = sampler_fix.object else {
        unreachable!()
    };
    let table = a.generated_objects.iter().find(|g| g.id == id).unwrap();
    assert_eq!(table.bytes.len(), 48);
    assert!(table.bytes[..32].iter().all(|b| *b == 0));
    assert!(table.bytes[32..].iter().any(|b| *b != 0));
    assert!(
        a.fixups
            .iter()
            .filter(|f| f.object == ObjectRef::External(UBO))
            .all(|f| f.object_offset + f.required_size <= 1120)
    );
    // IR3 promoted 1152 bytes, while binding is exactly1120: tail is explicit zeros.
    assert!(packets(&a, 0x34).iter().any(|p| p.len() == 11
        && p[0] == 70 | 1 << 14 | 12 << 18 | 2 << 22
        && p[3..].iter().all(|w| *w == 0)));
    let ubo = a
        .fixups
        .iter()
        .find(|f| {
            f.object == ObjectRef::External(UBO)
                && f.encoding == AddressEncoding::GpuVa49UboDescriptor
        })
        .unwrap();
    assert_eq!(a.words[ubo.word_offset as usize + 1], 70 << 17);
}
#[test]
fn compute_uses_reflected_local_size_ssbo_permissions_and_exec_cs() {
    for name in ["cs", "push", "storage"] {
        let a = compute_case(name)
            .compile()
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(packets(&a, 0x33), vec![vec![0, 2, 1, 1]]);
        assert!(packets(&a, 0x38).is_empty());
        let local = compute_pipeline(name).shader.artifact.metadata.local_size[0];
        assert_eq!(register(&a, 0xb991), vec![local * 2]);
        assert!(
            a.fixups
                .iter()
                .any(|f| f.object == ObjectRef::External(STORAGE) && f.required_size == 512)
        );
        assert_eq!(
            a.fixups
                .iter()
                .filter(|f| f.object == ObjectRef::NativeShader(CS))
                .count(),
            2
        );
    }
}
#[test]
fn validators_reject_unsupported_metadata_and_missing_link_without_resources() {
    let mut p = render_pipeline("fs");
    assert!(validate_native_render_pipeline(&p).is_ok());
    Arc::make_mut(&mut p.vertex.artifact).metadata.pvtmem_size = 128;
    assert!(validate_native_render_pipeline(&p).is_err());
    let mut p = render_pipeline("fs");
    Arc::make_mut(&mut p.vertex.artifact)
        .metadata
        .outputs
        .retain(|o| o.slot == 0);
    assert!(validate_native_render_pipeline(&p).is_err());
    let mut p = compute_pipeline("cs");
    Arc::make_mut(&mut p.shader.artifact).metadata.local_size = [1024, 2, 1];
    assert!(validate_native_compute_pipeline(&p).is_err());
}
#[test]
fn missing_bindings_write_authority_bounds_and_pass_order_are_rejected() {
    let mut c = compute_case("cs");
    c.operations
        .retain(|o| !matches!(o, Operation::SetBindGroup { .. }));
    assert!(c.compile().is_err());
    let mut c = compute_case("cs");
    for o in &mut c.operations {
        if let Operation::SetBindGroup { bindings, .. } = o {
            if let NativeBindingResource::Buffer { write, .. } = &mut bindings[0].resource {
                *write = false;
            }
        }
    }
    assert!(c.compile().is_err());
    let mut c = compute_case("cs");
    c.resources
        .iter_mut()
        .find(|r| r.id == STORAGE)
        .unwrap()
        .size = 256;
    assert!(c.compile().is_err());
    let mut c = compute_case("cs");
    c.operations.pop();
    assert!(c.compile().is_err());
    let mut c = compute_case("cs");
    c.operations
        .insert(0, Operation::Dispatch { x: 1, y: 1, z: 1 });
    assert!(c.compile().is_err());
}
#[test]
fn rodata_is_data_authority_and_full_padded_code_stays_cached() {
    let p = render_pipeline("fs");
    let a = render_fixture("fs");
    let m = &p.vertex.artifact.metadata;
    assert!(m.constant_data_bytes > 0);
    assert!(a.fixups.iter().any(|f| f.object == ObjectRef::External(VS)
        && f.object_offset == u64::from(m.constant_data_offset)
        && f.encoding == AddressEncoding::GpuVa49UboDescriptor));
    assert!(
        a.generated_objects
            .iter()
            .all(|g| g.bytes != p.vertex.artifact.allocation)
    );
}
#[test]
fn instanced_ranges_use_base_instance_after_divisor_and_validate_vertex_slots() {
    use sgfx_core::ir::*;
    let mut c = render_case("glass");
    for op in &mut c.operations {
        match op {
            Operation::SetProgrammablePipeline(p) => {
                let p = Arc::make_mut(p);
                p.descriptor = p
                    .descriptor
                    .clone()
                    .with_vertex_buffers(vec![
                        VertexBufferLayout::new(
                            16,
                            vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)],
                        )
                        .unwrap()
                        .with_step_mode(VertexStepMode::Instance)
                        .with_instance_divisor(2)
                        .unwrap(),
                    ])
                    .unwrap();
            }
            Operation::Draw { .. } => {
                *op = Operation::DrawInstanced {
                    vertex_count: 6,
                    instance_count: 5,
                    first_vertex: 0,
                    first_instance: 3,
                }
            }
            _ => {}
        }
    }
    let a = c.compile().unwrap();
    assert_eq!(register(&a, 0xa00f), vec![3]);
    assert_eq!(register(&a, 0xa091), vec![2]);
    assert_ne!(register(&a, 0xa090)[0] & (1 << 17), 0);
    assert_eq!(packets(&a, 0x38).last().unwrap(), &[0x84, 5, 6]);
    c.resources
        .iter_mut()
        .find(|r| r.id == VERTICES)
        .unwrap()
        .size = 80;
    assert_eq!(c.compile(), Err(CompileError::OutOfBounds));
    let mut c = render_case("glass");
    c.operations
        .retain(|o| !matches!(o, Operation::SetVertexBufferSlot { .. }));
    assert!(c.compile().is_err());
}
#[test]
fn padded_uniform_copy_waits_each_chunk_and_never_reads_past_binding() {
    let mut c = compute_case("cs");
    c.resources.iter_mut().find(|r| r.id == UBO).unwrap().size = 516;
    for op in &mut c.operations {
        if let Operation::SetBindGroup { bindings, .. } = op {
            for b in bindings {
                if let NativeBindingResource::Buffer { object, size, .. } = &mut b.resource {
                    if *object == UBO {
                        *size = 516;
                    }
                }
            }
        }
    }
    let a = c.compile().unwrap();
    let ps: Vec<_> = Packets::new(&a.words).map(|p| p.unwrap()).collect();
    let mut copies = 0;
    for (i, p) in ps.iter().enumerate() {
        if matches!(p.header, Header::Type7 { opcode: 0x75, .. }) {
            copies += 1;
            assert!(matches!(
                ps[i + 1].header,
                Header::Type7 {
                    opcode: adreno_a6xx_pm4::opcode::WAIT_MEM_WRITES,
                    ..
                }
            ));
        }
    }
    assert_eq!(copies, 2);
    assert!(
        a.fixups
            .iter()
            .filter(|f| f.object == ObjectRef::External(UBO))
            .all(|f| f.object_offset + f.required_size <= 516)
    );
    assert!(
        a.fixups
            .iter()
            .any(|f| matches!(f.object, ObjectRef::Generated(_))
                && f.required_size == 528
                && f.encoding == AddressEncoding::GpuVa49UboDescriptor)
    );
}
#[test]
fn sequential_dispatches_preserve_push_updates_and_clean_before_invalidating() {
    let mut c = compute_case("push");
    c.operations.pop();
    c.operations.extend([
        Operation::SetPushConstants {
            stages: sgfx_core::ir::ShaderStages::COMPUTE,
            offset: 0,
            data: &[9, 0, 0, 0],
        },
        Operation::Dispatch { x: 1, y: 1, z: 1 },
        Operation::EndComputePass,
    ]);
    let a = c.compile().unwrap();
    let constants = packets(&a, 0x34);
    assert!(
        constants
            .iter()
            .any(|p| p.len() == 7 && p[0] == (1 << 14 | 13 << 18 | 1 << 22) && p[3] == 7)
    );
    assert!(
        constants
            .iter()
            .any(|p| p.len() == 7 && p[0] == (1 << 14 | 13 << 18 | 1 << 22) && p[3] == 9)
    );
    let ps: Vec<_> = Packets::new(&a.words).map(|p| p.unwrap()).collect();
    let mut dispatches = 0;
    for (i, p) in ps.iter().enumerate() {
        if matches!(p.header, Header::Type7 { opcode: 0x33, .. }) {
            dispatches += 1;
            let clean = ps[i + 1];
            assert_eq!(clean.payload[0], 0x40000004);
            assert!(clean.payload[3] > 0);
            assert!(
                a.fixups
                    .iter()
                    .any(|f| f.word_offset == clean.word_offset + 2
                        && f.access == Access::WRITE
                        && f.required_size == 4)
            );
            assert_eq!(ps[i + 2].payload, &[0x31]);
            assert!(matches!(
                ps[i + 3].header,
                Header::Type7 {
                    opcode: adreno_a6xx_pm4::opcode::WAIT_FOR_IDLE,
                    ..
                }
            ));
        }
    }
    assert_eq!(dispatches, 2);
}
#[test]
fn descriptor_prologue_is_included_in_command_budget() {
    let c = render_case("horizontal");
    let count = c.compile().unwrap().words.len();
    assert_eq!(
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, count as u32 - 1),
            resources: &c.resources,
            pipelines: &[],
            operations: &c.operations
        }),
        Err(CompileError::CommandBudgetExceeded)
    );
}
#[test]
fn sparse_flat_components_are_packed_in_interpolation_state() {
    let mut c = render_case("fs");
    for op in &mut c.operations {
        if let Operation::SetProgrammablePipeline(p) = op {
            let p = Arc::make_mut(p);
            let f = Arc::make_mut(&mut p.fragment.artifact);
            let i = f.metadata.inputs.iter_mut().find(|i| !i.sysval).unwrap();
            i.compmask = 5;
            i.flat = true;
        }
    }
    let a = c.compile().unwrap();
    assert_eq!(register(&a, 0x9200), vec![5]);
}
#[test]
fn preload_is_capped_to_a618_cache_but_authorizes_full_program_allocation() {
    let mut c = compute_case("storage");
    for op in &mut c.operations {
        if let Operation::SetComputePipeline(p) = op {
            let shader = Arc::make_mut(&mut Arc::make_mut(p).shader.artifact);
            shader.allocation.resize(65 * 128, 0);
            shader.metadata.allocation_bytes = 65 * 128;
            shader.metadata.instrlen = 65;
            shader.metadata.sizedwords = 65 * 32;
        }
    }
    c.resources.iter_mut().find(|r| r.id == CS).unwrap().size = 65 * 128;
    let a = c.compile().unwrap();
    assert_eq!(register(&a, 0xa9bc), vec![65]);
    assert!(
        packets(&a, 0x34)
            .iter()
            .any(|p| p.len() == 3 && p[0] == (2 << 16 | 13 << 18 | 64 << 22))
    );
    assert!(
        a.fixups
            .iter()
            .filter(|f| f.object == ObjectRef::NativeShader(CS))
            .all(|f| f.required_size == 65 * 128)
    );
}
// Synthetic interface mutations isolate table joining from the shader compiler;
// executable fixture bytes remain unchanged and these tests never run a GPU.
fn storage_graphics_case(vertex_write: bool, fragment_write: bool) -> Case {
    let mut c = render_case("fs");
    c.resources
        .push(buffer(STORAGE, 512, sgfx_core::ir::BufferUsage::STORAGE));
    for op in &mut c.operations {
        match op {
            Operation::SetProgrammablePipeline(p) => {
                let p = Arc::make_mut(p);
                for (shader, write, min) in [
                    (&mut p.vertex, vertex_write, 64),
                    (&mut p.fragment, fragment_write, 128),
                ] {
                    let a = Arc::make_mut(&mut shader.artifact);
                    a.bindings = vec![sgfx_shader_ir3::Binding {
                        group: 0,
                        binding: 0,
                        slot: 0,
                        kind: sgfx_shader_ir3::ResourceKind::StorageBuffer,
                        writable: write,
                        minimum_size: min,
                    }];
                    a.metadata.num_uavs = 1;
                    a.metadata.num_ssbos = 1;
                    a.metadata.has_ssbo = true;
                }
            }
            Operation::SetBindGroup { bindings, .. } => {
                *bindings = vec![NativeBinding {
                    binding: 0,
                    resource: NativeBindingResource::Buffer {
                        object: STORAGE,
                        offset: 0,
                        size: 512,
                        write: true,
                    },
                }]
            }
            _ => {}
        }
    }
    c
}
#[test]
fn graphics_rejects_cross_module_uav_slot_collision_before_admission() {
    let mut p = render_pipeline("fs");
    let c = storage_graphics_case(false, true);
    for op in c.operations {
        if let Operation::SetProgrammablePipeline(value) = op {
            p = (*value).clone();
        }
    }
    assert!(validate_native_render_pipeline(&p).is_ok());
    Arc::make_mut(&mut p.fragment.artifact).bindings[0].group = 1;
    assert_eq!(
        validate_native_render_pipeline(&p),
        Err(CompileError::UnsupportedFeatureReason(
            "native graphics UAV slot collision"
        ))
    );
    Arc::make_mut(&mut p.fragment.artifact).bindings[0].group = 0;
    Arc::make_mut(&mut p.fragment.artifact).bindings[0].kind =
        sgfx_shader_ir3::ResourceKind::UniformBuffer;
    assert_eq!(
        validate_native_render_pipeline(&p),
        Err(CompileError::UnsupportedFeatureReason(
            "native graphics binding type mismatch"
        ))
    );
}
#[test]
fn graphics_shared_binding_joins_write_permissions_and_maximum_minimum_size() {
    for (vs_write, fs_write) in [(true, false), (false, true)] {
        let mut c = storage_graphics_case(vs_write, fs_write);
        let a = c.compile().unwrap();
        let refs: Vec<_> = a
            .fixups
            .iter()
            .filter(|f| {
                f.object == ObjectRef::External(STORAGE)
                    && f.encoding == AddressEncoding::GpuVa49TexDescriptor
            })
            .collect();
        // VS and FS share the same table, including their joined write authority.
        assert_eq!(refs.len(), 1);
        assert!(
            refs.iter()
                .all(|f| f.access == Access::READ | Access::WRITE)
        );
        for op in &mut c.operations {
            if let Operation::SetBindGroup { bindings, .. } = op {
                if let NativeBindingResource::Buffer { write, .. } = &mut bindings[0].resource {
                    *write = false;
                }
            }
        }
        assert_eq!(c.compile(), Err(CompileError::InvalidResource));
    }
    let mut c = storage_graphics_case(false, true);
    for op in &mut c.operations {
        if let Operation::SetBindGroup { bindings, .. } = op {
            if let NativeBindingResource::Buffer { size, .. } = &mut bindings[0].resource {
                *size = 96;
            }
        }
    }
    assert!(
        c.compile().is_err(),
        "96 satisfies VS minimum64 but not FS minimum128"
    );
}
#[test]
fn native_indexed_instances_keep_index_origin_signed_base_and_instance_origin() {
    let mut c = render_case("glass");
    let index = ObjectId::new(30);
    c.resources
        .push(buffer(index, 16, sgfx_core::ir::BufferUsage::INDEX));
    let draw = c
        .operations
        .iter()
        .position(|o| matches!(o, Operation::Draw { .. }))
        .unwrap();
    c.operations[draw] = Operation::DrawIndexedInstanced {
        index_count: 6,
        instance_count: 2,
        first_index: 1,
        base_vertex: -1,
        first_instance: 5,
    };
    c.operations.insert(
        draw,
        Operation::SetIndexBuffer {
            buffer: index,
            offset: 2,
            format: sgfx_core::ir::IndexFormat::Uint16,
        },
    );
    let a = c.compile().unwrap();
    let draws = packets(&a, 0x38);
    let p = draws.last().unwrap();
    assert_eq!(p[..4], [0x404, 2, 6, 1]);
    assert_eq!(p[6], 7);
    assert_eq!(register(&a, 0xa00e), vec![u32::MAX]);
    assert_eq!(register(&a, 0xa00f), vec![5]);
    assert!(
        a.fixups
            .iter()
            .any(|f| f.object == ObjectRef::External(index)
                && f.object_offset == 2
                && f.required_size == 14
                && f.access == Access::READ)
    );
    c.resources.iter_mut().find(|r| r.id == index).unwrap().size = 14;
    assert_eq!(c.compile(), Err(CompileError::OutOfBounds));
}
