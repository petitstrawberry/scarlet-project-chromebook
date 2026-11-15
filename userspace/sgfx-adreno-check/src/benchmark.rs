// SPDX-License-Identifier: GPL-2.0-only
//! Offscreen measurements with validated pixels, no display/capture traffic.
use sgfx_core::ir::*;

pub fn run(library: &str) -> std::result::Result<(), String> {
    verify_overlapping_uniforms(library)?;
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    for (width, height) in [(32, 32), (512, 512), (1080, 720), (2160, 1440)] {
        let table = ResourceTable::new();
        let extent = check!(Extent2D::new(width, height));
        let target = check!(table.define_texture(check!(TextureDesc::new(
            TextureFormat::Bgra8Unorm,
            extent,
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::PRESENT | TextureUsage::COPY_SRC
        ))))
        .id();
        let source = check!(table.define_texture(check!(TextureDesc::new(
            TextureFormat::Bgra8Unorm,
            extent,
            TextureUsage::SAMPLED | TextureUsage::COPY_DST
        ))))
        .id();
        let vertex = check!(table.define_buffer(check!(BufferDesc::new(
            96,
            BufferUsage::VERTEX | BufferUsage::COPY_DST
        ))))
        .id();
        let sampler = check!(table.define_sampler(SamplerDesc::new(
            FilterMode::Linear,
            FilterMode::Linear,
            AddressMode::ClampToEdge,
            AddressMode::ClampToEdge
        )))
        .id();
        let mut pipelines = Vec::new();
        let over = BlendComponent::new(
            BlendFactor::One,
            BlendFactor::OneMinusSourceAlpha,
            BlendOp::Add,
        );
        for fragment in [
            FragmentProgram::Solid,
            FragmentProgram::Texture(TextureSampleMode::Rgba),
        ] {
            pipelines.push(
                check!(table.define_render_pipeline(check!(RenderPipelineDesc::new(
                    TextureFormat::Bgra8Unorm,
                    PrimitiveTopology::TriangleList,
                    check!(VertexBufferLayout::new(
                        16,
                        vec![
                            VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                            VertexAttribute::new(1, VertexFormat::Float32x2, 8),
                        ]
                    )),
                    fragment,
                    BlendState::new(over, over),
                    RasterState::new(CullMode::None, FrontFace::CounterClockwise)
                ))))
                .id(),
            );
        }
        let mut data = Vec::new();
        for [x, y] in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [-1.0, 1.0],
            [-1.0, 1.0],
            [1.0, -1.0],
            [1.0, 1.0],
        ] {
            for v in [x, y, (x + 1.0) * 0.5, (y + 1.0) * 0.5] {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        let area = check!(PixelRect::new(0, 0, width, height));
        let green = check!(Color::rgba(0.0, 1.0, 0.0, 1.0));
        let mut session = crate::abi_session::Session::new(library, &table, &[0])?;
        let pixels = [0u8, 255, 0, 255].repeat(width as usize * height as usize);
        let mut encoder = CommandEncoder::new(&table);
        check!(encoder.write_buffer(check!(table.buffer_ref(vertex)), 0, &data));
        check!(encoder.write_texture(
            check!(table.texture_ref(source)),
            check!(TextureWrite::new(area, width * 4, &pixels))
        ));
        session.submit(&check!(encoder.finish()))?;
        for (name, draws, sampled, upload) in [
            ("checkpoint", 0, false, false),
            ("clear", 0, false, false),
            ("solid", 1, false, false),
            ("sample", 1, true, false),
            ("sample7", 7, true, false),
            ("sample7-vertices", 7, true, true),
        ] {
            let mut encoder = CommandEncoder::new(&table);
            if upload {
                check!(encoder.write_buffer(check!(table.buffer_ref(vertex)), 0, &data));
            }
            if name == "checkpoint" {
                check!(encoder.resource_barrier(ResourceBarrier::Global));
            } else {
                let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
                    &table,
                    check!(table.texture_ref(target)),
                    area,
                    if draws == 0 {
                        LoadOp::Clear(green)
                    } else {
                        LoadOp::Load
                    },
                    StoreOp::Store
                ))));
                if draws > 0 {
                    check!(pass.set_pipeline(check!(
                        table.render_pipeline_ref(pipelines[usize::from(sampled)])
                    )));
                    check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertex)), 0));
                    check!(pass.set_uniforms(DrawUniforms::new(
                        Transform::identity(),
                        if sampled {
                            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
                        } else {
                            green
                        }
                    )));
                    if sampled {
                        check!(pass.set_texture(check!(table.texture_ref(source))));
                        check!(pass.set_sampler(check!(table.sampler_ref(sampler))));
                    }
                    for _ in 0..draws {
                        check!(pass.draw(6, 0));
                    }
                }
                check!(pass.end());
            }
            let commands = check!(encoder.finish());
            let mut samples = Vec::new();
            for i in 0..28 {
                let timing = session.submit_measured(&commands)?;
                if i >= 4 {
                    samples.push(timing);
                }
            }
            if name == "sample" {
                for depth in [4, 16] {
                    let mut bursts = Vec::new();
                    let mut retries = 0;
                    for i in 0..12 {
                        let t = session.submit_burst(&commands, depth)?;
                        if i >= 4 {
                            bursts.push(t[0] + t[1]);
                            retries += t[2];
                        }
                    }
                    bursts.sort_unstable();
                    println!(
                        "SGFX_ADRENO_BURST width={width} height={height} depth={depth} batch_p50_us={} per_submit_us={} busy_retries={retries}",
                        bursts[4] / 1000,
                        bursts[4] / 1000 / depth as u64
                    );
                }
            }
            if name != "checkpoint" {
                // The mapped-image ABI writes at the source's coordinates.
                let mut readback = vec![0u8; width as usize * height as usize * 4];
                session.readback(
                    0,
                    &mut readback,
                    width * 4,
                    check!(PixelRect::new(width / 2, height / 2, 1, 1)),
                )?;
                let offset = ((height / 2 * width + width / 2) * 4) as usize;
                let pixel = &readback[offset..offset + 4];
                if pixel != [0, 255, 0, 255] {
                    return Err(format!("benchmark {name}: wrong pixel {pixel:?}"));
                }
            }
            let mut totals: Vec<_> = samples.iter().map(|t| t[0] + t[1]).collect();
            let mut admission: Vec<_> = samples.iter().map(|t| t[0]).collect();
            let mut retirement: Vec<_> = samples.iter().map(|t| t[1]).collect();
            totals.sort_unstable();
            admission.sort_unstable();
            retirement.sort_unstable();
            println!(
                "SGFX_ADRENO_BENCH mode={name} width={width} height={height} samples={} total_p50_us={} total_p90_us={} admission_p50_us={} wait_p50_us={} busy_retries={}",
                samples.len(),
                totals[12] / 1000,
                totals[21] / 1000,
                admission[12] / 1000,
                retirement[12] / 1000,
                samples.iter().map(|t| t[2]).sum::<u64>()
            );
        }
    }
    Ok(())
}

