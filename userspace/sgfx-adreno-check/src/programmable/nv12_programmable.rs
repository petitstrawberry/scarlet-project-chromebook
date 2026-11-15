// SPDX-License-Identifier: GPL-2.0-only
//! Actual public WGSL/SPIR-V NV12 specialization, queries, user math and rebind.
use super::*;
const SIDE: u32 = 16;
const SOURCE: &str = include_str!("nv12-programmable-fixtures/shader.wgsl");
struct Scene {
    source: TextureId,
    targets: [TextureId; 2],
    regular: TextureId,
    vertex: BufferId,
    canonical: RenderPipelineId,
    pipeline: ProgrammableRenderPipelineId,
    groups: [BindGroupId; 2],
    samplers: [SamplerId; 2],
    spirv: bool,
}
fn scene(table: &ResourceTable, spirv: bool) -> Result<Scene, String> {
    table.enable_abi_commands();
    let source = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Nv12,
        checked!(Extent2D::new(128, 128)),
        TextureUsage::SAMPLED
    ))));
    let regular = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(128, 128)),
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ))));
    let mut targets = Vec::new();
    for _ in 0..2 {
        targets.push(
            checked!(table.define_texture(checked!(TextureDesc::new(
                TextureFormat::Bgra8Unorm,
                checked!(Extent2D::new(SIDE, SIDE)),
                TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
            ))))
            .id(),
        );
    }
    let vertex = checked!(table.define_buffer(checked!(BufferDesc::new(
        144,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let canonical = checked!(
        table.define_render_pipeline(checked!(RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            checked!(VertexBufferLayout::new(
                24,
                vec![
                    VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                    VertexAttribute::new(1, VertexFormat::Float32x2, 16)
                ]
            )),
            FragmentProgram::Texture(TextureSampleMode::Rgba),
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )))
    );
    let modules = if spirv {
        [
            include_bytes!("nv12-programmable-fixtures/vs.spv").as_slice(),
            include_bytes!("nv12-programmable-fixtures/fs.spv").as_slice(),
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
    let texture_layout = checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::FRAGMENT,
        BindingType::SampledTexture
    )]));
    let sampler_layout = checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::FRAGMENT,
        BindingType::Sampler
    )]));
    let combined = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0, ShaderStages::FRAGMENT, BindingType::SampledTexture),
        BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, BindingType::Sampler)
    ]));
    let layout = checked!(PipelineLayoutDesc::new(if spirv {
        vec![
            checked!(BindGroupLayoutDesc::new(vec![])),
            checked!(BindGroupLayoutDesc::new(vec![])),
            texture_layout.clone(),
            sampler_layout.clone(),
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
    let samplers = [FilterMode::Nearest, FilterMode::Linear].map(|f| {
        table
            .define_sampler(SamplerDesc::new(
                f,
                f,
                AddressMode::ClampToEdge,
                AddressMode::ClampToEdge,
            ))
            .unwrap()
            .id()
    });
    // Texture groups for both sources; sampler groups are recorded per filter.
    let groups = [source, regular].map(|texture| {
        table
            .define_bind_group(
                BindGroupDesc::new(
                    table,
                    if spirv {
                        texture_layout.clone()
                    } else {
                        combined.clone()
                    },
                    if spirv {
                        vec![BindGroupEntry::new(
                            0,
                            BindingResource::Texture(texture.id()),
                        )]
                    } else {
                        vec![
                            BindGroupEntry::new(0, BindingResource::Texture(texture.id())),
                            BindGroupEntry::new(1, BindingResource::Sampler(samplers[0])),
                        ]
                    },
                )
                .unwrap(),
            )
            .unwrap()
            .id()
    });
    Ok(Scene {
        source: source.id(),
        targets: targets.try_into().unwrap(),
        regular: regular.id(),
        vertex: vertex.id(),
        canonical: canonical.id(),
        pipeline: pipeline.id(),
        groups,
        samplers,
        spirv,
    })
}
fn record<'r, 'd>(
    table: &'r ResourceTable,
    s: &Scene,
    filter: usize,
    vertices: &'d [u8],
    regular: &'d [u8],
) -> Result<CommandBuffer<'r, 'd>, String> {
    let sampled = checked!(table.texture_ref(s.source));
    let regular_ref = checked!(table.texture_ref(s.regular));
    let sampler = checked!(table.sampler_ref(s.samplers[filter]));
    let mut encoder = CommandEncoder::new(table);
    checked!(encoder.write_buffer(checked!(table.buffer_ref(s.vertex)), 0, vertices));
    checked!(encoder.write_texture(
        regular_ref,
        checked!(TextureWrite::new(
            checked!(PixelRect::new(0, 0, 128, 128)),
            512,
            regular
        ))
    ));
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        checked!(table.texture_ref(s.targets[0])),
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(0.0, 0.0, 0.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_pipeline(checked!(table.render_pipeline_ref(s.canonical))));
    checked!(pass.set_vertex_buffer(checked!(table.buffer_ref(s.vertex)), 0));
    checked!(pass.set_uniforms(DrawUniforms::new(
        Transform::identity(),
        checked!(Color::rgba(1.0, 1.0, 1.0, 1.0))
    )));
    checked!(pass.set_texture(sampled));
    checked!(pass.set_sampler(sampler));
    checked!(pass.draw(6, 0));
    checked!(pass.end());
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        checked!(table.texture_ref(s.targets[1])),
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(
        pass.set_programmable_pipeline(checked!(
            table.programmable_render_pipeline_ref(s.pipeline)
        ))
    );
    // Keep the texture group separate from the sampler update in the SPIR-V
    // case to exercise partial group replacement, including its native slots.
    let mut selected = Vec::new();
    for (i, texture) in [sampled, regular_ref].into_iter().enumerate() {
        if s.spirv {
            selected.push(checked!(table.bind_group_ref(s.groups[i])));
        } else {
            let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
                table,
                checked!(BindGroupLayoutDesc::new(vec![
                    BindGroupLayoutEntry::new(
                        0,
                        ShaderStages::FRAGMENT,
                        BindingType::SampledTexture
                    ),
                    BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, BindingType::Sampler)
                ])),
                vec![
                    BindGroupEntry::new(0, BindingResource::Texture(texture.id())),
                    BindGroupEntry::new(1, BindingResource::Sampler(sampler.id()))
                ]
            ))));
            selected.push(group);
        }
    }
    let sampler_group = if s.spirv {
        Some(checked!(table.define_bind_group(checked!(
            BindGroupDesc::new(
                table,
                checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
                    0,
                    ShaderStages::FRAGMENT,
                    BindingType::Sampler
                )])),
                vec![BindGroupEntry::new(
                    0,
                    BindingResource::Sampler(sampler.id())
                )]
            )
        ))))
    } else {
        None
    };
    checked!(pass.set_bind_group(if s.spirv { 2 } else { 0 }, selected[0]));
    if let Some(group) = sampler_group {
        checked!(pass.set_bind_group(3, group));
    }
    checked!(pass.draw(3, 0));
    // Same pass/pipeline, switch to an ordinary BGRA source. The backend must
    // restore its unspecialized shader and descriptor rather than reuse NV12.
    checked!(pass.set_bind_group(if s.spirv { 2 } else { 0 }, selected[1]));
    checked!(pass.set_scissor(Some(checked!(PixelRect::new(7, 7, 2, 2)))));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    Ok(checked!(encoder.finish()))
}
fn check_pixels(c: YcbcrConversion, pixels: &[Vec<u8>; 2]) -> Result<(), String> {
    for y in 0..16 {
        for x in 0..16 {
            let off = (y * 16 + x) * 4;
            let canonical = &pixels[0][off..off + 4];
            let want = if (7..9).contains(&x) && (7..9).contains(&y) {
                [192, 64, 48, 128]
            } else {
                [
                    canonical[2],
                    (f32::from(canonical[1]) * 0.5).round() as u8,
                    (f32::from(canonical[0]) * 0.75).round() as u8,
                    128,
                ]
            };
            for k in 0..4 {
                if pixels[1][off + k].abs_diff(want[k]) > 2 {
                    return Err(format!(
                        "programmable NV12 pixel={x},{y} channel={k} got={} want={} canonical={canonical:?}",
                        pixels[1][off + k],
                        want[k]
                    ));
                }
            }
        }
    }
    for y in [1usize, 5, 9, 13] {
        for x in [1usize, 5, 9, 13] {
            let rgb = super::nv12::cpu_rgb(c, (8 * x + 8).min(127), (128 - 8 * y).min(127));
            let off = (y * 16 + x) * 4;
            let wants = [
                [rgb[2], rgb[1], rgb[0], 1.0],
                [rgb[0], rgb[1] * 0.5, rgb[2] * 0.75, 0.5],
            ];
            for (n, want) in wants.into_iter().enumerate() {
                for k in 0..4 {
                    let byte = (want[k] * 255.0).round() as u8;
                    if pixels[n][off + k].abs_diff(byte) > 2 {
                        return Err(format!(
                            "programmable NV12 CPU target={n} pixel={x},{y} channel={k} got={} want={byte}",
                            pixels[n][off + k]
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}
#[cfg(target_os = "scarlet")]
pub fn run(path: &str) -> Result<(), String> {
    super::base_clamp::run(path)?;
    use scarlet_video_client::{DecodedOutput, DecoderOptions, ScarletVideoDecoder, VideoFormat};
    let mut decoder =
        ScarletVideoDecoder::open_with_options(DecoderOptions::new().with_shared_images(true))
            .map_err(|e| format!("NV12 producer prerequisite: {e}"))?;
    let frame = decoder
        .decode_output(
            VideoFormat::H264,
            include_bytes!("nv12-fixtures/frame.h264"),
            1,
        )?
        .ok_or("NV12 producer returned no frame")?;
    let image = match frame {
        DecodedOutput::Image(image) => image,
        DecodedOutput::Nv12(_) => return Err("NV12 producer requires shared image".into()),
    };
    let descriptor = image.descriptor();
    if descriptor.width != 128
        || descriptor.height != 128
        || descriptor.format != u32::from_le_bytes(*b"NV12")
    {
        return Err("unexpected NV12 producer descriptor".into());
    }
    let handle = image.into_handle();
    drop(decoder);
    for spirv in [false, true] {
        let table = ResourceTable::new();
        let s = scene(&table, spirv)?;
        let vertices = super::nv12::vertex_data(false);
        let regular = [64u8, 128, 192, 255].repeat(128 * 128);
        let commands = [
            record(&table, &s, 0, &vertices, &regular)?,
            record(&table, &s, 1, &vertices, &regular)?,
        ];
        let slots = s
            .targets
            .map(|id| table.texture_ref(id).unwrap().slot() as u32);
        let mut session = crate::abi_session::Session::new(path, &table, &slots)?;
        for sweep in 0..2 {
            for (index, c) in super::nv12::conversions().into_iter().enumerate() {
                let owned = handle
                    .duplicate()
                    .map_err(|e| format!("duplicate producer: {e:?}"))?;
                let conversion = abi::YcbcrConversion {
                    size: 24,
                    reserved: 0,
                    matrix: match c.matrix {
                        YcbcrMatrix::Bt601 => 1,
                        YcbcrMatrix::Bt709 => 2,
                    },
                    range: match c.range {
                        YcbcrRange::Limited => 1,
                        YcbcrRange::Full => 2,
                    },
                    chroma_x: match c.chroma_x {
                        ChromaLocation::Cosited => 1,
                        ChromaLocation::Midpoint => 2,
                    },
                    chroma_y: match c.chroma_y {
                        ChromaLocation::Cosited => 1,
                        ChromaLocation::Midpoint => 2,
                    },
                };
                let source_slot = table.texture_ref(s.source).unwrap().slot() as u32;
                session.import_ycbcr(source_slot, conversion, || {
                    let raw = owned.as_raw();
                    core::mem::forget(owned);
                    raw
                })?;
                for filter in 0..2 {
                    session.submit(&commands[filter])?;
                    let mut pixels = [vec![0xa5; 1024], vec![0xa5; 1024]];
                    for n in 0..2 {
                        session.readback(
                            slots[n],
                            &mut pixels[n],
                            64,
                            checked!(PixelRect::new(0, 0, 16, 16)),
                        )?;
                    }
                    check_pixels(c, &pixels)?;
                    println!(
                        "PASS nv12-programmable spirv={spirv} conversion={index} sweep={sweep} filter={filter} pixels=512 cpu-interiors=32 queries=dimensions+levels math=swizzle+scale regular-rebind=true"
                    );
                }
                session.release_import(source_slot)?;
            }
        }
    }
    println!(
        "PASS nv12-programmable ALL languages=2 conversions=16 sweeps=2 submits=128 pixels=65536"
    );
    Ok(())
}
#[cfg(not(target_os = "scarlet"))]
pub fn run(_path: &str) -> Result<(), String> {
    Err("NV12 programmable oracle requires Scarlet".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_public_wgsl_and_spirv_groups_targets_and_rebind_commands_encode() {
        for spirv in [false, true] {
            let table = ResourceTable::new();
            let s = scene(&table, spirv).unwrap();
            let vertices = super::super::nv12::vertex_data(false);
            let regular = [64u8, 128, 192, 255].repeat(128 * 128);
            for filter in 0..2 {
                let c = record(&table, &s, filter, &vertices, &regular).unwrap();
                assert!(c.abi_batch().unwrap().count > 0);
            }
            for id in s.targets {
                assert!(
                    table
                        .texture(table.texture_ref(id).unwrap())
                        .unwrap()
                        .usage()
                        .contains(TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT)
                );
            }
        }
    }
}
