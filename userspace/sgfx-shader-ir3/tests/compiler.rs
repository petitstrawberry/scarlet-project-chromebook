#![cfg(feature = "mesa")]
use sgfx_shader_ir3::{ResourceKind, Source, Stage, compile};
const PROCEDURAL: &str = include_str!("shaders/procedural.wgsl");
#[test]
fn arbitrary_vertex_retains_embedded_constants_and_fragment_changes_isa() {
    let vs = compile(Source::Wgsl(PROCEDURAL), Stage::Vertex, "vs").unwrap();
    assert!(vs.metadata.constant_data_bytes >= 24);
    assert!(vs.allocation.len() > vs.metadata.sizedwords as usize * 4);
    let start = vs.metadata.constant_data_offset as usize;
    let end = start + vs.metadata.constant_data_bytes as usize;
    let bytes = &vs.allocation[start..end];
    assert!(bytes.windows(4).any(|v| v == (-0.8f32).to_le_bytes()));
    let fs = compile(Source::Wgsl(PROCEDURAL), Stage::Fragment, "fs").unwrap();
    let modified = PROCEDURAL.replace(
        "input.color * input.color + vec3(0.05)",
        "input.color * input.color * input.color + vec3(0.17)",
    );
    let changed = compile(Source::Wgsl(&modified), Stage::Fragment, "fs").unwrap();
    assert_ne!(fs.allocation, changed.allocation);
    assert!(!fs.metadata.inputs.is_empty());
    let roundtrip = compile(Source::SpirV(&fs.spirv), Stage::Fragment, "fs").unwrap();
    assert_eq!(fs.allocation, roundtrip.allocation);
}
#[test]
fn compute_storage_output_has_real_buffer_and_workgroup_metadata() {
    let cs = compile(
        Source::Wgsl(include_str!("shaders/compute.wgsl")),
        Stage::Compute,
        "cs",
    )
    .unwrap();
    assert_eq!(cs.metadata.local_size, [64, 1, 1]);
    assert!(cs.metadata.has_ssbo);
    assert_eq!(cs.bindings[0].kind, ResourceKind::StorageBuffer);
    assert!(cs.bindings[0].writable);
    assert_eq!(cs.bindings[1].minimum_size, 16);
    assert!(cs.metadata.compute.is_some());
    assert!(cs.metadata.num_uavs >= 1);
}
#[test]
fn glass_all_six_entries_compile_uniform_arrays_textures_and_loops() {
    let source = include_str!("shaders/glass.wgsl");
    for (stage, entry) in [
        (Stage::Vertex, "blur_vertex"),
        (Stage::Vertex, "glass_vertex"),
        (Stage::Fragment, "horizontal"),
        (Stage::Fragment, "vertical"),
        (Stage::Fragment, "glass"),
        (Stage::Fragment, "erase"),
    ] {
        let shader =
            compile(Source::Wgsl(source), stage, entry).unwrap_or_else(|e| panic!("{entry}: {e}"));
        assert_eq!(shader.bindings[0].minimum_size, 1120);
        let slots: Vec<_> = shader.bindings.iter().map(|b| b.slot).collect();
        if matches!(entry, "horizontal" | "vertical" | "glass") {
            assert_eq!(slots, [0, 1, 2]);
        } else {
            assert_eq!(slots, [0]);
        }
        assert_eq!(shader.metadata.const_allocations[5].size_vec4, 0);
        assert_eq!(shader.metadata.num_sampler_prefetch, 0);
        assert!(shader.metadata.num_app_ubos >= 1);
        assert!(!shader.allocation.is_empty());
        if matches!(entry, "horizontal" | "vertical" | "glass") {
            assert!(shader.metadata.num_samp >= 2);
        }
    }
}
#[test]
fn malformed_and_wrong_entry_are_errors() {
    assert!(compile(Source::Wgsl("broken"), Stage::Vertex, "vs").is_err());
    assert!(compile(Source::Wgsl(PROCEDURAL), Stage::Compute, "vs").is_err());
    assert!(compile(Source::SpirV(&[0; 5]), Stage::Vertex, "vs").is_err());
}
#[test]
fn independent_callers_compile_deterministically() {
    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                compile(Source::Wgsl(PROCEDURAL), Stage::Fragment, "fs")
                    .unwrap()
                    .allocation
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(results.windows(2).all(|v| v[0] == v[1]));
}

