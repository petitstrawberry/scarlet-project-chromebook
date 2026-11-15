// SPDX-License-Identifier: GPL-2.0-only
//! Bounded single-pass A/B measurement; clock spans include submission overhead.
use super::*;
use std::time::Instant;

const WIDTH: u32 = 784;
const HEIGHT: u32 = 826;
const SHADER: &str = include_str!("../../../sgfx-shader-ir3/tests/shaders/glass.wgsl");

fn shader(hoisted: bool) -> String {
    if !hoisted {
        return SHADER.into();
    }
    SHADER.replace(
        "fn blur(position: vec2<f32>, axis: vec2<f32>) -> vec4<f32> {",
        "fn blur(position: vec2<f32>, axis: vec2<f32>) -> vec4<f32> {\n    let inverse_extent = 1.0 / p.extent.xy;",
    ).replace(
        "sample_position / p.extent.xy, 0.0) * p.kernel[i].y",
        "sample_position * inverse_extent, 0.0) * p.kernel[i].y",
    )
}

fn kernel(radius: u32) -> Vec<(f32, f32)> {
    if radius == 0 {
        return vec![(0.0, 1.0)];
    }
    if radius <= 4 {
        return [1., 8., 28., 56., 70., 56., 28., 8., 1.]
            .into_iter()
            .enumerate()
            .map(|(i, w)| ((i as f32 - 4.) * radius as f32 * 0.25, w / 256.))
            .collect();
    }
    // Same bilinear pairing and accumulation order as renderer-sgfx.
    let variance = (radius * radius) as f32 / 8.;
    let weight = |x: u32| (-((x * x) as f32) / (2. * variance)).exp();
    let mut taps = vec![(0., 1.)];
    let mut total = 1.;
    for x in (1..=radius).step_by(2) {
        let a = weight(x);
        let b = if x < radius { weight(x + 1) } else { 0. };
        let sum = a + b;
        let offset = x as f32 + b / sum;
        taps.extend([(-offset, sum), (offset, sum)]);
        total += 2. * sum;
    }
    for tap in &mut taps {
        tap.1 /= total;
    }
    taps
}

fn pixel(x: u32, y: u32) -> [u8; 4] {
    [
        ((x * 31 + y * 7) % 251) as u8,
        ((x * 13 + y * 29) % 251) as u8,
        ((x * 47 + y * 11) % 251) as u8,
        255,
    ]
}

fn expected(x: u32, y: u32, vertical: bool, taps: &[(f32, f32)]) -> [u8; 4] {
    let mut value = [0f32; 4];
    for &(offset, weight) in taps {
        let coordinate = if vertical { y } else { x } as f32 + offset;
        let low = coordinate.floor();
        let fraction = coordinate - low;
        let limit = if vertical { HEIGHT } else { WIDTH } - 1;
        let at = |c: f32| {
            let c = c.clamp(0., limit as f32) as u32;
            if vertical { pixel(x, c) } else { pixel(c, y) }
        };
        for ((out, a), b) in value.iter_mut().zip(at(low)).zip(at(low + 1.)) {
            *out += (f32::from(a) * (1. - fraction) + f32::from(b) * fraction) * weight;
        }
    }
    value.map(|v| v.round().clamp(0., 255.) as u8)
}

