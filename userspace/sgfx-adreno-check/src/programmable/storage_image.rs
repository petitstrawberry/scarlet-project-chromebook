// SPDX-License-Identifier: GPL-2.0-only
//! Write-only RGBA8 images and colorless fragment GPU readback oracle.
use super::*;
const W: u32 = 8;
#[derive(Clone, Copy, Debug)]
enum Mode {
    Compute,
    Vertex,
    Fragment,
    DepthOnly,
    Discard,
}
#[derive(Clone, Copy)]
struct Pair {
    wgsl: &'static str,
    spirv: &'static [u8],
}
macro_rules! pair {
    ($n:literal) => {
        Pair {
            wgsl: include_str!(concat!("storage-fixtures/", $n, ".wgsl")),
            spirv: include_bytes!(concat!("storage-fixtures/", $n, ".source.spv")),
        }
    };
}
fn module(p: Pair, spv: bool) -> Result<ShaderModuleDesc, String> {
    Ok(if spv {
        checked!(ShaderModuleDesc::spirv(
            p.spirv
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect()
        ))
    } else {
        checked!(ShaderModuleDesc::wgsl(p.wgsl.into()))
    })
}
fn execute(path: &str, mode: Mode, spv: bool, seed: u32) -> Result<(), String> {
    println!("BEGIN programmable-storage {mode:?} spirv={spv} seed={seed}");
    let t = ResourceTable::new();
    t.enable_abi_commands();
    let extent = checked!(Extent2D::new(W, W));
    let area = checked!(PixelRect::new(0, 0, W, W));
    let storage = checked!(t.define_texture(checked!(TextureDesc::new(
        TextureFormat::Rgba8Unorm,
        extent,
        TextureUsage::STORAGE | TextureUsage::COPY_SRC | TextureUsage::COPY_DST
    ))));
    let params = checked!(t.define_buffer(checked!(BufferDesc::new(
        4,
        BufferUsage::STORAGE | BufferUsage::COPY_DST
    ))));
    let has_image = matches!(mode, Mode::Compute | Mode::Vertex | Mode::Fragment);
    let visibility = match mode {
        Mode::Compute => ShaderStages::COMPUTE,
        Mode::Vertex => ShaderStages::VERTEX,
        _ => ShaderStages::FRAGMENT,
    };
    let layout = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(
            0,
            visibility,
            BindingType::StorageTexture {
                format: TextureFormat::Rgba8Unorm,
                access: StorageTextureAccess::WriteOnly
            }
        ),
        BindGroupLayoutEntry::new(
            1,
            visibility,
            BindingType::StorageBuffer { read_only: true }
        )
    ]));
    let group = checked!(t.define_bind_group(checked!(BindGroupDesc::new(
        &t,
        layout.clone(),
        vec![
            BindGroupEntry::new(0, BindingResource::Texture(storage.id())),
            BindGroupEntry::new(
                1,
                BindingResource::Buffer {
                    buffer: params.id(),
                    offset: 0,
                    size: 4
                }
            )
        ]
    ))));
    let pipeline_layout = checked!(PipelineLayoutDesc::new(if has_image {
        vec![layout]
    } else {
        vec![]
    }));
    let seed_bytes = seed.to_le_bytes();
    let sentinel = vec![0xcd; (W * W * 4) as usize];
    let mut e = CommandEncoder::new(&t);
    checked!(e.write_buffer(params, 0, &seed_bytes));
    checked!(e.write_texture(storage, checked!(TextureWrite::new(area, W * 4, &sentinel))));
    checked!(e.resource_barrier(ResourceBarrier::Global));
    let mut shaders = vec![];
    let pipeline_slot;
    let target;
    let depth;
    if matches!(mode, Mode::Compute) {
        let m = checked!(t.define_shader_module(module(pair!("compute"), spv)?));
        shaders.push(m.slot());
        let p = checked!(t.define_compute_pipeline(checked!(ComputePipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                m,
                ShaderStage::Compute,
                "main".into()
            )),
            pipeline_layout
        ))));
        pipeline_slot = p.slot();
        let mut pass = checked!(e.begin_compute_pass());
        checked!(pass.set_pipeline(p));
        checked!(pass.set_bind_group(0, group));
        checked!(pass.dispatch(2, 2, 1));
        checked!(pass.end());
        target = None;
        depth = None;
    } else {
        let color = checked!(t.define_texture(checked!(TextureDesc::new(
            TextureFormat::Bgra8Unorm,
            extent,
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
        ))));
        let z = if matches!(mode, Mode::DepthOnly | Mode::Discard) {
            Some(checked!(t.define_texture(checked!(TextureDesc::new(
                TextureFormat::Depth32Float,
                extent,
                TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
            )))))
        } else {
            None
        };
        let v = checked!(t.define_shader_module(module(
            if matches!(mode, Mode::Vertex) {
                pair!("vertex")
            } else {
                pair!("plain_vertex")
            },
            spv
        )?));
        let f = checked!(t.define_shader_module(module(
            match mode {
                Mode::Fragment => pair!("fragment"),
                Mode::DepthOnly => pair!("depth_only"),
                _ => pair!("discard"),
            },
            spv
        )?));
        shaders.extend([v.slot(), f.slot()]);
        let mut d = checked!(ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(v, ShaderStage::Vertex, "main".into())),
            checked!(ShaderEntryPoint::new(
                f,
                ShaderStage::Fragment,
                "main".into()
            )),
            pipeline_layout,
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        ));
        if z.is_some() {
            d = checked!(d.with_depth_state(DepthState::new(
                TextureFormat::Depth32Float,
                CompareFunction::Always,
                true
            )));
        }
        let p = checked!(t.define_programmable_render_pipeline(d));
        pipeline_slot = p.slot();
        let mut desc = checked!(RenderPassDesc::new(
            &t,
            color,
            area,
            LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ));
        if let Some(z) = z {
            desc = checked!(desc.with_depth_attachment(
                &t,
                z,
                DepthLoadOp::Clear(0.75),
                StoreOp::Store
            ));
        }
        let mut pass = checked!(e.begin_render_pass(desc));
        checked!(pass.set_programmable_pipeline(p));
        if has_image {
            checked!(pass.set_bind_group(0, group));
        }
        checked!(pass.draw(3, 0));
        checked!(pass.end());
        target = Some(color.slot());
        depth = z.map(|v| v.slot());
    }
    checked!(e.resource_barrier(ResourceBarrier::Global));
    let commands = checked!(e.finish());
    let driver = DriverSession::new(path, &t)?;
    for s in shaders {
        driver.validate(abi::VALIDATE_SHADER, s)?;
    }
    driver.validate(
        if matches!(mode, Mode::Compute) {
            abi::VALIDATE_COMPUTE_PIPELINE
        } else {
            abi::VALIDATE_RENDER_PIPELINE
        },
        pipeline_slot,
    )?;
    if std::env::args().any(|a| a == "--compile-only") {
        println!("COMPILED programmable-storage {mode:?} spirv={spv}");
        return Ok(());
    }
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut pixels = vec![0xa5; (W * W * 4) as usize];
        if has_image {
            driver.read_texture(storage.slot(), &mut pixels)?;
            for (i, p) in pixels.chunks_exact(4).enumerate() {
                let x = i as u32 % W;
                let y = i as u32 / W;
                let expected = [
                    (13 + 3 * x + seed) as u8,
                    (29 + 5 * y) as u8,
                    (51 + 7 * (x ^ y)) as u8,
                    255,
                ];
                if p.iter().zip(expected).any(|(a, b)| a.abs_diff(b) > 1) {
                    return Err(format!(
                        "storage {mode:?} spirv={spv} ({x},{y}) RGBA={p:?} expected={expected:?}"
                    ));
                }
            }
        }
        if let Some(target) = target {
            driver.read_texture(target, &mut pixels)?;
            for (i, p) in pixels.chunks_exact(4).enumerate() {
                if p != [255, 0, 255, 255] {
                    return Err(format!(
                        "colorless {mode:?} modified color pixel {i}: {p:?}"
                    ));
                }
            }
        }
        if let Some(depth) = depth {
            driver.read_texture(depth, &mut pixels)?;
            let expected = if matches!(mode, Mode::DepthOnly) {
                0.25f32
            } else {
                0.75f32
            };
            for (i, p) in pixels.chunks_exact(4).enumerate() {
                let v = f32::from_le_bytes(p.try_into().unwrap());
                if (v - expected).abs() > 1e-6 || !v.is_finite() {
                    return Err(format!(
                        "colorless {mode:?} depth pixel{i} got={v} expected={expected}"
                    ));
                }
            }
        }
        println!(
            "PASS programmable-storage {mode:?} spirv={spv} seed={seed} replay={replay} pixels={}",
            W * W
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    for seed in [29, 97] {
        for spv in [false, true] {
            for mode in [
                Mode::Compute,
                Mode::Vertex,
                Mode::Fragment,
                Mode::DepthOnly,
                Mode::Discard,
            ] {
                execute(path, mode, spv, seed)?;
            }
        }
    }
    Ok(())
}
