// SPDX-License-Identifier: GPL-2.0-only
//! Linear-light render/blend and byte-readback oracle for normalized targets.
use super::*;

const BACKGROUND: [f32; 4] = [0.25, 0.5, 0.75, 0.4];
const SOURCE: [f32; 4] = [0.6, 0.2, 0.1, 0.5];
const FORMATS: [TextureFormat; 4] = [
    TextureFormat::Bgra8UnormSrgb,
    TextureFormat::Rgba8UnormSrgb,
    TextureFormat::R8Unorm,
    TextureFormat::Rg8Unorm,
];

fn srgb(format: TextureFormat) -> bool {
    matches!(
        format,
        TextureFormat::Bgra8UnormSrgb | TextureFormat::Rgba8UnormSrgb
    )
}
fn encode(format: TextureFormat, value: [f32; 4]) -> Vec<u8> {
    let mut rgba = [0; 4];
    for (i, mut v) in value.into_iter().enumerate() {
        v = v.clamp(0.0, 1.0);
        if srgb(format) && i < 3 {
            v = if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
        }
        rgba[i] = (v * 255.0).round() as u8;
    }
    match format {
        TextureFormat::Bgra8UnormSrgb => vec![rgba[2], rgba[1], rgba[0], rgba[3]],
        TextureFormat::R8Unorm => vec![rgba[0]],
        TextureFormat::Rg8Unorm => vec![rgba[0], rgba[1]],
        _ => rgba.to_vec(),
    }
}
fn stored_background(format: TextureFormat) -> [f32; 4] {
    let bytes = encode(format, BACKGROUND);
    if format == TextureFormat::R8Unorm {
        return [f32::from(bytes[0]) / 255.0, 0.0, 0.0, 1.0];
    }
    if format == TextureFormat::Rg8Unorm {
        return [f32::from(bytes[0])/255.0, f32::from(bytes[1])/255.0, 0.0, 1.0];
    }
    let bytes = if format == TextureFormat::Bgra8UnormSrgb {
        [bytes[2], bytes[1], bytes[0], bytes[3]]
    } else {
        bytes.try_into().unwrap()
    };
    let mut result = bytes.map(|v| f32::from(v) / 255.0);
    for v in &mut result[..3] {
        *v = if *v <= 0.04045 {
            *v / 12.92
        } else {
            ((*v + 0.055) / 1.055).powf(2.4)
        };
    }
    result
}
fn blend(mode: usize) -> BlendState {
    match mode {
        0 => BlendState::REPLACE,
        1 => BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
        2 => BlendState::new(
            BlendComponent::new(
                BlendFactor::One,
                BlendFactor::OneMinusSourceAlpha,
                BlendOp::Add,
            ),
            BlendComponent::new(
                BlendFactor::One,
                BlendFactor::OneMinusSourceAlpha,
                BlendOp::Add,
            ),
        ),
        _ => BlendState::new(
            BlendComponent::new(
                BlendFactor::DestinationAlpha,
                BlendFactor::OneMinusDestinationAlpha,
                BlendOp::Add,
            ),
            BlendComponent::new(BlendFactor::One, BlendFactor::Zero, BlendOp::Add),
        ),
    }
}
fn expected(format: TextureFormat, mode: usize) -> Vec<u8> {
    let dst = stored_background(format);
    let mut result = SOURCE;
    for i in 0..3 {
        result[i] = match mode {
            0 => SOURCE[i],
            1 => SOURCE[i] * SOURCE[3] + dst[i] * (1.0 - SOURCE[3]),
            2 => SOURCE[i] + dst[i] * (1.0 - SOURCE[3]),
            _ => SOURCE[i] * dst[3] + dst[i] * (1.0 - dst[3]),
        };
    }
    if mode == 1 || mode == 2 {
        result[3] = SOURCE[3] + dst[3] * (1.0 - SOURCE[3]);
    }
    encode(format, result)
}

