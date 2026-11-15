//! Emit original-binding WGSL/SPIR-V pairs for storage/colorless/coverage GPU oracles.
//! Usage: cargo run --example storage_oracle -- OUTPUT_DIRECTORY
//! No GPU calls. Native equality is verified; execution is a separate parent test.
use sgfx_shader_ir3::{Source, Stage, compile};
use std::{error::Error, path::Path};
fn pair(out: &Path, name: &str, source: &str, stage: Stage) -> Result<(), Box<dyn Error>> {
    let module = naga::front::wgsl::parse_str(source)?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::PUSH_CONSTANT,
    )
    .validate(&module)?;
    let shader_stage = match stage {
        Stage::Vertex => naga::ShaderStage::Vertex,
        Stage::Fragment => naga::ShaderStage::Fragment,
        Stage::Compute => naga::ShaderStage::Compute,
    };
    let words = naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options {
            flags: naga::back::spv::WriterFlags::empty(),
            ..Default::default()
        },
        Some(&naga::back::spv::PipelineOptions {
            shader_stage,
            entry_point: "main".into(),
        }),
    )?;
    std::fs::write(out.join(format!("{name}.wgsl")), source)?;
    std::fs::write(
        out.join(format!("{name}.source.spv")),
        words
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    let wgsl = compile(Source::Wgsl(source), stage, "main")?;
    let spirv = compile(Source::SpirV(&words), stage, "main")?;
    assert_eq!(wgsl.allocation, spirv.allocation, "{name} ISA mismatch");
    assert_eq!(
        wgsl.sampled_bindings()?,
        spirv.sampled_bindings()?,
        "{name} interface mismatch"
    );
    for (kind, shader) in [("wgsl", wgsl), ("spirv", spirv)] {
        std::fs::write(out.join(format!("{name}.{kind}.bin")), &shader.allocation)?;
        std::fs::write(
            out.join(format!("{name}.{kind}.json")),
            serde_json::to_vec_pretty(&shader.metadata)?,
        )?;
        std::fs::write(
            out.join(format!("{name}.{kind}.bindings.json")),
            serde_json::to_vec_pretty(&shader.bindings)?,
        )?;
    }
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: storage_oracle OUTPUT_DIRECTORY")?;
    let out = Path::new(&path);
    std::fs::create_dir_all(out)?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sgfx-adreno-check/src/programmable/storage-fixtures");
    for (name, stage) in [
        ("compute", Stage::Compute),
        ("fragment", Stage::Fragment),
        ("vertex", Stage::Vertex),
        ("plain_vertex", Stage::Vertex),
        ("discard", Stage::Fragment),
        ("depth_only", Stage::Fragment),
        ("sample_mask", Stage::Fragment),
        ("sample_plain", Stage::Fragment),
    ] {
        let source = std::fs::read_to_string(fixtures.join(format!("{name}.wgsl")))?;
        pair(out, name, &source, stage)?;
        std::fs::copy(
            out.join(format!("{name}.source.spv")),
            fixtures.join(format!("{name}.source.spv")),
        )?;
    }
    Ok(())
}
