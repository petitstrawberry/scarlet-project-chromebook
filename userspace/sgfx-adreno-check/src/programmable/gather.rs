// SPDX-License-Identifier: GPL-2.0-only
//! Execute equivalent WGSL and original-binding SPIR-V gather shaders. Each
//! readback must match the CPU oracle, then both readbacks must match exactly.
use super::*;

const WIDTH: u32 = 4;
const PIXELS: usize = 16;

#[derive(Clone, Copy)]
struct ShaderPair {
    wgsl: &'static str,
    spirv: &'static [u8],
}
macro_rules! shader_pair {
    ($name:literal) => {
        ShaderPair {
            wgsl: include_str!(concat!("gather-fixtures/", $name, ".wgsl")),
            spirv: include_bytes!(concat!("gather-fixtures/", $name, ".source.spv")),
        }
    };
}
const VERTEX_PAIR: ShaderPair = shader_pair!("fullscreen");

#[derive(Clone, Copy, Debug)]
enum SourceKind {
    Wgsl,
    Spirv,
}
impl SourceKind {
    fn module(self, pair: ShaderPair) -> Result<ShaderModuleDesc, String> {
        Ok(match self {
            Self::Wgsl => checked!(ShaderModuleDesc::wgsl(pair.wgsl.into())),
            Self::Spirv => checked!(ShaderModuleDesc::spirv(spirv_words(pair.spirv)?)),
        })
    }
}

fn spirv_words(bytes: &[u8]) -> Result<Vec<u32>, String> {
    if bytes.len() < 20 || bytes.len() % 4 != 0 {
        return Err("gather fixture: invalid SPIR-V byte length".into());
    }
    let words: Vec<_> = bytes
        .chunks_exact(4)
        .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    if words[0] != 0x0723_0203 {
        return Err("gather fixture: invalid SPIR-V magic".into());
    }
    Ok(words)
}

struct Case {
    name: &'static str,
    fragment: ShaderPair,
    component: usize,
    offset: [i32; 2],
    depth: bool,
    compare: Option<CompareFunction>,
}

fn cases() -> [Case; 8] {
    [
        Case {
            name: "red",
            fragment: shader_pair!("red"),
            component: 0,
            offset: [0, 0],
            depth: false,
            compare: None,
        },
        Case {
            name: "green",
            fragment: shader_pair!("green"),
            component: 1,
            offset: [0, 0],
            depth: false,
            compare: None,
        },
        Case {
            name: "blue",
            fragment: shader_pair!("blue"),
            component: 2,
            offset: [0, 0],
            depth: false,
            compare: None,
        },
        Case {
            name: "alpha",
            fragment: shader_pair!("alpha"),
            component: 3,
            offset: [0, 0],
            depth: false,
            compare: None,
        },
        Case {
            name: "red_offset",
            fragment: shader_pair!("red_offset"),
            component: 0,
            offset: [1, -1],
            depth: false,
            compare: None,
        },
        Case {
            name: "depth_regular",
            fragment: shader_pair!("depth_regular"),
            component: 0,
            offset: [0, 0],
            depth: true,
            compare: None,
        },
        Case {
            name: "depth_less",
            fragment: shader_pair!("depth_less"),
            component: 0,
            offset: [0, 0],
            depth: true,
            compare: Some(CompareFunction::Less),
        },
        Case {
            name: "depth_less_equal",
            fragment: shader_pair!("depth_less_equal"),
            component: 0,
            offset: [0, 0],
            depth: true,
            compare: Some(CompareFunction::LessEqual),
        },
    ]
}

