use sgfx_shader_ir3::{BindingAssignment as Assignment, BindingMap, ResourceKind as Kind};

fn assignment(group: u32, binding: u32, kind: Kind, slot: u32) -> Assignment {
    Assignment {
        group,
        binding,
        kind,
        slot,
    }
}

#[test]
fn canonical_map_identity_and_namespace_validation() {
    use std::hash::{Hash, Hasher};
    let first = assignment(0, 3, Kind::StorageBuffer, 2);
    let second = assignment(1, 2, Kind::StorageBuffer, 7);
    let a = BindingMap::new(vec![first, second]).unwrap();
    let b = BindingMap::new(vec![second, first]).unwrap();
    let hash = |value: &BindingMap| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    };
    assert_eq!(a, b);
    assert_eq!(hash(&a), hash(&b));
    assert_ne!(
        a,
        BindingMap::new(vec![first, Assignment { slot: 8, ..second }]).unwrap()
    );
    assert!(BindingMap::new(vec![first, first]).is_err());
    assert!(
        BindingMap::new(vec![
            first,
            Assignment {
                slot: first.slot,
                ..second
            }
        ])
        .is_err()
    );
    assert!(BindingMap::new(vec![Assignment { slot: 16, ..first }]).is_err());
    // Equal native slots in different descriptor namespaces are independent.
    BindingMap::new(vec![first, assignment(2, 1, Kind::UniformBuffer, 2)]).unwrap();
    // Equal original identities cannot change type across stages.
    assert!(
        BindingMap::new(vec![
            first,
            Assignment {
                kind: Kind::Texture,
                ..first
            }
        ])
        .is_err()
    );
}

#[cfg(feature = "mesa")]
mod native {
    use super::*;
    use sgfx_shader_ir3::{Source, Stage, compile, compile_with_bindings};

