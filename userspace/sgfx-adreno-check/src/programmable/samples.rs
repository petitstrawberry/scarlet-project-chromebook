// SPDX-License-Identifier: GPL-2.0-only
//! Typed sampling through the installed DriverApi, with independent pixel oracles.
use super::*;

struct Upload {
    mip: u32,
    layer: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bytes: Vec<u8>,
}

struct Case {
    name: String,
    source: TextureDesc,
    view: TextureViewDesc,
    compare: Option<CompareFunction>,
    fragment: String,
    uploads: Vec<Upload>,
    width: u32,
    height: u32,
    expected: Vec<[u8; 4]>, // BGRA8 readback from a linear UNORM target.
    tolerance: u8,
}

struct Recorded<'r, 'data> {
    commands: CommandBuffer<'r, 'data>,
    target: usize,
    shaders: [usize; 2],
    pipeline: usize,
}

fn bgra(rgba: [f32; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], rgba[3]].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn upload(mip: u32, layer: u32, width: u32, height: u32, bpp: u32, tight: Vec<u8>) -> Upload {
    assert_eq!(tight.len(), (width * height * bpp) as usize);
    // Padding is deliberately not a valid texel pattern. No final-row padding.
    let row = width * bpp;
    let pitch = row + 16;
    let mut bytes = vec![0xcd; (pitch * (height - 1) + row) as usize];
    for y in 0..height as usize {
        bytes[y * pitch as usize..y * pitch as usize + row as usize]
            .copy_from_slice(&tight[y * row as usize..(y + 1) * row as usize]);
    }
    Upload {
        mip,
        layer,
        width,
        height,
        pitch,
        bytes,
    }
}

fn record<'r, 'data>(
    table: &'r ResourceTable,
    case: &'data Case,
) -> Result<Recorded<'r, 'data>, String> {
    let source = checked!(table.define_texture(case.source));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(case.width, case.height)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC,
    ))));
    let depth = case.view.format() == TextureFormat::Depth32Float;
    let layout = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(
            0,
            ShaderStages::FRAGMENT,
            BindingType::SampledTextureView {
                dimension: case.view.dimension(),
                depth
            }
        ),
        BindGroupLayoutEntry::new(
            1,
            ShaderStages::FRAGMENT,
            if depth {
                BindingType::ComparisonSampler
            } else {
                BindingType::Sampler
            }
        ),
    ]));
    let sampler = checked!(
        table.define_sampler(
            checked!(
                SamplerDesc::new(
                    FilterMode::Nearest,
                    FilterMode::Nearest,
                    AddressMode::ClampToEdge,
                    AddressMode::ClampToEdge,
                )
                .with_mip_filter(
                    FilterMode::Nearest,
                    0.0,
                    (case.view.mip_level_count() - 1) as f32
                )
            )
            .with_compare(case.compare)
        )
    );
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        table,
        layout.clone(),
        vec![
            BindGroupEntry::new(
                0,
                BindingResource::TextureView {
                    texture: source.id(),
                    view: case.view
                }
            ),
            BindGroupEntry::new(1, BindingResource::Sampler(sampler.id())),
        ],
    ))));
    let vs = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fs = checked!(
        table.define_shader_module(checked!(ShaderModuleDesc::wgsl(case.fragment.clone())))
    );
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(vs, ShaderStage::Vertex, "vs".into())),
            checked!(ShaderEntryPoint::new(
                fs,
                ShaderStage::Fragment,
                "fs".into()
            )),
            checked!(PipelineLayoutDesc::new(vec![layout])),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
    )));
    table.enable_abi_commands();
    let mut encoder = CommandEncoder::new(table);
    for data in &case.uploads {
        checked!(
            encoder.write_texture(
                source,
                checked!(TextureWrite::new(
                    checked!(PixelRect::new(0, 0, data.width, data.height)),
                    data.pitch,
                    &data.bytes,
                ))
                .with_mip_level(data.mip)
                .with_array_layer(data.layer)
            )
        );
    }
    checked!(encoder.resource_barrier(ResourceBarrier::Global));
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        target,
        checked!(PixelRect::new(0, 0, case.width, case.height)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 0.0))),
        StoreOp::Store,
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    Ok(Recorded {
        commands: checked!(encoder.finish()),
        target: target.slot(),
        shaders: [vs.slot(), fs.slot()],
        pipeline: pipeline.slot(),
    })
}

