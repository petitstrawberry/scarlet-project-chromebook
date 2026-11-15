// SPDX-License-Identifier: GPL-2.0-only
//! Exercise arbitrary source through the installed DSO and check every result.
use sgfx_backend_abi as abi;
use sgfx_backend_loader::{LoadedBackend, Manifest};
use sgfx_core::ir::*;
use std::{result::Result, sync::Arc};

macro_rules! checked {
    ($e:expr) => {
        $e.map_err(|error| format!("{}: {error:?}", stringify!($e)))?
    };
}
mod transfers;
mod samples;
mod timestamps;
mod features;
mod gather;
mod private_memory;
mod storage_image;
mod sample_mask;
mod render_targets;
mod glass_profile;
mod client_vm;
mod primitive_index;
mod single_sample;
mod fixed_topology;
mod fixed_native;
mod nv12;
mod nv12_programmable;
mod nv12_reconstruction;
mod base_clamp;

struct Owned(abi::Object, unsafe extern "C" fn(abi::Object));
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) };
        }
    }
}
fn status(value: i32) -> Result<(), String> {
    if value == abi::OK {
        Ok(())
    } else {
        Err(format!("driver status={value}"))
    }
}
struct DriverSession {
    queue: Owned,
    resources: Owned,
    context: Owned,
    _device: Owned,
    api: abi::DriverApi,
    library: Arc<LoadedBackend>,
}
impl DriverSession {
    fn new(path: &str, table: &ResourceTable) -> Result<Self, String> {
        let library = checked!(LoadedBackend::load(&Manifest {
            name: "scarlet-adreno".into(),
            gpu_backend: "qcom-adreno".into(),
            library: path.into()
        }));
        let api = library.driver.ok_or("missing SGFX DriverApi")?;
        let base = &library.api;
        let mut device = Owned(core::ptr::null_mut(), base.drop_device);
        let mut context = Owned(core::ptr::null_mut(), base.drop_context);
        let mut resources = Owned(core::ptr::null_mut(), api.drop_resources);
        let mut queue = Owned(core::ptr::null_mut(), api.drop_queue);
        table.enable_abi_commands();
        let snapshot = checked!(table.abi_snapshot());
        let mut caps = 0;
        unsafe {
            status((base.open)(
                abi::Span::from_slice(b"/dev/gpu0"),
                &mut device.0,
                &mut caps,
            ))?;
            status((base.create_context)(device.0, &mut context.0))?;
            status((api.create_resources)(
                context.0,
                abi::Span::from_slice(&snapshot),
                &mut resources.0,
            ))?;
            status((api.create_queue)(context.0, &mut queue.0))?;
        }
        if [device.0, context.0, resources.0, queue.0]
            .iter()
            .any(|value| value.is_null())
        {
            return Err("driver returned null object".into());
        }
        Ok(Self {
            queue,
            resources,
            context,
            _device: device,
            api,
            library,
        })
    }
    fn validate(&self, kind: u32, slot: usize) -> Result<(), String> {
        unsafe { status((self.api.validate)(self.resources.0, kind, slot as u32)) }
    }
    fn submit(&self, commands: &CommandBuffer<'_, '_>) -> Result<(), String> {
        self.submit_checked(commands, false)
    }
    fn submit_unwaited(&self, commands: &CommandBuffer<'_, '_>) -> Result<Owned, String> {
        let encoded = commands.abi_batch().ok_or("missing ABI command batch")?;
        let batch = abi::Batch {
            table: encoded.table,
            count: encoded.count,
            words: abi::Span {
                data: encoded.words.data,
                len: encoded.words.len,
            },
        };
        let mut result = abi::SubmitResult::default();
        unsafe { (self.api.submit)(self.queue.0, self.resources.0, &batch, &mut result) };
        let receipt = Owned(result.receipt, self.library.api.drop_receipt);
        status(result.error)?;
        if result.disposition != abi::ACCEPTED || receipt.0.is_null() {
            return Err("expected accepted GPU receipt".into());
        }
        Ok(receipt)
    }
    fn submit_checked(
        &self,
        commands: &CommandBuffer<'_, '_>,
        retired_on_return: bool,
    ) -> Result<(), String> {
        let receipt = self.submit_unwaited(commands)?;
        let mut state = abi::PENDING;
        unsafe {
            status((self.library.api.wait)(
                receipt.0,
                if retired_on_return { 0 } else { 10_000_000_000 },
                &mut state,
            ))?;
        }
        if state != abi::COMPLETE {
            return Err(format!("native execution did not complete: {state}"));
        }
        Ok(())
    }
    fn read_texture(&self, slot: usize, output: &mut [u8]) -> Result<(), String> {
        unsafe {
            status((self.api.read_texture)(
                self.context.0,
                self.resources.0,
                slot as u32,
                output.as_mut_ptr(),
                output.len(),
            ))
        }
    }
    fn read_buffer(&self, slot: usize, output: &mut [u8]) -> Result<(), String> {
        unsafe {
            status((self.api.read_buffer)(
                self.resources.0,
                slot as u32,
                0,
                output.as_mut_ptr(),
                output.len(),
            ))
        }
    }
}

