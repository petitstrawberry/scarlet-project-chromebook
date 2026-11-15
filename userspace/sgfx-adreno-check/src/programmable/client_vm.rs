// SPDX-License-Identifier: GPL-2.0-only
//! Bounded real-device client VM residency and public-object lifetime oracle.
use super::*;

const CLIENTS: usize = 32;
const ROUNDS: usize = 2;
const SIDE: u32 = 16;
const CLOSE_ATTEMPTS: usize = 8;
const CLOSE_DRAWS: usize = 128;
const WAIT_NS: u64 = 10_000_000_000;

// Keep the DSO alive independently after every public session object is closed.
// Field order destroys the receipt before releasing the library reference.
struct Receipt {
    owned: Owned,
    library: Arc<LoadedBackend>,
}
impl Receipt {
    fn state(&self, timeout_ns: u64) -> Result<u32, String> {
        let mut state = u32::MAX;
        unsafe {
            status((self.library.api.wait)(
                self.owned.0,
                timeout_ns,
                &mut state,
            ))?
        };
        if !matches!(state, abi::PENDING | abi::COMPLETE) {
            return Err(format!(
                "client VM receipt returned unexpected state={state}"
            ));
        }
        Ok(state)
    }
    fn finish(&self) -> Result<(), String> {
        if self.state(WAIT_NS)? != abi::COMPLETE {
            return Err("client VM receipt did not retire within ten seconds".into());
        }
        Ok(())
    }
}

fn color(client: usize, round: usize) -> [u8; 4] {
    // Unique across all 64 ordinary submissions. Use integer byte colors so
    // every retained target can be checked after every other client has run.
    [
        client as u8 * 7 + 17,
        round as u8 * 79 + 41,
        239 - client as u8 * 5,
        255,
    ]
}
fn push_color(rgba: [u8; 4]) -> [u8; 16] {
    let mut bytes = [0; 16];
    for (channel, value) in rgba.into_iter().enumerate() {
        bytes[channel * 4..channel * 4 + 4]
            .copy_from_slice(&(f32::from(value) / 255.0).to_le_bytes());
    }
    bytes
}

fn record<'r, 'data>(
    table: &'r ResourceTable,
    target: TextureRef<'r>,
    pipeline: ProgrammableRenderPipelineRef<'r>,
    push: &'data [u8; 16],
    draws: usize,
) -> Result<CommandBuffer<'r, 'data>, String> {
    let mut encoder = CommandEncoder::new(table);
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        target,
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store,
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_push_constants(ShaderStages::FRAGMENT, 0, push));
    for _ in 0..draws {
        checked!(pass.draw(3, 0));
    }
    checked!(pass.end());
    Ok(checked!(encoder.finish()))
}

fn verify(
    driver: &DriverSession,
    target_slot: usize,
    client: usize,
    round: usize,
) -> Result<(), String> {
    let mut pixels = vec![0xa5; (SIDE * SIDE * 4) as usize];
    driver.read_texture(target_slot, &mut pixels)?;
    let [r, g, b, a] = color(client, round);
    let expected = [b, g, r, a];
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        if pixel
            .iter()
            .zip(expected)
            .any(|(&got, want)| got.abs_diff(want) > 1)
        {
            return Err(format!(
                "client VM client={client} round={round} pixel={index} got={pixel:?} expected={expected:?}"
            ));
        }
    }
    Ok(())
}