    fn storage_source(stage: Stage, group: u32) -> String {
        let mut source = String::new();
        for binding in 0..4 {
            source.push_str(&format!(
                "@group({group}) @binding({binding}) var<storage,read> b{binding}:array<f32>;\n"
            ));
        }
        let sum = "b0[0] + b1[0] + b2[0] + b3[0]";
        if stage == Stage::Vertex {
            source.push_str(&format!("@vertex fn main() -> @builtin(position) vec4<f32> {{ return vec4<f32>({sum}, 0.0, 0.0, 1.0); }}"));
        } else {
            source.push_str(&format!(
                "@fragment fn main() -> @location(0) vec4<f32> {{ return vec4<f32>({sum}); }}"
            ));
        }
        source
    }
    fn shared_storage_map() -> BindingMap {
        BindingMap::new(
            (0..2)
                .flat_map(|group| {
                    (0..4).map(move |binding| {
                        assignment(group, binding, Kind::StorageBuffer, group * 4 + binding)
                    })
                })
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn separate_vs_fs_use_one_shared_uav_map_and_cache_identity() {
        let vs_source = storage_source(Stage::Vertex, 0);
        let fs_source = storage_source(Stage::Fragment, 1);
        let map = shared_storage_map();
        let vs =
            compile_with_bindings(Source::Wgsl(&vs_source), Stage::Vertex, "main", &map).unwrap();
        let fs =
            compile_with_bindings(Source::Wgsl(&fs_source), Stage::Fragment, "main", &map).unwrap();
        assert_eq!(
            vs.bindings.iter().map(|b| b.slot).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(
            fs.bindings.iter().map(|b| b.slot).collect::<Vec<_>>(),
            [4, 5, 6, 7]
        );
        assert!(vs.bindings.iter().all(|b| b.group == 0 && !b.writable));
        assert!(fs.bindings.iter().all(|b| b.group == 1 && !b.writable));
        assert!(fs.metadata.num_uavs >= 8 || fs.metadata.image_mapping.num_tex >= 4);
        let legacy = compile(Source::Wgsl(&fs_source), Stage::Fragment, "main").unwrap();
        assert_ne!(
            legacy.allocation, fs.allocation,
            "mapping must alter actual native descriptor indices"
        );
        let mut changed = map.assignments().to_vec();
        changed
            .iter_mut()
            .filter(|b| b.group == 1)
            .for_each(|b| b.slot += 4);
        let changed_map = BindingMap::new(changed).unwrap();
        assert_ne!(map, changed_map);
        let fs_changed = compile_with_bindings(
            Source::Wgsl(&fs_source),
            Stage::Fragment,
            "main",
            &changed_map,
        )
        .unwrap();
        assert_ne!(fs.allocation, fs_changed.allocation);
        let vs_changed = compile_with_bindings(
            Source::Wgsl(&vs_source),
            Stage::Vertex,
            "main",
            &changed_map,
        )
        .unwrap();
        assert_eq!(
            vs.allocation, vs_changed.allocation,
            "unused-stage entries affect cache identity, not instructions"
        );
    }

    #[test]
    fn partial_stage_map_accepts_glass_unused_resources_and_overlapping_namespaces() {
        let glass = include_str!("shaders/glass.wgsl");
        let vs_map = BindingMap::new(vec![assignment(0, 0, Kind::UniformBuffer, 0)]).unwrap();
        let vs = compile_with_bindings(Source::Wgsl(glass), Stage::Vertex, "blur_vertex", &vs_map)
            .unwrap();
        assert_eq!(vs.bindings.len(), 1);
        let map = BindingMap::new(vec![
            assignment(0, 0, Kind::UniformBuffer, 0),
            assignment(0, 1, Kind::Texture, 0),
            assignment(0, 2, Kind::Sampler, 0),
        ])
        .unwrap();
        for entry in ["horizontal", "vertical", "glass"] {
            let fs =
                compile_with_bindings(Source::Wgsl(glass), Stage::Fragment, entry, &map).unwrap();
            assert_eq!(
                fs.bindings.iter().map(|b| b.slot).collect::<Vec<_>>(),
                [0, 0, 0]
            );
            assert!(fs.metadata.num_app_ubos >= 1 && fs.metadata.num_samp >= 1);
            assert_eq!(fs.bindings[0].minimum_size, 1120);
        }
        let missing = compile_with_bindings(Source::Wgsl(glass), Stage::Fragment, "glass", &vs_map)
            .unwrap_err();
        assert!(missing.to_string().contains("missing binding map entry"));
        let wrong = BindingMap::new(vec![assignment(0, 0, Kind::StorageBuffer, 0)]).unwrap();
        assert!(
            compile_with_bindings(Source::Wgsl(glass), Stage::Vertex, "blur_vertex", &wrong)
                .unwrap_err()
                .to_string()
                .contains("kind mismatch")
        );
    }

    #[test]
    fn same_binding_declarations_in_other_entries_do_not_leak_reflection() {
        let source = "@group(0) @binding(0) var<uniform> small: vec4<f32>;
@group(0) @binding(0) var<uniform> large: mat4x4<f32>;
@vertex fn small_vs()->@builtin(position) vec4<f32>{return small;}
@vertex fn large_vs()->@builtin(position) vec4<f32>{return large[0];}";
        let map = BindingMap::new(vec![assignment(0, 0, Kind::UniformBuffer, 3)]).unwrap();
        for (entry, size) in [("small_vs", 16), ("large_vs", 64)] {
            let shader =
                compile_with_bindings(Source::Wgsl(source), Stage::Vertex, entry, &map).unwrap();
            assert_eq!(shader.bindings.len(), 1);
            assert_eq!(shader.bindings[0].minimum_size, size);
            assert_eq!(shader.bindings[0].slot, 3);
        }
    }

    #[test]
    fn compute_sparse_storage_map_preserves_access_and_runtime_array_lengths() {
        let source = "@group(3) @binding(7) var<storage,read> src:array<u32>;
@group(2) @binding(9) var<storage,read_write> dst:array<atomic<u32>>;
@compute @workgroup_size(64) fn cs(@builtin(global_invocation_id) id:vec3<u32>){if(id.x<arrayLength(&src) && id.x<arrayLength(&dst)){atomicAdd(&dst[id.x],src[id.x]);}}";
        let map = BindingMap::new(vec![
            assignment(3, 7, Kind::StorageBuffer, 1),
            assignment(2, 9, Kind::StorageBuffer, 5),
        ])
        .unwrap();
        let shader =
            compile_with_bindings(Source::Wgsl(source), Stage::Compute, "cs", &map).unwrap();
        assert_eq!(shader.metadata.local_size, [64, 1, 1]);
        assert!(shader.metadata.has_ssbo && shader.metadata.num_uavs >= 6);
        assert_eq!(
            shader
                .bindings
                .iter()
                .map(|b| (b.group, b.binding, b.slot, b.writable))
                .collect::<Vec<_>>(),
            [(2, 9, 5, true), (3, 7, 1, false)]
        );
        assert!(shader.bindings.iter().all(|b| b.minimum_size == 4));
    }

    #[test]
    fn source_spirv_obeys_the_same_original_binding_map() {
        let source = storage_source(Stage::Fragment, 1);
        let module = naga::front::wgsl::parse_str(&source).unwrap();
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::PUSH_CONSTANT,
        )
        .validate(&module)
        .unwrap();
        let options = naga::back::spv::Options {
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        };
        let original_spirv = naga::back::spv::write_vec(
            &module,
            &info,
            &options,
            Some(&naga::back::spv::PipelineOptions {
                shader_stage: naga::ShaderStage::Fragment,
                entry_point: "main".into(),
            }),
        )
        .unwrap();
        let map = shared_storage_map();
        let wgsl =
            compile_with_bindings(Source::Wgsl(&source), Stage::Fragment, "main", &map).unwrap();
        let spv = compile_with_bindings(
            Source::SpirV(&original_spirv),
            Stage::Fragment,
            "main",
            &map,
        )
        .unwrap();
        assert_eq!(wgsl.bindings, spv.bindings);
        assert_eq!(wgsl.allocation, spv.allocation);
    }

    #[test]
    fn more_than_sixteen_total_bindings_fit_independent_native_namespaces() {
        let mut source = String::new();
        let mut assignments = Vec::new();
        let mut terms = Vec::new();
        for binding in 0..9 {
            source.push_str(&format!(
                "@group(0) @binding({binding}) var<uniform> u{binding}: vec4<f32>;\n"
            ));
            source.push_str(&format!(
                "@group(1) @binding({binding}) var<storage,read> s{binding}:array<f32>;\n"
            ));
            assignments.push(assignment(0, binding, Kind::UniformBuffer, binding));
            assignments.push(assignment(1, binding, Kind::StorageBuffer, binding));
            terms.push(format!("u{binding}.x+s{binding}[0]"));
        }
        source.push_str(&format!(
            "@fragment fn main()->@location(0) vec4<f32>{{return vec4<f32>({});}}",
            terms.join("+")
        ));
        let map = BindingMap::new(assignments).unwrap();
        let shader =
            compile_with_bindings(Source::Wgsl(&source), Stage::Fragment, "main", &map).unwrap();
        assert_eq!(shader.bindings.len(), 18);
        assert!(shader.metadata.num_app_ubos >= 9);
        assert!(shader.metadata.num_uavs >= 9 || shader.metadata.image_mapping.num_tex >= 9);
    }
}