const VERTEX: &str = "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {\n let x = f32((i << 1u) & 2u); let y = f32(i & 2u); return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0); }";

fn graphics(path: &str, gradient: bool) -> Result<(), String> {
    println!("BEGIN programmable-graphics gradient={gradient}");
    let started = std::time::Instant::now();
    let table = ResourceTable::new();
    let fs = if gradient {
        "@fragment fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> { let ramp = fract(p.x / 8.0); return vec4<f32>(ramp, select(0.0, 1.0, p.y >= 16.0), 0.25, 1.0); }"
    } else {
        "@fragment fn fs() -> @location(0) vec4<f32> { return vec4<f32>(0.25, 0.5, 0.75, 1.0); }"
    };
    let vertex =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fragment =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(fs.into()))));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        checked!(Extent2D::new(32, 32)),
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
        ProgrammableRenderPipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                vertex,
                ShaderStage::Vertex,
                "vs".into()
            )),
            checked!(ShaderEntryPoint::new(
                fragment,
                ShaderStage::Fragment,
                "fs".into()
            )),
            checked!(PipelineLayoutDesc::new(vec![])),
            TextureFormat::Bgra8Unorm,
            None,
            PrimitiveTopology::TriangleList,
            BlendState::REPLACE,
            RasterState::new(CullMode::None, FrontFace::CounterClockwise),
        )
    )));
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_SHADER, vertex.slot())?;
    driver.validate(abi::VALIDATE_SHADER, fragment.slot())?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    println!(
        "COMPILED programmable-graphics gradient={gradient} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    if std::env::args().any(|arg| arg == "--compile-only") {
        return Ok(());
    }
    let mut encoder = CommandEncoder::new(&table);
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        checked!(PixelRect::new(0, 0, 32, 32)),
        LoadOp::Clear(checked!(Color::rgba(1.0, 0.0, 1.0, 1.0))),
        StoreOp::Store
    ))));
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.draw(3, 0));
    checked!(pass.end());
    driver.submit(&checked!(encoder.finish()))?;
    let mut pixels = vec![0xa5; 32 * 32 * 4];
    driver.read_texture(target.slot(), &mut pixels)?;
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        let x = index % 32;
        let y = index / 32;
        let expected = if gradient {
            [
                64,
                if y >= 16 { 255 } else { 0 },
                (((x % 8) as f32 + 0.5) / 8.0 * 255.0).round() as u8,
                255,
            ]
        } else {
            [191, 128, 64, 255]
        };
        if pixel
            .iter()
            .zip(expected)
            .any(|(got, expected)| got.abs_diff(expected) > 1)
        {
            return Err(format!(
                "programmable graphics gradient={gradient} pixel={x},{y}: got={pixel:?} expected={expected:?}"
            ));
        }
    }
    println!("PASS programmable-graphics gradient={gradient} pixels=1024");
    Ok(())
}

