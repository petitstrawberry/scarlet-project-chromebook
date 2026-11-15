// SPDX-License-Identifier: GPL-2.0-only
//! Reproduce the retained UI tile path: render straight-alpha content into a
//! transparent texture, then composite its premultiplied pixels over blue.
use sgfx::{
    backend::{CommandSubmitter, Completion, CompletionStatus},
    ir::*,
};
use std::{rc::Rc, time::Duration};

pub fn check(device: &sgfx::dynamic::Device) -> std::result::Result<(), String> {
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    let table = Rc::new(ResourceTable::new());
    let extent = check!(Extent2D::new(32, 32));
    let tile = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::SAMPLED
    ))))
    .id();
    let target = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::PRESENT | TextureUsage::COPY_SRC
    ))))
    .id();
    let vertices = check!(table.define_buffer(check!(BufferDesc::new(
        12 * 40,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))))
    .id();
    let layout = check!(VertexBufferLayout::new(
        40,
        vec![
            VertexAttribute::new(0, VertexFormat::Float32x4, 0),
            VertexAttribute::new(1, VertexFormat::Float32x4, 16),
            VertexAttribute::new(2, VertexFormat::Float32x2, 32),
        ]
    ));
    let over = BlendComponent::new(
        BlendFactor::One,
        BlendFactor::OneMinusSourceAlpha,
        BlendOp::Add,
    );
    let pipeline = |fragment, blend| -> std::result::Result<RenderPipelineId, String> {
        Ok(
            check!(table.define_render_pipeline(check!(RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                layout.clone(),
                fragment,
                blend,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise)
            ))))
            .id(),
        )
    };
    let paint = pipeline(
        FragmentProgram::VertexColor,
        BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
    )?;
    let composite = pipeline(
        FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
        BlendState::new(over, over),
    )?;
    let sampler = check!(table.define_sampler(SamplerDesc::new(
        FilterMode::Nearest,
        FilterMode::Nearest,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge
    )))
    .id();
    let mut data = Vec::new();
    for color in [[1.0f32, 0.0, 0.0, 0.5], [1.0; 4]] {
        for [x, y] in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [-1.0, 1.0],
            [-1.0, 1.0],
            [1.0, -1.0],
            [1.0, 1.0],
        ] {
            for value in [x, y, 0.0, 1.0]
                .into_iter()
                .chain(color)
                .chain([(x + 1.0) * 0.5, (y + 1.0) * 0.5])
            {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    let context = check!(device.create_context());
    let mut session = check!(context.create_mapped_target_session(table.clone(), &[target]));
    let area = check!(PixelRect::new(0, 0, 32, 32));
    let mut encoder = CommandEncoder::new(&table);
    check!(encoder.write_buffer(check!(table.buffer_ref(vertices)), 0, &data));
    for (output, pipeline, offset, clear, sampled) in [
        (tile, paint, 0, [0.0, 0.0, 0.0, 0.0], false),
        (target, composite, 240, [0.0, 0.0, 1.0, 1.0], true),
    ] {
        let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
            &table,
            check!(table.texture_ref(output)),
            area,
            LoadOp::Clear(check!(Color::rgba(clear[0], clear[1], clear[2], clear[3]))),
            StoreOp::Store
        ))));
        check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
        check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertices)), offset));
        check!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        if sampled {
            check!(pass.set_texture(check!(table.texture_ref(tile))));
            check!(pass.set_sampler(check!(table.sampler_ref(sampler))));
        }
        check!(pass.draw(6, 0));
        check!(pass.end());
    }
    let commands = check!(encoder.finish());
    let receipt = check!(session.executor().submit(&commands));
    drop(commands);
    if check!(receipt.wait(Some(Duration::from_secs(10)))) != CompletionStatus::Complete {
        return Err("tile composition timeout".into());
    }
    let mut pixels = vec![0; 32 * 32 * 4];
    check!(session.readback_bgra(target, &mut pixels, 128, area));
    for (i, p) in pixels.chunks_exact(4).enumerate() {
        // Eight-bit attachment rounding may choose either side of 0.5.
        if !(127..=128).contains(&p[0]) || p[1] != 0 || !(127..=128).contains(&p[2]) || p[3] != 255
        {
            return Err(format!(
                "premultiplied tile pixel={i} actual={p:?}, expected=[127..128,0,127..128,255]"
            ));
        }
    }
    println!("SGFX_ADRENO_PREMULTIPLIED_TILE_OK pixels=1024");
    Ok(())
}
