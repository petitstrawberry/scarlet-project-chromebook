// SPDX-License-Identifier: GPL-2.0-only
//! Installed-driver GPU-clock and pending-context retirement oracle.
use super::*;

pub(super) fn sample(driver: &DriverSession) -> Result<u64, String> {
    let api = driver
        .library
        .timestamp
        .ok_or("missing TimestampApi export")?;
    let mut ns = u64::MAX;
    unsafe {
        status((api.timestamp_ns)(driver.context.0, &mut ns))?;
    }
    if ns == u64::MAX {
        return Err("timestamp did not replace output sentinel".into());
    }
    Ok(ns)
}

// Zero timeout observes status; it does not retire pending work for the test.
fn state(driver: &DriverSession, receipt: &Owned) -> Result<u32, String> {
    let mut state = abi::PENDING;
    unsafe {
        status((driver.library.api.wait)(receipt.0, 0, &mut state))?;
    }
    Ok(state)
}

pub(super) fn run(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-timestamps");
    let table = ResourceTable::new();
    let vertex =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fragment = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
        "@fragment fn fs(@builtin(position) p:vec4<f32>)->@location(0) vec4<f32>{return vec4<f32>(fract(p.x/8.0),select(0.0,1.0,p.y>=128.0),0.25,1.0);}".into()
    ))));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(256, 256)),
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
                "fs".into()
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
        println!("SKIP programmable-timestamps execution (--compile-only)");
        return Ok(());
    }
    let other_table = ResourceTable::new();
    let other = DriverSession::new(path, &other_table)?;
    let first = sample(&driver)?;
    let middle = sample(&other)?;
    let mut previous = sample(&driver)?;
    if !(first <= middle && middle <= previous && previous > first) {
        return Err(format!(
            "GPU clock regressed/stalled across contexts: {first}, {middle}, {previous}"
        ));
    }

    let mut observed_pending = false;
    let mut rounds = 0;
    // Bound GPU work and retries. Fast completion is explicitly inconclusive,
    // never claimed as evidence that TimestampApi retired pending work.
    for round in 0..8 {
        let mut encoder = CommandEncoder::new(&table);
        let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
            &table,
            target,
            checked!(PixelRect::new(0, 0, 256, 256)),
            LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))));
        checked!(pass.set_programmable_pipeline(pipeline));
        for _ in 0..32 {
            checked!(pass.draw(3, 0));
        }
        checked!(pass.end());
        let commands = checked!(encoder.finish());
        let receipt = driver.submit_unwaited(&commands)?;
        let before = state(&driver, &receipt)?;
        if before != abi::PENDING && before != abi::COMPLETE {
            return Err(format!("unexpected pre-timestamp receipt state {before}"));
        }
        let ns = sample(&driver)?;
        // Check BEFORE texture readback, which has its own retirement barrier.
        let after = state(&driver, &receipt)?;
        if after != abi::COMPLETE || ns <= previous {
            return Err(format!(
                "timestamp did not retire/advance: receipt={after} ns={ns} previous={previous}"
            ));
        }
        observed_pending |= before == abi::PENDING;
        println!(
            "TIMESTAMP round={round} before={before} after={after} gpu_ns={ns} delta_ns={}",
            ns - previous
        );
        previous = ns;
        rounds += 1;
        if observed_pending {
            break;
        }
    }
    let mut pixels = vec![0xa5; 256 * 256 * 4];
    driver.read_texture(target.slot(), &mut pixels)?;
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        let x = index % 256;
        let y = index / 256;
        let expected = [
            64,
            if y >= 128 { 255 } else { 0 },
            (((x % 8) as f32 + 0.5) / 8.0 * 255.0).round() as u8,
            255,
        ];
        if pixel
            .iter()
            .zip(expected)
            .any(|(got, want)| got.abs_diff(want) > 1)
        {
            return Err(format!(
                "timestamp retired output pixel={x},{y} got={pixel:?} expected={expected:?}"
            ));
        }
    }
    if !observed_pending {
        return Err(
            "timestamp oracle inconclusive: all eight receipts were already complete".into(),
        );
    }
    println!(
        "PASS programmable-timestamps contexts=2 monotonic-gpu-clock=true pending-retired=true rounds={rounds} pixels=65536"
    );
    Ok(())
}