#[test]
fn validate_all_entries_without_entry_selection() {
    use sgfx_shader_ir3::validate;
    validate(Source::Wgsl(PROCEDURAL)).unwrap();
    validate(Source::Wgsl(include_str!("shaders/glass.wgsl"))).unwrap();
    assert!(
        validate(Source::Wgsl(
            "@fragment fn broken() -> @location(0) vec4<f32> { return 1u; }"
        ))
        .is_err()
    );
}

#[test]
fn push_constants_and_sparse_binding_map_are_real_and_stage_scoped() {
    let source = include_str!("shaders/push.wgsl");
    let cs = compile(Source::Wgsl(source), Stage::Compute, "cs").unwrap();
    assert_eq!(cs.push_constant_bytes, 4);
    assert_eq!(cs.metadata.const_allocations[0].offset_vec4, 0);
    assert!(cs.metadata.const_allocations[0].size_vec4 >= 1);
    assert_eq!(
        (
            cs.bindings[0].group,
            cs.bindings[0].binding,
            cs.bindings[0].slot
        ),
        (3, 9, 0)
    );
    assert_eq!(cs.metadata.local_size, [128, 1, 1]);
    let vs = compile(Source::Wgsl(source), Stage::Vertex, "vs").unwrap();
    assert!(vs.bindings.is_empty());
    assert_eq!(vs.push_constant_bytes, 0);
}

#[test]
fn readonly_storage_loads_and_two_writable_buffers_compile() {
    let source = include_str!("shaders/storage.wgsl");
    let cs = compile(Source::Wgsl(source), Stage::Compute, "cs").unwrap();
    assert!(!cs.bindings[0].writable);
    assert!(cs.bindings[1].writable && cs.bindings[2].writable);
    assert!(cs.metadata.num_uavs >= 3);
    assert!(cs.metadata.has_ssbo); // IR3 may choose LDIB instead of an ISAM descriptor.
}

// Decode only the instruction classes relevant to these compiler regressions.
// Encodings are from pinned Mesa src/freedreno/isa/ir3-cat{6,7}.xml; skip
// trailing allocation padding/constant data, which are not instructions.
fn count_ir3_op(shader: &sgfx_shader_ir3::CompiledShader, category: u64, opcode: u64) -> usize {
    shader.allocation[..shader.metadata.sizedwords as usize * 4]
        .chunks_exact(8)
        .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        .filter(|word| {
            word >> 61 == category
                && match category {
                    6 => (word >> 54) & 31 == opcode,
                    7 => (word >> 55) & 15 == opcode,
                    _ => false,
                }
        })
        .count()
}
const WORKGROUP: &str = include_str!("shaders/workgroup.wgsl");

#[test]
fn shared_reduction_emits_local_load_store_and_cross_wave_barriers() {
    let cs = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "reduction").unwrap();
    assert_eq!(cs.metadata.local_size, [128, 1, 1]);
    assert_eq!(cs.metadata.shared_size, 128 * 4);
    assert!(cs.metadata.has_barrier);
    assert!(count_ir3_op(&cs, 6, 1) > 0, "LDL shared loads must survive");
    assert!(
        count_ir3_op(&cs, 6, 4) > 0,
        "STL shared stores must survive"
    );
    assert!(count_ir3_op(&cs, 7, 0) >= 2, "BAR rendezvous must survive");
    assert!(
        count_ir3_op(&cs, 7, 1) > 0,
        "shared-memory FENCE must survive"
    );
    let roundtrip = compile(Source::SpirV(&cs.spirv), Stage::Compute, "reduction").unwrap();
    assert_eq!(roundtrip.metadata.shared_size, cs.metadata.shared_size);
    assert!(count_ir3_op(&roundtrip, 7, 0) >= 2);
}