fn execute(path: &str, case: &Case) -> Result<(), String> {
    println!("BEGIN programmable-sample {}", case.name);
    let table = ResourceTable::new();
    let recorded = record(&table, case)?;
    let driver = DriverSession::new(path, &table)?;
    for shader in recorded.shaders {
        driver.validate(abi::VALIDATE_SHADER, shader)?;
    }
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, recorded.pipeline)?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!(
            "COMPILED programmable-sample {} (execution skipped)",
            case.name
        );
        return Ok(());
    }
    driver.submit(&recorded.commands)?;
    let mut output = vec![0xa5; case.expected.len() * 4];
    driver.read_texture(recorded.target, &mut output)?;
    check_pixels(case, &output)?;
    println!(
        "PASS programmable-sample {} pixels={} tolerance={}",
        case.name,
        case.expected.len(),
        case.tolerance
    );
    Ok(())
}

fn check_pixels(case: &Case, output: &[u8]) -> Result<(), String> {
    if output.len() != case.expected.len() * 4 {
        return Err(format!(
            "{}: incorrect readback length {}",
            case.name,
            output.len()
        ));
    }
    for (i, (got, expected)) in output.chunks_exact(4).zip(&case.expected).enumerate() {
        if got
            .iter()
            .zip(expected)
            .any(|(&a, &b)| a.abs_diff(b) > case.tolerance)
        {
            return Err(format!(
                "{}: pixel=({}, {}) BGRA={got:?} expected={expected:?}",
                case.name,
                i % case.width as usize,
                i / case.width as usize
            ));
        }
    }
    Ok(())
}

// IEC sRGB transfer function, independent of the GPU texture descriptor/swizzle.
fn linear_srgb(byte: u8) -> f32 {
    let encoded = f32::from(byte) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn color_case(format: TextureFormat, view_format: TextureFormat) -> Result<Case, String> {
    let (width, height) = (8, 4);
    let source = checked!(TextureDesc::new(
        format,
        checked!(Extent2D::new(width, height)),
        TextureUsage::COPY_DST | TextureUsage::SAMPLED
    ));
    let view = checked!(TextureViewDesc::new(
        source,
        view_format,
        TextureViewDimension::D2,
        0,
        1,
        0,
        1
    ));
    let mut bytes = Vec::new();
    let mut expected = Vec::new();
    let values = [0u8, 10, 11, 32, 64, 128, 200, 255];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let rgba = [
                values[x],
                values[(x + 3 * y + 2) % 8],
                values[(x + y + 5) % 8],
                [32, 96, 160, 224][y],
            ];
            let logical = match format {
                TextureFormat::R8Unorm => {
                    bytes.push(rgba[0]);
                    [rgba[0], 0, 0, 255]
                }
                TextureFormat::Rg8Unorm => {
                    bytes.extend_from_slice(&rgba[..2]);
                    [rgba[0], rgba[1], 0, 255]
                }
                TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb => {
                    bytes.extend([rgba[2], rgba[1], rgba[0], rgba[3]]);
                    rgba
                }
                TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb => {
                    bytes.extend(rgba);
                    rgba
                }
                _ => unreachable!(),
            };
            let mut linear = logical.map(|v| f32::from(v) / 255.0);
            if matches!(
                view_format,
                TextureFormat::Rgba8UnormSrgb | TextureFormat::Bgra8UnormSrgb
            ) {
                for c in 0..3 {
                    linear[c] = linear_srgb(logical[c]);
                }
            }
            expected.push(bgra(linear));
        }
    }
    Ok(Case {
        name: format!("color-{format:?}-view-{view_format:?}"),
        source,
        view,
        compare: None,
        fragment: "@group(0) @binding(0) var image: texture_2d<f32>;
            @group(0) @binding(1) var point: sampler;
            @fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
                return textureSampleLevel(image, point, p.xy / vec2<f32>(8.0, 4.0), 0.0);
            }"
        .into(),
        uploads: vec![upload(
            0,
            0,
            width,
            height,
            format.bytes_per_pixel().unwrap(),
            bytes,
        )],
        width,
        height,
        expected,
        tolerance: 1,
    })
}