// Explicit independent CPU texel functions; expected pixels are not taken from
// compiler output or generated expected files at runtime.
fn color(x: u8, y: u8) -> [u8; 4] {
    [
        13 + 11 * x + 23 * y,
        37 + 17 * x + 7 * y,
        19 + 3 * x + 29 * y,
        251 - 13 * x - 19 * y,
    ]
}
fn depth(x: u8, y: u8) -> f32 {
    [0.125, 0.5, 0.625, 0.875][usize::from((x + 2 * y) % 4)]
}
impl Case {
    fn expected_rgba(&self) -> [u8; 4] {
        // uv=.5, 4x4: gather xyzw = lower-left, lower-right, upper-right,
        // upper-left in increasing texture-v coordinates, before channel swizzle.
        [[1, 2], [2, 2], [2, 1], [1, 1]].map(|[x, y]| {
            let x = (x + self.offset[0]) as u8;
            let y = (y + self.offset[1]) as u8;
            if !self.depth {
                return color(x, y)[self.component];
            }
            let value = depth(x, y);
            match self.compare {
                Some(CompareFunction::Less) => u8::from(0.5 < value) * 255,
                Some(CompareFunction::LessEqual) => u8::from(0.5 <= value) * 255,
                None => (value * 255.0).round() as u8,
                _ => unreachable!("only Less and LessEqual fixture samplers"),
            }
        })
    }
    fn expected_bgra(&self) -> [u8; 4] {
        let [r, g, b, a] = self.expected_rgba();
        [b, g, r, a]
    }
    fn input(&self) -> &'static [u8] {
        if self.depth {
            include_bytes!("gather-fixtures/input.depth32f")
        } else {
            include_bytes!("gather-fixtures/input.rgba8")
        }
    }
    fn sampler_binding(&self) -> BindingType {
        // A depth texture also supports ordinary gather with a non-comparison
        // sampler. Only textureGatherCompare requires ComparisonSampler.
        if self.compare.is_some() {
            BindingType::ComparisonSampler
        } else {
            BindingType::Sampler
        }
    }
}

