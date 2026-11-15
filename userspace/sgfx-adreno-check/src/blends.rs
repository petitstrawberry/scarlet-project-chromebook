// SPDX-License-Identifier: GPL-2.0-only
//! Read back the blend operations used by retained tiles and backdrop filters.
use sgfx_core::ir::*;
pub fn check(library: &str) -> std::result::Result<(), String> {
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    use BlendFactor as F;
    use BlendOp as O;
    let same = |src, dst, op| {
        let c = BlendComponent::new(src, dst, op);
        BlendState::new(c, c)
    };
    let cases = [
        ("replace", BlendState::REPLACE, [0u8, 32, 64, 128]),
        // Min/Max must ignore the supplied factors, including all-zero ones.
        ("min", same(F::Zero, F::Zero, O::Min), [0, 32, 32, 128]),
        ("max", same(F::Zero, F::Zero, O::Max), [128, 64, 64, 255]),
        (
            "min-color-max-alpha",
            BlendState::new(
                BlendComponent::new(F::Zero, F::Zero, O::Min),
                BlendComponent::new(F::Zero, F::Zero, O::Max),
            ),
            [0, 32, 32, 255],
        ),
        ("add", same(F::One, F::One, O::Add), [128, 96, 96, 255]),
        (
            "erase",
            same(F::Zero, F::OneMinusSourceAlpha, O::Add),
            [64, 32, 16, 128],
        ),
        ("subtract", same(F::One, F::One, O::Subtract), [0, 0, 32, 0]),
        (
            "reverse-subtract",
            same(F::One, F::One, O::ReverseSubtract),
            [128, 32, 0, 128],
        ),
        (
            "destination-in",
            BlendState::DESTINATION_IN,
            [64, 32, 16, 128],
        ),
        (
            "source-color",
            same(F::SourceColor, F::Zero, O::Add),
            [0, 4, 16, 64],
        ),
    ];
    let table = ResourceTable::new();
    let target = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        check!(Extent2D::new(16, 16)),
        TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))))
    .id();
    let vertices = check!(table.define_buffer(check!(BufferDesc::new(
        192,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))))
    .id();
    let mut pipelines = Vec::new();
    for (_, blend, _) in cases {
        pipelines.push(
            check!(table.define_render_pipeline(check!(RenderPipelineDesc::new(
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
                blend,
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
        for v in [x, y, 0.0, 1.0, 0.25, 0.125, 0.0, 0.5] {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    if check!(table.abi_texture(0)).id() != target {
        return Err("target ABI slot mismatch".into());
    }
    let mut session = crate::abi_session::Session::new(library, &table, &[0])?;
    let rect = check!(PixelRect::new(0, 0, 16, 16));
    for ((name, _, expected), pipeline) in cases.into_iter().zip(pipelines) {
        let mut encoder = CommandEncoder::new(&table);
        check!(encoder.write_buffer(check!(table.buffer_ref(vertices)), 0, &data));
        let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
            &table,
            check!(table.texture_ref(target)),
            rect,
            LoadOp::Clear(check!(Color::rgba(0.125, 0.25, 0.5, 1.0))),
            StoreOp::Store
        ))));
        check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
        check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertices)), 0));
        check!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        check!(pass.draw(6, 0));
        check!(pass.end());
        let commands = check!(encoder.finish());
        session.submit(&commands)?;
        let mut pixels = vec![0; 16 * 16 * 4];
        session.readback(0, &mut pixels, 64, rect)?;
        for (i, p) in pixels.chunks_exact(4).enumerate() {
            if p.iter()
                .zip(expected)
                .any(|(&actual, want)| actual.abs_diff(want) > 1)
            {
                return Err(format!(
                    "blend={name} pixel={i} actual={p:?} expected={expected:?}"
                ));
            }
        }
        println!("SGFX_ADRENO_BLEND_OK mode={name} pixels=256");
    }
    Ok(())
}