fn compute(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-compute");
    let started = std::time::Instant::now();
    let table = ResourceTable::new();
    let source = "@group(0) @binding(0) var<storage, read_write> data: array<u32>; @compute @workgroup_size(32) fn cs(@builtin(global_invocation_id) id: vec3<u32>) { data[id.x] = (id.x * 1664525u + 1013904223u) ^ 0xa5a55a5au; }";
    let shader =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(source.into()))));
    let buffer = checked!(table.define_buffer(checked!(BufferDesc::new(
        128 * 4,
        BufferUsage::STORAGE | BufferUsage::COPY_SRC | BufferUsage::COPY_DST
    ))));
    let layout = checked!(BindGroupLayoutDesc::new(vec![BindGroupLayoutEntry::new(
        0,
        ShaderStages::COMPUTE,
        BindingType::StorageBuffer { read_only: false }
    )]));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        layout.clone(),
        vec![BindGroupEntry::new(
            0,
            BindingResource::Buffer {
                buffer: buffer.id(),
                offset: 0,
                size: 128 * 4
            }
        )]
    ))));
    let pipeline = checked!(
        table.define_compute_pipeline(checked!(ComputePipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                shader,
                ShaderStage::Compute,
                "cs".into()
            )),
            checked!(PipelineLayoutDesc::new(vec![layout]))
        )))
    );
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_SHADER, shader.slot())?;
    driver.validate(abi::VALIDATE_COMPUTE_PIPELINE, pipeline.slot())?;
    println!(
        "COMPILED programmable-compute elapsed_ms={}",
        started.elapsed().as_millis()
    );
    let initial = [0xcc; 128 * 4];
    if std::env::args().any(|arg| arg == "--compile-only") {
        return Ok(());
    }
    let mut encoder = CommandEncoder::new(&table);
    checked!(encoder.write_buffer(buffer, 0, &initial));
    let mut pass = checked!(encoder.begin_compute_pass());
    checked!(pass.set_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.dispatch(4, 1, 1));
    checked!(pass.end());
    driver.submit_checked(&checked!(encoder.finish()), true)?;
    let mut output = vec![0xa5; 128 * 4];
    driver.read_buffer(buffer.slot(), &mut output)?;
    for (index, bytes) in output.chunks_exact(4).enumerate() {
        let got = u32::from_le_bytes(bytes.try_into().unwrap());
        let expected = (index as u32)
            .wrapping_mul(1664525)
            .wrapping_add(1013904223)
            ^ 0xa5a55a5a;
        if got != expected {
            return Err(format!(
                "compute word={index} got={got:#x} expected={expected:#x}"
            ));
        }
    }
    println!(
        "PASS programmable-compute workgroups=4 invocations=128 words=128 retired-on-submit-return=true"
    );
    Ok(())
}

pub fn run(path: &str) -> Result<(), String> {
    if std::env::args().any(|arg|["--nv12-reconstruction-only","--nv12-border-only","--nv12-slots-only"].contains(&arg.as_str())) {return nv12_reconstruction::run(path);}
    if std::env::args().any(|arg|arg=="--base-clamp-only") {return base_clamp::run(path);}
    if std::env::args().any(|arg|arg=="--nv12-programmable-only") {return nv12_programmable::run(path);}
    if std::env::args().any(|arg|arg=="--nv12-only") {return nv12::run(path);}
    if std::env::args().any(|arg|arg=="--fixed-native-only") {return fixed_native::run(path);}
    if std::env::args().any(|arg| arg == "--fixed-topology-only") { return fixed_topology::run(path); }
    if std::env::args().any(|arg| arg == "--single-sample-only") {
        return single_sample::run(path);
    }
    if std::env::args().any(|arg| arg == "--primitive-index-only") {
        return primitive_index::run(path);
    }
    if std::env::args().any(|arg| arg == "--client-vm-only") {
        return client_vm::run(path);
    }
    if std::env::args().any(|arg| arg == "--glass-profile-only") {
        return glass_profile::run(path);
    }
    if std::env::args().any(|arg| arg == "--storage-only") { return storage_image::run(path); }
    if std::env::args().any(|arg| arg == "--sample-mask-only") {
        return sample_mask::run(path);
    }
    if std::env::args().any(|arg| arg == "--render-targets-only") {
        return render_targets::run(path);
    }
    if std::env::args().any(|arg| arg == "--private-only") { return private_memory::run(path); }
    let graphics_after = std::env::args().find_map(|arg| arg.strip_prefix("--graphics-after=").map(str::to_owned));
    if graphics_after.is_some() || std::env::args().any(|arg| arg == "--graphics-features") {
        match graphics_after.as_deref() {
            Some("timestamps") => timestamps::run(path)?,
            Some("samples") => samples::run(path)?,
            Some("transfers") => transfers::run(path)?,
            Some("compute") => compute_portable(path)?,
            Some("mrt") => float_mrt(path)?,
            None => (),
            Some(other) => return Err(format!("unknown graphics predecessor: {other}")),
        }
        features::run(path)?;
        return gather::run(path);
    }
    graphics(path, false)?;
    graphics(path, true)?;
    compute(path)?;
    glass_blur(path, false, false)?;
    glass_blur(path, true, false)?;
    glass_blur(path, false, true)?;
    if std::env::args().any(|arg| arg == "--extended") {
        immediate_buffers(path)?;
        compute_portable(path)?;
        float_mrt(path)?;
        transfers::run(path)?;
        samples::run(path)?;
        timestamps::run(path)?;
        features::run(path)?;
        gather::run(path)?;
        private_memory::run(path)?;
        storage_image::run(path)?;
        render_targets::run(path)?;
        sample_mask::run(path)?;
    }
    Ok(())
}

