// SPDX-License-Identifier: GPL-2.0-only
//! Installed-driver oracles for the remaining portable graphics capabilities.
//! No capability is enabled here. Every draw is checked through image readback.
use super::*;

const SIZE: u32 = 4;
const FLAT_FS: &str = "@fragment fn fs(@location(0) @interpolate(flat) value:vec4<f32>)->@location(0) vec4<f32>{return value;}";
fn target(table: &ResourceTable, width: u32) -> Result<TextureRef<'_>, String> {
    table.enable_abi_commands();
    Ok(checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(width, SIZE)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    )))))
}
fn pipeline<'r>(
    table: &'r ResourceTable,
    vs: ShaderModuleDesc,
    fs: ShaderModuleDesc,
    layout: PipelineLayoutDesc,
    buffers: Vec<VertexBufferLayout>,
) -> Result<ProgrammableRenderPipelineRef<'r>, String> {
    let vertex = checked!(table.define_shader_module(vs));
    let fragment = checked!(table.define_shader_module(fs));
    let desc = checked!(ProgrammableRenderPipelineDesc::new(
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
        layout,
        TextureFormat::Bgra8Unorm,
        None,
        PrimitiveTopology::TriangleList,
        BlendState::REPLACE,
        RasterState::new(CullMode::None, FrontFace::CounterClockwise)
    ));
    Ok(checked!(table.define_programmable_render_pipeline(
        checked!(desc.with_vertex_buffers(buffers))
    )))
}
fn wgsl(source: &str) -> Result<ShaderModuleDesc, String> {
    Ok(checked!(ShaderModuleDesc::wgsl(source.into())))
}
fn empty_layout() -> Result<PipelineLayoutDesc, String> {
    Ok(checked!(PipelineLayoutDesc::new(vec![])))
}
fn pass_desc<'r>(
    table: &'r ResourceTable,
    target: TextureRef<'r>,
    width: u32,
) -> Result<RenderPassDesc<'r>, String> {
    Ok(checked!(RenderPassDesc::new(
        table,
        target,
        checked!(PixelRect::new(0, 0, width, SIZE)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    )))
}
fn check_pixels(
    driver: &DriverSession,
    target: TextureRef<'_>,
    width: u32,
    label: &str,
    expected: impl Fn(u32, u32) -> [u8; 4],
) -> Result<(), String> {
    let mut bytes = vec![0xa5; (width * SIZE * 4) as usize];
    driver.read_texture(target.slot(), &mut bytes)?;
    for (i, pixel) in bytes.chunks_exact(4).enumerate() {
        let x = i as u32 % width;
        let y = i as u32 / width;
        let want = expected(x, y);
        let got = [pixel[2], pixel[1], pixel[0], pixel[3]];
        if got.iter().zip(want).any(|(a, b)| a.abs_diff(b) > 1) {
            return Err(format!(
                "{label} pixel={x},{y} RGBA={got:?} expected={want:?}"
            ));
        }
    }
    Ok(())
}
// Keep encoding identical between host tests and hardware. The test-only path
// stops after constructing a real ABI batch; it never reports a GPU PASS.
fn execute(
    path: &str,
    table: &ResourceTable,
    pipeline: ProgrammableRenderPipelineRef<'_>,
    commands: &CommandBuffer<'_, '_>,
) -> Result<Option<DriverSession>, String> {
    #[cfg(test)]
    if path == "__encode_only__" {
        let batch = commands.abi_batch().ok_or("missing feature ABI batch")?;
        if batch.count == 0 || batch.words.len == 0 {
            return Err("empty feature ABI batch".into());
        }
        return Ok(None);
    }
    let driver = DriverSession::new(path, table)?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    if compile_only() {
        return Ok(None);
    }
    driver.submit(commands)?;
    Ok(Some(driver))
}
fn compile_only() -> bool {
    std::env::args().any(|a| a == "--compile-only")
}
const POSITION: &str = "let x=f32((vertex<<1u)&2u);let y=f32(vertex&2u);let position=vec4<f32>(x*2.0-1.0,y*2.0-1.0,0.0,1.0);";