pub(super) fn run(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-client-vm clients={CLIENTS} rounds={ROUNDS} target={SIDE}x{SIDE}");
    let table = ResourceTable::new();
    let vertex =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fragment = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
        "struct Params { color: vec4<f32> }; var<push_constant> p: Params; @fragment fn fs() -> @location(0) vec4<f32> { return p.color; }".into()
    ))));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC,
    ))));
    let layout = checked!(
        checked!(PipelineLayoutDesc::new(vec![])).with_push_constant_ranges(vec![checked!(
            PushConstantRange::new(ShaderStages::FRAGMENT, 0, 16,)
        )])
    );
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
            layout,
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
    )));
    if std::env::args().any(|arg| arg == "--compile-only") {
        let driver = DriverSession::new(path, &table)?;
        driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
        println!(
            "SKIP programmable-client-vm residency/lifetime (--compile-only; one compile context only)"
        );
        return Ok(());
    }

    let mut sessions = Vec::with_capacity(CLIENTS);
    for client in 0..CLIENTS {
        let driver = DriverSession::new(path, &table)
            .map_err(|error| format!("client VM create client={client}: {error}"))?;
        driver
            .validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())
            .map_err(|error| format!("client VM compile client={client}: {error}"))?;
        sessions.push(driver);
    }
    println!(
        "LIVE programmable-client-vm clients={} shared-metadata=true unchanged-resource-table=true",
        sessions.len()
    );

    // All 32 sessions, native resource sets and queues stay live across both
    // sweeps. There is no resource release, resync or recreation between them.
    for round in 0..ROUNDS {
        for (client, driver) in sessions.iter().enumerate() {
            let push = push_color(color(client, round));
            let commands = record(&table, target, pipeline, &push, 1)?;
            driver.submit(&commands).map_err(|error| {
                format!("client VM submit client={client} round={round}: {error}")
            })?;
        }
        // Delayed reads catch a later client accidentally overwriting an older
        // client's resource; an immediate submit/read pair would miss that.
        for (client, driver) in sessions.iter().enumerate() {
            verify(driver, target.slot(), client, round)?;
        }
        println!(
            "PASS programmable-client-vm-sweep round={round} live-clients={} submissions={CLIENTS} verified-pixels={}",
            sessions.len(),
            CLIENTS * (SIDE * SIDE) as usize
        );
    }
    println!(
        "PASS programmable-client-vm-residency live-clients={CLIENTS} sweeps={ROUNDS} submissions={} verified-pixels={} resource-table-unchanged=true",
        CLIENTS * ROUNDS,
        CLIENTS * ROUNDS * (SIDE * SIDE) as usize
    );

    // ACCEPTED here is public DriverApi admission. It does not expose when a
    // logical worker reaches kernel admission. The DSO independently retains
    // native resources, so closing these public objects is NOT proof of a raw
    // GPU_CONTEXT_DETACH_* while DMA is pending. Never label it as that proof.
    let mut pending_before_close = false;
    let mut pending_after_close = false;
    let mut attempts = 0;
    for attempt in 0..CLOSE_ATTEMPTS {
        let driver = sessions
            .pop()
            .ok_or("client VM close oracle exhausted sessions")?;
        let push = push_color(color(attempt, ROUNDS));
        let commands = record(&table, target, pipeline, &push, CLOSE_DRAWS)?;
        let library = Arc::clone(&driver.library);
        let receipt = Receipt {
            owned: driver.submit_unwaited(&commands)?,
            library,
        };
        let before = receipt.state(0)?;
        // No public resource/queue/context/device object from this session
        // remains. Only the DSO reference and independent receipt survive.
        drop(driver);
        let after = receipt.state(0)?;
        if before == abi::COMPLETE && after == abi::PENDING {
            return Err("client VM receipt regressed from complete to pending after close".into());
        }
        receipt.finish()?;
        attempts += 1;
        pending_before_close |= before == abi::PENDING;
        pending_after_close |= after == abi::PENDING;
        println!(
            "OBSERVED programmable-client-vm-close attempt={attempt} accepted=true pending-before-close={} pending-after-close={} completed-after-close=true remaining-clients={}",
            before == abi::PENDING,
            after == abi::PENDING,
            sessions.len()
        );
        if pending_after_close {
            break;
        }
    }
    if !pending_after_close {
        // Closing may race completion or synchronously drain. Neither proves
        // that pending work outlived public objects, so do not report PASS.
        return Err(format!(
            "INCONCLUSIVE programmable-client-vm-close attempts={attempts} pending-before-close={pending_before_close}: no receipt remained pending after all public session objects closed; residency passed but pending public-object lifetime unproven"
        ));
    }
    println!(
        "PASS programmable-client-vm-public-close attempts={attempts} pending-before-close=true pending-after-close={pending_after_close} completed-after-close=true raw-kernel-detach=not-observed kernel-admission=not-observed"
    );
    println!(
        "PASS programmable-client-vm clients={CLIENTS} sweeps={ROUNDS} public-object-lifetime=true pending-after-close={pending_after_close} raw-kernel-detach=not-observed"
    );
    Ok(())
}