fn float_mrt(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-float-mrt attachments=8");
    let table = ResourceTable::new();
    let formats = [
        TextureFormat::Bgra8Unorm,
        TextureFormat::Rgba8Unorm,
        TextureFormat::Rgba16Float,
        TextureFormat::Rgba32Float,
        TextureFormat::Rgba16Float,
        TextureFormat::Rgba32Float,
        TextureFormat::Bgra8Unorm,
        TextureFormat::Rgba16Float,
    ];
    let mut targets = Vec::new();
    let mut states = Vec::new();
    let mut source = String::from("struct Output {\n");
    for (i, format) in formats.into_iter().enumerate() {
        targets.push(checked!(table.define_texture(checked!(TextureDesc::new(
            format,
            checked!(Extent2D::new(8, 8)),
            TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC | TextureUsage::COPY_DST
        )))));
        states.push(checked!(ColorTargetState::new(
            format,
            BlendState::REPLACE,
            ColorWriteMask::ALL
        )));
        source += &format!("@location({i}) c{i}: vec4<f32>,\n");
    }
    source += "}; @fragment fn fs() -> Output { var o: Output;\n";
    for (i, format) in formats.into_iter().enumerate() {
        source += &if matches!(
            format,
            TextureFormat::Rgba16Float | TextureFormat::Rgba32Float
        ) {
            format!("o.c{i} = vec4<f32>(-2.0, {}.0, 0.5, 1.0);\n", i + 1)
        } else {
            format!("o.c{i} = vec4<f32>(0.25, 0.5, 0.75, 1.0);\n")
        };
    }
    source += "return o; }";
    let vertex =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(VERTEX.into()))));
    let fragment = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(source))));
    let pipeline = checked!(table.define_programmable_render_pipeline(checked!(
            checked!(ProgrammableRenderPipelineDesc::new(
                checked!(ShaderEntryPoint::new(
                    vertex,
                    ShaderStage::Vertex,
                    "vs".into()
                )),
                checked!(ShaderEntryPoint::new(
                    fragment,
                    ShaderStage::Fragment,
                    "fs".into()
                )),
                checked!(PipelineLayoutDesc::new(vec![])),
                formats[0],
                None,
                PrimitiveTopology::TriangleList,
                BlendState::REPLACE,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise)
            ))
            .with_color_targets(states)
        )));
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        return Ok(());
    }
    for draw in [false, true] {
        let clear = checked!(Color::rgba(0.25, 0.5, 0.75, 1.0));
        let mut desc = checked!(RenderPassDesc::new(
            &table,
            targets[0],
            checked!(PixelRect::new(0, 0, 8, 8)),
            LoadOp::Clear(clear),
            StoreOp::Store
        ));
        for &target in &targets[1..] {
            desc = checked!(desc.with_color_attachment(
                &table,
                target,
                LoadOp::Clear(clear),
                StoreOp::Store
            ));
        }
        let mut encoder = CommandEncoder::new(&table);
        let mut pass = checked!(encoder.begin_render_pass(desc));
        if draw {
            checked!(pass.set_programmable_pipeline(pipeline));
            checked!(pass.draw(3, 0));
        }
        checked!(pass.end());
        driver.submit(&checked!(encoder.finish()))?;
        for (slot, (&target, format)) in targets.iter().zip(formats).enumerate() {
            let pixel_bytes = format.bytes_per_pixel().unwrap() as usize;
            let mut output = vec![0xa5; 64 * pixel_bytes];
            driver.read_texture(target.slot(), &mut output)?;
            let expected: Vec<u8> = match format {
                TextureFormat::Bgra8Unorm => vec![191, 128, 64, 255],
                TextureFormat::Rgba8Unorm => vec![64, 128, 191, 255],
                TextureFormat::Rgba16Float => {
                    let integer_half = [
                        0x3c00u16, 0x4000, 0x4200, 0x4400, 0x4500, 0x4600, 0x4700, 0x4800,
                    ][slot];
                    (if draw {
                        [0xc000u16, integer_half, 0x3800, 0x3c00]
                    } else {
                        [0x3400, 0x3800, 0x3a00, 0x3c00]
                    })
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect()
                }
                TextureFormat::Rgba32Float => (if draw {
                    [-2.0f32, (slot + 1) as f32, 0.5, 1.0]
                } else {
                    [0.25, 0.5, 0.75, 1.0]
                })
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
                _ => unreachable!(),
            };
            for (pixel, got) in output.chunks_exact(pixel_bytes).enumerate() {
                let ok = if pixel_bytes == 4 {
                    got.iter().zip(&expected).all(|(a, b)| a.abs_diff(*b) <= 1)
                } else {
                    got == expected
                };
                if !ok {
                    return Err(format!(
                        "float MRT draw={draw} slot={slot} format={format:?} pixel={pixel}: got={got:?} expected={expected:?}"
                    ));
                }
            }
        }
        println!(
            "PASS programmable-float-mrt draw={draw} attachments=8 pixels=512 exact-float-bits=true"
        );
    }
    Ok(())
}

