// SPDX-License-Identifier: GPL-2.0-only
//! Fixed public semantics through generated native shaders, not a programmable
//! API substitute: the caller records actual RenderPipelineDesc commands.
use super::*;
const SIDE: u32 = 8;
const UNIFORM: [f32; 4] = [1.0, 0.5, 0.75, 0.5];
fn fragments() -> [FragmentProgram; 9] {
    use FragmentProgram::*;
    use TextureSampleMode::*;
    [
        Solid,
        VertexColor,
        Texture(Rgba),
        Texture(RgbIgnoreAlpha),
        Texture(AlphaMask),
        TextureVertexColor(Rgba),
        TextureVertexColor(RgbIgnoreAlpha),
        TextureVertexColor(AlphaMask),
        Texture(AlphaMask),
    ]
}
fn source_format(case: usize) -> Option<TextureFormat> {
    match case {
        0 | 1 => None,
        2 | 6 => Some(TextureFormat::Rg8Unorm),
        3 | 7 => Some(TextureFormat::Rgba16Float),
        4 => Some(TextureFormat::R8Unorm),
        5 => Some(TextureFormat::Rgba32Float),
        _ => Some(TextureFormat::Rgba8Unorm),
    }
}
fn texture_pixels(format: TextureFormat) -> (Vec<u8>, [[f32; 4]; 4]) {
    let normalized = [
        [64u8, 128, 191, 64],
        [192, 64, 128, 128],
        [128, 192, 64, 192],
        [32, 96, 224, 224],
    ];
    let floats = [
        [1.5f32, 0.5, 0.25, 0.5],
        [0.25, 1.5, 0.75, 0.25],
        [0.5, 0.25, 1.5, 0.75],
        [0.75, 0.5, 0.125, 1.0],
    ];
    let mut bytes = Vec::new();
    let mut samples = [[0.0; 4]; 4];
    for i in 0..4 {
        match format {
            TextureFormat::R8Unorm => {
                bytes.push(normalized[i][0]);
                samples[i] = [f32::from(normalized[i][0]) / 255.0, 0.0, 0.0, 1.0];
            }
            TextureFormat::Rg8Unorm => {
                bytes.extend_from_slice(&normalized[i][..2]);
                samples[i] = [
                    f32::from(normalized[i][0]) / 255.0,
                    f32::from(normalized[i][1]) / 255.0,
                    0.0,
                    1.0,
                ];
            }
            TextureFormat::Rgba8Unorm => {
                bytes.extend_from_slice(&normalized[i]);
                samples[i] = normalized[i].map(|v| f32::from(v) / 255.0);
            }
            TextureFormat::Rgba16Float => {
                for v in floats[i] {
                    let b = v.to_bits();
                    let h = (((b >> 23 & 255) - 112) << 10 | (b >> 13 & 1023)) as u16;
                    bytes.extend_from_slice(&h.to_le_bytes());
                }
                samples[i] = floats[i];
            }
            _ => {
                for v in floats[i] {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                samples[i] = floats[i];
            }
        }
    }
    (bytes, samples)
}
fn half(v: u16) -> f32 {
    let sign = if v & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = (v >> 10) & 31;
    let m = v & 1023;
    if e == 0 {
        sign * f32::from(m) / 16777216.0
    } else {
        f32::from_bits(
            (u32::from(v & 0x8000) << 16) | ((u32::from(e) + 112) << 23) | (u32::from(m) << 13),
        )
    }
}
fn decode(format: TextureFormat, bytes: &[u8]) -> [f32; 4] {
    match format {
        TextureFormat::Bgra8Unorm => {
            [bytes[2], bytes[1], bytes[0], bytes[3]].map(|b| f32::from(b) / 255.0)
        }
        TextureFormat::Rgba16Float => core::array::from_fn(|i| {
            half(u16::from_le_bytes(
                bytes[i * 2..i * 2 + 2].try_into().unwrap(),
            ))
        }),
        _ => core::array::from_fn(|i| {
            f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
        }),
    }
}
fn expected(
    fragment: FragmentProgram,
    color: [f32; 4],
    sample: [f32; 4],
    format: Option<TextureFormat>,
) -> [f32; 4] {
    use FragmentProgram::*;
    use TextureSampleMode::*;
    let vertex = matches!(fragment, VertexColor | TextureVertexColor(_));
    let mut output = core::array::from_fn(|i| UNIFORM[i] * if vertex { color[i] } else { 1.0 });
    match fragment {
        Solid | VertexColor => {}
        Texture(mode) | TextureVertexColor(mode) => match mode {
            Rgba => {
                for i in 0..4 {
                    output[i] *= sample[i];
                }
            }
            RgbIgnoreAlpha => {
                for i in 0..3 {
                    output[i] *= sample[i];
                }
            }
            AlphaMask => {
                output[3] *= if format == Some(TextureFormat::R8Unorm) {
                    sample[0]
                } else {
                    sample[3]
                }
            }
        },
    }
    output
}
fn execute(path: &str, case: usize, format: TextureFormat) -> Result<(), String> {
    let fragment = fragments()[case];
    let sampled = source_format(case);
    println!(
        "BEGIN fixed-native case={case} target={format:?} fragment={fragment:?} source={sampled:?}"
    );
    let table = ResourceTable::new();
    table.enable_abi_commands();
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        format,
        checked!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let position = [
        VertexFormat::Float32x2,
        VertexFormat::Float32x3,
        VertexFormat::Float32x4,
    ][case % 3];
    let color_format = [
        VertexFormat::Float32x4,
        VertexFormat::Float32x3,
        VertexFormat::Unorm8x4,
    ][case % 3];
    let colored = matches!(
        fragment,
        FragmentProgram::VertexColor | FragmentProgram::TextureVertexColor(_)
    );
    let mut attrs = vec![VertexAttribute::new(0, position, 8)];
    if colored {
        attrs.push(VertexAttribute::new(1, color_format, 32));
    }
    if sampled.is_some() {
        attrs.push(VertexAttribute::new(
            if colored { 2 } else { 1 },
            VertexFormat::Float32x2,
            56,
        ));
    }
    let culled = case == 1 && format == TextureFormat::Bgra8Unorm;
    let pipeline = checked!(
        table.define_render_pipeline(checked!(RenderPipelineDesc::new(
            format,
            PrimitiveTopology::TriangleStrip,
            checked!(VertexBufferLayout::new(80, attrs)),
            fragment,
            BlendState::REPLACE,
            RasterState::new(
                if culled {
                    CullMode::Back
                } else {
                    CullMode::Front
                },
                FrontFace::Clockwise
            )
        )))
    );
    let indexed = case == 2;
    let count = if indexed { 4 } else { 5 };
    let vertex = checked!(table.define_buffer(checked!(BufferDesc::new(
        count * 80,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let mut bytes = vec![0xa5; (count * 80) as usize];
    let mut color: [f32; 4] = [1.0, 0.5, 0.25, 0.5];
    if color_format == VertexFormat::Float32x3 {
        color[3] = 1.0;
    }
    if color_format == VertexFormat::Unorm8x4 {
        color = [1.0, 128.0 / 255.0, 64.0 / 255.0, 128.0 / 255.0];
    }
    for (i, [x, y]) in [[-1.0f32, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]]
        .into_iter()
        .enumerate()
    {
        let off = (i + usize::from(!indexed)) * 80;
        let components = match position {
            VertexFormat::Float32x2 => 2,
            VertexFormat::Float32x3 => 3,
            _ => 4,
        };
        for j in 0..components {
            let v = [x, y, 0.0, 1.0][j];
            bytes[off + 8 + j * 4..off + 12 + j * 4].copy_from_slice(&v.to_le_bytes());
        }
        if colored {
            if color_format == VertexFormat::Unorm8x4 {
                bytes[off + 32..off + 36].copy_from_slice(&[255, 128, 64, 128]);
            } else {
                let n = if color_format == VertexFormat::Float32x3 {
                    3
                } else {
                    4
                };
                for j in 0..n {
                    bytes[off + 32 + j * 4..off + 36 + j * 4]
                        .copy_from_slice(&color[j].to_le_bytes());
                }
            }
        }
        for j in 0..2 {
            let v = (if j == 0 { x } else { y }) * 0.5 + 0.5;
            bytes[off + 56 + j * 4..off + 60 + j * 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    let index = if indexed {
        Some(checked!(table.define_buffer(checked!(BufferDesc::new(
            12,
            BufferUsage::INDEX | BufferUsage::COPY_DST
        )))))
    } else {
        None
    };
    let (texture, sampler, upload, sample) = if let Some(f) = sampled {
        let texture = checked!(table.define_texture(checked!(TextureDesc::new(
            f,
            checked!(Extent2D::new(2, 2)),
            TextureUsage::COPY_DST | TextureUsage::SAMPLED
        ))));
        let sampler = checked!(table.define_sampler(SamplerDesc::new(
            FilterMode::Nearest,
            FilterMode::Nearest,
            AddressMode::ClampToEdge,
            AddressMode::ClampToEdge
        )));
        let (pixels, sample) = texture_pixels(f);
        (Some(texture), Some(sampler), pixels, sample)
    } else {
        (None, None, vec![], [[1.0; 4]; 4])
    };
    // A same-pass canonical transition checks that native fixed state does not
    // leak into a subsequent ordinary shader-pack draw.
    let reset = format == TextureFormat::Bgra8Unorm;
    let plain = if reset {
        Some(checked!(table.define_render_pipeline(checked!(
            RenderPipelineDesc::new(
                format,
                PrimitiveTopology::TriangleList,
                checked!(VertexBufferLayout::new(
                    24,
                    vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)]
                )),
                FragmentProgram::Solid,
                BlendState::REPLACE,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise)
            )
        ))))
    } else {
        None
    };
    let plain_vertex = if reset {
        Some(checked!(table.define_buffer(checked!(BufferDesc::new(
            144,
            BufferUsage::VERTEX | BufferUsage::COPY_DST
        )))))
    } else {
        None
    };
    let index_data = [65535u16, 1, 2, 3, 4, 0]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let mut encoder = CommandEncoder::new(&table);
    checked!(encoder.write_buffer(vertex, 0, &bytes));
    if let Some(index) = index {
        checked!(encoder.write_buffer(index, 0, &index_data));
    }
    if let Some(texture) = texture {
        checked!(encoder.write_texture(
            texture,
            checked!(TextureWrite::new(
                checked!(PixelRect::new(0, 0, 2, 2)),
                2 * sampled.unwrap().bytes_per_pixel().unwrap(),
                &upload
            ))
        ));
    }
    let mut plain_data = Vec::new();
    if let Some(v) = plain_vertex {
        for [x, y] in [
            [-1.0f32, -1.0],
            [1.0, -1.0],
            [-1.0, 1.0],
            [-1.0, 1.0],
            [1.0, -1.0],
            [1.0, 1.0],
        ] {
            for v in [x, y, 0.0, 1.0, 0.0, 0.0] {
                plain_data.extend_from_slice(&v.to_le_bytes());
            }
        }
        checked!(encoder.write_buffer(v, 0, &plain_data));
    }
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        checked!(PixelRect::new(0, 0, SIDE, SIDE)),
        LoadOp::Clear(checked!(Color::rgba(0.0, 0.0, 0.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_pipeline(pipeline));
    checked!(pass.set_vertex_buffer(vertex, 0));
    checked!(pass.set_uniforms(DrawUniforms::new(
        Transform::identity(),
        checked!(Color::rgba(UNIFORM[0], UNIFORM[1], UNIFORM[2], UNIFORM[3]))
    )));
    if let Some(texture) = texture {
        checked!(pass.set_texture(texture));
        checked!(pass.set_sampler(sampler.unwrap()));
    }
    if let Some(index) = index {
        checked!(pass.set_index_buffer(index, 0, IndexFormat::Uint16));
        checked!(pass.draw_indexed(4, 1, -1));
    } else {
        checked!(pass.draw(4, 1));
    }
    if let Some(plain) = plain {
        checked!(pass.set_pipeline(plain));
        checked!(pass.set_vertex_buffer(plain_vertex.unwrap(), 0));
        checked!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            checked!(Color::rgba(0.0, 1.0, 0.0, 1.0))
        )));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(3, 3, 2, 2)))));
        checked!(pass.draw(6, 0));
    }
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    #[cfg(test)]
    if path == "__encode_only__" {
        let batch = commands
            .abi_batch()
            .ok_or("missing fixed native ABI batch")?;
        if batch.count == 0 || batch.words.len == 0 {
            return Err("empty fixed native ABI batch".into());
        }
        return Ok(());
    }
    let driver = DriverSession::new(path, &table)?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!("SKIP fixed-native pixels (--compile-only)");
        return Ok(());
    }
    let bpp = format.bytes_per_pixel().unwrap() as usize;
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut pixels = vec![0xa5; (SIDE * SIDE) as usize * bpp];
        driver.read_texture(target.slot(), &mut pixels)?;
        for y in 0..SIDE {
            for x in 0..SIDE {
                // UV is (x+.5)/SIDE,1-(y+.5)/SIDE. Distinct 2x2
                // texels prove both UV components and channel/alpha decoding.
                let texel = usize::from(x >= SIDE / 2) + 2 * usize::from(y < SIDE / 2);
                let base = expected(fragment, color, sample[texel], sampled);
                let mut want = if reset && (3..5).contains(&x) && (3..5).contains(&y) {
                    [0.0, 1.0, 0.0, 1.0]
                } else if culled {
                    [0.0, 0.0, 0.0, 1.0]
                } else {
                    base
                };
                if format == TextureFormat::Bgra8Unorm {
                    want = want.map(|v| v.clamp(0.0, 1.0));
                }
                let off = ((y * SIDE + x) as usize) * bpp;
                let got = decode(format, &pixels[off..off + bpp]);
                let tolerance = if format == TextureFormat::Bgra8Unorm {
                    1.1 / 255.0
                } else {
                    0.0015
                };
                if got
                    .iter()
                    .zip(want)
                    .any(|(&a, b)| !a.is_finite() || (a - b).abs() > tolerance)
                {
                    return Err(format!(
                        "fixed-native case={case} target={format:?} replay={replay} pixel={x},{y} got={got:?} want={want:?}"
                    ));
                }
            }
        }
        println!(
            "PASS fixed-native case={case} target={format:?} replay={replay} pixels=64 position={position:?} color={color_format:?} stride=80 source={sampled:?} negative-base={indexed} culled={culled} canonical-reset={reset}"
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    for format in [
        TextureFormat::Bgra8Unorm,
        TextureFormat::Rgba16Float,
        TextureFormat::Rgba32Float,
    ] {
        for case in 0..9 {
            execute(path, case, format)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_native_oracle_records_all_general_public_fixed_cases() {
        super::run("__encode_only__").unwrap();
    }
}