fn run_case(path: &str, vertical: bool, radius: u32, hoisted: bool) -> Result<(), String> {
    let table = ResourceTable::new();
    table.enable_abi_commands();
    let module =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(shader(hoisted)))));
    let extent = checked!(Extent2D::new(WIDTH, HEIGHT));
    let source = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ))));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let uniform = checked!(table.define_buffer(checked!(BufferDesc::new(
        1120,
        BufferUsage::UNIFORM | BufferUsage::COPY_DST
    ))));
    let sampler = checked!(table.define_sampler(SamplerDesc::new(
        FilterMode::Linear,
        FilterMode::Linear,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge
    )));
    let layout = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(
            0,
            ShaderStages::VERTEX | ShaderStages::FRAGMENT,
            BindingType::UniformBuffer
        ),
        BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, BindingType::SampledTexture),
        BindGroupLayoutEntry::new(2, ShaderStages::FRAGMENT, BindingType::Sampler),
    ]));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        layout.clone(),
        vec![
            BindGroupEntry::new(
                0,
                BindingResource::Buffer {
                    buffer: uniform.id(),
                    offset: 0,
                    size: 1120
                }
            ),
            BindGroupEntry::new(1, BindingResource::Texture(source.id())),
            BindGroupEntry::new(2, BindingResource::Sampler(sampler.id())),
        ]
    ))));
    let entry = if vertical { "vertical" } else { "horizontal" };
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                module,
                ShaderStage::Vertex,
                "blur_vertex".into()
            )),
            checked!(ShaderEntryPoint::new(
                module,
                ShaderStage::Fragment,
                entry.into()
            )),
            checked!(PipelineLayoutDesc::new(vec![layout])),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
    )));
    let taps = kernel(radius);
    let mut params = [0f32; 280];
    params[2..8].copy_from_slice(&[
        WIDTH as f32,
        HEIGHT as f32,
        WIDTH as f32,
        HEIGHT as f32,
        1.,
        4.,
    ]);
    params[14..16].copy_from_slice(&[1., taps.len() as f32]);
    params[18..20].copy_from_slice(&[WIDTH as f32, HEIGHT as f32]);
    for (i, &(offset, weight)) in taps.iter().enumerate() {
        params[20 + 4 * i] = offset;
        params[21 + 4 * i] = weight;
    }
    let params: Vec<u8> = params.into_iter().flat_map(f32::to_le_bytes).collect();
    let area = checked!(PixelRect::new(0, 0, WIDTH, HEIGHT));
    let mut encoder = CommandEncoder::new(&table);
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        area,
        LoadOp::Load,
        StoreOp::Store
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.draw(6, 0));
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    #[cfg(test)]
    if path == "__encode_only__" {
        assert!(commands.abi_batch().unwrap().count > 0);
        return Ok(());
    }
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    if std::env::args().any(|a| a == "--compile-only") {
        return Ok(());
    }
    let bytes: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|i| pixel(i % WIDTH, i / WIDTH))
        .collect();
    let mut setup = CommandEncoder::new(&table);
    checked!(setup.write_buffer(uniform, 0, &params));
    checked!(setup.write_texture(source, checked!(TextureWrite::new(area, WIDTH * 4, &bytes))));
    driver.submit(&checked!(setup.finish()))?;
    let variant = if hoisted { "hoisted" } else { "baseline" };
    println!(
        "BEGIN glass-profile axis={entry} variant={variant} taps={} width={WIDTH} height={HEIGHT} warmups=4 samples=3",
        taps.len()
    );
    // Cover all four upload-arena slots before measuring the stable workload.
    for _ in 0..4 {
        driver.submit(&commands)?;
    }
    for round in 0..3 {
        let before = timestamps::sample(&driver)?;
        let start = Instant::now();
        let receipt = driver.submit_unwaited(&commands)?;
        let submitted = Instant::now();
        let mut state = abi::PENDING;
        unsafe {
            status((driver.library.api.wait)(
                receipt.0,
                10_000_000_000,
                &mut state,
            ))?;
        }
        if state != abi::COMPLETE {
            return Err(format!("glass profile receipt state={state}"));
        }
        let retired = Instant::now();
        let after = timestamps::sample(&driver)?;
        let span = after.checked_sub(before).ok_or("GPU clock regressed")?;
        println!(
            "GLASS_PROFILE axis={entry} variant={variant} taps={} round={round} submit_us={} completion_wait_us={} wall_us={} retired_clock_span_ns={span} clock_span_includes_host_queue_kernel=true",
            taps.len(),
            submitted.duration_since(start).as_micros(),
            retired.duration_since(submitted).as_micros(),
            retired.duration_since(start).as_micros()
        );
    }
    let mut result = vec![0xa5; bytes.len()];
    driver.read_texture(target.slot(), &mut result)?;
    // Deterministic interior and edge probes, independent bilinear CPU oracle.
    for y in [0, 1, 17, HEIGHT / 2, HEIGHT - 18, HEIGHT - 2, HEIGHT - 1] {
        for x in [0, 1, 17, WIDTH / 2, WIDTH - 18, WIDTH - 2, WIDTH - 1] {
            let offset = ((y * WIDTH + x) * 4) as usize;
            let got = &result[offset..offset + 4];
            let want = expected(x, y, vertical, &taps);
            if got.iter().zip(want).any(|(&a, b)| a.abs_diff(b) > 2) {
                return Err(format!(
                    "glass-profile {entry}/{variant}/{} pixel={x},{y} got={got:?} want={want:?}",
                    taps.len()
                ));
            }
        }
    }
    println!(
        "PASS glass-profile axis={entry} variant={variant} taps={} probes=49 tolerance=2",
        taps.len()
    );
    Ok(())
}

pub(super) fn run(path: &str) -> Result<(), String> {
    println!(
        "GLASS_PROFILE_SCOPE single-pass; no UI/SWS/presentation; no per-round texture allocation/upload; correlate a618-profile for kernel execution time"
    );
    for vertical in [false, true] {
        for radius in [0, 4, 24, 64] {
            for hoisted in [false, true] {
                run_case(path, vertical, radius, hoisted)?;
            }
        }
    }
    Ok(())
}

#[test]
fn bounded_cases_encode_and_kernel_matches_reference_properties() {
    for radius in [0, 4, 24, 64] {
        let taps = kernel(radius);
        assert_eq!(
            taps.len(),
            match radius {
                0 => 1,
                4 => 9,
                24 => 25,
                _ => 65,
            }
        );
        assert!((taps.iter().map(|t| t.1).sum::<f32>() - 1.).abs() < 1e-6);
        for vertical in [false, true] {
            if radius == 0 {
                assert_eq!(expected(17, 19, vertical, &taps), pixel(17, 19));
            }
            for hoisted in [false, true] {
                run_case("__encode_only__", vertical, radius, hoisted).unwrap();
            }
        }
    }
    assert_ne!(shader(false), shader(true));
    assert_eq!(
        shader(true)
            .matches("sample_position * inverse_extent")
            .count(),
        1
    );
}
