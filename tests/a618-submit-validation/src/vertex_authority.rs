//! Actual IR3 fixtures -> production codegen -> production submit validator.
use super::*;
use sgfx_codegen_adreno_a6xx::{ObjectId, Operation};
use sgfx_core::ir::{
    BufferUsage, IndexFormat, VertexAttribute, VertexBufferLayout, VertexFormat, VertexStepMode,
};
use std::sync::Arc;

const OFFSET: u64 = 64;
const CAPACITY: u64 = 196_608 * 40;

fn case(
    indexed: bool,
    divisor: Option<u32>,
    first: u32,
    instances: u32,
    base: i32,
) -> fixtures::Case {
    let mut c = fixtures::render_case("glass");
    let mut layout = VertexBufferLayout::new(
        32,
        std::vec![VertexAttribute::new(0, VertexFormat::Float32x4, 4)],
    )
    .unwrap();
    if let Some(divisor) = divisor {
        layout = layout
            .with_step_mode(VertexStepMode::Instance)
            .with_instance_divisor(divisor)
            .unwrap();
    }
    for op in &mut c.operations {
        match op {
            Operation::SetProgrammablePipeline(p) => {
                let p = Arc::make_mut(p);
                p.descriptor = p
                    .descriptor
                    .clone()
                    .with_vertex_buffers(std::vec![layout.clone()])
                    .unwrap();
            }
            Operation::SetVertexBufferSlot { offset, .. } => *offset = OFFSET,
            Operation::Draw { .. } => {
                *op = if indexed {
                    Operation::DrawIndexedInstanced {
                        index_count: 3,
                        instance_count: instances,
                        first_index: 2,
                        base_vertex: base,
                        first_instance: first,
                    }
                } else {
                    Operation::DrawInstanced {
                        vertex_count: 3,
                        instance_count: instances,
                        first_vertex: first,
                        first_instance: first,
                    }
                }
            }
            _ => {}
        }
    }
    set_size(&mut c, CAPACITY);
    if indexed {
        let index = ObjectId::new(90);
        c.resources
            .push(fixtures::buffer(index, 32, BufferUsage::INDEX));
        let pos = c.operations.len() - 2;
        c.operations.insert(
            pos,
            Operation::SetIndexBuffer {
                buffer: index,
                offset: 0,
                format: IndexFormat::Uint32,
            },
        );
    }
    c
}

fn set_size(c: &mut fixtures::Case, size: u64) {
    c.resources
        .iter_mut()
        .find(|r| r.id == fixtures::VERTICES)
        .unwrap()
        .size = size;
}

fn check(c: &fixtures::Case, expected: u64) -> RelocatablePm4 {
    let a = c.compile().unwrap();
    validate_with_images(&a, &case_images(c)).unwrap();
    let object = ObjectRef::External(fixtures::VERTICES);
    let access: Vec<_> = a.accesses.iter().filter(|r| r.object == object).collect();
    assert_eq!(access.len(), 1);
    assert_eq!((access[0].offset, access[0].size), (OFFSET, expected));
    let fixup = a.fixups.iter().find(|r| r.object == object).unwrap();
    assert_eq!(
        (fixup.object_offset, fixup.required_size),
        (OFFSET, expected)
    );
    // VFD_FETCH[0].SIZE must match the narrowed relocation, not allocation capacity.
    let size = adreno_a6xx_pm4::Packets::new(&a.words)
        .map(|p| p.unwrap())
        .find(|p| {
            matches!(
                p.header,
                adreno_a6xx_pm4::Header::Type4 {
                    register: 0xa012,
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(size.payload[0], expected as u32);
    a
}

#[test]
fn native_vertex_authority_nonindexed_exact_end_and_truncation() {
    for first in [0, 5, 1000] {
        let mut c = case(false, None, first, 1, 0);
        let required = u64::from(first + 2) * 32 + 20;
        let a = check(&c, required);
        let mut bad = a.clone();
        mutate_register(&mut bad, 0xa012, 0, 0);
        assert!(validate_with_images(&bad, &case_images(&c)).is_err());
        set_size(&mut c, OFFSET + required);
        check(&c, required);
        set_size(&mut c, OFFSET + required - 1);
        assert!(c.compile().is_err());
    }
}

#[test]
fn native_vertex_authority_divisors_apply_to_instances_even_when_indexed() {
    for indexed in [false, true] {
        for divisor in [1, 2, 5, u32::MAX] {
            for instances in [1, 2, 6] {
                let mut c = case(indexed, Some(divisor), 7, instances, -3);
                let required = u64::from(7 + (instances - 1) / divisor) * 32 + 20;
                check(&c, required);
                set_size(&mut c, OFFSET + required);
                check(&c, required);
                set_size(&mut c, OFFSET + required - 1);
                assert!(c.compile().is_err());
            }
        }
    }
}

#[test]
fn native_vertex_authority_indexed_vertex_rate_keeps_unknown_index_bounds() {
    for base in [-3, 0, 7] {
        let c = case(true, None, 5, 2, base);
        check(&c, CAPACITY - OFFSET);
    }
}