// Different generated uniform bytes in 16 submissions expose premature reuse
// across the four arenas. Verify every destination after all receipts retire.
fn verify_overlapping_uniforms(library: &str) -> std::result::Result<(), String> {
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    let table = ResourceTable::new();
    let extent = check!(Extent2D::new(32, 32));
    let mut targets = Vec::new();
    for _ in 0..16 {
        targets.push(
            check!(table.define_texture(check!(TextureDesc::new(
                TextureFormat::Bgra8Unorm,
                extent,
                TextureUsage::RENDER_ATTACHMENT | TextureUsage::PRESENT | TextureUsage::COPY_SRC
            ))))
            .id(),
        );
    }
    let vertex = check!(table.define_buffer(check!(BufferDesc::new(
        96,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))))
    .id();
    let pipeline = check!(table.define_render_pipeline(check!(RenderPipelineDesc::new(
        TextureFormat::Bgra8Unorm,
        PrimitiveTopology::TriangleList,
        check!(VertexBufferLayout::new(
            16,
            vec![
                VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                VertexAttribute::new(1, VertexFormat::Float32x2, 8)
            ]
        )),
        FragmentProgram::Solid,
        BlendState::new(
            BlendComponent::new(BlendFactor::One, BlendFactor::Zero, BlendOp::Add),
            BlendComponent::new(BlendFactor::One, BlendFactor::Zero, BlendOp::Add)
        ),
        RasterState::new(CullMode::None, FrontFace::CounterClockwise)
    ))))
    .id();
    let mut data = Vec::new();
    for [x, y] in [
        [-1.0f32, -1.0],
        [1.0, -1.0],
        [-1.0, 1.0],
        [-1.0, 1.0],
        [1.0, -1.0],
        [1.0, 1.0],
    ] {
        for value in [x, y, 0.0, 0.0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
    let area = check!(PixelRect::new(0, 0, 32, 32));
    let mut session =
        crate::abi_session::Session::new(library, &table, &(0..16).collect::<Vec<_>>())?;
    let mut upload = CommandEncoder::new(&table);
    check!(upload.write_buffer(check!(table.buffer_ref(vertex)), 0, &data));
    session.submit(&check!(upload.finish()))?;
    for round in 0..3 {
        let mut commands = Vec::new();
        for (index, &target) in targets.iter().enumerate() {
            let mut encoder = CommandEncoder::new(&table);
            let color = check!(Color::rgba(
                ((index + round) % 16) as f32 / 15.0,
                0.0,
                0.0,
                1.0
            ));
            let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
                &table,
                check!(table.texture_ref(target)),
                area,
                LoadOp::Load,
                StoreOp::Store
            ))));
            check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
            check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertex)), 0));
            check!(pass.set_uniforms(DrawUniforms::new(Transform::identity(), color)));
            check!(pass.draw(6, 0));
            check!(pass.end());
            commands.push(check!(encoder.finish()));
        }
        session.submit_sequence(&commands.iter().collect::<Vec<_>>())?;
        for index in 0..16 {
            let mut pixels = vec![0u8; 32 * 32 * 4];
            session.readback(index, &mut pixels, 128, area)?;
            let expected = ((index as usize + round) % 16 * 17) as u8;
            for pixel in pixels.chunks_exact(4) {
                if pixel[0] != 0
                    || pixel[1] != 0
                    || pixel[2].abs_diff(expected) > 1
                    || pixel[3] != 255
                {
                    return Err(format!(
                        "in-flight uniforms overwritten round={round} target={index} expected_red={expected} got={pixel:?}"
                    ));
                }
            }
        }
    }
    println!("SGFX_ADRENO_BURST_VERIFY targets=16 rounds=3 pixels=49152 result=PASS");
    Ok(())
}
