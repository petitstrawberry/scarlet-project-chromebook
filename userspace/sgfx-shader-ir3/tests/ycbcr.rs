#![cfg(feature = "mesa")]
use sgfx_shader_ir3::*;
const SOURCE: &str = include_str!("shaders/nv12.wgsl");
fn map() -> BindingMap {
    BindingMap::new(vec![
        BindingAssignment {
            group: 0,
            binding: 0,
            kind: ResourceKind::Texture,
            slot: 3,
        },
        BindingAssignment {
            group: 0,
            binding: 1,
            kind: ResourceKind::Sampler,
            slot: 5,
        },
        BindingAssignment {
            group: 0,
            binding: 2,
            kind: ResourceKind::StorageBuffer,
            slot: 0,
        },
    ])
    .unwrap()
}
fn normalized() -> BindingMap {
    BindingMap::new(vec![
        BindingAssignment {
            group: 2,
            binding: 3,
            kind: ResourceKind::Texture,
            slot: 3,
        },
        BindingAssignment {
            group: 3,
            binding: 5,
            kind: ResourceKind::Sampler,
            slot: 5,
        },
        BindingAssignment {
            group: 1,
            binding: 0,
            kind: ResourceKind::StorageBuffer,
            slot: 0,
        },
    ])
    .unwrap()
}
#[test]
fn real_nv12_profiles_compile_wgsl_and_spirv_with_exact_stage_and_slot_metadata() {
    for (entry, stage) in [
        ("implicit_sample", Stage::Fragment),
        ("biased_sample", Stage::Fragment),
        ("explicit_level", Stage::Fragment),
        ("explicit_gradient", Stage::Fragment),
        ("sample_offset", Stage::Fragment),
        ("dimensions", Stage::Fragment),
        ("levels", Stage::Fragment),
        ("vertex_sample", Stage::Vertex),
        ("compute_sample", Stage::Compute),
    ] {
        let plain = compile_with_bindings(Source::Wgsl(SOURCE), stage, entry, &map()).unwrap();
        assert!(plain.metadata.nv12_profiles().unwrap().is_none());
        for matrix in [1, 2] {
            for range in [1, 2] {
                for chroma_x in [1, 2] {
                    for chroma_y in [1, 2] {
                        let p = Nv12Profile {
                            texture_slot: 3,
                            matrix,
                            range,
                            chroma_x,
                            chroma_y,
                        };
                        let actual = compile_with_bindings_ycbcr(
                            Source::Wgsl(SOURCE),
                            stage,
                            entry,
                            &map(),
                            &[p],
                        )
                        .unwrap();
                        let metadata = actual.metadata.nv12_profiles().unwrap().unwrap();
                        assert_eq!(metadata.len(), 1);
                        assert_eq!(metadata[0].profile, p);
                        let round = compile_with_bindings_ycbcr(
                            Source::SpirV(&plain.spirv),
                            stage,
                            entry,
                            &normalized(),
                            &[p],
                        )
                        .unwrap();
                        assert_eq!(actual.allocation, round.allocation);
                        assert_eq!(actual.metadata.imm_values, round.metadata.imm_values);
                        assert_eq!(
                            actual.metadata.nv12_profiles().unwrap(),
                            round.metadata.nv12_profiles().unwrap()
                        );
                        if matches!(entry, "dimensions" | "levels") {
                            assert_eq!(plain.allocation, actual.allocation);
                            assert_eq!(metadata[0].sample_instructions, 0);
                            assert!(metadata[0].query_instructions > 0);
                        } else {
                            assert!(metadata[0].sample_instructions > 0);
                            assert_ne!(plain.allocation, actual.allocation);
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn reconstruction_load_and_explicit_sampler_profiles_preserve_validation() {
    let p = Nv12Profile {
        texture_slot: 3,
        matrix: 1,
        range: 1,
        chroma_x: 1,
        chroma_y: 1,
    };
    let load=compile_with_bindings_ycbcr(Source::Wgsl(SOURCE),Stage::Fragment,"fetch",&map(),&[p]).unwrap();
    assert!(load.metadata.nv12_reconstruction().unwrap().unwrap().nearest_sampler.is_some());
    let missing=compile_with_bindings_ycbcr(Source::Wgsl(SOURCE),Stage::Fragment,"gather",&map(),&[p]).unwrap_err();
    assert!(missing.0.contains("sampler profile"),"{}",missing.0);
    for address in 0..4 {
        let sampler=Nv12SamplerProfile {slot:5,address_u:address,address_v:(address+1)%4,min_filter:1,mag_filter:0};
        let shader=compile_with_bindings_ycbcr_samplers(Source::Wgsl(SOURCE),Stage::Fragment,"gather",&map(),&[p],&[sampler]).unwrap();
        let reconstruction=shader.metadata.nv12_reconstruction().unwrap().unwrap();
        assert_eq!(reconstruction.samplers,vec![sampler]);
        assert_ne!(reconstruction.nearest_sampler,Some(5));
        let mut malformed=shader.metadata.clone();
        malformed.extra.get_mut("sgfx_nv12_profile").unwrap()["nearest_sampler"]=serde_json::json!(5);
        assert!(malformed.nv12_reconstruction().is_err());
    }
    for profiles in [
        vec![p, p],
        vec![Nv12Profile {
            texture_slot: 16,
            ..p
        }],
        vec![Nv12Profile { matrix: 0, ..p }],
        vec![Nv12Profile {
            texture_slot: 2,
            ..p
        }],
    ] {
        assert!(
            compile_with_bindings_ycbcr(
                Source::Wgsl(SOURCE),
                Stage::Fragment,
                "implicit_sample",
                &map(),
                &profiles
            )
            .is_err()
        );
    }
}
#[test]
fn generic_nv12_fixture_compiles_actual_application_math_and_queries() {
    let source = include_str!("shaders/nv12_programmable.wgsl");
    let map = BindingMap::new(vec![
        BindingAssignment {
            group: 0,
            binding: 0,
            kind: ResourceKind::Texture,
            slot: 0,
        },
        BindingAssignment {
            group: 0,
            binding: 1,
            kind: ResourceKind::Sampler,
            slot: 0,
        },
    ])
    .unwrap();
    let profile = Nv12Profile {
        texture_slot: 0,
        matrix: 2,
        range: 1,
        chroma_x: 2,
        chroma_y: 2,
    };
    let vs = compile_with_bindings(Source::Wgsl(source), Stage::Vertex, "vs", &map).unwrap();
    let fs = compile_with_bindings_ycbcr(
        Source::Wgsl(source),
        Stage::Fragment,
        "fs",
        &map,
        &[profile],
    )
    .unwrap();
    let metadata = fs.metadata.nv12_profiles().unwrap().unwrap();
    assert_eq!(metadata[0].sample_instructions, 1);
    assert_eq!(metadata[0].query_instructions, 2);
    if let Some(dir) = std::env::var_os("SGFX_NV12_GENERIC_FIXTURE_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("generic_nv12.wgsl"), source).unwrap();
        for (name, shader) in [("generic_nv12_vs", vs), ("generic_nv12_fs", fs)] {
            std::fs::write(dir.join(format!("{name}.bin")), shader.allocation).unwrap();
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_vec_pretty(&shader.metadata).unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("{name}.bindings.json")),
                serde_json::to_vec_pretty(&shader.bindings).unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("{name}.spv")),
                shader
                    .spirv
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        }
    }
}
#[test]
fn base_clamp_public_2d_compiles_with_nv12_profile_and_normal_spirv_roundtrip() {
    let source=include_str!("shaders/base_clamp.wgsl");
    let map=BindingMap::new(vec![BindingAssignment {group:0,binding:0,kind:ResourceKind::Texture,slot:0},BindingAssignment {group:0,binding:1,kind:ResourceKind::Sampler,slot:0}]).unwrap();
    let normalized=BindingMap::new(vec![BindingAssignment {group:2,binding:0,kind:ResourceKind::Texture,slot:0},BindingAssignment {group:3,binding:0,kind:ResourceKind::Sampler,slot:0}]).unwrap();
    let vs=compile_with_bindings(Source::Wgsl(source),Stage::Vertex,"vs",&map).unwrap();
    let fs=compile_with_bindings(Source::Wgsl(source),Stage::Fragment,"fs",&map).unwrap();
    let round=compile_with_bindings(Source::SpirV(&fs.spirv),Stage::Fragment,"fs",&normalized).unwrap();assert_eq!(fs.allocation,round.allocation);
    let p=Nv12Profile {texture_slot:0,matrix:2,range:1,chroma_x:2,chroma_y:2};
    let converted=compile_with_bindings_ycbcr(Source::Wgsl(source),Stage::Fragment,"fs",&map,&[p]).unwrap();let proof=converted.metadata.nv12_profiles().unwrap().unwrap();assert_eq!(proof[0].sample_instructions,1);assert_eq!(proof[0].query_instructions,1);
    if let Some(dir)=std::env::var_os("SGFX_BASE_CLAMP_FIXTURE_OUT") {
        let dir=std::path::PathBuf::from(dir);std::fs::create_dir_all(&dir).unwrap();std::fs::write(dir.join("shader.wgsl"),source).unwrap();
        for (name,s) in [("vs",vs),("fs",fs)] {std::fs::write(dir.join(format!("{name}.spv")),s.spirv.iter().flat_map(|w|w.to_le_bytes()).collect::<Vec<_>>()).unwrap();}
    }
}
