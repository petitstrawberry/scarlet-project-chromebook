// SPDX-License-Identifier: GPL-2.0-only
//! Single-sample shader coverage and masked->ordinary pipeline transition.
use super::*;
const VS: &str = include_str!("storage-fixtures/plain_vertex.wgsl");
const FS: &str = include_str!("storage-fixtures/sample_mask.wgsl");
const PLAIN: &str = include_str!("storage-fixtures/sample_plain.wgsl");
fn module(source: &str, bytes: &[u8], spv: bool) -> Result<ShaderModuleDesc, String> {
    Ok(if spv {
        checked!(ShaderModuleDesc::spirv(
            bytes
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect()
        ))
    } else {
        checked!(ShaderModuleDesc::wgsl(source.into()))
    })
}
fn execute(path: &str, spv: bool, reset: bool) -> Result<(), String> {
    println!("BEGIN programmable-sample-mask spirv={spv} reset={reset}");
    let t = ResourceTable::new();
    t.enable_abi_commands();
    let extent = checked!(Extent2D::new(8, 8));
    let area = checked!(PixelRect::new(0, 0, 8, 8));
    let target = checked!(t.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let depth = checked!(t.define_texture(checked!(TextureDesc::new(
        TextureFormat::Depth32Float,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let vertex = checked!(t.define_shader_module(module(
        VS,
        include_bytes!("storage-fixtures/plain_vertex.source.spv"),
        spv
    )?));
    let fragment = checked!(t.define_shader_module(module(
        FS,
        include_bytes!("storage-fixtures/sample_mask.source.spv"),
        spv
    )?));
    let plain = checked!(t.define_shader_module(module(
        PLAIN,
        include_bytes!("storage-fixtures/sample_plain.source.spv"),
        spv
    )?));
    let pipeline = |f| -> Result<_, String> {
        Ok(checked!(
            t.define_programmable_render_pipeline(checked!(
                checked!(ProgrammableRenderPipelineDesc::new(
                    checked!(ShaderEntryPoint::new(
                        vertex,
                        ShaderStage::Vertex,
                        "main".into()
                    )),
                    checked!(ShaderEntryPoint::new(
                        f,
                        ShaderStage::Fragment,
                        "main".into()
                    )),
                    checked!(PipelineLayoutDesc::new(vec![])),
                    TextureFormat::Bgra8Unorm,
                    None,
                    PrimitiveTopology::TriangleList,
                    BlendState::REPLACE,
                    RasterState::new(CullMode::None, FrontFace::CounterClockwise)
                ))
                .with_depth_state(DepthState::new(
                    TextureFormat::Depth32Float,
                    CompareFunction::Always,
                    true
                ))
            ))
        ))
    };
    let masked = pipeline(fragment)?;
    let normal = pipeline(plain)?;
    let driver = DriverSession::new(path, &t)?;
    for m in [vertex, fragment, plain] {
        driver.validate(abi::VALIDATE_SHADER, m.slot())?;
    }
    for p in [masked, normal] {
        driver.validate(abi::VALIDATE_RENDER_PIPELINE, p.slot())?;
    }
    if std::env::args().any(|a| a == "--compile-only") {
        println!("COMPILED programmable-sample-mask spirv={spv}");
        return Ok(());
    }
    let mut e = CommandEncoder::new(&t);
    let pass_desc = checked!(
        checked!(RenderPassDesc::new(
            &t,
            target,
            area,
            LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))
        .with_depth_attachment(&t, depth, DepthLoadOp::Clear(0.75), StoreOp::Store)
    );
    let mut pass = checked!(e.begin_render_pass(pass_desc));
    checked!(pass.set_programmable_pipeline(masked));
    checked!(pass.draw(3, 0));
    if reset {
        checked!(pass.set_programmable_pipeline(normal));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(0, 0, 8, 1)))));
        checked!(pass.draw(3, 0));
    }
    checked!(pass.end());
    let commands = checked!(e.finish());
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut color = vec![0xa5; 256];
        let mut z = vec![0xa5; 256];
        driver.read_texture(target.slot(), &mut color)?;
        driver.read_texture(depth.slot(), &mut z)?;
        for i in 0..64 {
            let x = i % 8;
            let y = i / 8;
            let covered = (x + y) % 2 == 0;
            let (expected, d) = if reset && y == 0 {
                ([255u8, 0, 0, 255], 0.5f32)
            } else if covered {
                ([102, 153, 51, 255], 0.25)
            } else {
                ([255, 0, 255, 255], 0.75)
            };
            let pixel = &color[i * 4..i * 4 + 4];
            if pixel.iter().zip(expected).any(|(a, b)| a.abs_diff(b) > 1) {
                return Err(format!(
                    "sample-mask spirv={spv} reset={reset} ({x},{y}) BGRA={pixel:?} expected={expected:?}"
                ));
            }
            let actual = f32::from_le_bytes(z[i * 4..i * 4 + 4].try_into().unwrap());
            if !actual.is_finite() || (actual - d).abs() > 1e-6 {
                return Err(format!(
                    "sample-mask depth ({x},{y}) got={actual} expected={d}"
                ));
            }
        }
        println!(
            "PASS programmable-sample-mask spirv={spv} reset={reset} replay={replay} pixels=64 depth=64"
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    for spv in [false, true] {
        for reset in [false, true] {
            execute(path, spv, reset)?;
        }
    }
    Ok(())
}