fn run_case(path: &str, format: TextureFormat, native: bool, mode: usize) -> Result<(), String> {
    let table = ResourceTable::new();
    table.enable_abi_commands();
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        format,
        checked!(Extent2D::new(8, 4)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC | TextureUsage::COPY_DST
    ))));
    let vs = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fs = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
        "@fragment fn fs()->@location(0) vec4<f32>{return vec4<f32>(0.6,0.2,0.1,0.5);}".into()
    ))));
    let native_pipeline = if native {
        Some(checked!(table.define_programmable_render_pipeline(
            checked!(ProgrammableRenderPipelineDesc::new(
                checked!(ShaderEntryPoint::new(vs, ShaderStage::Vertex, "vs".into())),
                checked!(ShaderEntryPoint::new(
                    fs,
                    ShaderStage::Fragment,
                    "fs".into()
                )),
                checked!(PipelineLayoutDesc::new(vec![])),
                format,
                None,
                PrimitiveTopology::TriangleList,
                blend(mode),
                RasterState::new(CullMode::None, FrontFace::CounterClockwise)
            ))
        )))
    } else {
        None
    };
    let fixed_pipeline = if !native {
        Some(checked!(table.define_render_pipeline(checked!(
            RenderPipelineDesc::new(
                format,
                PrimitiveTopology::TriangleList,
                checked!(VertexBufferLayout::new(
                    32,
                    vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16)
                    ]
                )),
                FragmentProgram::VertexColor,
                blend(mode),
                RasterState::new(CullMode::None, FrontFace::CounterClockwise)
            )
        ))))
    } else {
        None
    };
    let vertex_buffer = checked!(table.define_buffer(checked!(BufferDesc::new(
        192,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let mut vertices = Vec::new();
    for [x, y] in [
        [-1.0f32, -1.0],
        [1.0, -1.0],
        [-1.0, 1.0],
        [-1.0, 1.0],
        [1.0, -1.0],
        [1.0, 1.0],
    ] {
        for value in [x, y, 0.0, 1.0, SOURCE[0], SOURCE[1], SOURCE[2], SOURCE[3]] {
            vertices.extend_from_slice(&value.to_le_bytes());
        }
    }
    let rect = checked!(PixelRect::new(0, 0, 8, 4));
    let background = encode(format, BACKGROUND).repeat(32);
    let mut encoder = CommandEncoder::new(&table);
    if !native {
        checked!(encoder.write_buffer(vertex_buffer, 0, &vertices));
    }
    // Upload deliberately stores R8 in the established alpha-byte expansion,
    // whose unused physical alpha-under-XYZW is zero. Destination alpha must
    // nevertheless be one in the logical R8 blend equation.
    if mode == 3 {
        checked!(encoder.write_texture(
            target,
            checked!(TextureWrite::new(
                rect,
                8 * format.bytes_per_pixel().unwrap(),
                &background
            ))
        ));
    }
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        rect,
        if mode == 3 {
            LoadOp::Load
        } else {
            LoadOp::Clear(checked!(Color::rgba(
                BACKGROUND[0],
                BACKGROUND[1],
                BACKGROUND[2],
                BACKGROUND[3]
            )))
        },
        StoreOp::Store
    ))));
    checked!(pass.set_scissor(Some(checked!(PixelRect::new(0, 0, 4, 4)))));
    if let Some(p) = native_pipeline {
        checked!(pass.set_programmable_pipeline(p));
        checked!(pass.draw(3, 0));
    } else {
        checked!(pass.set_pipeline(fixed_pipeline.unwrap()));
        checked!(pass.set_vertex_buffer(vertex_buffer, 0));
        checked!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            checked!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        checked!(pass.draw(6, 0));
    }
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    #[cfg(test)]
    if path == "__encode_only__" {
        let batch = commands
            .abi_batch()
            .ok_or("missing normalized target ABI batch")?;
        if batch.count == 0 || batch.words.len == 0 {
            return Err("empty normalized target batch".into());
        }
        return Ok(());
    }
    let driver = DriverSession::new(path, &table)?;
    if let Some(p) = native_pipeline {
        driver.validate(abi::VALIDATE_RENDER_PIPELINE, p.slot())?;
    }
    if std::env::args().any(|a| a == "--compile-only") {
        return Ok(());
    }
    driver.submit(&commands)?;
    let bpp = format.bytes_per_pixel().unwrap() as usize;
    let mut pixels = vec![0xa5; 32 * bpp];
    driver.read_texture(target.slot(), &mut pixels)?;
    let left = expected(format, mode);
    let right = encode(format, BACKGROUND);
    for (i, pixel) in pixels.chunks_exact(bpp).enumerate() {
        let want = if i % 8 < 4 { &left } else { &right };
        if pixel.iter().zip(want).any(|(&a, &b)| a.abs_diff(b) > 1) {
            return Err(format!(
                "target={format:?} native={native} blend={mode} pixel={i} got={pixel:?} want={want:?}"
            ));
        }
    }
    println!(
        "PASS normalized-render-target format={format:?} native={native} blend={mode} pixels=32"
    );
    Ok(())
}

pub(super) fn run(path: &str) -> Result<(), String> {
    for format in FORMATS {
        for native in [false, true] {
            for mode in 0..4 {
                run_case(path, format, native, mode)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_render_target_oracles_encode_all_native_and_fixed_cases() {
        run("__encode_only__").unwrap();
    }
    #[test]
    fn normalized_target_oracle_distinguishes_linear_blend_and_missing_alpha() {
        assert_eq!(
            encode(TextureFormat::Rgba8UnormSrgb, [0.25, 0.5, 0.75, 0.5]),
            vec![137, 188, 225, 128]
        );
        assert_eq!(expected(TextureFormat::R8Unorm, 3), vec![153]);
        assert_eq!(stored_background(TextureFormat::R8Unorm)[3], 1.0);
        assert_ne!(
            expected(TextureFormat::Rgba8UnormSrgb, 1)[0],
            ((203u16 + 137) / 2) as u8
        );
    }
}
