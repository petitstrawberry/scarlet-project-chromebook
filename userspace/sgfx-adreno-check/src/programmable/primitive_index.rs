// SPDX-License-Identifier: GPL-2.0-only
//! Two real primitive IDs plus interpolated UVs, including plain-FS reset.
use super::*;
const SIDE: u32 = 16;
const SOURCE: &str = include_str!("primitive-fixtures/primitive_index.wgsl");

fn module(bytes: &[u8], spirv: bool) -> Result<ShaderModuleDesc, String> {
    Ok(if spirv {
        checked!(ShaderModuleDesc::spirv(
            bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        ))
    } else {
        checked!(ShaderModuleDesc::wgsl(SOURCE.into()))
    })
}

fn execute(path: &str, spirv: bool, reset: bool) -> Result<(), String> {
    println!("BEGIN programmable-primitive-index spirv={spirv} reset={reset}");
    let table = ResourceTable::new();
    let vertex = checked!(
        table.define_shader_module(module(include_bytes!("primitive-fixtures/vs.spv"), spirv)?)
    );
    let fragment = checked!(
        table.define_shader_module(module(include_bytes!("primitive-fixtures/fs.spv"), spirv)?)
    );
    let plain = checked!(table.define_shader_module(module(
        include_bytes!("primitive-fixtures/plain.spv"),
        spirv
    )?));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC,
    ))));
    let pipeline = |module, entry: &str| -> Result<_, String> {
        Ok(checked!(table.define_programmable_render_pipeline(
            checked!(ProgrammableRenderPipelineDesc::new(
                checked!(ShaderEntryPoint::new(
                    vertex,
                    ShaderStage::Vertex,
                    "vs".into()
                )),
                checked!(ShaderEntryPoint::new(
                    module,
                    ShaderStage::Fragment,
                    entry.into()
                )),
                checked!(PipelineLayoutDesc::new(vec![])),
                TextureFormat::Bgra8Unorm,
                None,
                PrimitiveTopology::TriangleList,
                BlendState::REPLACE,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            ))
        )))
    };
    let primitive = pipeline(fragment, "fs")?;
    let ordinary = pipeline(plain, "plain")?;
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, primitive.slot())?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, ordinary.slot())?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!("SKIP programmable-primitive-index pixels (--compile-only)");
        return Ok(());
    }
    let mut encoder = CommandEncoder::new(&table);
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store,
    ))));
    checked!(pass.set_programmable_pipeline(primitive));
    // VS supplies only position and UV. The fragment's ID must be supplied by
    // actual primitive assembly: the lower-left triangle is 0, upper-right 1.
    checked!(pass.draw(6, 0));
    if reset {
        checked!(pass.set_programmable_pipeline(ordinary));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(6, 6, 4, 4)))));
        checked!(pass.draw(6, 0));
    }
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut pixels = vec![0xa5; (SIDE * SIDE * 4) as usize];
        driver.read_texture(target.slot(), &mut pixels)?;
        let mut primitive_pixels = [0usize; 2];
        let mut plain_pixels = 0;
        for y in 0..SIDE {
            for x in 0..SIDE {
                let ordinary = reset && (6..10).contains(&x) && (6..10).contains(&y);
                // Exclude shared-edge coverage/rounding; never infer an ID
                // from a pixel whose primitive ownership is implementation-dependent.
                if !ordinary && x.abs_diff(y) < 2 {
                    continue;
                }
                let expected = if ordinary {
                    plain_pixels += 1;
                    [191, 128, 64, 255]
                } else {
                    let id = usize::from(x > y);
                    primitive_pixels[id] += 1;
                    [
                        ((1.0 - (y as f32 + 0.5) / SIDE as f32) * 255.0).round() as u8,
                        (((x as f32 + 0.5) / SIDE as f32) * 255.0).round() as u8,
                        17 + id as u8 * 97,
                        255,
                    ]
                };
                let offset = ((y * SIDE + x) * 4) as usize;
                let got = &pixels[offset..offset + 4];
                if got.iter().zip(expected).any(|(&a, b)| a.abs_diff(b) > 1) {
                    return Err(format!(
                        "primitive-index spirv={spirv} reset={reset} replay={replay} pixel={x},{y} got={got:?} expected={expected:?}"
                    ));
                }
            }
        }
        if primitive_pixels.contains(&0) || (reset && plain_pixels != 16) {
            return Err(
                "primitive-index oracle did not cover both primitive IDs/plain reset".into(),
            );
        }
        println!(
            "PASS programmable-primitive-index spirv={spirv} reset={reset} replay={replay} primitive0={} primitive1={} plain={} interpolated-uv=true",
            primitive_pixels[0], primitive_pixels[1], plain_pixels
        );
    }
    Ok(())
}

pub(super) fn run(path: &str) -> Result<(), String> {
    for spirv in [false, true] {
        for reset in [false, true] {
            execute(path, spirv, reset)?;
        }
    }
    Ok(())
}
