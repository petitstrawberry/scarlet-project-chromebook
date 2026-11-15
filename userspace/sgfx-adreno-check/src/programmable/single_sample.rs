// SPDX-License-Identifier: GPL-2.0-only
//! One-sample builtins and sample/center interpolation, against analytic values.
use super::*;
const SIDE: u32 = 16;
const SAMPLE: &str = include_str!("single-sample-fixtures/sample.wgsl");
const CENTER: &str = include_str!("single-sample-fixtures/center.wgsl");

fn module(bytes: &[u8], spirv: bool, center: bool) -> Result<ShaderModuleDesc, String> {
    Ok(if spirv {
        checked!(ShaderModuleDesc::spirv(
            bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        ))
    } else {
        checked!(ShaderModuleDesc::wgsl(
            if center { CENTER } else { SAMPLE }.into()
        ))
    })
}

// Clip-space W is 1,2,4, with an oversized triangle at NDC
// (-1,-1),(3,-1),(-1,3); attribute values are 0,1,0. This deliberately
// distinguishes true perspective interpolation from the linear input.
fn perspective(x: u32, y: u32) -> f32 {
    let t = (x as f32 + 0.5) / SIDE as f32;
    let r = 1.0 - (y as f32 + 0.5) / SIDE as f32;
    (t * 0.25) / (1.0 - t * 0.25 - r * 0.375)
}
fn unorm(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}
fn expected(x: u32, y: u32, derivatives: bool) -> [u8; 4] {
    if derivatives {
        let qx = x & !1;
        let qy = y & !1;
        [
            unorm(-(perspective(x, qy + 1) - perspective(x, qy)) * 16.0),
            128,
            unorm((perspective(qx + 1, y) - perspective(qx, y)) * 8.0),
            255,
        ]
    } else {
        [
            unorm((x as f32 + 0.5) / (2.0 * SIDE as f32)),
            unorm(perspective(x, y)),
            17,
            255,
        ]
    }
}
fn execute(path: &str, spirv: bool, center: bool, derivatives: bool) -> Result<(), String> {
    println!(
        "BEGIN programmable-single-sample spirv={spirv} center={center} derivatives={derivatives}"
    );
    let table = ResourceTable::new();
    let vertex = checked!(table.define_shader_module(module(
        include_bytes!("single-sample-fixtures/vs.spv"),
        spirv,
        center
    )?));
    let fs_bytes: &[u8] = match (center, derivatives) {
        (false, false) => include_bytes!("single-sample-fixtures/sample.spv"),
        (true, false) => include_bytes!("single-sample-fixtures/center.spv"),
        (false, true) => include_bytes!("single-sample-fixtures/sample-derivatives.spv"),
        (true, true) => include_bytes!("single-sample-fixtures/center-derivatives.spv"),
    };
    let fragment = checked!(table.define_shader_module(module(fs_bytes, spirv, center)?));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                vertex,
                ShaderStage::Vertex,
                "vs".into()
            )),
            checked!(ShaderEntryPoint::new(
                fragment,
                ShaderStage::Fragment,
                if derivatives { "derivatives" } else { "fs" }.into()
            )),
            checked!(PipelineLayoutDesc::new(vec![])),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )
    )));
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!("SKIP programmable-single-sample pixels (--compile-only)");
        return Ok(());
    }
    let mut encoder = CommandEncoder::new(&table);
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut pixels = vec![0xa5; (SIDE * SIDE * 4) as usize];
        driver.read_texture(target.slot(), &mut pixels)?;
        let mut checked_pixels = 0;
        // Keep complete derivative quads away from viewport boundaries.
        let border = if derivatives { 2 } else { 0 };
        for y in border..SIDE - border {
            for x in border..SIDE - border {
                let expected = expected(x, y, derivatives);
                let offset = ((y * SIDE + x) * 4) as usize;
                let got = &pixels[offset..offset + 4];
                if got.iter().zip(expected).any(|(&a, b)| a.abs_diff(b) > 2) {
                    return Err(format!(
                        "single-sample spirv={spirv} center={center} derivatives={derivatives} replay={replay} pixel={x},{y} got={got:?} expected={expected:?}"
                    ));
                }
                checked_pixels += 1;
            }
        }
        println!(
            "PASS programmable-single-sample spirv={spirv} center={center} derivatives={derivatives} replay={replay} pixels={checked_pixels} sample-index-zero={} perspective=true linear=true",
            !derivatives
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    for spirv in [false, true] {
        for center in [false, true] {
            for derivatives in [false, true] {
                execute(path, spirv, center, derivatives)?;
            }
        }
    }
    Ok(())
}
