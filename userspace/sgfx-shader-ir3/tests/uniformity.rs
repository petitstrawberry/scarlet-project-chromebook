#![cfg(feature = "frontend")]
use sgfx_shader_ir3::{Source, validate};
const PREFIX: &str = "var<workgroup> scratch: array<u32,128>;
var<workgroup> total: atomic<u32>;
@group(0) @binding(0) var<uniform> config:vec4<u32>;
@group(0) @binding(1) var<storage,read> input:array<u32>;
@group(0) @binding(2) var<storage,read_write> output:array<u32>;";
fn source(helpers: &str, body: &str) -> String {
    format!(
        "{PREFIX}\n{helpers}\n@compute @workgroup_size(128) fn main(@builtin(local_invocation_index) lane:u32,@builtin(workgroup_id) group:vec3<u32>){{{body}}}"
    )
}
fn rejected(helpers: &str, body: &str) {
    let source = source(helpers, body);
    let error = validate(Source::Wgsl(&source)).expect_err(&source);
    assert!(
        error.to_string().contains("workgroup uniformity"),
        "must fail our validated IR convergence check, not parsing/typing: {error}\n{source}"
    );
}
fn accepted(helpers: &str, body: &str) {
    let source = source(helpers, body);
    validate(Source::Wgsl(&source)).unwrap_or_else(|e| panic!("{e}\n{source}"));
}
#[test]
fn divergent_barriers_load_control_and_load_addresses_are_rejected() {
    for body in [
        "if(lane==0u){workgroupBarrier();}",
        "if(lane==0u){storageBarrier();}",
        "if(lane==0u){let value=workgroupUniformLoad(&scratch[0]);}",
        "let value=workgroupUniformLoad(&scratch[lane]);",
        "if(lane==0u){return;} workgroupBarrier();",
        "if(lane==0u){return;} let value=workgroupUniformLoad(&scratch[0]);",
        "switch(lane){case 0u:{workgroupBarrier();} default:{}}",
        "switch(lane){case 0u:{return;} default:{}} workgroupBarrier();",
    ] {
        rejected("", body);
    }
}
#[test]
fn variable_assignments_and_storage_data_propagate_divergence() {
    for body in [
        "var condition=0u; if(lane==0u){condition=1u;} if(condition==1u){workgroupBarrier();}",
        "var lanes=vec2<u32>(0); lanes.x=lane; if(lanes.x==0u){workgroupBarrier();}",
        "if(input[lane]==0u){workgroupBarrier();}",
        "if(input[0]==0u){workgroupBarrier();}",
        "if(scratch[0]==0u){workgroupBarrier();}",
        "let n=atomicAdd(&total,1u); if(n==0u){workgroupBarrier();}",
        "if(config[lane % 4u]==0u){workgroupBarrier();}",
        "let n=select(0u,1u,lane==0u); if(n==0u){workgroupBarrier();}",
    ] {
        rejected("", body);
    }
}
#[test]
fn loop_backedges_catch_divergence_before_and_after_barriers() {
    for body in [
        "for(var i=0u;i<lane;i++){workgroupBarrier();}",
        "loop{workgroupBarrier();if(lane==0u){break;}}",
        "loop{if(lane==0u){continue;}workgroupBarrier();break;}",
        "loop{workgroupBarrier();continuing{break if lane==0u;}}",
        "loop{workgroupBarrier();if(lane==0u){return;}break;}",
        "var stop=0u;loop{workgroupBarrier();if(stop!=0u){break;}stop=lane;}",
        "var stop=0u;loop{if(stop!=0u){break;}workgroupBarrier();stop=lane;}",
        "var i=0u;loop{if(i>2u){break;}if(lane==0u){continue;}continuing{workgroupBarrier();i++;}}",
        "for(var i=0u;i<4u;i++){for(var j=0u;j<lane;j++){workgroupBarrier();}}",
    ] {
        rejected("", body);
    }
}
#[test]
fn helpers_are_checked_with_actual_value_and_pointer_arguments() {
    rejected(
        "fn sync(flag:bool){if(flag){workgroupBarrier();}}",
        "sync(lane==0u);",
    );
    rejected("fn sync(){workgroupBarrier();}", "if(lane==0u){sync();}");
    rejected(
        "fn selected(x:u32)->u32 {if(x==0u){return 0u;}return 1u;}",
        "if(selected(lane)==0u){workgroupBarrier();}",
    );
    rejected(
        "fn write(p:ptr<function,u32>,x:u32){*p=x;}",
        "var n=0u;write(&n,lane);if(n==0u){workgroupBarrier();}",
    );
    rejected(
        "fn write(p:ptr<function,u32>,x:u32){if(x==0u){*p=1u;return;}*p=2u;}",
        "var n=0u;write(&n,lane);if(n==0u){workgroupBarrier();}",
    );
    rejected(
        "fn barrier_after_return(x:u32){if(x==0u){return;}workgroupBarrier();}",
        "barrier_after_return(lane);",
    );
    rejected(
        "fn sync(flag:bool){if(flag){workgroupBarrier();}} fn relay(flag:bool){sync(flag);}",
        "relay(lane==0u);",
    );
    // Calling the same function at a different site must not reuse uniform facts.
    rejected(
        "fn sync(flag:bool){if(flag){workgroupBarrier();}}",
        "sync(group.x==0u);sync(lane==0u);",
    );
}
#[test]
fn reconverged_branches_uniform_values_and_loops_stay_valid() {
    for body in [
        "if(lane==0u){scratch[0]=1u;}workgroupBarrier();",
        "switch(lane){case 0u:{scratch[0]=1u;}default:{scratch[lane]=2u;}}workgroupBarrier();",
        "if(group.x==0u){workgroupBarrier();}",
        "if(config.x==0u){workgroupBarrier();}",
        "if(group.x==0u){return;}workgroupBarrier();",
        "for(var stride=64u;stride>0u;stride>>=1u){if(lane<stride){scratch[lane]+=scratch[lane+stride];}workgroupBarrier();}",
        "for(var i=0u;i<config.x;i++){workgroupBarrier();}",
        "var i=0u;loop{workgroupBarrier();i++;continuing{break if i==4u;}}",
        "var i=0u;loop{if(i==4u){break;}i++;if(i==2u){continue;}workgroupBarrier();}",
        "for(var i=0u;i<4u;i++){for(var j=0u;j<4u;j++){workgroupBarrier();}}",
        "for(var i=0u;i<4u;i++){switch(lane){case 0u:{break;}default:{scratch[lane]=1u;}}workgroupBarrier();}",
        "if(lane==0u){scratch[0]=4u;} let bound=workgroupUniformLoad(&scratch[0]);for(var i=0u;i<bound;i++){workgroupBarrier();}",
        // Divergent break without an in-loop barrier reconverges after the loop.
        "var i=0u;loop{if(i==lane){break;}i++;}workgroupBarrier();",
        // The load is an SSA snapshot, not a reference to future local stores.
        "var n=0u;let bound=n;n=lane;if(bound==0u){workgroupBarrier();}",
    ] {
        accepted("", body);
    }
}
#[test]
fn uniform_helpers_and_local_pointer_mutations_stay_valid() {
    accepted(
        "fn sync(flag:bool){if(flag){workgroupBarrier();}}",
        "sync(group.x==0u);",
    );
    accepted(
        "fn bound(x:u32)->u32{return x+1u;}",
        "for(var i=0u;i<bound(config.x);i++){workgroupBarrier();}",
    );
    accepted(
        "fn write(p:ptr<function,u32>,x:u32){*p=x;}",
        "var n=0u;write(&n,config.x);for(var i=0u;i<n;i++){workgroupBarrier();}",
    );
    accepted(
        "fn independent(x:u32)->u32{return 4u;}",
        "for(var i=0u;i<independent(lane);i++){workgroupBarrier();}",
    );
    accepted(
        "fn mutate(x:u32){if(x==0u){return;}scratch[x]=1u;}",
        "mutate(lane);workgroupBarrier();",
    );
}
#[test]
fn entry_struct_members_keep_their_own_builtin_uniformity() {
    let prefix = "struct Args{@builtin(local_invocation_index) lane:u32,@builtin(workgroup_id) group:vec3<u32>};";
    let uniform = format!(
        "{prefix}@compute @workgroup_size(128) fn main(a:Args){{if(a.group.x==0u){{workgroupBarrier();}}}}"
    );
    validate(Source::Wgsl(&uniform)).unwrap();
    let divergent = uniform.replace("a.group.x", "a.lane");
    assert!(
        validate(Source::Wgsl(&divergent))
            .unwrap_err()
            .to_string()
            .contains("workgroup uniformity")
    );
}
#[test]
fn whole_module_validation_checks_other_entries() {
    let source = format!(
        "{} @compute @workgroup_size(1) fn safe(){{}}",
        source("", "if(lane==0u){workgroupBarrier();}")
    );
    assert!(
        validate(Source::Wgsl(&source))
            .unwrap_err()
            .to_string()
            .contains("entry main")
    );
}
#[test]
fn source_spirv_is_checked_after_semantic_import_too() {
    for (body, valid) in [
        ("if(lane==0u){workgroupBarrier();}", false),
        ("loop{workgroupBarrier();if(lane==0u){break;}}", false),
        ("for(var i=0u;i<4u;i++){workgroupBarrier();}", true),
    ] {
        let source = source("", body);
        let module = naga::front::wgsl::parse_str(&source).unwrap();
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::PUSH_CONSTANT,
        )
        .validate(&module)
        .unwrap();
        let spv = naga::back::spv::write_vec(
            &module,
            &info,
            &naga::back::spv::Options::default(),
            Some(&naga::back::spv::PipelineOptions {
                shader_stage: naga::ShaderStage::Compute,
                entry_point: "main".into(),
            }),
        )
        .unwrap();
        let result = validate(Source::SpirV(&spv));
        if valid {
            result.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("workgroup uniformity")
            );
        }
    }
}
#[cfg(feature = "mesa")]
#[test]
fn cross_wave_shared_sources_preserve_native_barrier_metadata() {
    use sgfx_shader_ir3::{Stage, compile};
    let source = include_str!("shaders/workgroup.wgsl");
    for entry in [
        "reduction",
        "uniform_load",
        "atomic_sum",
        "transpose",
        "wide_shared",
    ] {
        let shader = compile(Source::Wgsl(source), Stage::Compute, entry).unwrap();
        assert!(shader.metadata.has_barrier);
        assert!(shader.metadata.shared_size > 0);
    }
    let unsafe_source = source_of_divergent_barrier();
    assert!(
        compile(Source::Wgsl(&unsafe_source), Stage::Compute, "main")
            .unwrap_err()
            .to_string()
            .contains("workgroup uniformity")
    );
}
#[cfg(feature = "mesa")]
fn source_of_divergent_barrier() -> String {
    source("", "if(lane==0u){workgroupBarrier();}")
}

