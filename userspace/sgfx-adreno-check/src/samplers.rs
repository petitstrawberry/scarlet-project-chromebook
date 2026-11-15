// SPDX-License-Identifier: GPL-2.0-only
//! Sample out-of-range coordinates, independent filters, and anisotropic rows.
use sgfx_core::ir::*;
pub fn check(library: &str) -> std::result::Result<(), String> {
    macro_rules! check {
        ($e:expr) => {
            $e.map_err(|e| format!("{}: {e:?}", stringify!($e)))?
        };
    }
    let table = ResourceTable::new();
    let target = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        check!(Extent2D::new(16, 16)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::PRESENT | TextureUsage::COPY_SRC
    ))))
    .id();
    let texture = check!(table.define_texture(check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        check!(Extent2D::new(2, 2)),
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ))))
    .id();
    let vertices = check!(table.define_buffer(check!(BufferDesc::new(
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
        FragmentProgram::Texture(TextureSampleMode::Rgba),
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise)
    ))))
    .id();
    use AddressMode as A;
    use FilterMode as F;
    let mut cases = Vec::new();
    for (name, u, v, uv, expected) in [
        (
            "clamp",
            A::ClampToEdge,
            A::ClampToEdge,
            [-0.25f32, 0.25],
            [0, 0, 255, 255],
        ),
        (
            "repeat-u",
            A::Repeat,
            A::ClampToEdge,
            [-0.25, 0.25],
            [0, 255, 0, 255],
        ),
        (
            "mirror-u",
            A::MirrorRepeat,
            A::ClampToEdge,
            [-0.25, 0.25],
            [0, 0, 255, 255],
        ),
        (
            "border-u",
            A::ClampToBorder,
            A::ClampToEdge,
            [-0.25, 0.25],
            [0, 0, 0, 0],
        ),
        (
            "repeat-v",
            A::ClampToEdge,
            A::Repeat,
            [0.25, -0.25],
            [255, 0, 0, 255],
        ),
        (
            "mirror-v",
            A::ClampToEdge,
            A::MirrorRepeat,
            [0.25, -0.25],
            [0, 0, 255, 255],
        ),
        (
            "border-v",
            A::ClampToEdge,
            A::ClampToBorder,
            [0.25, -0.25],
            [0, 0, 0, 0],
        ),
    ] {
        cases.push((
            name.to_string(),
            check!(table.define_sampler(SamplerDesc::new(F::Nearest, F::Nearest, u, v))).id(),
            uv,
            expected,
        ));
    }
    cases.push((
        "independent-mag-linear".into(),
        check!(table.define_sampler(SamplerDesc::new(
            F::Nearest,
            F::Linear,
            A::ClampToEdge,
            A::ClampToEdge
        )))
        .id(),
        [0.5, 0.25],
        [0, 128, 128, 255],
    ));
    for aniso in 1..=16 {
        let sampler = check!(
            SamplerDesc::new(F::Linear, F::Linear, A::Repeat, A::MirrorRepeat)
                .with_anisotropy(aniso)
        );
        cases.push((
            format!("anisotropy-{aniso}"),
            check!(table.define_sampler(sampler)).id(),
            [0.75, 0.25],
            [0, 255, 0, 255],
        ));
    }
    let checker_desc = check!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        check!(Extent2D::new(32, 32)),
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ));
    let checker = check!(table.define_texture(check!(checker_desc.with_mip_level_count(6)))).id();
    let mut minification_samplers = Vec::new();
    for level in 0..6 {
        let desc = check!(
            SamplerDesc::new(F::Linear, F::Linear, A::Repeat, A::Repeat).with_mip_filter(
                F::Nearest,
                level as f32,
                level as f32
            )
        );
        minification_samplers.push((
            format!("fixed-lod-{level}"),
            check!(table.define_sampler(desc)).id(),
            Some(level),
        ));
    }
    for aniso in [1, 2, 4, 8, 16] {
        let desc = check!(
            check!(
                SamplerDesc::new(F::Linear, F::Linear, A::Repeat, A::Repeat).with_anisotropy(aniso)
            )
            .with_mip_filter(F::Nearest, 0.0, 5.0)
        );
        minification_samplers.push((
            format!("anisotropy-{aniso}"),
            check!(table.define_sampler(desc)).id(),
            None,
        ));
    }
    // The target is the first texture slot in the public snapshot.
    if check!(table.abi_texture(0)).id() != target {
        return Err("target ABI slot mismatch".into());
    }
    let mut session = crate::abi_session::Session::new(library, &table, &[0])?;
    let area = check!(PixelRect::new(0, 0, 16, 16));
    let source = [
        0u8, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
    ];
    for (name, sampler, uv, expected) in cases {
        // Constant UV has a degenerate derivative ellipse. Use a uniform
        // image for the descriptor-row sweep; the non-degenerate footprint
        // test below verifies actual anisotropic filtering separately.
        let source = if name.starts_with("anisotropy-") {
            [0, 255, 0, 255].repeat(4)
        } else {
            source.to_vec()
        };
        let mut data = Vec::new();
        for [x, y] in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [-1.0, 1.0],
            [-1.0, 1.0],
            [1.0, -1.0],
            [1.0, 1.0],
        ] {
            for value in [x, y, uv[0], uv[1]] {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
        let mut encoder = CommandEncoder::new(&table);
        check!(encoder.write_buffer(check!(table.buffer_ref(vertices)), 0, &data));
        check!(encoder.write_texture(
            check!(table.texture_ref(texture)),
            check!(TextureWrite::new(
                check!(PixelRect::new(0, 0, 2, 2)),
                8,
                &source
            ))
        ));
        let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
            &table,
            check!(table.texture_ref(target)),
            area,
            LoadOp::Clear(check!(Color::rgba(0.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))));
        check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
        check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertices)), 0));
        check!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        check!(pass.set_texture(check!(table.texture_ref(texture))));
        check!(pass.set_sampler(check!(table.sampler_ref(sampler))));
        check!(pass.draw(6, 0));
        check!(pass.end());
        let commands = check!(encoder.finish());
        session.submit(&commands)?;
        let mut pixels = vec![0; 16 * 16 * 4];
        session.readback(0, &mut pixels, 64, area)?;
        for (i, pixel) in pixels.chunks_exact(4).enumerate() {
            if pixel
                .iter()
                .zip(expected)
                .any(|(&value, want)| value.abs_diff(want) > 1)
            {
                return Err(format!(
                    "sampler={name} pixel={i} actual={pixel:?} expected={expected:?}"
                ));
            }
        }
        println!("SGFX_ADRENO_SAMPLER_OK mode={name} pixels=256");
    }
    // Distinct solid mip colors isolate native LOD selection from sample
    // placement. Check every fixed LOD first, then require anisotropy to retain
    // more detail than isotropic sampling for a non-degenerate 16:2 footprint.
    // Exact anisotropic LOD/tap selection is implementation dependent.
    let mip_pixels: Vec<Vec<u8>> = [
        [0, 0, 255, 255],
        [0, 255, 0, 255],
        [255, 0, 0, 255],
        [255, 255, 255, 255],
        [128, 128, 128, 255],
        [255, 255, 0, 255],
    ]
    .into_iter()
    .enumerate()
    .map(|(level, color)| color.repeat((32 >> level) * (32 >> level)))
    .collect();
    let mut footprint_failure = None;
    let mut selected_lods = Vec::new();
    let mut data = Vec::new();
    for [x, y] in [
        [-1.0f32, -1.0],
        [1.0, -1.0],
        [-1.0, 1.0],
        [-1.0, 1.0],
        [1.0, -1.0],
        [1.0, 1.0],
    ] {
        for value in [x, y, (x + 1.0) * 4.0 + 1.0 / 32.0, (y + 1.0) * 0.5] {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
    for (name, sampler, fixed_level) in minification_samplers {
        let mut encoder = CommandEncoder::new(&table);
        check!(encoder.write_buffer(check!(table.buffer_ref(vertices)), 0, &data));
        for (level, pixels) in mip_pixels.iter().enumerate() {
            let side = 32 >> level;
            check!(
                encoder.write_texture(
                    check!(table.texture_ref(checker)),
                    check!(TextureWrite::new(
                        check!(PixelRect::new(0, 0, side, side)),
                        side * 4,
                        pixels
                    ))
                    .with_mip_level(level as u32)
                )
            );
        }
        let mut pass = check!(encoder.begin_render_pass(check!(RenderPassDesc::new(
            &table,
            check!(table.texture_ref(target)),
            area,
            LoadOp::Clear(check!(Color::rgba(0.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))));
        check!(pass.set_pipeline(check!(table.render_pipeline_ref(pipeline))));
        check!(pass.set_vertex_buffer(check!(table.buffer_ref(vertices)), 0));
        check!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            check!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        check!(pass.set_texture(check!(table.texture_ref(checker))));
        check!(pass.set_sampler(check!(table.sampler_ref(sampler))));
        check!(pass.draw(6, 0));
        check!(pass.end());
        session.submit(&check!(encoder.finish()))?;
        let mut pixels = vec![0; 16 * 16 * 4];
        session.readback(0, &mut pixels, 64, area)?;
        let mut histogram = [0usize; 6];
        for (i, pixel) in pixels.chunks_exact(4).enumerate() {
            let selected = mip_pixels
                .iter()
                .position(|mip| {
                    pixel
                        .iter()
                        .zip(&mip[..4])
                        .all(|(&value, &expected)| value.abs_diff(expected) <= 2)
                })
                .ok_or_else(|| format!("{name} pixel={i}: unknown mip color {pixel:?}"))?;
            histogram[selected] += 1;
            if fixed_level.is_some_and(|level| level != selected) {
                footprint_failure.get_or_insert_with(|| {
                    format!("sampler={name} pixel={i} selected={selected} expected={fixed_level:?}")
                });
            }
        }
        if fixed_level.is_none() {
            selected_lods.push(
                histogram
                    .iter()
                    .enumerate()
                    .map(|(level, count)| level * count)
                    .sum::<usize>(),
            );
        }
        println!("SGFX_ADRENO_SAMPLER_LOD mode={name} pixels=256 histogram={histogram:?}");
    }
    if let Some(error) = footprint_failure {
        return Err(error);
    }
    if selected_lods.len() != 5
        || selected_lods[4] >= selected_lods[0]
        || selected_lods.iter().any(|&level| level > selected_lods[0])
    {
        return Err(format!(
            "anisotropic filtering did not preserve more detail: {selected_lods:?}"
        ));
    }
    println!("SGFX_ADRENO_ANISOTROPIC_LOD_OK summed_levels={selected_lods:?}");
    Ok(())
}