fn storage_sources() -> (String, String) {
    let mut declarations = String::new();
    for i in 0..4 {
        declarations += &format!("@group(0) @binding({i}) var<storage,read> b{i}:array<u32>;\n");
    }
    let vs=format!("{declarations} struct Out {{ @builtin(position) position:vec4<f32>,@location(0) @interpolate(flat) value:vec4<u32> }};
@vertex fn main(@builtin(vertex_index) vertex:u32)->Out {{ {POSITION} return Out(position,vec4<u32>(b0[65535u]+b0[32768u],b1[65535u]+b1[32768u],b2[65535u]+b2[32768u],b3[65535u]+b3[32768u])); }}");
    let fs=format!("{declarations} @fragment fn main(@builtin(position) pos:vec4<f32>,@location(0) @interpolate(flat) value:vec4<u32>)->@location(0) vec4<f32> {{
 let pixel=u32(pos.x)+4u*u32(pos.y);let i=pixel*4369u;
 let v=value+vec4<u32>(b0[min(i,arrayLength(&b0)-1u)],b1[min(i,arrayLength(&b1)-1u)],b2[min(i,arrayLength(&b2)-1u)],b3[min(i,arrayLength(&b3)-1u)]);
 return vec4<f32>(v&vec4<u32>(255u))/255.0; }}");
    (vs, fs)
}
fn storage_value(binding: u32, index: u32) -> u32 {
    index
        .wrapping_mul(17 + binding * 6)
        .wrapping_add(11 + binding * 43)
}
fn readonly_storage(path: &str) -> Result<(), String> {
    println!(
        "BEGIN programmable-feature readonly-storage-graphics buffers=4 bytes-each=262144 offset=4 stages=VS+FS"
    );
    let table = ResourceTable::new();
    let target = target(&table, SIZE)?;
    let (vs, fs) = storage_sources();
    let mut buffers = Vec::new();
    let mut uploads = Vec::new();
    let mut entries = Vec::new();
    let mut layout_entries = Vec::new();
    for i in 0..4 {
        let b = checked!(table.define_buffer(checked!(BufferDesc::new(
            262152,
            BufferUsage::STORAGE | BufferUsage::COPY_DST | BufferUsage::COPY_SRC
        ))));
        let mut data = 0xdeadc0deu32.to_le_bytes().to_vec();
        for word in 0..65536 {
            data.extend_from_slice(&storage_value(i, word).to_le_bytes());
        }
        data.extend_from_slice(&0xbaadf00du32.to_le_bytes());
        uploads.push(data);
        buffers.push(b);
        layout_entries.push(BindGroupLayoutEntry::new(
            i,
            ShaderStages::VERTEX | ShaderStages::FRAGMENT,
            BindingType::StorageBuffer { read_only: true },
        ));
        entries.push(BindGroupEntry::new(
            i,
            BindingResource::Buffer {
                buffer: b.id(),
                offset: 4,
                size: 262144,
            },
        ));
    }
    let layout = checked!(BindGroupLayoutDesc::new(layout_entries));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        layout.clone(),
        entries
    ))));
    let p = pipeline(
        &table,
        wgsl(&vs)?,
        wgsl(&fs)?,
        checked!(PipelineLayoutDesc::new(vec![layout])),
        vec![],
    )?;
    let mut encoder = CommandEncoder::new(&table);
    for (b, data) in buffers.iter().zip(&uploads) {
        checked!(encoder.write_buffer(*b, 0, data));
    }
    let mut pass = checked!(encoder.begin_render_pass(pass_desc(&table, target, SIZE)?));
    checked!(pass.set_programmable_pipeline(p));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    let Some(driver) = execute(path, &table, p, &commands)? else {
        return Ok(());
    };
    check_pixels(&driver, target, SIZE, "readonly-4x256KiB", |x, y| {
        core::array::from_fn(|c| {
            storage_value(c as u32, (x + y * 4) * 4369)
                .wrapping_add(storage_value(c as u32, 65535))
                .wrapping_add(storage_value(c as u32, 32768)) as u8
        })
    })?;
    for (b, want) in buffers.iter().zip(&uploads) {
        let mut got = vec![0; want.len()];
        driver.read_buffer(b.slot(), &mut got)?;
        if &got != want {
            return Err("readonly storage or prefix/suffix guard changed".into());
        }
    }
    println!(
        "PASS programmable-feature readonly-storage-graphics pixels=16 immutable-bytes=1048608"
    );
    Ok(())
}
fn push_sources() -> (String, String) {
    let declaration = "struct Push { values:array<vec4<u32>,8> };var<push_constant> p:Push;";
    let vs=format!("{declaration} struct Out {{@builtin(position) position:vec4<f32>,@location(0) @interpolate(flat) value:vec4<u32>}};
@vertex fn main(@builtin(vertex_index) vertex:u32)->Out {{{POSITION} var sum=vec4<u32>(0u);for(var i=0u;i<8u;i++){{sum+=p.values[i]*(i+1u);}}return Out(position,sum);}}");
    let fs = format!(
        "{declaration} @fragment fn main(@location(0) @interpolate(flat) value:vec4<u32>)->@location(0) vec4<f32>{{var sum=value;for(var i=0u;i<8u;i++){{sum+=p.values[i]*(i+3u);}}return vec4<f32>(sum&vec4<u32>(255u))/255.0;}}"
    );
    (vs, fs)
}
fn push128(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-feature push128-graphics both-stages+partial-updates");
    let table = ResourceTable::new();
    let target = target(&table, 12)?;
    let (vs, fs) = push_sources();
    let stages = ShaderStages::VERTEX | ShaderStages::FRAGMENT;
    // SGFX follows the overlapping-range contract: writes must include every
    // stage whose declared range overlaps. Exercise the full shared range.
    let layout = checked!(
        empty_layout()?
            .with_push_constant_ranges(vec![checked!(PushConstantRange::new(stages, 0, 128))])
    );
    let p = pipeline(&table, wgsl(&vs)?, wgsl(&fs)?, layout, vec![])?;
    let mut vertex: [u32; 32] = core::array::from_fn(|i| 3 * i as u32 + 1);
    let vb: Vec<_> = vertex.iter().flat_map(|v| v.to_le_bytes()).collect();
    let patches = [173u32.to_le_bytes(), 211u32.to_le_bytes()];
    let mut expected = Vec::new();
    let mut encoder = CommandEncoder::new(&table);
    let mut pass = checked!(encoder.begin_render_pass(pass_desc(&table, target, 12)?));
    checked!(pass.set_programmable_pipeline(p));
    checked!(pass.set_push_constants(stages, 0, &vb));
    for region in 0..3 {
        if region == 1 {
            vertex[31] = 173;
            checked!(pass.set_push_constants(stages, 124, &patches[0]));
        }
        if region == 2 {
            vertex[0] = 211;
            checked!(pass.set_push_constants(stages, 0, &patches[1]));
        }
        expected.push(core::array::from_fn::<_, 4, _>(|c| {
            (0..8)
                .map(|i| vertex[i * 4 + c] * (2 * i as u32 + 4))
                .sum::<u32>() as u8
        }));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(region * 4, 0, 4, SIZE)))));
        checked!(pass.draw(3, 0));
    }
    checked!(pass.end());
    let commands = checked!(encoder.finish());
    let Some(driver) = execute(path, &table, p, &commands)? else {
        return Ok(());
    };
    check_pixels(&driver, target, 12, "push128-both-stages", |x, _| {
        expected[x as usize / 4]
    })?;
    println!(
        "PASS programmable-feature push128-graphics pixels=48 stages=VS+FS partial-offsets=124,0"
    );
    Ok(())
}