#[test]
fn paths_that_never_rejoin_cannot_hide_barrier_divergence() {
    rejected("", "if(lane==0u){loop{}} workgroupBarrier();");
    rejected(
        "",
        "switch(lane){case 0u:{loop{}}default:{}} workgroupBarrier();
",
    );
    rejected(
        "fn never(x:u32){if(x==0u){return;}loop{}}",
        "never(lane);workgroupBarrier();",
    );
    rejected(
        "fn never(){loop{}}",
        "if(lane==0u){never();}workgroupBarrier();",
    );
}

#[test]
fn mixed_vector_constructor_operands_do_not_masquerade_as_components() {
    for body in [
        "let v=vec4<u32>(id,0u);if(v[1]==0u){workgroupBarrier();}",
        "let v=vec4<u32>(0u,id);if(v[1]==0u){workgroupBarrier();}",
        "let v=vec4<u32>(id.xy,vec2<u32>(0));if(v[1]==0u){workgroupBarrier();}",
    ] {
        let source = format!(
            "@compute @workgroup_size(8,8) fn main(@builtin(local_invocation_id) id:vec3<u32>){{{body}}}"
        );
        assert!(
            validate(Source::Wgsl(&source))
                .unwrap_err()
                .to_string()
                .contains("workgroup uniformity")
        );
    }
}