// The actual ScarletUI shader, including its 1120-byte UBO, dynamic kernel
// indexing, embedded vertex constants and separate texture/sampler slots.
fn glass_blur(path: &str, vertical: bool, optics: bool) -> Result<(), String> {
    let entry = if optics {
        "glass"
    } else if vertical {
        "vertical"
    } else {
        "horizontal"
    };
    println!("BEGIN programmable-glass-blur axis={entry}");
    let table = ResourceTable::new();
    let shader = checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(
        include_str!("../../sgfx-shader-ir3/tests/shaders/glass.wgsl").into()
    ))));
    let extent = checked!(Extent2D::new(32, 32));
    let source = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::SAMPLED | TextureUsage::COPY_DST
    ))));
    let target = checked!(table.define_texture(checked!(TextureDesc::new(
        TextureFormat::Bgra8Unorm,
        extent,
        TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_SRC
    ))));
    let uniform = checked!(table.define_buffer(checked!(BufferDesc::new(
        1120,
        BufferUsage::UNIFORM | BufferUsage::COPY_DST
    ))));
    let sampler = checked!(table.define_sampler(SamplerDesc::new(
        FilterMode::Linear,
        FilterMode::Linear,
        AddressMode::ClampToEdge,
        AddressMode::ClampToEdge
    )));
    let layout = checked!(BindGroupLayoutDesc::new(vec![
        BindGroupLayoutEntry::new(
            0,
            ShaderStages::VERTEX | ShaderStages::FRAGMENT,
            BindingType::UniformBuffer
        ),
        BindGroupLayoutEntry::new(1, ShaderStages::FRAGMENT, BindingType::SampledTexture),
        BindGroupLayoutEntry::new(2, ShaderStages::FRAGMENT, BindingType::Sampler),
    ]));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        layout.clone(),
        vec![
            BindGroupEntry::new(
                0,
                BindingResource::Buffer {
                    buffer: uniform.id(),
                    offset: 0,
                    size: 1120
                }
            ),
            BindGroupEntry::new(1, BindingResource::Texture(source.id())),
            BindGroupEntry::new(2, BindingResource::Sampler(sampler.id())),
        ]
    ))));
    let make_pipeline = |fragment: &str, blend| -> Result<_, String> {
        let vertices = if optics {
            Some(checked!(VertexBufferLayout::new(
                16,
                vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)]
            )))
        } else {
            None
        };
        Ok(checked!(table.define_programmable_render_pipeline(
            checked!(ProgrammableRenderPipelineDesc::new(
                checked!(ShaderEntryPoint::new(
                        shader,
                        ShaderStage::Vertex,
                        if optics {
                            "glass_vertex"
                        } else {
                            "blur_vertex"
                        }
                        .into()
                    )),
                checked!(ShaderEntryPoint::new(
                    shader,
                    ShaderStage::Fragment,
                    fragment.into()
                )),
                checked!(PipelineLayoutDesc::new(vec![layout.clone()])),
                TextureFormat::Bgra8Unorm,
                vertices,
                PrimitiveTopology::TriangleList,
                blend,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            ))
        )))
    };
    let add = BlendComponent::new(BlendFactor::One, BlendFactor::One, BlendOp::Add);
    let erase = BlendComponent::new(
        BlendFactor::Zero,
        BlendFactor::OneMinusSourceAlpha,
        BlendOp::Add,
    );
    let pipeline = make_pipeline(
        entry,
        if optics {
            BlendState::new(add, add)
        } else {
            BlendState::REPLACE
        },
    )?;
    let erase_pipeline = if optics {
        Some(make_pipeline("erase", BlendState::new(erase, erase))?)
    } else {
        None
    };
    let vertices = checked!(table.define_buffer(checked!(BufferDesc::new(
        96,
        BufferUsage::VERTEX | BufferUsage::COPY_DST
    ))));
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_SHADER, shader.slot())?;
    driver.validate(abi::VALIDATE_RENDER_PIPELINE, pipeline.slot())?;
    if let Some(erase) = erase_pipeline {
        driver.validate(abi::VALIDATE_RENDER_PIPELINE, erase.slot())?;
    }
    if std::env::args().any(|arg| arg == "--compile-only") {
        return Ok(());
    }
    let mut params = [0.0f32; 280];
    params[2..4].copy_from_slice(&[32.0, 32.0]);
    params[4..8].copy_from_slice(&[32.0, 32.0, 1.0, 4.0]);
    params[14..16].copy_from_slice(&[1.0, 3.0]);
    params[18..20].copy_from_slice(&[32.0, 32.0]);
    if optics {
        params[..4].copy_from_slice(&[4.0, 4.0, 24.0, 24.0]);
        params[8..12].copy_from_slice(&[1.0, 0.0, 0.0, 0.25]);
        params[13] = 1.0;
    }
    for (index, (offset, weight)) in [(-1.0, 0.25), (0.0, 0.5), (1.0, 0.25)]
        .into_iter()
        .enumerate()
    {
        params[20 + index * 4] = offset;
        params[21 + index * 4] = weight;
    }
    let params: Vec<u8> = params.into_iter().flat_map(f32::to_le_bytes).collect();
    let pixel = |x: usize, y: usize| {
        if optics {
            [153, 102, 51, 255]
        } else {
            [
                ((x * 31 + y * 7) % 251) as u8,
                ((x * 13 + y * 29) % 251) as u8,
                ((x * 47 + y * 11) % 251) as u8,
                255u8,
            ]
        }
    };
    let source_bytes: Vec<u8> = (0..1024).flat_map(|i| pixel(i % 32, i / 32)).collect();
    let area = checked!(PixelRect::new(0, 0, 32, 32));
    let mut encoder = CommandEncoder::new(&table);
    checked!(encoder.write_buffer(uniform, 0, &params));
    checked!(encoder.write_texture(
        source,
        checked!(TextureWrite::new(area, 128, &source_bytes))
    ));
    let vertex_bytes: Vec<u8> = [
        [4.0f32, 4.0],
        [28.0, 4.0],
        [28.0, 28.0],
        [4.0, 4.0],
        [28.0, 28.0],
        [4.0, 28.0],
    ]
    .into_iter()
    .flat_map(|[x, y]| [x, y, 0.0, 1.0])
    .flat_map(f32::to_le_bytes)
    .collect();
    if optics {
        checked!(encoder.write_buffer(vertices, 0, &vertex_bytes));
    }
    let mut pass = checked!(encoder.begin_render_pass(checked!(RenderPassDesc::new(
        &table,
        target,
        area,
        LoadOp::Clear(checked!(Color::rgba(0.2, 0.4, 0.6, 1.0))),
        StoreOp::Store
    ))));
    if let Some(erase) = erase_pipeline {
        checked!(pass.set_programmable_pipeline(erase));
        checked!(pass.set_bind_group(0, group));
        checked!(pass.set_vertex_buffer(vertices, 0));
        checked!(pass.draw(6, 0));
    }
    checked!(pass.set_programmable_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.draw(6, 0));
    checked!(pass.end());
    driver.submit(&checked!(encoder.finish()))?;
    let mut output = vec![0xa5; 4096];
    driver.read_texture(target.slot(), &mut output)?;
    for (index, got) in output.chunks_exact(4).enumerate() {
        let (x, y) = (index % 32, index / 32);
        if optics {
            let expected = if !(4..28).contains(&x) || !(4..28).contains(&y) {
                Some([153u8, 102, 51, 255])
            } else if (12..20).contains(&x) && (12..20).contains(&y) {
                Some([115, 77, 102, 255])
            } else {
                None
            };
            if let Some(expected) = expected {
                if got.iter().zip(expected).any(|(g, e)| g.abs_diff(e) > 1) {
                    return Err(format!(
                        "glass optics pixel={x},{y}: got={got:?} expected={expected:?}"
                    ));
                }
            }
            if got[3].abs_diff(255) > 1 {
                return Err(format!("glass optics alpha pixel={x},{y}: {got:?}"));
            }
            continue;
        }
        let mut sum = [0u32; 4];
        for (offset, weight) in [(-1isize, 1), (0, 2), (1, 1)] {
            let sx = if vertical {
                x
            } else {
                (x as isize + offset).clamp(0, 31) as usize
            };
            let sy = if vertical {
                (y as isize + offset).clamp(0, 31) as usize
            } else {
                y
            };
            for (i, value) in pixel(sx, sy).into_iter().enumerate() {
                sum[i] += u32::from(value) * weight;
            }
        }
        let expected = sum.map(|s| ((s + 2) / 4) as u8);
        if got.iter().zip(expected).any(|(g, e)| g.abs_diff(e) > 1) {
            return Err(format!(
                "glass blur axis={entry} pixel={x},{y}: got={got:?} expected={expected:?}"
            ));
        }
    }
    if optics {
        println!(
            "PASS programmable-glass-optics checked_color_pixels=512 checked_alpha_pixels=1024 vertex-buffer=true erase-add-blending=true"
        );
    } else {
        println!("PASS programmable-glass-blur axis={entry} pixels=1024 taps=3");
    }
    Ok(())
}