struct FormatCase {
    format: VertexFormat,
    scalar: &'static str,
    count: usize,
    bytes: Vec<u8>,
    values: Vec<f32>,
    normalized: bool,
}
fn format_cases() -> Vec<FormatCase> {
    use VertexFormat::*;
    let mut cases = Vec::new();
    for (format, count) in [(Float32, 1), (Float32x2, 2), (Float32x3, 3), (Float32x4, 4)] {
        let values = [0.25f32, -0.5, 0.75, 1.0][..count].to_vec();
        let bytes = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        cases.push(FormatCase {
            format,
            scalar: "f32",
            count,
            bytes,
            values,
            normalized: false,
        });
    }
    for (format, count) in [(Float16x2, 2), (Float16x4, 4)] {
        cases.push(FormatCase {
            format,
            scalar: "f32",
            count,
            bytes: [0x3400u16, 0xb800, 0x3a00, 0x3c00][..count]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            values: [0.25, -0.5, 0.75, 1.0][..count].to_vec(),
            normalized: false,
        });
    }
    for (format, count) in [(Uint32, 1), (Uint32x2, 2), (Uint32x3, 3), (Uint32x4, 4)] {
        cases.push(FormatCase {
            format,
            scalar: "u32",
            count,
            bytes: [17u32, 31, 63, 127][..count]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            values: [17.0, 31.0, 63.0, 127.0][..count].to_vec(),
            normalized: false,
        });
    }
    for (format, count) in [(Sint32, 1), (Sint32x2, 2), (Sint32x3, 3), (Sint32x4, 4)] {
        cases.push(FormatCase {
            format,
            scalar: "i32",
            count,
            bytes: [-17i32, -31, 63, 127][..count]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            values: [-17.0, -31.0, 63.0, 127.0][..count].to_vec(),
            normalized: false,
        });
    }
    for (format, count) in [(Sint16x2, 2), (Sint16x4, 4)] {
        cases.push(FormatCase {
            format,
            scalar: "i32",
            count,
            bytes: [-17i16, -31, 63, 127][..count]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect(),
            values: [-17.0, -31.0, 63.0, 127.0][..count].to_vec(),
            normalized: false,
        });
    }
    cases.push(FormatCase {
        format: Uint16x2,
        scalar: "u32",
        count: 2,
        bytes: [17u16, 31].iter().flat_map(|v| v.to_le_bytes()).collect(),
        values: vec![17.0, 31.0],
        normalized: false,
    });
    cases.push(FormatCase {
        format: Unorm8x4,
        scalar: "f32",
        count: 4,
        bytes: vec![13, 67, 129, 251],
        values: vec![13.0 / 255.0, 67.0 / 255.0, 129.0 / 255.0, 251.0 / 255.0],
        normalized: true,
    });
    cases.push(FormatCase {
        format: Snorm8x4,
        scalar: "f32",
        count: 4,
        bytes: vec![128, 192, 64, 127],
        values: vec![-1.0, -64.0 / 127.0, 64.0 / 127.0, 1.0],
        normalized: false,
    });
    let packed = 512u32 | (768 << 10) | (256 << 20) | (1 << 30);
    cases.push(FormatCase {
        format: Snorm10_10_10_2,
        scalar: "f32",
        count: 4,
        bytes: packed.to_le_bytes().to_vec(),
        values: vec![-1.0, -256.0 / 511.0, 256.0 / 511.0, 1.0],
        normalized: false,
    });
    cases
}
fn format_source(c: &FormatCase) -> String {
    let ty = if c.count == 1 {
        c.scalar.into()
    } else {
        format!("vec{}<{}>", c.count, c.scalar)
    };
    let components: Vec<_> = (0..4)
        .map(|i| {
            if i >= c.count {
                return if i == 3 { "1.0".into() } else { "0.0".into() };
            }
            let v = if c.count == 1 {
                "f32(value)".into()
            } else {
                format!("f32(value[{i}])")
            };
            match c.scalar {
                "u32" => format!("{v}/255.0"),
                "i32" => format!("({v}+128.0)/255.0"),
                _ if c.normalized => v,
                _ => format!("{v}*0.25+0.5"),
            }
        })
        .collect();
    format!("struct Out {{@builtin(position) position:vec4<f32>,@location(0) @interpolate(flat) value:vec4<f32>}};
@vertex fn main(@builtin(vertex_index) vertex:u32,@location(0) value:{ty})->Out {{{POSITION}return Out(position,vec4<f32>({}));}}",components.join(","))
}
fn format_expected(c: &FormatCase) -> [u8; 4] {
    core::array::from_fn(|i| {
        if i >= c.count {
            return if i == 3 { 255 } else { 0 };
        }
        let v = c.values[i];
        let v = match c.scalar {
            "u32" => v / 255.0,
            "i32" => (v + 128.0) / 255.0,
            _ if c.normalized => v,
            _ => v * 0.25 + 0.5,
        };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    })
}
fn vertex_formats(path: &str) -> Result<(), String> {
    for c in format_cases() {
        println!("BEGIN programmable-feature vertex-format={:?}", c.format);
        let table = ResourceTable::new();
        let target = target(&table, SIZE)?;
        let stride = c.bytes.len() + 8;
        let mut upload = vec![0xcd; 8 + stride * 3];
        for i in 0..3 {
            upload[8 + i * stride + 4..8 + i * stride + 4 + c.bytes.len()]
                .copy_from_slice(&c.bytes);
        }
        let buffer = checked!(table.define_buffer(checked!(BufferDesc::new(
            upload.len() as u64,
            BufferUsage::VERTEX | BufferUsage::COPY_DST
        ))));
        let layout = checked!(VertexBufferLayout::new(
            stride as u32,
            vec![VertexAttribute::new(0, c.format, 4)]
        ));
        let p = pipeline(
            &table,
            wgsl(&format_source(&c))?,
            wgsl(&FLAT_FS.replace("fn fs(", "fn main("))?,
            empty_layout()?,
            vec![layout],
        )?;
        let mut encoder = CommandEncoder::new(&table);
        checked!(encoder.write_buffer(buffer, 0, &upload));
        let mut pass = checked!(encoder.begin_render_pass(pass_desc(&table, target, SIZE)?));
        checked!(pass.set_programmable_pipeline(p));
        checked!(pass.set_vertex_buffer(buffer, 8));
        checked!(pass.draw(3, 0));
        checked!(pass.end());
        let commands = checked!(encoder.finish());
        let Some(driver) = execute(path, &table, p, &commands)? else {
            continue;
        };
        check_pixels(
            &driver,
            target,
            SIZE,
            &format!("vertex-format={:?}", c.format),
            |_, _| format_expected(&c),
        )?;
        println!(
            "PASS programmable-feature vertex-format={:?} pixels=16 binding-offset=8 attribute-offset=4 stride={stride}",
            c.format
        );
    }
    Ok(())
}
const DIVISOR_VS: &str = r#"struct Out {@builtin(position) position:vec4<f32>,@location(0) @interpolate(flat) value:vec4<f32>};
@vertex fn main(@builtin(vertex_index) vertex:u32,@builtin(instance_index) instance:u32,@location(0) value:u32)->Out {
 var corners=array<vec2<f32>,6>(vec2(0.0,0.0),vec2(1.0,0.0),vec2(0.0,1.0),vec2(0.0,1.0),vec2(1.0,0.0),vec2(1.0,1.0));
 let p=corners[vertex];let x=(f32(instance-3u)+p.x)/8.0;
 return Out(vec4<f32>(x*2.0-1.0,p.y*2.0-1.0,0.0,1.0),vec4<f32>(f32(value),f32(value^85u),0.0,255.0)/255.0);
}"#;
fn divisors(path: &str) -> Result<(), String> {
    let table = ResourceTable::new();
    let target = target(&table, 32)?;
    let mut upload = vec![0xcd; 4 + 12 * 12];
    for i in 0..12 {
        upload[4 + i * 12 + 4..4 + i * 12 + 8].copy_from_slice(&(i as u32 * 17 + 5).to_le_bytes());
    }
    let buffer = checked!(table.define_buffer(checked!(BufferDesc::new(
        upload.len() as u64,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let mut pipelines = Vec::new();
    for divisor in [1, 2, 5, u32::MAX] {
        let layout = checked!(
            checked!(VertexBufferLayout::new(
                12,
                vec![VertexAttribute::new(0, VertexFormat::Uint32, 4)]
            ))
            .with_step_mode(VertexStepMode::Instance)
            .with_instance_divisor(divisor)
        );
        pipelines.push((
            divisor,
            pipeline(
                &table,
                wgsl(DIVISOR_VS)?,
                wgsl(&FLAT_FS.replace("fn fs(", "fn main("))?,
                empty_layout()?,
                vec![layout],
            )?,
        ));
    }
    for (divisor, p) in pipelines {
        println!("BEGIN programmable-feature divisor={divisor} first-instance=3");
        let mut encoder = CommandEncoder::new(&table);
        checked!(encoder.write_buffer(buffer, 0, &upload));
        let mut pass = checked!(encoder.begin_render_pass(pass_desc(&table, target, 32)?));
        checked!(pass.set_programmable_pipeline(p));
        checked!(pass.set_vertex_buffer_slot(0, buffer, 4));
        checked!(pass.draw_instanced(6, 0, 8, 3));
        checked!(pass.end());
        let commands = checked!(encoder.finish());
        let Some(driver) = execute(path, &table, p, &commands)? else {
            continue;
        };
        check_pixels(&driver, target, 32, "vertex-divisor", |x, _| {
            let value = ((3 + (x / 4) / divisor) * 17 + 5) as u8;
            [value, value ^ 85, 0, 255]
        })?;
        println!(
            "PASS programmable-feature divisor={divisor} first-instance=3 instances=8 pixels=128"
        );
    }
    Ok(())
}
fn draw_parameters(path: &str) -> Result<(), String> {
    let variants: [(&str, &[u8]); 2] = [
        (
            "core",
            include_bytes!(
                "../../../sgfx-codegen-adreno-a6xx/tests/native-fixtures/draw_vs_core.spv"
            ),
        ),
        (
            "khr",
            include_bytes!(
                "../../../sgfx-codegen-adreno-a6xx/tests/native-fixtures/draw_vs_khr.spv"
            ),
        ),
    ];
    for (variant, bytes) in variants {
        println!("BEGIN programmable-feature draw-parameters variant={variant}");
        let table = ResourceTable::new();
        let target = target(&table, 12)?;
        let words = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let p = pipeline(
            &table,
            checked!(ShaderModuleDesc::spirv(words)),
            wgsl(include_str!(
                "../../../sgfx-codegen-adreno-a6xx/tests/native-fixtures/draw_fs.wgsl"
            ))?,
            empty_layout()?,
            vec![],
        )?;
        let index = checked!(table.define_buffer(checked!(BufferDesc::new(
            12,
            BufferUsage::INDEX | BufferUsage::COPY_DST
        ))));
        let data: Vec<_> = [5u32, 6, 7]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let mut encoder = CommandEncoder::new(&table);
        checked!(encoder.write_buffer(index, 0, &data));
        let mut pass = checked!(encoder.begin_render_pass(pass_desc(&table, target, 12)?));
        checked!(pass.set_programmable_pipeline(p));
        checked!(pass.set_index_buffer(index, 0, IndexFormat::Uint32));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(0, 0, 4, SIZE)))));
        checked!(pass.set_draw_index(13));
        checked!(pass.draw_instanced(3, 7, 2, 11));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(4, 0, 4, SIZE)))));
        checked!(pass.set_draw_index(19));
        checked!(pass.draw_indexed_instanced(3, 0, -5, 2, 17));
        checked!(pass.set_scissor(Some(checked!(PixelRect::new(8, 0, 4, SIZE)))));
        checked!(pass.set_draw_index(5));
        checked!(pass.draw_instanced(3, 2, 1, 3));
        checked!(pass.end());
        let commands = checked!(encoder.finish());
        let Some(driver) = execute(path, &table, p, &commands)? else {
            continue;
        };
        check_pixels(&driver, target, 12, "draw-parameters", |x, _| {
            [[13, 135, 11, 255], [19, 123, 17, 255], [5, 130, 3, 255]][x as usize / 4]
        })?;
        println!(
            "PASS programmable-feature draw-parameters variant={variant} pixels=48 signed-base=-5 sequential-draw-updates=true"
        );
    }
    Ok(())
}
/// Parent connects this to the installed-driver extended functional suite.
pub(super) fn run(path: &str) -> Result<(), String> {
    readonly_storage(path)?;
    push128(path)?;
    vertex_formats(path)?;
    divisors(path)?;
    draw_parameters(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_graphics_feature_oracles_encode_public_api_and_abi_batches() {
        run("__encode_only__").unwrap();
    }
    #[test]
    fn format_inputs_cover_all_twenty_portable_formats_and_signed_boundaries() {
        let cases = format_cases();
        assert_eq!(cases.len(), 20);
        for (i, c) in cases.iter().enumerate() {
            assert_eq!(c.bytes.len(), c.format.byte_size() as usize);
            assert!(!cases[..i].iter().any(|v| v.format == c.format));
        }
        let packed = cases
            .iter()
            .find(|c| c.format == VertexFormat::Snorm10_10_10_2)
            .unwrap();
        assert_eq!(format_expected(packed), [64, 96, 159, 191]);
        let signed = cases
            .iter()
            .find(|c| c.format == VertexFormat::Sint16x4)
            .unwrap();
        assert_eq!(format_expected(signed), [111, 97, 191, 255]);
    }
    #[test]
    #[ignore = "requires SGFX_FEATURE_IR3_CLI and SGFX_FEATURE_IR3_OUTPUT"]
    fn real_compiler_accepts_all_generated_graphics_oracles() {
        let cli = std::env::var("SGFX_FEATURE_IR3_CLI").unwrap();
        let root = std::path::PathBuf::from(std::env::var("SGFX_FEATURE_IR3_OUTPUT").unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let (storage_vs, storage_fs) = storage_sources();
        let (push_vs, push_fs) = push_sources();
        let mut cases = vec![
            ("storage_vs".into(), "vertex", storage_vs),
            ("storage_fs".into(), "fragment", storage_fs),
            ("push128_vs".into(), "vertex", push_vs),
            ("push128_fs".into(), "fragment", push_fs),
            ("divisor".into(), "vertex", DIVISOR_VS.into()),
            (
                "flat".into(),
                "fragment",
                FLAT_FS.replace("fn fs(", "fn main("),
            ),
        ];
        cases.extend(
            format_cases()
                .iter()
                .map(|c| (format!("{:?}", c.format), "vertex", format_source(c))),
        );
        for (name, stage, source) in cases {
            let file = root.join(format!("{name}.wgsl"));
            std::fs::write(&file, source).unwrap();
            let output = std::process::Command::new(&cli)
                .args([stage, "main"])
                .arg(&file)
                .arg(root.join(&name))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