struct Recorded<'r> {
    commands: CommandBuffer<'r, 'static>,
    target: usize,
    shaders: [usize; 2],
    pipeline: usize,
}
fn record<'r>(
    table: &'r ResourceTable,
    case: &Case,
    kind: SourceKind,
) -> Result<Recorded<'r>, String> {
    let format = if case.depth {
        TextureFormat::Depth32Float
    } else {
        TextureFormat::Rgba8Unorm
    };
    let mut usage = TextureUsage::SAMPLED | TextureUsage::COPY_DST;
    if case.depth {
        usage |= TextureUsage::RENDER_ATTACHMENT;
    }
    let desc = checked!(TextureDesc::new(
        format,
        checked!(Extent2D::new(WIDTH, WIDTH)),
        usage
    ));
    let source = checked!(table.define_texture(desc));
    let view = checked!(TextureViewDesc::new(
        desc,
        format,
        TextureViewDimension::D2,
        0,
        1,
        0,
        1
    ));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(WIDTH, WIDTH)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC,
    ))));
    let layout = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(
            0,
            ShaderStages::FRAGMENT,
            BindingType::SampledTextureView {
                dimension: TextureViewDimension::D2,
                depth: case.depth
            }
        ),
        BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, case.sampler_binding()),
    ]));
    let sampler = checked!(
        table.define_sampler(
            checked!(
                SamplerDesc::new(
                    FilterMode::Nearest,
                    FilterMode::Nearest,
                    AddressMode::ClampToEdge,
                    AddressMode::ClampToEdge
                )
                .with_mip_filter(FilterMode::Nearest, 0.0, 0.0)
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
                    view
                }
            ),
            BindGroupEntry::new(1, BindingResource::Sampler(sampler.id())),
        ]
    ))));
    let vs = checked!(table.define_shader_module(kind.module(VERTEX_PAIR)?));
    let fs = checked!(table.define_shader_module(kind.module(case.fragment)?));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                vs,
                ShaderStage::Vertex,
                "main".into()
            )),
            checked!(ShaderEntryPoint::new(
                fs,
                ShaderStage::Fragment,
                "main".into()
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
    checked!(encoder.write_texture(
        source,
        checked!(TextureWrite::new(
            checked!(PixelRect::new(0, 0, WIDTH, WIDTH)),
            WIDTH * 4,
            case.input(),
        ))
    ));
    checked!(encoder.resource_barrier(ResourceBarrier::Global));
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        target,
        checked!(PixelRect::new(0, 0, WIDTH, WIDTH)),
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

fn check_pixels(case: &Case, output: &[u8]) -> Result<(), String> {
    if output.len() != PIXELS * 4 {
        return Err(format!("{}: readback length={}", case.name, output.len()));
    }
    let expected = case.expected_bgra();
    // Depth fractions can round by one UNORM step; comparison results are exact.
    let tolerance = if case.compare.is_some() { 0 } else { 1 };
    for (i, got) in output.chunks_exact(4).enumerate() {
        if got
            .iter()
            .zip(expected)
            .any(|(&a, b)| a.abs_diff(b) > tolerance)
        {
            return Err(format!(
                "{} pixel=({}, {}) BGRA={got:?} expected={expected:?} tolerance={tolerance}",
                case.name,
                i % WIDTH as usize,
                i / WIDTH as usize
            ));
        }
    }
    Ok(())
}
fn check_pair(case: &Case, wgsl: &[u8], spirv: &[u8]) -> Result<(), String> {
    check_pixels(case, wgsl).map_err(|e| format!("WGSL {e}"))?;
    check_pixels(case, spirv).map_err(|e| format!("SPIR-V {e}"))?;
    if let Some(i) = wgsl.iter().zip(spirv).position(|(a, b)| a != b) {
        return Err(format!(
            "{} WGSL/SPIR-V differ: pixel=({}, {}) channel={} WGSL={} SPIR-V={}",
            case.name,
            (i / 4) % WIDTH as usize,
            (i / 4) / WIDTH as usize,
            i % 4,
            wgsl[i],
            spirv[i]
        ));
    }
    Ok(())
}
fn execute(
    path: &str,
    case: &Case,
    kind: SourceKind,
    compile_only: bool,
) -> Result<Option<Vec<u8>>, String> {
    println!("BEGIN programmable-gather {} source={kind:?}", case.name);
    let table = ResourceTable::new();
    let recorded = record(&table, case, kind)?;
    let driver = DriverSession::new(path, &table)?;
    for shader in recorded.shaders {
        driver.validate(abi::VALIDATE_SHADER, shader)?;
    }
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, recorded.pipeline)?;
    if compile_only {
        println!(
            "COMPILED programmable-gather {} source={kind:?} (execution skipped)",
            case.name
        );
        return Ok(None);
    }
    driver.submit(&recorded.commands)?;
    let mut output = vec![0xa5; PIXELS * 4];
    driver.read_texture(recorded.target, &mut output)?;
    check_pixels(case, &output)?;
    println!(
        "PASS programmable-gather {} source={kind:?} pixels={PIXELS}",
        case.name
    );
    Ok(Some(output))
}

pub(super) fn run(path: &str) -> Result<(), String> {
    let compile_only = std::env::args().any(|arg| arg == "--compile-only");
    let mut failures = Vec::new();
    for case in cases() {
        // Try both sources even when one fails, so import and execution failures
        // can be distinguished in a single hardware log.
        let wgsl = execute(path, &case, SourceKind::Wgsl, compile_only);
        let spirv = execute(path, &case, SourceKind::Spirv, compile_only);
        let result = match (wgsl, spirv) {
            (Ok(Some(a)), Ok(Some(b))) => check_pair(&case, &a, &b).map(|()| {
                println!(
                    "PASS programmable-gather-pair {} pixels={} exact-WGSL-SPIRV=true",
                    case.name,
                    PIXELS * 2
                );
            }),
            (Ok(None), Ok(None)) => Ok(()),
            (a, b) => Err(format!(
                "{} WGSL={:?} SPIR-V={:?}",
                case.name,
                a.err(),
                b.err()
            )),
        };
        if let Err(error) = result {
            println!("FAIL programmable-gather {error}");
            failures.push(error);
        }
    }
    if !failures.is_empty() {
        return Err(format!("gather failures: {}", failures.join("; ")));
    }
    if !compile_only {
        println!("PASS programmable-gather cases=8 source-runs=16 oracle-pixels=256");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_source_pairs_record_typed_bindings_and_abi_commands() {
        for case in cases() {
            for kind in [SourceKind::Wgsl, SourceKind::Spirv] {
                let table = ResourceTable::new();
                let recorded = record(&table, &case, kind).unwrap();
                assert!(recorded.commands.abi_batch().is_some());
                assert_eq!(
                    recorded
                        .commands
                        .commands()
                        .iter()
                        .filter(|c| matches!(c, Command::WriteTexture { .. }))
                        .count(),
                    1
                );
            }
        }
        assert_eq!(cases()[5].sampler_binding(), BindingType::Sampler);
        assert_eq!(cases()[6].sampler_binding(), BindingType::ComparisonSampler);
    }

    #[test]
    fn cpu_oracles_detect_gather_order_channels_offset_and_depth_equality() {
        let expected = [
            [70, 81, 58, 47],
            [68, 85, 78, 61],
            [80, 83, 54, 51],
            [200, 187, 206, 219],
            [58, 69, 46, 35],
            [128, 159, 32, 223],
            [0, 255, 0, 255],
            [255, 255, 0, 255],
        ];
        for (case, rgba) in cases().iter().zip(expected) {
            assert_eq!(case.expected_rgba(), rgba, "{}", case.name);
            let good = case.expected_bgra().repeat(PIXELS);
            check_pair(case, &good, &good).unwrap();
            let mut bad = good.clone();
            bad[(PIXELS - 1) * 4] ^= 0x80;
            assert!(check_pair(case, &good, &bad).is_err());
            assert!(check_pair(case, &bad, &good).is_err());
            assert!(check_pixels(case, &good[..good.len() - 1]).is_err());
            if case.compare.is_none() {
                let mut almost = good.clone();
                almost[0] += 1;
                check_pixels(case, &almost).unwrap();
                assert!(
                    check_pair(case, &good, &almost).is_err(),
                    "pair must match exactly"
                );
            }
        }
    }

    #[test]
    fn embedded_input_matches_independent_texel_functions() {
        for case in cases() {
            for (i, pixel) in case.input().chunks_exact(4).enumerate() {
                let (x, y) = ((i % 4) as u8, (i / 4) as u8);
                if case.depth {
                    assert_eq!(pixel, depth(x, y).to_le_bytes());
                } else {
                    assert_eq!(pixel, color(x, y));
                }
            }
            assert_eq!(case.input().len(), PIXELS * 4);
        }
    }

    #[test]
    fn spirv_fixtures_have_real_gather_opcodes_and_original_bindings() {
        assert!(spirv_words(&[]).is_err());
        assert!(spirv_words(&[0; 20]).is_err());
        for case in cases() {
            let words = spirv_words(case.fragment.spirv).unwrap();
            let mut i = 5;
            let mut gathers = [0; 2];
            let mut bindings = Vec::new();
            while i < words.len() {
                let count = (words[i] >> 16) as usize;
                assert!(count > 0 && i + count <= words.len());
                match words[i] & 0xffff {
                    96 => gathers[0] += 1, // OpImageGather
                    97 => gathers[1] += 1, // OpImageDrefGather
                    71 if count == 4 && words[i + 2] == 33 => bindings.push(words[i + 3]), // Binding
                    71 if count == 4 && words[i + 2] == 34 => assert_eq!(words[i + 3], 0), // DescriptorSet
                    _ => (),
                }
                i += count;
            }
            bindings.sort_unstable();
            assert_eq!(bindings, [0, 1]);
            assert_eq!(
                gathers,
                if case.compare.is_some() {
                    [0, 1]
                } else {
                    [1, 0]
                }
            );
        }
    }
}
