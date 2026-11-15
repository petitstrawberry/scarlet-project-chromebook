// SPDX-License-Identifier: GPL-2.0-only
//! Real public fixed-pipeline primitive assembly, with independent pixel probes.
use super::*;
const SIDE: u32 = 16;
fn vertices(t: PrimitiveTopology) -> Vec<[f32; 2]> {
    use PrimitiveTopology::*;
    match t {
        TriangleList => vec![
            [0.0, 16.0],
            [16.0, 16.0],
            [0.0, 0.0],
            [0.0, 0.0],
            [16.0, 16.0],
            [16.0, 0.0],
        ],
        TriangleStrip => vec![[0.0, 16.0], [16.0, 16.0], [0.0, 0.0], [16.0, 0.0]],
        TriangleFan => vec![[0.0, 16.0], [16.0, 16.0], [16.0, 0.0], [0.0, 0.0]],
        LineList => vec![[2.5, 4.5], [13.5, 4.5], [2.5, 11.5], [13.5, 11.5]],
        LineStrip => vec![[3.5, 3.5], [12.5, 3.5], [12.5, 12.5]],
        PointList => vec![[3.5, 3.5], [8.5, 8.5], [12.5, 12.5]],
    }
}
fn probes(t: PrimitiveTopology) -> Vec<(u32, u32, bool)> {
    use PrimitiveTopology::*;
    let mut p = vec![(0, 0, false), (15, 15, false)];
    match t {
        TriangleList | TriangleStrip | TriangleFan => {
            p.clear();
            for y in 1..15 {
                for x in 1..15 {
                    p.push((x, y, true));
                }
            }
        }
        LineList => {
            for y in [4, 11] {
                for x in 4..12 {
                    p.push((x, y, true));
                }
            }
            p.push((8, 8, false));
        }
        LineStrip => {
            for x in 5..11 {
                p.push((x, 3, true));
            }
            for y in 5..11 {
                p.push((12, y, true));
            }
            p.push((8, 8, false));
        }
        PointList => {
            for (x, y) in [(3, 3), (8, 8), (12, 12)] {
                p.push((x, y, true));
                p.push((x + 1, y, false));
                p.push((x, y + 1, false));
            }
        }
    }
    p
}
fn execute(path: &str, topology: PrimitiveTopology, index_mode: u32) -> Result<(), String> {
    println!("BEGIN fixed-topology topology={topology:?} index-mode={index_mode}");
    let table = ResourceTable::new();
    table.enable_abi_commands();
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(SIDE, SIDE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let pipeline = checked!(
        table.define_render_pipeline(checked!(RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            topology,
            checked!(VertexBufferLayout::new(
                24,
                vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)]
            )),
            FragmentProgram::Solid,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )))
    );
    let mut positions = vertices(topology);
    let count = positions.len() as u32;
    if index_mode != 0 {
        positions.reverse();
    }
    let mut data = Vec::new();
    for [x, y] in positions {
        for v in [x / 8.0 - 1.0, 1.0 - y / 8.0, 0.0, 1.0, 0.0, 0.0] {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    let vertex = checked!(table.define_buffer(checked!(BufferDesc::new(
        data.len() as u64,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let mut indices = Vec::new();
    if index_mode != 0 {
        if index_mode == 16 {
            indices.extend_from_slice(&u16::MAX.to_le_bytes());
        } else {
            indices.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        for i in (0..count).rev() {
            if index_mode == 16 {
                indices.extend_from_slice(&(i as u16).to_le_bytes());
            } else {
                indices.extend_from_slice(&i.to_le_bytes());
            }
        }
        indices.resize(indices.len().next_multiple_of(4), 0);
    }
    let index = if index_mode != 0 {
        Some(checked!(table.define_buffer(checked!(BufferDesc::new(
            indices.len() as u64,
            BufferUsage::INDEX | BufferUsage::COPY_DST
        )))))
    } else {
        None
    };
    let mut encoder = CommandEncoder::new(&table);
    checked!(encoder.write_buffer(vertex, 0, &data));
    if let Some(index) = index {
        checked!(encoder.write_buffer(index, 0, &indices));
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
        checked!(Color::rgba(0.25, 0.5, 0.75, 1.0))
    )));
    if let Some(index) = index {
        checked!(pass.set_index_buffer(
            index,
            0,
            if index_mode == 16 {
                IndexFormat::Uint16
            } else {
                IndexFormat::Uint32
            }
        ));
        checked!(pass.draw_indexed(count, 1, 0));
    } else {
        checked!(pass.draw(count, 0));
    }
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    #[cfg(test)]
    if path == "__encode_only__" {
        let batch = commands
            .abi_batch()
            .ok_or("missing fixed topology ABI batch")?;
        if batch.count == 0 || batch.words.len == 0 {
            return Err("empty fixed topology ABI batch".into());
        }
        return Ok(());
    }
    // DriverApi VALIDATE_RENDER_PIPELINE addresses the programmable pipeline
    // table. Fixed pipelines are validated by core recording and real submit.
    let driver = DriverSession::new(path, &table)?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        println!("SKIP fixed-topology pixels (--compile-only)");
        return Ok(());
    }
    for replay in 0..2 {
        driver.submit(&commands)?;
        let mut pixels = vec![0xa5; (SIDE * SIDE * 4) as usize];
        driver.read_texture(target.slot(), &mut pixels)?;
        let probes = probes(topology);
        for &(x, y, covered) in &probes {
            let off = ((y * SIDE + x) * 4) as usize;
            let got = &pixels[off..off + 4];
            let want = if covered {
                [191, 128, 64, 255]
            } else {
                [0, 0, 0, 255]
            };
            if got.iter().zip(want).any(|(&a, b)| a.abs_diff(b) > 1) {
                return Err(format!(
                    "fixed-topology {topology:?} index-mode={index_mode} replay={replay} pixel={x},{y} got={got:?} expected={want:?}"
                ));
            }
        }
        println!(
            "PASS fixed-topology topology={topology:?} index-mode={index_mode} replay={replay} pixel-probes={} reversed-index-fetch={}",
            probes.len(),
            index_mode != 0
        );
    }
    Ok(())
}
pub(super) fn run(path: &str) -> Result<(), String> {
    for topology in [
        PrimitiveTopology::TriangleList,
        PrimitiveTopology::TriangleStrip,
        PrimitiveTopology::TriangleFan,
        PrimitiveTopology::LineList,
        PrimitiveTopology::LineStrip,
        PrimitiveTopology::PointList,
    ] {
        for index_mode in [0, 16, 32] {
            execute(path, topology, index_mode)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_fixed_topology_records_real_public_fixed_commands_without_programmable_validation() {
        super::run("__encode_only__").unwrap();
    }
}
