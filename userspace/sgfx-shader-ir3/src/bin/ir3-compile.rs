// SPDX-License-Identifier: GPL-2.0-only
//! Developer diagnostic, also builds for Scarlet. Does not submit to a GPU.
use sgfx_shader_ir3::{Source, Stage, compile};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err(
            "usage: ir3-compile vertex|fragment|compute ENTRY INPUT.wgsl|INPUT.spv OUTPUT_PREFIX"
                .into(),
        );
    }
    let stage = match args[1].as_str() {
        "vertex" => Stage::Vertex,
        "fragment" => Stage::Fragment,
        "compute" => Stage::Compute,
        _ => return Err("invalid stage".into()),
    };
    let input = std::fs::read(&args[3])?;
    let words: Vec<_> = input
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let source = if args[3].ends_with(".spv") {
        if input.len() % 4 != 0 {
            return Err("SPIR-V byte length is not word aligned".into());
        }
        Source::SpirV(&words)
    } else {
        Source::Wgsl(std::str::from_utf8(&input)?)
    };
    let started = std::time::Instant::now();
    let result = compile(source, stage, &args[2])?;
    println!(
        "IR3 {:?} {}: {} allocation bytes, {} instruction dwords, {} constant bytes, {} us (compile only)",
        stage,
        args[2],
        result.allocation.len(),
        result.metadata.sizedwords,
        result.metadata.constant_data_bytes,
        started.elapsed().as_micros()
    );
    std::fs::write(format!("{}.bin", args[4]), &result.allocation)?;
    std::fs::write(
        format!("{}.json", args[4]),
        serde_json::to_vec_pretty(&result.metadata)?,
    )?;
    std::fs::write(
        format!("{}.bindings.json", args[4]),
        serde_json::to_vec_pretty(&result.bindings)?,
    )?;
    std::fs::write(
        format!("{}.spv", args[4]),
        result
            .spirv
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>(),
    )?;
    Ok(())
}
