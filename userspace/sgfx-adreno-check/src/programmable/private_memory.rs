// SPDX-License-Identifier: GPL-2.0-only
//! Real VS/FS/CS private-memory oracle: dynamic 256-word function-local arrays.
use super::*;

const VS: &str = include_str!("private-shaders/vertex.wgsl");
const FS: &str = include_str!("private-shaders/fragment.wgsl");
const PLAIN_VS: &str = include_str!("private-shaders/plain_vertex.wgsl");
const PLAIN_FS: &str = include_str!("private-shaders/plain_fragment.wgsl");
const CS: &str = include_str!("private-shaders/compute.wgsl");
const WIDTH: usize = 8;
const WORDS: usize = 512;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Vertex,
    Fragment,
    Both,
    Compute,
}
struct Case {
    mode: Mode,
    params: [u32; 4],
    input: Vec<u8>,
}
fn cases() -> Vec<Case> {
    let mut result = Vec::new();
    for params in [[19, 0x1234, 173, 0], [503, 0x4321, 31, 137]] {
        for mode in [Mode::Vertex, Mode::Fragment, Mode::Both, Mode::Compute] {
            result.push(Case {
                mode,
                params,
                input: params.iter().flat_map(|p| p.to_le_bytes()).collect(),
            });
        }
    }
    result
}
fn value(params: [u32; 4], id: u32) -> u32 {
    let [seed, mask, index, select] = params;
    let index = (index + id * 7) % 256;
    let selected = (index + select) % 256;
    // Independent direct evaluation of the one mutated element, not a shader
    // interpreter or a copy of the GPU array/loop implementation.
    if selected == index {
        (((index + 1) % 256) * 17 + id * 13 + seed) ^ mask ^ 0x5a
    } else {
        (selected * 17 + id * 13 + seed) ^ mask
    }
}
impl Case {
    fn expected(&self) -> Vec<u8> {
        if matches!(self.mode, Mode::Compute) {
            return (0..WORDS)
                .flat_map(|i| value(self.params, i as u32).to_le_bytes())
                .collect();
        }
        let v = if matches!(self.mode, Mode::Vertex | Mode::Both) {
            value(self.params, 0)
        } else {
            7
        };
        (0..WIDTH * WIDTH)
            .flat_map(|i| {
                let n = if matches!(self.mode, Mode::Fragment | Mode::Both) {
                    value(self.params, i as u32) ^ v
                } else {
                    v
                };
                [(n >> 16) as u8, (n >> 8) as u8, n as u8, 255] // BGRA8
            })
            .collect()
    }
}
struct Recorded<'r, 'd> {
    commands: CommandBuffer<'r, 'd>,
    output: usize,
    shaders: Vec<usize>,
    pipeline: usize,
    compute: bool,
}
fn record<'r, 'd>(table: &'r ResourceTable, case: &'d Case) -> Result<Recorded<'r, 'd>, String> {
    let compute = matches!(case.mode, Mode::Compute);
    let params = checked!(table.define_buffer(checked!(BufferDesc::new(
        16,
        BufferUsage::UNIFORM | BufferUsage::COPY_DST
    ))));
    let mut entries = vec![BindGroupLayoutEntry::new(
        0,
        if compute {
            ShaderStages::COMPUTE
        } else {
            ShaderStages::VERTEX | ShaderStages::FRAGMENT
        },
        BindingType::UniformBuffer,
    )];
    let output_buffer = if compute {
        Some(checked!(table.define_buffer(checked!(BufferDesc::new(
            (WORDS * 4) as u64,
            BufferUsage::STORAGE | BufferUsage::COPY_SRC | BufferUsage::COPY_DST
        )))))
    } else {
        None
    };
    if compute {
        entries.push(BindGroupLayoutEntry::new(
            1,
            ShaderStages::COMPUTE,
            BindingType::StorageBuffer { read_only: false },
        ));
    }
    let layout = checked!(BindGroupLayoutDesc::new(entries));
    let mut entries = vec![BindGroupEntry::new(
        0,
        BindingResource::Buffer {
            buffer: params.id(),
            offset: 0,
            size: 16,
        },
    )];
    if let Some(buffer) = output_buffer {
        entries.push(BindGroupEntry::new(
            1,
            BindingResource::Buffer {
                buffer: buffer.id(),
                offset: 0,
                size: (WORDS * 4) as u64,
            },
        ));
    }
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        table,
        layout.clone(),
        entries
    ))));
    table.enable_abi_commands();
    let mut encoder = CommandEncoder::new(table);
    checked!(encoder.write_buffer(params, 0, &case.input));
    if let Some(buffer) = output_buffer {
        static SENTINEL: [u8; WORDS * 4] = [0xcd; WORDS * 4];
        checked!(encoder.write_buffer(buffer, 0, &SENTINEL));
        checked!(encoder.resource_barrier(ResourceBarrier::Global));
        let shader =
            checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(CS.into()))));
        let pipeline = checked!(
            table.define_compute_pipeline(checked!(ComputePipelineDesc::new(
                checked!(ShaderEntryPoint::new(
                    shader,
                    ShaderStage::Compute,
                    "main".into()
                )),
                checked!(PipelineLayoutDesc::new(vec![layout]))
            )))
        );
        let mut pass = checked!(encoder.begin_compute_pass());
        checked!(pass.set_pipeline(pipeline));
        checked!(pass.set_bind_group(0, group));
        checked!(pass.dispatch(4, 1, 1));
        checked!(pass.end());
        return Ok(Recorded {
            commands: checked!(encoder.finish()),
            output: buffer.slot(),
            shaders: vec![shader.slot()],
            pipeline: pipeline.slot(),
            compute,
        });
    }
    checked!(encoder.resource_barrier(ResourceBarrier::Global));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(WIDTH as u32, WIDTH as u32)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let vertex = checked!(
        table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
            if matches!(case.mode, Mode::Vertex | Mode::Both) {
                VS
            } else {
                PLAIN_VS
            }
            .into()
        )))
    );
    let fragment = checked!(
        table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
            if matches!(case.mode, Mode::Fragment | Mode::Both) {
                FS
            } else {
                PLAIN_FS
            }
            .into()
        )))
    );
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                vertex,
                ShaderStage::Vertex,
                "main".into()
            )),
            checked!(ShaderEntryPoint::new(
                fragment,
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
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        target,
        checked!(PixelRect::new(0, 0, WIDTH as u32, WIDTH as u32)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 0.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    Ok(Recorded {
        commands: checked!(encoder.finish()),
        output: target.slot(),
        shaders: vec![vertex.slot(), fragment.slot()],
        pipeline: pipeline.slot(),
        compute,
    })
}
fn check(case: &Case, output: &[u8]) -> Result<(), String> {
    let expected = case.expected();
    if output.len() != expected.len() {
        return Err("private oracle readback length".into());
    }
    if matches!(case.mode, Mode::Compute) {
        for (i, (a, b)) in output
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .enumerate()
        {
            if a != b {
                return Err(format!("private Compute word={i} got={a:?} expected={b:?}"));
            }
        }
    } else {
        for (i, (a, b)) in output
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .enumerate()
        {
            if a.iter().zip(b).any(|(a, b)| a.abs_diff(*b) > 1) {
                return Err(format!(
                    "private {:?} pixel=({},{}) BGRA={a:?} expected={b:?}",
                    case.mode,
                    i % WIDTH,
                    i / WIDTH
                ));
            }
        }
    }
    Ok(())
}
fn execute(path: &str, case: &Case) -> Result<(), String> {
    println!(
        "BEGIN programmable-private mode={:?} params={:?}",
        case.mode, case.params
    );
    let table = ResourceTable::new();
    let recorded = record(&table, case)?;
    let driver = DriverSession::new(path, &table)?;
    for shader in recorded.shaders {
        driver.validate(abi::VALIDATE_SHADER, shader)?;
    }
    driver.validate(
        if recorded.compute {
            abi::VALIDATE_COMPUTE_PIPELINE
        } else {
            abi::VALIDATE_RENDER_PIPELINE
        },
        recorded.pipeline,
    )?;
    if std::env::args().any(|a| a == "--compile-only") {
        println!(
            "COMPILED programmable-private {:?} (execution skipped)",
            case.mode
        );
        return Ok(());
    }
    // Replay through the same driver/cache to exercise persistent scratch BOs.
    for replay in 0..2 {
        driver.submit(&recorded.commands)?;
        let mut output = vec![0xa5; case.expected().len()];
        if recorded.compute {
            driver.read_buffer(recorded.output, &mut output)?;
        } else {
            driver.read_texture(recorded.output, &mut output)?;
        }
        check(case, &output)?;
        println!(
            "PASS programmable-private {:?} replay={replay} {}={}",
            case.mode,
            if recorded.compute { "words" } else { "pixels" },
            output.len() / 4
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for case in cases() {
        if let Err(e) = execute(path, &case) {
            println!("FAIL programmable-private {e}");
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_sources_record_graphics_compute_and_detect_wrong_results() {
        for case in cases() {
            let table = ResourceTable::new();
            let recorded = record(&table, &case).unwrap();
            assert!(recorded.commands.abi_batch().is_some());
            let good = case.expected();
            check(&case, &good).unwrap();
            let mut bad = good;
            let last = bad.len() - 4;
            bad[last] ^= 0x80;
            assert!(check(&case, &bad).is_err());
        }
    }
    #[test]
    fn direct_oracle_matches_independent_array_simulation_including_mutation() {
        for case in cases() {
            for id in 0..WORDS as u32 {
                let mut a = [0u32; 256];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = (i as u32 * 17 + id * 13 + case.params[0]) ^ case.params[1];
                }
                let i = ((case.params[2] + id * 7) % 256) as usize;
                a[i] = a[(i + 1) % 256] ^ 0x5a;
                assert_eq!(
                    value(case.params, id),
                    a[(i + case.params[3] as usize) % 256]
                );
            }
        }
    }
}