fn depth_case(compare: CompareFunction) -> Result<Case, String> {
    let (width, height) = (8, 4);
    let source = checked!(TextureDesc::new(
        TextureFormat::Depth32Float,
        checked!(Extent2D::new(width, height)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ));
    let view = checked!(TextureViewDesc::new(
        source,
        TextureFormat::Depth32Float,
        TextureViewDimension::D2,
        0,
        1,
        0,
        1
    ));
    let mut bytes = Vec::new();
    let mut expected = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let depth = [0.125f32, 0.375, 0.5, 0.625, 0.875][((x + y * 2) % 5) as usize];
            bytes.extend(depth.to_le_bytes());
            let pass = if compare == CompareFunction::Less {
                0.5 < depth
            } else {
                0.5 <= depth
            };
            expected.push(bgra([
                depth,
                f32::from(pass),
                f32::from(compare == CompareFunction::LessEqual),
                1.0,
            ]));
        }
    }
    Ok(Case {
        name: format!("depth-load-compare-{compare:?}"),
        source,
        view,
        compare: Some(compare),
        fragment: "@group(0) @binding(0) var image: texture_depth_2d;
            @group(0) @binding(1) var point: sampler_comparison;
            @fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
                let loaded = textureLoad(image, vec2<i32>(p.xy), 0);
                let uv = p.xy / vec2<f32>(8.0, 4.0);
                let compared = textureSampleCompareLevel(image, point, uv, 0.5);
                let equal = textureSampleCompareLevel(image, point, uv, loaded);
                return vec4<f32>(loaded, compared, equal, 1.0);
            }"
        .into(),
        uploads: vec![upload(0, 0, width, height, 4, bytes)],
        width,
        height,
        expected,
        tolerance: 1,
    })
}

// Every physical layer/mip and quadrant has a distinct value. Layers outside
// the view are initialized too, so ignoring base offsets fails deterministically.
fn layered_pixel(layer: u32, mip: u32, x: u32, y: u32) -> [f32; 4] {
    [
        (layer + 1) as f32 / 16.0,
        (mip + 1) as f32 / 4.0,
        (x + y * 2 + 1) as f32 / 32.0,
        (layer % 3 + 1) as f32 / 4.0,
    ]
}

fn layered_case(cube: bool) -> Result<Case, String> {
    let format = TextureFormat::Rgba32Float;
    let source = checked!(
        checked!(
            checked!(TextureDesc::new(
                format,
                checked!(Extent2D::new(8, 8)),
                TextureUsage::SAMPLED | TextureUsage::COPY_DST
            ))
            .with_mip_level_count(3)
        )
        .with_array_layer_count(8)
    );
    let source = checked!(source.with_cube_compatible(cube));
    let layers = if cube { 6 } else { 3 };
    let view = checked!(TextureViewDesc::new(
        source,
        format,
        if cube {
            TextureViewDimension::Cube
        } else {
            TextureViewDimension::D2Array
        },
        1,
        2,
        1,
        layers
    ));
    let mut uploads = Vec::new();
    for layer in 0..8 {
        for mip in 0..3 {
            let size = 8 >> mip;
            let bytes = (0..size * size)
                .flat_map(|i| {
                    layered_pixel(layer, mip, i % size, i / size)
                        .into_iter()
                        .flat_map(f32::to_le_bytes)
                })
                .collect();
            uploads.push(upload(mip, layer, size, size, 16, bytes));
        }
    }
    let width = layers * 2;
    let height = 4;
    let mut expected = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let mip = 1 + y / 2;
            let size = 8 >> mip;
            let sx = (x % 2) * (size - 1);
            let sy = (y % 2) * (size - 1);
            expected.push(bgra(layered_pixel(1 + x / 2, mip, sx, sy)));
        }
    }
    let fragment = if cube {
        // Inverse cube face mapping in +X,-X,+Y,-Y,+Z,-Z order. Four
        // off-centre samples per face expose face order and axis inversions.
        "@group(0) @binding(0) var image: texture_cube<f32>;
        @group(0) @binding(1) var point: sampler;
        @fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
            let face = u32(p.x) / 2u;
            let lod = u32(p.y) / 2u;
            let size = f32(4u >> lod);
            let uv = (vec2<f32>(f32(u32(p.x) % 2u), f32(u32(p.y) % 2u)) * (size - 1.0) + 0.5) / size;
            let s = uv.x * 2.0 - 1.0;
            let t = uv.y * 2.0 - 1.0;
            let directions = array<vec3<f32>, 6>(vec3(1.0,-t,-s), vec3(-1.0,-t,s),
                vec3(s,1.0,t), vec3(s,-1.0,-t), vec3(s,-t,1.0), vec3(-s,-t,-1.0));
            return textureSampleLevel(image, point, directions[face], f32(lod));
        }"
    } else {
        "@group(0) @binding(0) var image: texture_2d_array<f32>;
        @group(0) @binding(1) var point: sampler;
        @fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
            let lod = u32(p.y) / 2u;
            let size = f32(4u >> lod);
            let uv = (vec2<f32>(f32(u32(p.x) % 2u), f32(u32(p.y) % 2u)) * (size - 1.0) + 0.5) / size;
            return textureSampleLevel(image, point, uv, i32(u32(p.x) / 2u), f32(lod));
        }"
    };
    Ok(Case {
        name: format!(
            "{}-base-layer1-base-mip1",
            if cube {
                "cube-six-faces"
            } else {
                "array-three-layers"
            }
        ),
        source,
        view,
        compare: None,
        fragment: fragment.into(),
        uploads,
        width,
        height,
        expected,
        tolerance: 1,
    })
}