// Exercise the portable compute floor together, including cross-wave rendezvous
// and offset views, rather than treating successful dispatch as API coverage.
fn compute_portable(path: &str) -> Result<(), String> {
    println!("BEGIN programmable-compute-portable");
    let table = ResourceTable::new();
    let source = r#"
struct Params { values: array<vec4<u32>, 8>, }
@group(0) @binding(2) var<storage, read> a: array<u32>;
@group(0) @binding(5) var<storage, read> b: array<u32>;
@group(0) @binding(9) var<storage, read_write> output: array<u32>;
@group(0) @binding(13) var<storage, read_write> sums: array<u32>;
@group(0) @binding(17) var<uniform> params: Params;
var<push_constant> push: Params;
var<workgroup> scratch: array<u32, 128>;
@compute @workgroup_size(128)
fn cs(@builtin(local_invocation_index) lane:u32,
      @builtin(global_invocation_id) id:vec3<u32>,
      @builtin(workgroup_id) group:vec3<u32>) {
    let value = a[id.x] + b[id.x] + params.values[lane / 16u][lane % 4u]
                + push.values[lane / 16u][lane % 4u];
    output[id.x] = value;
    scratch[lane] = value;
    workgroupBarrier();
    for (var stride = 64u; stride > 0u; stride >>= 1u) {
        if (lane < stride) { scratch[lane] += scratch[lane + stride]; }
        workgroupBarrier();
    }
    if (lane == 0u) { sums[group.x] = scratch[0]; }
}
"#;
    let shader =
        checked!(table.define_shader_module(checked!(ShaderModuleDesc::wgsl(source.into()))));
    let mut buffers = Vec::new();
    for words in [512u64, 512, 512, 4] {
        buffers.push(checked!(table.define_buffer(checked!(BufferDesc::new(
            (words + 2) * 4,
            BufferUsage::STORAGE | BufferUsage::COPY_SRC | BufferUsage::COPY_DST
        )))));
    }
    let uniform = checked!(table.define_buffer(checked!(BufferDesc::new(
        128,
        BufferUsage::UNIFORM | BufferUsage::COPY_DST
    ))));
    let indices = [2, 5, 9, 13];
    let mut layout_entries = Vec::new();
    let mut entries = Vec::new();
    for (index, buffer) in buffers.iter().enumerate() {
        layout_entries.push(BindGroupLayoutEntry::new(
            indices[index],
            ShaderStages::COMPUTE,
            BindingType::StorageBuffer {
                read_only: index < 2,
            },
        ));
        entries.push(BindGroupEntry::new(
            indices[index],
            BindingResource::Buffer {
                buffer: buffer.id(),
                offset: 4,
                size: if index == 3 { 16 } else { 2048 },
            },
        ));
    }
    layout_entries.push(BindGroupLayoutEntry::new(
        17,
        ShaderStages::COMPUTE,
        BindingType::UniformBuffer,
    ));
    entries.push(BindGroupEntry::new(
        17,
        BindingResource::Buffer {
            buffer: uniform.id(),
            offset: 0,
            size: 128,
        },
    ));
    let layout = checked!(BindGroupLayoutDesc::new(layout_entries));
    let group = checked!(table.define_bind_group(checked!(BindGroupDesc::new(
        &table,
        layout.clone(),
        entries
    ))));
    let pipeline_layout = checked!(
        checked!(PipelineLayoutDesc::new(vec![layout])).with_push_constant_ranges(vec![checked!(
            PushConstantRange::new(ShaderStages::COMPUTE, 0, 128)
        )])
    );
    let pipeline = checked!(
        table.define_compute_pipeline(checked!(ComputePipelineDesc::new(
            checked!(ShaderEntryPoint::new(
                shader,
                ShaderStage::Compute,
                "cs".into()
            )),
            pipeline_layout
        )))
    );
    let driver = DriverSession::new(path, &table)?;
    driver.validate(abi::VALIDATE_COMPUTE_PIPELINE, pipeline.slot())?;
    if std::env::args().any(|arg| arg == "--compile-only") {
        return Ok(());
    }
    let guard = 0xdeadc0deu32;
    let mut uploads = Vec::new();
    for index in 0..4 {
        let count = if index == 3 { 4 } else { 512 };
        let mut bytes = guard.to_le_bytes().to_vec();
        for word in 0..count {
            let value = match index {
                0 => word as u32 + 1,
                1 => word as u32 * 3 + 7,
                _ => 0xcccccccc,
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&guard.to_le_bytes());
        uploads.push(bytes);
    }
    let uniform_bytes: Vec<u8> = (0..32u32).flat_map(|n| (n + 11).to_le_bytes()).collect();
    let push_bytes: Vec<u8> = (0..32u32)
        .flat_map(|n| (n * 2 + 13).to_le_bytes())
        .collect();
    let mut encoder = CommandEncoder::new(&table);
    for (buffer, upload) in buffers.iter().zip(&uploads) {
        checked!(encoder.write_buffer(*buffer, 0, upload));
    }
    checked!(encoder.write_buffer(uniform, 0, &uniform_bytes));
    let mut pass = checked!(encoder.begin_compute_pass());
    checked!(pass.set_pipeline(pipeline));
    checked!(pass.set_bind_group(0, group));
    checked!(pass.set_push_constants(ShaderStages::COMPUTE, 0, &push_bytes));
    checked!(pass.dispatch(4, 1, 1));
    checked!(pass.end());
    driver.submit_checked(&checked!(encoder.finish()), true)?;
    let expected: Vec<u32> = (0..512u32)
        .map(|i| {
            let lane = i % 128;
            let param = lane / 16 * 4 + lane % 4;
            i * 4 + 8 + param * 3 + 24
        })
        .collect();
    for index in 0..4 {
        let mut bytes = vec![0xa5; uploads[index].len()];
        driver.read_buffer(buffers[index].slot(), &mut bytes)?;
        let words: Vec<_> = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        if words[0] != guard || *words.last().unwrap() != guard {
            return Err(format!(
                "compute portable buffer={index} offset-view guard overwritten"
            ));
        }
        for (i, got) in words[1..words.len() - 1].iter().enumerate() {
            let want = match index {
                0 => i as u32 + 1,
                1 => i as u32 * 3 + 7,
                2 => expected[i],
                _ => expected[i * 128..(i + 1) * 128].iter().sum(),
            };
            if *got != want {
                return Err(format!(
                    "compute portable buffer={index} word={i} got={got} expected={want}"
                ));
            }
        }
    }
    println!(
        "PASS programmable-compute-portable ssbo=4 offset=4 uniform=128 push=128 local=128 groups=4 shared-reduction=true guards=intact"
    );
    Ok(())
}

fn immediate_buffers(path: &str) -> Result<(), String> {
    println!("BEGIN immediate-buffer-publication");
    let table = ResourceTable::new();
    let usage = BufferUsage::COPY_SRC | BufferUsage::COPY_DST;
    let a = checked!(table.define_buffer(checked!(BufferDesc::new(4096, usage))));
    let b = checked!(table.define_buffer(checked!(BufferDesc::new(4096, usage))));
    let driver = DriverSession::new(path, &table)?;
    let mut expected_a = vec![0u8; 4096];
    let mut expected_b = vec![0u8; 4096];
    let mut receipts = Vec::new();
    for frame in 0..16usize {
        let payload: Vec<u8> = (0..128)
            .map(|i| ((i * 17 + frame * 43) & 255) as u8)
            .collect();
        let offset = frame * 192;
        let mut encoder = CommandEncoder::new(&table);
        if frame == 0 {
            checked!(encoder.write_buffer(a, 0, &expected_a));
            checked!(encoder.write_buffer(b, 0, &expected_b));
        }
        checked!(encoder.write_buffer(a, offset as u64, &payload));
        checked!(encoder.copy_buffer_to_buffer(a, offset as u64, b, (offset + 4) as u64, 128));
        let receipt = driver.submit_unwaited(&checked!(encoder.finish()))?;
        // No receipt wait or poll precedes either read.
        expected_a[offset..offset + 128].copy_from_slice(&payload);
        expected_b[offset + 4..offset + 132].copy_from_slice(&payload);
        let mut actual_a = vec![0xa5; 4096];
        let mut actual_b = vec![0xa5; 4096];
        driver.read_buffer(a.slot(), &mut actual_a)?;
        driver.read_buffer(b.slot(), &mut actual_b)?;
        if actual_a != expected_a || actual_b != expected_b {
            return Err(format!(
                "immediate buffer publication mismatch frame={frame}"
            ));
        }
        receipts.push(receipt);
    }
    for receipt in receipts {
        let mut state = abi::PENDING;
        unsafe {
            status((driver.library.api.wait)(
                receipt.0,
                10_000_000_000,
                &mut state,
            ))?;
        }
        if state != abi::COMPLETE {
            return Err(format!("immediate buffer receipt failed state={state}"));
        }
    }
    println!(
        "PASS immediate-buffer-publication submissions=16 bytes=131072 reads-before-wait=true"
    );
    Ok(())
}
