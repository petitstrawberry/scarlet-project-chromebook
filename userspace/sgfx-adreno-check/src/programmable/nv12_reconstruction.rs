// SPDX-License-Identifier: GPL-2.0-only
//! Decoder-backed NV12 logical load/gather and sampler-state pixel oracle.
//! Contract A is an explicit SGFX implementation decision, not a quoted Vulkan
//! requirement: address logical luma texels, reconstruct nearest chroma with
//! producer-bound clamping, convert each color, then select gather components.
use super::*;
const WIDTH: u32 = 128;
const HEIGHT: u32 = 24;
const SOURCE_SIDE: i32 = 128;
const REFERENCE: &[u8] = include_bytes!("nv12-reconstruction-fixtures/reference.nv12");
const SOURCE: &str = include_str!("nv12-reconstruction-fixtures/shader.wgsl");
const AXIS_X: [i32; 16] = [
    -129, -2, -1, 0, 1, 2, 3, 31, 32, 63, 64, 125, 126, 127, 128, 257,
];
const AXIS_Y: [i32; 8] = [-129, -1, 0, 1, 63, 126, 127, 128];
const MODES: [AddressMode; 4] = [
    AddressMode::ClampToEdge,
    AddressMode::Repeat,
    AddressMode::MirrorRepeat,
    AddressMode::ClampToBorder,
];
fn probe(column: usize) -> [i32; 2] {
    [AXIS_X[column % 16], AXIS_Y[column / 16]]
}
fn address(q: i32, size: i32, mode: AddressMode) -> Option<usize> {
    Some(match mode {
        AddressMode::ClampToEdge => q.clamp(0, size - 1),
        AddressMode::Repeat => q.rem_euclid(size),
        AddressMode::MirrorRepeat => {
            let p = q.rem_euclid(2 * size);
            if p < size { p } else { 2 * size - 1 - p }
        }
        AddressMode::ClampToBorder => {
            if !(0..size).contains(&q) {
                return None;
            }
            q
        }
    } as usize)
}
fn conversions() -> Vec<YcbcrConversion> {
    let mut out = Vec::new();
    for matrix in [YcbcrMatrix::Bt601, YcbcrMatrix::Bt709] {
        for range in [YcbcrRange::Limited, YcbcrRange::Full] {
            for chroma_x in [ChromaLocation::Cosited, ChromaLocation::Midpoint] {
                for chroma_y in [ChromaLocation::Cosited, ChromaLocation::Midpoint] {
                    out.push(YcbcrConversion {
                        matrix,
                        range,
                        chroma_x,
                        chroma_y,
                    });
                }
            }
        }
    }
    out
}
fn rgb(c: YcbcrConversion, y: u8, cb: u8, cr: u8) -> [f32; 4] {
    let (kr, kb) = match c.matrix {
        YcbcrMatrix::Bt601 => (0.299f32, 0.114f32),
        YcbcrMatrix::Bt709 => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let (y, cb, cr) = match c.range {
        YcbcrRange::Limited => (
            (f32::from(y) - 16.0) / 219.0,
            (f32::from(cb) - 128.0) / 224.0,
            (f32::from(cr) - 128.0) / 224.0,
        ),
        YcbcrRange::Full => (
            f32::from(y) / 255.0,
            (f32::from(cb) - 128.0) / 255.0,
            (f32::from(cr) - 128.0) / 255.0,
        ),
    };
    [
        (y + 2.0 * (1.0 - kr) * cr).clamp(0.0, 1.0),
        (y - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr).clamp(0.0, 1.0),
        (y + 2.0 * (1.0 - kb) * cb).clamp(0.0, 1.0),
        1.0,
    ]
}
fn cosited(location: ChromaLocation) -> usize {
    usize::from(location == ChromaLocation::Cosited)
}
fn color_texel(c: YcbcrConversion, x: usize, y: usize) -> [f32; 4] {
    // Mesa implicit_downsampled_coords adds 1/(2*luma_dimension) for
    // cosited chroma. At a luma texel center this is floor((q+1)/2).
    // Contract A chooses positive-tie advance and clamps producer bounds.
    let cx = ((x + cosited(c.chroma_x)) / 2).min(63);
    let cy = ((y + cosited(c.chroma_y)) / 2).min(63);
    let uv = 128 * 128 + cy * 128 + cx * 2;
    rgb(c, REFERENCE[y * 128 + x], REFERENCE[uv], REFERENCE[uv + 1])
}
fn load(c: YcbcrConversion, q: [i32; 2], mip: i32) -> [f32; 4] {
    // Guard before touching either reference plane. Raw zero YCbCr is not
    // transparent RGB zero, so masking only the raw fetch would be wrong.
    if mip != 0 || q.iter().any(|v| !(0..SOURCE_SIDE).contains(v)) {
        return [0.0; 4];
    }
    color_texel(c, q[0] as usize, q[1] as usize)
}
fn logical_sample(c: YcbcrConversion, q: [i32; 2], mode: AddressMode) -> [f32; 4] {
    match (address(q[0], 128, mode), address(q[1], 128, mode)) {
        (Some(x), Some(y)) => color_texel(c, x, y),
        _ => [0.0; 4],
    }
}
// Optional model B: independently address each plane at the original normalized
// coordinate. Partial plane borders have unspecified converted-color/alpha
// semantics here and deliberately return None rather than guessed constants.
fn independent_plane_sample(
    c: YcbcrConversion,
    q: [i32; 2],
    mode: AddressMode,
) -> Option<[f32; 4]> {
    let x = address(q[0], 128, mode)?;
    let y = address(q[1], 128, mode)?;
    let cx = address((q[0] + cosited(c.chroma_x) as i32).div_euclid(2), 64, mode)?;
    let cy = address((q[1] + cosited(c.chroma_y) as i32).div_euclid(2), 64, mode)?;
    let uv = 128 * 128 + cy * 128 + cx * 2;
    Some(rgb(
        c,
        REFERENCE[y * 128 + x],
        REFERENCE[uv],
        REFERENCE[uv + 1],
    ))
}
fn regular_texel(x: usize, y: usize) -> [f32; 4] {
    [
        13 + (17 * x + 29 * y) % 211,
        31 + (7 * x + 19 * y) % 197,
        47 + (23 * x + 3 * y) % 181,
        127 + (x ^ y) % 129,
    ]
    .map(|v| v as f32 / 255.0)
}
fn regular_pixels() -> Vec<u8> {
    (0..128)
        .flat_map(|y| (0..128).flat_map(move |x| bgra(regular_texel(x, y))))
        .collect()
}
fn regular_sample(q: [i32; 2], mode: AddressMode) -> [f32; 4] {
    match (address(q[0], 128, mode), address(q[1], 128, mode)) {
        (Some(x), Some(y)) => regular_texel(x, y),
        _ => [0.0; 4],
    }
}
fn bgra(rgba: [f32; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], rgba[3]].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}
#[derive(Clone, Copy, Debug)]
enum Operation {
    Load([i32; 2], i32),
    Nearest(bool),
    Gather(usize, [i32; 2], bool),
    Query,
}
fn operation(row: usize) -> Operation {
    match row {
        0 => Operation::Load([0, 0], 0),
        1 => Operation::Load([0, 0], 1),
        2 => Operation::Load([0, 0], -1),
        3 => Operation::Nearest(false),
        4 => Operation::Nearest(true),
        5..=8 => Operation::Gather(row - 5, [0, 0], false),
        9..=12 => Operation::Gather(row - 9, [1, -1], true),
        13..=16 => Operation::Gather(row - 13, [-2, 1], false),
        17..=20 => Operation::Gather(row - 17, [-8, 7], true),
        21 => Operation::Load([1, -1], 0),
        22 => Operation::Load([-1, 1], 0),
        _ => Operation::Query,
    }
}
fn restored_regular(column: usize, row: usize) -> bool {
    (71..73).contains(&column) && (3..21).contains(&row)
}
fn expected(c: YcbcrConversion, mode_index: usize, column: usize, row: usize) -> [f32; 4] {
    let q = probe(column);
    let regular = restored_regular(column, row);
    let sample = |q, mode| {
        if regular {
            regular_sample(q, mode)
        } else {
            logical_sample(c, q, mode)
        }
    };
    match operation(row) {
        Operation::Load(d, mip) => load(c, [q[0] + d[0], q[1] + d[1]], mip),
        Operation::Nearest(second) => sample(q, MODES[(mode_index + usize::from(second)) % 4]),
        Operation::Gather(component, d, second) => {
            let mode = MODES[(mode_index + usize::from(second)) % 4];
            // uv=(q+.75)/128: floor(uv*128-.5)==q, including negatives.
            // Offset applies BEFORE addressing. WGSL order: (0,1),(1,1),(1,0),(0,0).
            [[0, 1], [1, 1], [1, 0], [0, 0]]
                .map(|p| sample([q[0] + d[0] + p[0], q[1] + d[1] + p[1]], mode)[component])
        }
        Operation::Query => [1.0; 4],
    }
}
fn match_pixel(got: &[u8], wanted: [u8; 4]) -> bool {
    got.iter().zip(wanted).all(|(a, b)| a.abs_diff(b) <= 2)
}
#[derive(Default, Debug)]
struct ControlAudit {
    agree: usize,
    edge_difference: usize,
    undefined_plane_border: usize,
    matches_a: usize,
    matches_b: usize,
    neither: usize,
    examples: Vec<String>,
}
fn border_only()->bool {std::env::args().any(|a|a=="--nv12-border-only")}
fn slots_only()->bool {std::env::args().any(|a|a=="--nv12-slots-only")}
fn sampler_count()->u32 {if slots_only() {16}else{4}}
fn extra_pixels(c:YcbcrConversion,filter:usize,pixels:&[Vec<u8>;3])->Result<(),String> {
    for row in 0..HEIGHT as usize {for column in 0..WIDTH as usize {
        let sample=|q|if restored_regular(column,row) {regular_sample(q,AddressMode::ClampToBorder)}
            else {logical_sample(c,q,AddressMode::ClampToBorder)};
        let mut q=probe(column);
        if row%6==4 {q=[q[0]+1,q[1]-1]}
        let wanted=if slots_only() {
            if restored_regular(column,row) {regular_texel(0,0)}else{color_texel(c,0,0)}
        } else if filter==0 {sample(q)}else{
            let a=sample(q);let b=sample([q[0]+1,q[1]]);let c=sample([q[0],q[1]+1]);let d=sample([q[0]+1,q[1]+1]);
            core::array::from_fn(|i|a[i]*0.5625+b[i]*0.1875+c[i]*0.1875+d[i]*0.0625)
        };
        let offset=(row*WIDTH as usize+column)*4;let got=&pixels[0][offset..offset+4];
        if !match_pixel(got,bgra(wanted)) {return Err(format!("extra NV12 oracle slots={} filter={filter} row={row} col={column} got={got:?} expected={:?}",slots_only(),bgra(wanted)))}
    }}
    Ok(())
}
fn check_pixels(
    c: YcbcrConversion,
    mode: usize,
    pixels: &[Vec<u8>; 3],
) -> Result<ControlAudit, String> {
    let mut audit = ControlAudit::default();
    if border_only() || slots_only() {extra_pixels(c,mode,pixels)?;return Ok(audit)}
    for row in 0..HEIGHT as usize {
        for column in 0..WIDTH as usize {
            let offset = (row * WIDTH as usize + column) * 4;
            let got = &pixels[0][offset..offset + 4];
            let want_a = bgra(expected(c, mode, column, row));
            if let Operation::Nearest(second) = operation(row) {
                if restored_regular(column, row) {
                    if !match_pixel(got, want_a) {
                        return Err(format!(
                            "ordinary texture restoration row={row} col={column} got={got:?} want={want_a:?}"
                        ));
                    }
                    continue;
                }
                let control = &pixels[1 + usize::from(second)][column * 4..column * 4 + 4];
                if !got.iter().zip(control).all(|(a, b)| a.abs_diff(*b) <= 2) {
                    return Err(format!(
                        "WGSL/SPIRV nearest vs canonical control row={row} col={column} programmable={got:?} fixed={control:?}"
                    ));
                }
                let want_b = independent_plane_sample(
                    c,
                    probe(column),
                    MODES[(mode + usize::from(second)) % 4],
                )
                .map(bgra);
                if want_b == Some(want_a) {
                    audit.agree += 1;
                    if !match_pixel(got, want_a) {
                        return Err(format!(
                            "nearest agreed A/B row={row} col={column} q={:?} got={got:?} expected={want_a:?}",
                            probe(column)
                        ));
                    }
                } else {
                    if want_b.is_some() {
                        audit.edge_difference += 1;
                    } else {
                        audit.undefined_plane_border += 1;
                    }
                    let ma = match_pixel(got, want_a);
                    let mb = want_b.is_some_and(|b| match_pixel(got, b));
                    audit.matches_a += usize::from(ma);
                    audit.matches_b += usize::from(mb);
                    audit.neither += usize::from(!ma && !mb);
                    if audit.examples.len() < 4 {
                        audit.examples.push(format!(
                            "col={column},q={:?},actual={got:?},A={want_a:?},B={want_b:?}",
                            probe(column)
                        ));
                    }
                }
            } else if !match_pixel(got, want_a) {
                return Err(format!(
                    "logical reconstruction row={row} col={column} q={:?} op={:?} mode={:?} got={got:?} expected-A={want_a:?}",
                    probe(column),
                    operation(row),
                    MODES[mode]
                ));
            }
        }
    }
    Ok(audit)
}
struct Groups {
    nv12: BindGroupId,
    regular: BindGroupId,
    sampler: Option<BindGroupId>,
}
struct Scene {
    source: TextureId,
    regular: TextureId,
    targets: [TextureId; 3],
    pipeline: ProgrammableRenderPipelineId,
    canonical: RenderPipelineId,
    vertex: BufferId,
    samplers: Vec<[SamplerId; 2]>,
    groups: Vec<Groups>,
    spirv: bool,
}
fn layout_texture() -> Result<BindGroupLayoutDesc, String> {
    Ok(checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(0, ShaderStages::FRAGMENT, BindingType::SampledTexture)
    ])))
}
fn layout_sampler() -> Result<BindGroupLayoutDesc, String> {
    Ok(checked!(BindGroupLayoutDesc::new(
        (0..sampler_count())
            .map(|i| BindGroupLayoutEntry::new(i, ShaderStages::FRAGMENT, BindingType::Sampler))
            .collect()
    )))
}
fn layout_combined() -> Result<BindGroupLayoutDesc, String> {
    let mut entries = vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::FRAGMENT,
        BindingType::SampledTexture,
    )];
    entries.extend(
        (1..=sampler_count()).map(|i| BindGroupLayoutEntry::new(i, ShaderStages::FRAGMENT, BindingType::Sampler)),
    );
    Ok(checked!(BindGroupLayoutDesc::new(entries)))
}
fn scene(table: &ResourceTable, spirv: bool) -> Result<Scene, String> {
    table.enable_abi_commands();
    let source = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Nv12,
        checked!(Extent2D::new(128, 128)),
        TextureUsage::SAMPLED
    ))))
    .id();
    let regular = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(128, 128)),
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ))))
    .id();
    let mut targets = Vec::new();
    for height in [HEIGHT, 1, 1] {
        targets.push(
            checked!(table.define_texture(checked!(TextureDesc::new(
                TextureFormat::Bgra8Unorm,
                checked!(Extent2D::new(WIDTH, height)),
                TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
            ))))
            .id(),
        );
    }
    let modules = if spirv {
        let mut result = Vec::new();
        for bytes in [
            include_bytes!("nv12-reconstruction-fixtures/vs.spv").as_slice(),
            if border_only() {include_bytes!("nv12-reconstruction-fixtures/fs_border.spv").as_slice()}
            else if slots_only() {include_bytes!("nv12-reconstruction-fixtures/fs_slots.spv").as_slice()}
            else {include_bytes!("nv12-reconstruction-fixtures/fs.spv").as_slice()},
        ] {
            let words = bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            result.push(checked!(
                table.define_shader_module(checked!(ShaderModuleDesc::spirv(words)))
            ));
        }
        [result[0], result[1]]
    } else {
        let m =
            checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(SOURCE.into()))));
        [m, m]
    };
    let layout = checked!(PipelineLayoutDesc::new(if spirv {
        vec![
            checked!(BindGroupLayoutDesc::new(vec![])),
            checked!(BindGroupLayoutDesc::new(vec![])),
            layout_texture()?,
            layout_sampler()?,
        ]
    } else {
        vec![layout_combined()?]
    }));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                modules[0],
                ShaderStage::Vertex,
                "vs".into()
            )),
            checked!(ShaderEntryPoint::new(
                modules[1],
                ShaderStage::Fragment,
                (if border_only() {"fs_border"}else if slots_only() {"fs_slots"}else{"fs"}).into()
            )),
            layout,
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )
    )))
    .id();
    let canonical = checked!(
        table.define_render_pipeline(checked!(RenderPipelineDesc::new(
            TextureFormat::Bgra8Unorm,
            PrimitiveTopology::TriangleList,
            checked!(VertexBufferLayout::new(
                24,
                vec![
                    VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                    VertexAttribute::new(1, VertexFormat::Float32x2, 16)
                ]
            )),
            FragmentProgram::Texture(TextureSampleMode::Rgba),
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise)
        )))
    )
    .id();
    let vertex = checked!(table.define_buffer(checked!(BufferDesc::new(
        u64::from(WIDTH) * 6 * 24,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))))
    .id();
    let mut samplers = Vec::new();
    for mode in MODES {
        samplers.push([
            checked!(table.define_sampler(SamplerDesc::new(
                FilterMode::Nearest,
                FilterMode::Nearest,
                mode,
                mode
            )))
            .id(),
            checked!(table.define_sampler(SamplerDesc::new(
                FilterMode::Linear,
                FilterMode::Linear,
                mode,
                mode
            )))
            .id(),
        ]);
    }
    let mut groups = Vec::new();
    for mode in 0..4 {
        for filter in 0..2 {
            let selected = [
                samplers[mode][filter],
                samplers[(mode + 1) % 4][filter],
                samplers[mode][0],
                samplers[(mode + 1) % 4][0],
            ];
            let selected:Vec<_>=if slots_only() {(0..16).map(|i|selected[2+i%2]).collect()}else{selected.to_vec()};
            let mut texture_groups = Vec::new();
            for texture in [source, regular] {
                let mut entries = vec![BindGroupEntry::new(0, BindingResource::Texture(texture))];
                if !spirv {
                    entries.extend(selected.iter().enumerate().map(|(i, id)| {
                        BindGroupEntry::new(i as u32 + 1, BindingResource::Sampler(*id))
                    }));
                }
                texture_groups.push(
                    checked!(table.define_bind_group(checked!(BindGroupDesc::new(
                        table,
                        if spirv {
                            layout_texture()?
                        } else {
                            layout_combined()?
                        },
                        entries
                    ))))
                    .id(),
                );
            }
            let sampler = if spirv {
                Some(
                    checked!(table.define_bind_group(checked!(BindGroupDesc::new(
                            table,
                            layout_sampler()?,
                            selected
                                .iter()
                                .enumerate()
                                .map(|(i, id)| BindGroupEntry::new(
                                    i as u32,
                                    BindingResource::Sampler(*id)
                                ))
                                .collect()
                        ))))
                    .id(),
                )
            } else {
                None
            };
            groups.push(Groups {
                nv12: texture_groups[0],
                regular: texture_groups[1],
                sampler,
            });
        }
    }
    Ok(Scene {
        source,
        regular,
        targets: targets.try_into().unwrap(),
        pipeline,
        canonical,
        vertex,
        samplers,
        groups,
        spirv,
    })
}
fn control_vertices() -> Vec<u8> {
    let mut data = Vec::new();
    for column in 0..WIDTH as usize {
        let q = probe(column);
        let uv = [(q[0] as f32 + 0.75) / 128.0, (q[1] as f32 + 0.75) / 128.0];
        let x0 = column as f32 / WIDTH as f32 * 2.0 - 1.0;
        let x1 = (column + 1) as f32 / WIDTH as f32 * 2.0 - 1.0;
        for [x, y] in [
            [x0, -1.0],
            [x1, -1.0],
            [x0, 1.0],
            [x0, 1.0],
            [x1, -1.0],
            [x1, 1.0],
        ] {
            for value in [x, y, 0.0, 1.0, uv[0], uv[1]] {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    data
}
fn record<'r, 'd>(
    table: &'r ResourceTable,
    s: &Scene,
    mode: usize,
    filter: usize,
    vertices: &'d [u8],
    regular: &'d [u8],
) -> Result<CommandBuffer<'r, 'd>, String> {
    let mut encoder = CommandEncoder::new(table);
    checked!(encoder.write_buffer(checked!(table.buffer_ref(s.vertex)), 0, vertices));
    checked!(encoder.write_texture(
        checked!(table.texture_ref(s.regular)),
        checked!(TextureWrite::new(
            checked!(PixelRect::new(0, 0, 128, 128)),
            512,
            regular
        ))
    ));
    for n in 0..2 {
        let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
            table,
            checked!(table.texture_ref(s.targets[n + 1])),
            checked!(PixelRect::new(0, 0, WIDTH, 1)),
            LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
            StoreOp::Store
        ))));
        checked!(pass.set_pipeline(checked!(table.render_pipeline_ref(s.canonical))));
        checked!(pass.set_vertex_buffer(checked!(table.buffer_ref(s.vertex)), 0));
        checked!(pass.set_uniforms(DrawUniforms::new(
            Transform::identity(),
            checked!(Color::rgba(1.0, 1.0, 1.0, 1.0))
        )));
        checked!(pass.set_texture(checked!(table.texture_ref(s.source))));
        checked!(pass.set_sampler(checked!(table.sampler_ref(s.samplers[(mode + n) % 4][0]))));
        checked!(pass.draw(WIDTH * 6, 0));
        checked!(pass.end());
    }
    let groups = &s.groups[mode * 2 + filter];
    let group_index = if s.spirv { 2 } else { 0 };
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        table,
        checked!(table.texture_ref(s.targets[0])),
        checked!(PixelRect::new(0, 0, WIDTH, HEIGHT)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(
        pass.set_programmable_pipeline(checked!(
            table.programmable_render_pipeline_ref(s.pipeline)
        ))
    );
    checked!(pass.set_bind_group(group_index, checked!(table.bind_group_ref(groups.nv12))));
    if let Some(id) = groups.sampler {
        checked!(pass.set_bind_group(3, checked!(table.bind_group_ref(id))));
    }
    checked!(pass.draw(3, 0));
    // Same pipeline/pass: NV12 -> ordinary BGRA -> NV12, leaving SPIR-V's
    // separately bound sampler group intact throughout both texture changes.
    checked!(pass.set_bind_group(group_index, checked!(table.bind_group_ref(groups.regular))));
    checked!(pass.set_scissor(Some(checked!(PixelRect::new(71, 3, 2, 18)))));
    checked!(pass.draw(3, 0));
    checked!(pass.set_bind_group(group_index, checked!(table.bind_group_ref(groups.nv12))));
    checked!(pass.set_scissor(Some(checked!(PixelRect::new(73, 3, 1, 18)))));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    Ok(checked!(encoder.finish()))
}
fn checksum(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325u64, |a, b| {
        (a ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[cfg(target_os = "scarlet")]
pub fn run(path: &str) -> Result<(), String> {
    use scarlet_video_client::{DecodedOutput, DecoderOptions, ScarletVideoDecoder, VideoFormat};
    println!(
        "BEGIN nv12-reconstruction contract=A-logical-texels nearest-B=diagnostic-only languages=WGSL+SPIRV producer=public-H264-shared-image"
    );
    let mut decoder =
        ScarletVideoDecoder::open_with_options(DecoderOptions::new().with_shared_images(true))
            .map_err(|e| format!("NV12 reconstruction producer: {e}"))?;
    let image = match decoder
        .decode_output(
            VideoFormat::H264,
            include_bytes!("nv12-reconstruction-fixtures/frame.h264"),
            1,
        )?
        .ok_or("NV12 producer returned no frame")?
    {
        DecodedOutput::Image(image) => image,
        DecodedOutput::Nv12(_) => return Err("NV12 shared-image producer required".into()),
    };
    let descriptor = image.descriptor();
    if descriptor.width != 128
        || descriptor.height != 128
        || descriptor.format != u32::from_le_bytes(*b"NV12")
    {
        return Err(format!("unexpected producer descriptor {descriptor:?}"));
    }
    let handle = image.into_handle();
    drop(decoder);
    let dump = std::env::args().find_map(|a| {
        a.strip_prefix("--nv12-reconstruction-dump=")
            .map(String::from)
    });
    if let Some(dir) = &dump {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut submitted = 0usize;
    let mut consensus = 0usize;
    let mut edge_audits = 0usize;
    for spirv in [false, true] {
        let table = ResourceTable::new();
        let s = scene(&table, spirv)?;
        let vertices = control_vertices();
        let regular = regular_pixels();
        let mut commands = Vec::new();
        for mode in 0..4 {
            commands.push([
                record(&table, &s, mode, 0, &vertices, &regular)?,
                record(&table, &s, mode, 1, &vertices, &regular)?,
            ]);
        }
        let slots = s
            .targets
            .map(|id| table.texture_ref(id).unwrap().slot() as u32);
        let source_slot = table.texture_ref(s.source).unwrap().slot() as u32;
        let mut session = crate::abi_session::Session::new(path, &table, &slots)?;
        for sweep in 0..2 {
            for (index, c) in conversions().into_iter().enumerate() {
                for mode in 0..4 {
                    if (border_only() && mode!=3)||(slots_only() && mode!=0) {continue}
                    let owned = handle
                        .duplicate()
                        .map_err(|e| format!("duplicate producer: {e:?}"))?;
                    let conversion = abi::YcbcrConversion {
                        size: core::mem::size_of::<abi::YcbcrConversion>() as u32,
                        reserved: 0,
                        matrix: match c.matrix {
                            YcbcrMatrix::Bt601 => abi::YCBCR_MATRIX_BT601,
                            YcbcrMatrix::Bt709 => abi::YCBCR_MATRIX_BT709,
                        },
                        range: match c.range {
                            YcbcrRange::Limited => abi::YCBCR_RANGE_LIMITED,
                            YcbcrRange::Full => abi::YCBCR_RANGE_FULL,
                        },
                        chroma_x: match c.chroma_x {
                            ChromaLocation::Cosited => abi::YCBCR_CHROMA_COSITED,
                            ChromaLocation::Midpoint => abi::YCBCR_CHROMA_MIDPOINT,
                        },
                        chroma_y: match c.chroma_y {
                            ChromaLocation::Cosited => abi::YCBCR_CHROMA_COSITED,
                            ChromaLocation::Midpoint => abi::YCBCR_CHROMA_MIDPOINT,
                        },
                    };
                    session.import_ycbcr(source_slot, conversion, || {
                        let raw = owned.as_raw();
                        core::mem::forget(owned);
                        raw
                    })?;
                    let mut failure = None;
                    for filter in 0..2 {
                        // Each call waits for COMPLETE. Both accepted buffers retire
                        // before release_import and before the stable ID is imported again.
                        session.submit(&commands[mode][filter])?;
                        submitted += 1;
                        let mut pixels = [
                            vec![0xa5; (WIDTH * HEIGHT * 4) as usize],
                            vec![0xa5; (WIDTH * 4) as usize],
                            vec![0xa5; (WIDTH * 4) as usize],
                        ];
                        for n in 0..3 {
                            session.readback(
                                slots[n],
                                &mut pixels[n],
                                WIDTH * 4,
                                checked!(PixelRect::new(
                                    0,
                                    0,
                                    WIDTH,
                                    if n == 0 { HEIGHT } else { 1 }
                                )),
                            )?;
                        }
                        if let Some(dir) = &dump {
                            for (n, data) in pixels.iter().enumerate() {
                                std::fs::write(format!("{dir}/lang{spirv}-sweep{sweep}-c{index}-mode{mode}-filter{filter}-target{n}.bgra"),data).map_err(|e|e.to_string())?;
                            }
                        }
                        match check_pixels(c, if border_only() || slots_only() {filter}else{mode}, &pixels) {
                            Ok(audit) => {
                                consensus += audit.agree;
                                edge_audits += audit.edge_difference + audit.undefined_plane_border;
                                println!(
                                    "PASS nv12-reconstruction spirv={spirv} sweep={sweep} conversion={index} modes={:?}/{:?} gather-filter={filter} pixels=3072 canonical-controls=256 hash={:016x}/{:016x}/{:016x} nearest-audit={audit:?}",
                                    MODES[mode],
                                    MODES[(mode + 1) % 4],
                                    checksum(&pixels[0]),
                                    checksum(&pixels[1]),
                                    checksum(&pixels[2])
                                );
                            }
                            Err(error) => {
                                println!(
                                    "FAIL nv12-reconstruction spirv={spirv} sweep={sweep} conversion={index} mode={mode} filter={filter}: {error}"
                                );
                                if failure.is_none() {
                                    failure = Some(error);
                                }
                            }
                        }
                    }
                    session.release_import(source_slot)?;
                    if let Some(error) = failure {
                        return Err(error);
                    }
                }
            }
        }
    }
    println!(
        "PASS nv12-reconstruction extra-border={} extra-slots={} logical-load+gather languages=2 conversions=16 address-modes=4 sweeps=2 submissions={submitted} checked-pixels={} nearest-consensus={consensus} sampling-edge-audits={edge_audits} nearest-edge-consistency=DIAGNOSTIC-ONLY source-reimport=public-release-after-retirement",
        border_only(),slots_only(),submitted * 3328
    );
    Ok(())
}
#[cfg(not(target_os = "scarlet"))]
pub fn run(_path: &str) -> Result<(), String> {
    Err("NV12 reconstruction GPU oracle requires Scarlet public H264 shared-image producer".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guard_precedes_color_conversion_and_covers_signed_mips() {
        for c in conversions() {
            assert_ne!(rgb(c, 0, 0, 0), [0.0; 4]);
            for q in [[-1, 0], [0, -1], [128, 0], [0, 128], [-129, 257]] {
                assert_eq!(load(c, q, 0), [0.0; 4]);
            }
            for mip in [-1, 1, 17] {
                assert_eq!(load(c, [31, 63], mip), [0.0; 4]);
            }
            assert_eq!(load(c, [31, 63], 0)[3], 1.0);
        }
    }
    #[test]
    fn addressing_keeps_negative_repeat_mirror_and_transparent_border_distinct() {
        for (mode, negative, positive) in [
            (AddressMode::ClampToEdge, Some(0), Some(127)),
            (AddressMode::Repeat, Some(127), Some(0)),
            (AddressMode::MirrorRepeat, Some(0), Some(127)),
            (AddressMode::ClampToBorder, None, None),
        ] {
            assert_eq!(address(-1, 128, mode), negative);
            assert_eq!(address(128, 128, mode), positive);
        }
        assert_eq!(address(-129, 128, AddressMode::Repeat), Some(127));
        assert_eq!(address(-129, 128, AddressMode::MirrorRepeat), Some(127));
        assert_eq!(address(257, 128, AddressMode::Repeat), Some(1));
        assert_eq!(address(257, 128, AddressMode::MirrorRepeat), Some(1));
        for c in conversions() {
            assert_eq!(
                logical_sample(c, [-1, 0], AddressMode::ClampToBorder),
                [0.0; 4]
            );
        }
    }
    #[test]
    fn fixture_is_decoded_nonconstant_at_every_relevant_neighborhood() {
        assert_eq!(REFERENCE.len(), 128 * 128 * 3 / 2);
        for y in 0..127 {
            for x in 0..127 {
                let samples = [
                    REFERENCE[y * 128 + x],
                    REFERENCE[y * 128 + x + 1],
                    REFERENCE[(y + 1) * 128 + x],
                    REFERENCE[(y + 1) * 128 + x + 1],
                ];
                for i in 0..4 {
                    for j in i + 1..4 {
                        assert_ne!(samples[i], samples[j]);
                    }
                }
            }
        }
        for y in 0..63 {
            for x in 0..63 {
                let samples = [[x, y], [x + 1, y], [x, y + 1], [x + 1, y + 1]].map(|[x, y]| {
                    &REFERENCE[128 * 128 + y * 128 + x * 2..128 * 128 + y * 128 + x * 2 + 2]
                });
                for i in 0..4 {
                    for j in i + 1..4 {
                        assert_ne!(samples[i], samples[j]);
                    }
                }
            }
        }
    }
    #[test]
    fn siting_uses_nearest_positive_ties_and_clamps_chroma_producer_bounds() {
        let mut c = conversions()[0];
        c.chroma_x = ChromaLocation::Cosited;
        c.chroma_y = ChromaLocation::Cosited;
        let uv = 128 * 128 + 64 * 2 + 2;
        assert_eq!(
            color_texel(c, 1, 1),
            rgb(c, REFERENCE[129], REFERENCE[uv], REFERENCE[uv + 1])
        );
        c.chroma_x = ChromaLocation::Midpoint;
        c.chroma_y = ChromaLocation::Midpoint;
        assert_eq!(
            color_texel(c, 1, 1),
            rgb(
                c,
                REFERENCE[129],
                REFERENCE[128 * 128],
                REFERENCE[128 * 128 + 1]
            )
        );
        c.chroma_x = ChromaLocation::Cosited;
        c.chroma_y = ChromaLocation::Cosited;
        let uv = 128 * 128 + 63 * 128 + 126;
        assert_eq!(
            color_texel(c, 127, 127),
            rgb(
                c,
                REFERENCE[128 * 128 - 1],
                REFERENCE[uv],
                REFERENCE[uv + 1]
            )
        );
    }
    #[test]
    fn gather_is_four_converted_texels_in_wgsl_order_and_offset_precedes_wrap() {
        // Probe63,63 crosses two independent chroma boundaries in a gather.
        // Compare explicit logical neighbors, never a vec4 YCbCr dot product.
        let column = 9 + 4 * 16;
        assert_eq!(probe(column), [63, 63]);
        for c in conversions() {
            for component in 0..4 {
                let wanted =
                    [[63, 64], [64, 64], [64, 63], [63, 63]].map(|q| load(c, q, 0)[component]);
                assert_eq!(expected(c, 0, column, 5 + component), wanted);
                let wanted =
                    [[64, 63], [65, 63], [65, 62], [64, 62]].map(|q| load(c, q, 0)[component]);
                assert_eq!(expected(c, 0, column, 9 + component), wanted);
            }
        }
        for c in conversions() {
            let q = probe(2 + 2 * 16);
            assert_eq!(q, [-1, 0]);
            let actual = expected(c, 2, 2 + 2 * 16, 9 + 3); // B is ClampToBorder, offset(1,-1).
            assert_eq!(actual, [1.0, 1.0, 0.0, 0.0]);
        }
    }
    #[test]
    fn agreement_controls_do_not_hide_the_a_b_edge_decision() {
        let c = conversions()[0];
        assert_eq!(
            bgra(logical_sample(c, [31, 63], AddressMode::Repeat)),
            independent_plane_sample(c, [31, 63], AddressMode::Repeat)
                .map(bgra)
                .unwrap()
        );
        assert!(conversions().into_iter().any(|c| {
            bgra(logical_sample(c, [127, 127], AddressMode::Repeat))
                != independent_plane_sample(c, [127, 127], AddressMode::Repeat)
                    .map(bgra)
                    .unwrap()
        }));
        assert_eq!(
            independent_plane_sample(c, [-1, 0], AddressMode::ClampToBorder),
            None
        );
    }
    #[test]
    fn every_mapped_target_has_public_usage_and_all_recorded_variants_encode() {
        for spirv in [false, true] {
            let table = ResourceTable::new();
            let s = scene(&table, spirv).unwrap();
            for id in s.targets {
                assert!(
                    table
                        .texture(table.texture_ref(id).unwrap())
                        .unwrap()
                        .usage()
                        .contains(TextureUsage::PRESENT | TextureUsage::RENDER_ATTACHMENT)
                );
            }
            let vertices = control_vertices();
            let regular = regular_pixels();
            assert_eq!(vertices.len(), WIDTH as usize * 6 * 24);
            for mode in 0..4 {
                for filter in 0..2 {
                    let buffer = record(&table, &s, mode, filter, &vertices, &regular).unwrap();
                    assert!(buffer.abi_batch().unwrap().count > 0);
                }
            }
        }
    }
    #[test]
    fn cpu_checker_rejects_lane_permutation_and_raw_zero_conversion_without_hiding_b_edges() {
        let c = conversions()[0];
        let mode = 1;
        let mut pixels = [Vec::new(), Vec::new(), Vec::new()];
        for second in 0..2 {
            for column in 0..WIDTH as usize {
                // Synthetic CPU checker test data only; never uploaded or
                // represented as device readback. Exercise log-only B edges.
                let control =
                    independent_plane_sample(c, probe(column), MODES[(mode + second) % 4])
                        .unwrap_or([0.0; 4]);
                pixels[1 + second].extend_from_slice(&bgra(control));
            }
        }
        for row in 0..HEIGHT as usize {
            for column in 0..WIDTH as usize {
                let value = if let Operation::Nearest(second) = operation(row) {
                    if !restored_regular(column, row) {
                        pixels[1 + usize::from(second)][column * 4..column * 4 + 4]
                            .try_into()
                            .unwrap()
                    } else {
                        bgra(expected(c, mode, column, row))
                    }
                } else {
                    bgra(expected(c, mode, column, row))
                };
                pixels[0].extend_from_slice(&value);
            }
        }
        let audit = check_pixels(c, mode, &pixels).unwrap();
        assert!(audit.agree > 0 && audit.edge_difference > 0 && audit.matches_b > 0);
        let witness = (5..9)
            .flat_map(|r| (0..WIDTH as usize).map(move |x| (r, x)))
            .find(|(r, x)| {
                let i = (r * WIDTH as usize + x) * 4;
                !restored_regular(*x, *r) && pixels[0][i].abs_diff(pixels[0][i + 1]) > 4
            })
            .unwrap();
        let i = (witness.0 * WIDTH as usize + witness.1) * 4;
        pixels[0].swap(i, i + 1);
        assert!(
            check_pixels(c, mode, &pixels)
                .unwrap_err()
                .contains("logical reconstruction")
        );
        pixels[0].swap(i, i + 1);
        pixels[0][..4].copy_from_slice(&bgra(rgb(c, 0, 0, 0)));
        assert!(
            check_pixels(c, mode, &pixels)
                .unwrap_err()
                .contains("logical reconstruction")
        );
    }
}