fn cases() -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for (format, view) in [
        (TextureFormat::Bgra8UnormSrgb, TextureFormat::Bgra8UnormSrgb),
        (TextureFormat::Rgba8UnormSrgb, TextureFormat::Rgba8UnormSrgb),
        (TextureFormat::Bgra8Unorm, TextureFormat::Bgra8UnormSrgb),
        (TextureFormat::Rgba8Unorm, TextureFormat::Rgba8UnormSrgb),
        (TextureFormat::Bgra8UnormSrgb, TextureFormat::Bgra8Unorm),
        (TextureFormat::Rgba8UnormSrgb, TextureFormat::Rgba8Unorm),
        (TextureFormat::R8Unorm, TextureFormat::R8Unorm),
        (TextureFormat::Rg8Unorm, TextureFormat::Rg8Unorm),
    ] {
        cases.push(color_case(format, view)?);
    }
    cases.push(depth_case(CompareFunction::Less)?);
    cases.push(depth_case(CompareFunction::LessEqual)?);
    cases.push(layered_case(false)?);
    cases.push(layered_case(true)?);
    Ok(cases)
}

pub(super) fn run(path: &str) -> Result<(), String> {
    let mut failures = Vec::new();
    for case in cases()? {
        if let Err(error) = execute(path, &case) {
            println!("FAIL programmable-sample {}: {error}", case.name);
            failures.push(format!("{}: {error}", case.name));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "programmable sampling failures: {}",
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sampling_cases_encode_typed_bindings_uploads_and_abi_commands() {
        let cases = cases().unwrap();
        assert_eq!(cases.len(), 12);
        for case in cases {
            assert_eq!(case.expected.len(), (case.width * case.height) as usize);
            let table = ResourceTable::new();
            let recorded = record(&table, &case).unwrap();
            assert!(recorded.commands.abi_batch().is_some());
            assert_eq!(
                recorded
                    .commands
                    .commands()
                    .iter()
                    .filter(|c| matches!(c, Command::WriteTexture { .. }))
                    .count(),
                case.uploads.len()
            );
            let good: Vec<_> = case.expected.iter().flatten().copied().collect();
            check_pixels(&case, &good).unwrap();
            let mut corrupt = good;
            corrupt[0] = if corrupt[0] < 128 { 255 } else { 0 };
            assert!(check_pixels(&case, &corrupt).is_err());
        }
    }

    #[test]
    fn oracles_distinguish_srgb_alpha_missing_channels_and_compare_equality() {
        assert_eq!(
            bgra([linear_srgb(128), 0.0, 0.0, 128.0 / 255.0]),
            [0, 0, 55, 128]
        );
        let red = color_case(TextureFormat::R8Unorm, TextureFormat::R8Unorm).unwrap();
        assert!(
            red.expected
                .iter()
                .all(|p| p[0] == 0 && p[1] == 0 && p[3] == 255)
        );
        let less = depth_case(CompareFunction::Less).unwrap();
        let equal = depth_case(CompareFunction::LessEqual).unwrap();
        assert!(less.expected.iter().all(|p| p[0] == 0));
        assert!(equal.expected.iter().all(|p| p[0] == 255));
        assert!(
            less.expected
                .iter()
                .zip(&equal.expected)
                .any(|(a, b)| a[1] != b[1])
        );
    }
}