#[test]
fn shared_atomic_rmw_is_zero_initialized_and_entry_scoped() {
    let cs = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "atomic_sum").unwrap();
    assert_eq!(
        cs.metadata.shared_size, 4,
        "unused arrays must not allocate shared memory"
    );
    assert!(cs.metadata.has_barrier);
    assert!(
        count_ir3_op(&cs, 6, 16) > 0,
        "local ATOMIC.ADD must survive"
    );
    assert!(
        count_ir3_op(&cs, 6, 4) > 0,
        "implicit WGSL zero init must emit STL"
    );
    assert!(
        count_ir3_op(&cs, 7, 0) >= 2,
        "zero init and user rendezvous must survive"
    );
    let plain = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "plain").unwrap();
    assert_eq!(plain.metadata.shared_size, 0);
    assert!(!plain.metadata.has_barrier);
    assert_eq!(count_ir3_op(&plain, 7, 0), 0);
}

#[test]
fn shared_vectors_preserve_two_dimensional_dispatch() {
    let cs = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "transpose").unwrap();
    assert_eq!(cs.metadata.local_size, [8, 8, 1]);
    assert_eq!(cs.metadata.shared_size, 64 * 16);
    assert!(cs.metadata.has_barrier);
    assert!(count_ir3_op(&cs, 6, 1) > 0);
    assert!(count_ir3_op(&cs, 6, 4) > 0);
}

#[test]
fn shared_storage_limits_and_invalid_stage_are_validated() {
    use sgfx_shader_ir3::validate;
    let source = "var<workgroup> a: array<u32, 8192>; @group(0) @binding(0) var<storage, read_write> out: array<u32>; @compute @workgroup_size(1) fn cs() { out[0] = a[0]; }";
    validate(Source::Wgsl(source)).unwrap();
    let oversized = source.replace("8192", "8193");
    assert!(
        validate(Source::Wgsl(&oversized))
            .unwrap_err()
            .to_string()
            .contains("32 KiB")
    );
    // Unused declarations do not consume an entry's hardware shared budget.
    validate(Source::Wgsl(&oversized.replace("a[0]", "7u"))).unwrap();
    let combined = source
        .replace("8192", "4096")
        .replace("@group", "var<workgroup> b: array<u32, 4097>; @group")
        .replace("= a[0]", "= a[0] + b[0]");
    assert!(
        validate(Source::Wgsl(&combined))
            .unwrap_err()
            .to_string()
            .contains("32 KiB")
    );
    assert!(validate(Source::Wgsl("var<workgroup> a: u32; @fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(f32(a)); }")).is_err());
}

#[test]
fn shared_compare_exchange_and_uniform_load_emit_native_operations() {
    let cs = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "atomic_cas").unwrap();
    assert_eq!(cs.metadata.shared_size, 4);
    assert!(
        count_ir3_op(&cs, 6, 21) > 0,
        "local ATOMIC.CMPXCHG must survive"
    );
    assert!(cs.metadata.has_barrier); // WGSL initialization rendezvous
    let load = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "uniform_load").unwrap();
    assert!(load.metadata.shared_size > 0 && load.metadata.shared_size <= 512);
    assert!(count_ir3_op(&load, 6, 1) > 0);
    assert!(count_ir3_op(&load, 7, 0) >= 2); // initialization + uniform load
}

#[test]
fn shared_allocation_crosses_two_kib_register_boundary() {
    let cs = compile(Source::Wgsl(WORKGROUP), Stage::Compute, "wide_shared").unwrap();
    assert_eq!(cs.metadata.shared_size, 4096);
    assert_eq!(cs.metadata.compute.as_ref().unwrap().req_local_mem, 0);
    assert_eq!(cs.metadata.local_size, [128, 1, 1]);
    assert!(cs.metadata.has_barrier);
    assert!(count_ir3_op(&cs, 6, 1) > 0 && count_ir3_op(&cs, 6, 4) > 0);
}
