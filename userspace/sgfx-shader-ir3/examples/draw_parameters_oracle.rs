//! Reproducible DrawParameters source and actual native fixture, without a GPU.
#[path = "../tests/support/draw_parameters.rs"]
mod fixture;
use sgfx_shader_ir3::{CompiledShader, Source, Stage, compile};
use std::{error::Error, path::Path};
fn write(
    out: &Path,
    name: &str,
    source: &[u32],
    shader: &CompiledShader,
) -> Result<(), Box<dyn Error>> {
    std::fs::write(
        out.join(format!("{name}.spv")),
        source
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    std::fs::write(out.join(format!("{name}.bin")), &shader.allocation)?;
    std::fs::write(
        out.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&shader.metadata)?,
    )?;
    std::fs::write(
        out.join(format!("{name}.bindings.json")),
        serde_json::to_vec_pretty(&shader.bindings)?,
    )?;
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: draw_parameters_oracle OUTPUT_DIRECTORY")?;
    let out = Path::new(&path);
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("vertex.template.wgsl"), fixture::TEMPLATE)?;
    std::fs::write(out.join("fragment.wgsl"), fixture::FRAGMENT)?;
    let mut native = Vec::new();
    for (name, extension) in [("vertex-core", false), ("vertex-khr", true)] {
        let words = fixture::spirv(extension);
        let shader = compile(Source::SpirV(&words), Stage::Vertex, "main")?;
        assert!(shader.metadata.need_driver_params && shader.metadata.num_driver_params >= 3);
        write(out, name, &words, &shader)?;
        native.push(shader.allocation);
    }
    assert_eq!(native[0], native[1]);
    let fs = compile(Source::Wgsl(fixture::FRAGMENT), Stage::Fragment, "main")?;
    write(out, "fragment", &fs.spirv, &fs)?;
    let cases = serde_json::json!([
        {"name":"nonindexed","first_vertex":7,"first_instance":11,"instance_count":2,"draw_index":13,"driver_words":[13,7,11,u32::MAX,0],"expected_rgba8":[13,135,11,255]},
        {"name":"indexed_negative_base","indices_u32":[5,6,7],"first_index":0,"base_vertex":-5,"first_instance":17,"instance_count":2,"draw_index":19,"driver_words":[19,(-5i32) as u32,17,u32::MAX,u32::MAX],"expected_rgba8":[19,123,17,255]},
        {"name":"following_draw_update","first_vertex":2,"first_instance":3,"instance_count":1,"draw_index":5,"driver_words":[5,2,3,u32::MAX,0],"expected_rgba8":[5,130,3,255]}
    ]);
    std::fs::write(
        out.join("oracle.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "compiler_cache_version":sgfx_shader_ir3::COMPILER_CACHE_VERSION,"gpu_executed":false,"entry":"main",
            "source_note":"vertex.template.wgsl is a generator template, not a draw-parameter WGSL shader. Generator patches three argument bindings in Naga IR to BaseVertex/BaseInstance/DrawID then emits valid original SPIR-V. Use vertex-core.spv or vertex-khr.spv as actual input.",
            "fragment_interface":"location0 flat vec3<u32> = DrawIndex,BaseVertex bits,BaseInstance; FS bitcasts base bits to signed i32 and adds128 for byte readback",
            "native_core_khr_equal":true,"vertex_count":3,"render":{"format":"Rgba8Unorm","size":[4,4],"blend":false,"depth_test":false,"cull":"None","topology":"TriangleList"},
            "procedure":"For codegen verify actual kind3 allocation writes driver_words at offset_vec4, using artifact metadata and driver_ubos if applicable. For GPU execute each case with both original SPV versions and fragment.wgsl/SPV, read every pixel separately before next draw; compare expected with tolerance1. Subsequent draw must update constants, not reuse cached first values.","cases":cases
        }))?,
    )?;
    println!(
        "DrawParameters core/KHR native equality PASS; actual VS+FS artifacts and3draw CPU oracles written; GPU pending"
    );
    Ok(())
}
