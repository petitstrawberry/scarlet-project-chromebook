// SPDX-License-Identifier: GPL-2.0-only
//! Independent nonsquare/mipped2D pixel oracle for the actual WGSL builtin.
use super::*;
const SOURCE: &str = include_str!("base-clamp-fixtures/shader.wgsl");
fn texels() -> Vec<u8> {
    let mut data = Vec::new();
    for y in 0..5u8 {
        for x in 0..3u8 {
            data.extend_from_slice(&[
                17 + 50 * x + 11 * y,
                29 + 13 * x + 41 * y,
                47 + 65 * x + 17 * y,
                255,
            ]);
        }
    }
    data
}
fn expected(x: u32, y: u32, linear: bool, data: &[u8]) -> [u8; 4] {
    let u = ((x as f32 + 0.5) / 16.0 * 2.5 - 0.75).clamp(0.5 / 3.0, 1.0 - 0.5 / 3.0);
    let v = ((y as f32 + 0.5) / 16.0 * 2.5 - 0.75).clamp(0.5 / 5.0, 1.0 - 0.5 / 5.0);
    let pixel = |x: i32, y: i32| {
        let offset = (y.clamp(0, 4) * 3 + x.clamp(0, 2)) as usize * 4;
        [
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]
    };
    if !linear {
        return pixel((u * 3.0).floor() as i32, (v * 5.0).floor() as i32);
    }
    let px = u * 3.0 - 0.5;
    let py = v * 5.0 - 0.5;
    let ix = px.floor() as i32;
    let iy = py.floor() as i32;
    let fx = px - ix as f32;
    let fy = py - iy as f32;
    let samples = [
        pixel(ix, iy),
        pixel(ix + 1, iy),
        pixel(ix, iy + 1),
        pixel(ix + 1, iy + 1),
    ];
    core::array::from_fn(|k| {
        ((f32::from(samples[0][k]) * (1.0 - fx) + f32::from(samples[1][k]) * fx) * (1.0 - fy)
            + (f32::from(samples[2][k]) * (1.0 - fx) + f32::from(samples[3][k]) * fx) * fy)
            .round() as u8
    })
}
fn execute(path: &str, spirv: bool, linear: bool, mirror: bool) -> Result<(), String> {
    let table = ResourceTable::new();
    table.enable_abi_commands();
    let modules = if spirv {
        [
            include_bytes!("base-clamp-fixtures/vs.spv").as_slice(),
            include_bytes!("base-clamp-fixtures/fs.spv").as_slice(),
        ]
        .map(|bytes| {
            table
                .define_shader_module(
                    ShaderModuleDesc::spirv(
                        bytes
                            .chunks_exact(4)
                            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                            .collect(),
                    )
                    .unwrap(),
                )
                .unwrap()
        })
    } else {
        let m =
            checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(SOURCE.into()))));
        [m, m]
    };
    let texture = checked!(
        table.define_texture(checked!(
            checked!(TextureDesc::new(
                TextureFormat::Bgra8Unorm,
                checked!(Extent2D::new(3, 5)),
                TextureUsage::SAMPLED | TextureUsage::COPY_DST
            ))
            .with_mip_level_count(3)
        ))
    );
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(16, 16)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let filter = if linear {
        FilterMode::Linear
    } else {
        FilterMode::Nearest
    };
    let address = if mirror {
        AddressMode::MirrorRepeat
    } else {
        AddressMode::Repeat
    };
    let sampler =
        checked!(table.define_sampler(SamplerDesc::new(filter, filter, address, address)));
    let combined = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0, ShaderStages::FRAGMENT, BindingType::SampledTexture),
        BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, BindingType::Sampler)
    ]));
    let tex_layout = checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::FRAGMENT,
        BindingType::SampledTexture
    )]));
    let sam_layout = checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::FRAGMENT,
        BindingType::Sampler
    )]));
    let layout = checked!(PipelineLayoutDesc::new(if spirv {
        vec![
            checked!(BindGroupLayoutDesc::new(vec![])),
            checked!(BindGroupLayoutDesc::new(vec![])),
            tex_layout.clone(),
            sam_layout.clone(),
        ]
    } else {
        vec![combined.clone()]
    }));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                modules[0],
                ShaderStage::Vertex,
                "vs".into()
            )),
            checked!(ShaderEntryPoint::new(
                modules[1],
                ShaderStage::Fragment,
                "fs".into()
            )),
            layout,
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )
    )));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        if spirv { tex_layout } else { combined },
        if spirv {
            vec![BindGroupEntry::new(
                0,
                BindingResource::Texture(texture.id()),
            )]
        } else {
            vec![
                BindGroupEntry::new(0, BindingResource::Texture(texture.id())),
                BindGroupEntry::new(1, BindingResource::Sampler(sampler.id())),
            ]
        }
    ))));
    let sampler_group = if spirv {
        Some(checked!(table.define_bind_group(checked!(
            BindGroupDesc::new(
                &table,
                sam_layout,
                vec![BindGroupEntry::new(
                    0,
                    BindingResource::Sampler(sampler.id())
                )]
            )
        ))))
    } else {
        None
    };
    let pixels = texels();
    let mip1 = [255u8, 0, 255, 255].repeat(2);
    let mip2 = [0u8, 255, 255, 255];
    let mut encoder = CommandEncoder::new(&table);
    for (level, width, height, data) in [
        (0, 3, 5, pixels.as_slice()),
        (1, 1, 2, mip1.as_slice()),
        (2, 1, 1, mip2.as_slice()),
    ] {
        checked!(
            encoder.write_texture(
                texture,
                checked!(TextureWrite::new(
                    checked!(PixelRect::new(0, 0, width, height)),
                    width * 4,
                    data
                ))
                .with_mip_level(level)
            )
        );
    }
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        checked!(PixelRect::new(0, 0, 16, 16)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_bind_group(if spirv { 2 } else { 0 }, group));
    if let Some(group) = sampler_group {
        checked!(pass.set_bind_group(3, group));
    }
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    #[cfg(test)]
    if path == "__encode__" {
        assert!(commands.abi_batch().unwrap().count > 0);
        return Ok(());
    }
    let driver = DriverSession::new(path, &table)?;
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut result = vec![0xa5; 1024];
        driver.read_texture(target.slot(), &mut result)?;
        for y in 0..16 {
            for x in 0..16 {
                let want = expected(x, y, linear, &pixels);
                let off = ((y * 16 + x) * 4) as usize;
                for k in 0..4 {
                    if result[off + k].abs_diff(want[k]) > 2 {
                        return Err(format!(
                            "base-clamp spirv={spirv} linear={linear} mirror={mirror} replay={replay} pixel={x},{y} channel={k} got={} want={}",
                            result[off + k],
                            want[k]
                        ));
                    }
                }
            }
        }
        println!(
            "PASS base-clamp spirv={spirv} linear={linear} mirror={mirror} replay={replay} nonsquare=3x5 mips=3 explicit-base=0 pixels=256"
        );
    }
    Ok(())
}
pub fn run(path: &str) -> Result<(), String> {
    for spirv in [false, true] {
        for linear in [false, true] {
            for mirror in [false, true] {
                execute(path, spirv, linear, mirror)?;
            }
        }
    }
    println!("PASS base-clamp ALL languages=2 filters=2 address-modes=2 replays=2 pixels=4096");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_public_nonsquare_three_mip_base_clamp_oracle_encodes_all_cases() {
        run("__encode__").unwrap();
        let bytes = texels();
        assert_eq!(expected(0, 0, false, &bytes), [17, 29, 47, 255]);
        assert_eq!(expected(15, 15, true, &bytes), [161, 219, 245, 255]);
    }
}
