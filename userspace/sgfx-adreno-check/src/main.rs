// SPDX-License-Identifier: GPL-2.0-only
//! Installed-driver checks. A successful fixed-pipeline test is never a claim
//! that programmable graphics, compute or floating-point targets are supported.
#[cfg(target_os = "scarlet")]
mod abi_session;
#[cfg(target_os = "scarlet")]
mod benchmark;
#[cfg(target_os = "scarlet")]
mod blends;
#[cfg(target_os = "scarlet")]
mod global;
#[cfg(any(test, target_os = "scarlet"))]
mod programmable;
#[cfg(target_os = "scarlet")]
mod samplers;
#[cfg(target_os = "scarlet")]
mod tiles;
#[cfg(target_os = "scarlet")]
fn run() -> Result<(), String> {
    use sgfx::{
        backend::{CommandSubmitter, Completion, CompletionStatus},
        ir::*,
    };
    use std::{rc::Rc, time::Duration};
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    let device = check!(sgfx::dynamic::Device::open("/dev/gpu0"));
    println!(
        "driver={} library={:?}",
        device.backend(),
        device.backend_library()
    );
    if !device
        .backend_library()
        .is_some_and(|path| path.ends_with("libsgfx_scarlet_adreno.so"))
    {
        return Err("expected the installed Adreno dynamic driver".into());
    }
    if std::env::args().any(|arg| arg == "--benchmark") {
        return benchmark::run(device.backend_library().ok_or("missing driver library")?);
    }
    if std::env::args().any(|arg| arg == "--programmable") {
        return programmable::run(device.backend_library().ok_or("missing driver library")?);
    }
    let caps = device.capabilities();
    let features = [
        ("rendering", caps.supports_rendering()),
        ("presentation", caps.supports_presentation()),
        ("image-upload", caps.supports_image_upload()),
        ("image-readback", caps.supports_image_readback()),
        ("depth", caps.supports_depth()),
        (
            "programmable-graphics",
            caps.supports_programmable_graphics(),
        ),
        ("texture-arrays", caps.supports_texture_arrays()),
        ("depth-sampling", caps.supports_depth_sampling()),
        ("image-mips", caps.supports_image_mips()),
        (
            "read-only-storage",
            caps.supports_read_only_storage_buffers(),
        ),
        ("typed-texture-views", caps.supports_typed_texture_views()),
        ("srgb-texture-views", caps.supports_srgb_texture_views()),
        (
            "extended-vertex-formats",
            caps.supports_extended_vertex_formats(),
        ),
        ("rgba8-attachment", caps.supports_rgba8_color_attachment()),
        ("image-blits", caps.supports_image_blits()),
        ("push-constants-128", caps.supports_push_constants_128()),
        ("color-attachments-8", caps.supports_color_attachments_8()),
        ("compute-buffers", caps.supports_compute_buffers()),
        ("anisotropy-16", caps.supports_anisotropy_16()),
        ("vertex-divisors", caps.supports_vertex_divisors()),
        (
            "shader-draw-parameters",
            caps.supports_shader_draw_parameters(),
        ),
        ("depth-transfers", caps.supports_depth_transfers()),
        ("float-color", caps.supports_float_color()),
        (
            "immediate-buffer-readback",
            caps.supports_immediate_buffer_readback(),
        ),
        ("global-barriers", caps.supports_global_resource_barriers()),
        (
            "implicit-barriers",
            caps.supports_implicit_resource_barriers(),
        ),
    ];
    for (name, supported) in features {
        println!("capability {name}={supported}");
    }
    let table = Rc::new(ResourceTable::new());
    // More than one upload/copy chunk, and distinct nonzero source/destination
    // offsets. Draw vertices from the final bytes to catch truncated copies.
    const SIZE: u64 = 320 * 1024;
    const SOURCE_OFFSET: u64 = 64;
    const DEST_OFFSET: u64 = 96;
    const VERTEX_BYTES: u64 = 6 * 32;
    const SIDE: u32 = 32;
    let source = check!(table.define_buffer(check!(BufferDesc::new(
        SIZE + SOURCE_OFFSET,
        BufferUsage::COPY_SRC | BufferUsage::COPY_DST
    ))))
    .id();
    let vertices = check!(table.define_buffer(check!(BufferDesc::new(
        SIZE + DEST_OFFSET,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))))
    .id();
    let target = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        check!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))))
    .id();
    let pipeline = check!(table.define_render_pipeline(check!(RenderPipelineDesc::new(
        TextureFormat::Bgra8Unorm,
        PrimitiveTopology::TriangleList,
        check!(VertexBufferLayout::new(
            32,
            vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x4, 16)
            ]
        )),
        FragmentProgram::VertexColor,
        BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise)
    ))))
    .id();
    let context = check!(device.create_context());
    let mut session = check!(context.create_mapped_target_session(table.clone(), &[target]));
    let area = check!(PixelRect::new(0, 0, SIDE, SIDE));
    for generation in 0..10 {
        let rgba = if generation % 2 == 0 {
            [1.0f32, 0.0, 0.0, 1.0]
        } else {
            [0.0, 1.0, 0.0, 1.0]
        };
        let mut data = vec![0xcd; (SIZE - VERTEX_BYTES) as usize];
        for [x, y] in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [-1.0, 1.0],
            [-1.0, 1.0],
            [1.0, -1.0],
            [1.0, 1.0],
        ] {
            for value in [x, y, 0.0, 1.0].into_iter().chain(rgba) {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
        let mut encoder = CommandEncoder::new(&table);
        check!(encoder.write_buffer(check!(table.buffer_ref(source)), SOURCE_OFFSET, &data));
        check!(encoder.copy_buffer_to_buffer(
            check!(table.buffer_ref(source)),
            SOURCE_OFFSET,
            check!(table.buffer_ref(vertices)),
            DEST_OFFSET,
            SIZE
        ));
        // Alternate explicit and implicit copy-to-vertex transitions.
        if generation % 2 == 0 {
            check!(encoder.resource_barrier(ResourceBarrier::Buffer {
                buffer: check!(table.buffer_ref(vertices)),
                before: BufferAccess::CopyDestination,
                after: BufferAccess::Vertex,
            }));
        }
        let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
            &table,
            check!(table.texture_ref(target)),
            area,
            LoadOp::Clear(check!(Color::rgba(0.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))));
        check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
        check!(pass.set_vertex_buffer(
            check!(table.buffer_ref(vertices)),
            DEST_OFFSET + SIZE - VERTEX_BYTES
        ));
        check!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        check!(pass.draw(6, 0));
        check!(pass.end());
        let commands = check!(encoder.finish());
        let receipt = check!(session.executor().submit(&commands));
        drop(commands);
        drop(data);
        if check!(receipt.wait(Some(Duration::from_secs(10)))) != CompletionStatus::Complete {
            return Err(format!("GPU timeout at generation {generation}"));
        }
        let mut pixels = vec![0xa5; (SIDE * SIDE * 4) as usize];
        check!(session.readback_bgra(target, &mut pixels, SIDE * 4, area));
        let expected = if generation % 2 == 0 {
            [0, 0, 255, 255]
        } else {
            [0, 255, 0, 255]
        };
        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
            if pixel != expected {
                return Err(format!(
                    "generation={generation} pixel={index} actual={pixel:?} expected={expected:?}"
                ));
            }
        }
        println!(
            "SGFX_ADRENO_COPY_OK generation={generation} bytes={SIZE} pixels={}",
            SIDE * SIDE
        );
    }
    global::check(device.backend_library().ok_or("missing driver library")?)?;
    tiles::check(&device)?;
    blends::check(device.backend_library().ok_or("missing driver library")?)?;
    samplers::check(device.backend_library().ok_or("missing driver library")?)?;
    println!("SGFX_ADRENO_FIXED_OK");
    let missing: Vec<_> = features
        .iter()
        .filter_map(|(name, supported)| (!supported).then_some(*name))
        .collect();
    if !missing.is_empty() {
        println!("SGFX_ADRENO_FULL_UNAVAILABLE missing={}", missing.join(","));
        if !std::env::args().any(|arg| arg == "--fixed-only") {
            return Err("full capability contract is not implemented".into());
        }
    }
    Ok(())
}
#[cfg(not(target_os = "scarlet"))]
fn run() -> Result<(), String> {
    Err("run this check on Scarlet/CoachZ".into())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sgfx-adreno-check: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
