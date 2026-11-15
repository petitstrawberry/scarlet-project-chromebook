//! Real native lowering for fixed descriptors outside the canonical shader pack.
//! The established canonical paths stay unchanged; no shader or CPU pixel stub.
use crate::{
    IrSubmitError, ir,
    resource::{ContextResources, RawBuffer},
    wire::BoundObject,
};
use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use sgfx_codegen_adreno_a6xx as c;
use sgfx_shader_ir3::{BindingAssignment, BindingMap, ResourceKind, Source, Stage};

struct Shader {
    native: c::NativeShader,
    allocation: Arc<RawBuffer>,
    private: Option<crate::native_private::PrivateMemoryBuffer>,
}
#[derive(Clone,Copy,PartialEq,Eq)]
struct FixedKey {pipeline:c::PipelineId,conversion:Option<ir::YcbcrConversion>,sampler:Option<sgfx_shader_ir3::Nv12SamplerProfile>}
struct Record {
    key: FixedKey,
    pipeline: Arc<c::NativeRenderPipeline>,
    shaders: [Shader; 2],
    _definitions: ir::ResourceTable,
}
#[derive(Default)]
pub(crate) struct FixedCache {
    records: Vec<Record>,
    probe: Vec<(c::ObjectId, SpecialTexture)>,
}

fn textured(d: &ir::RenderPipelineDesc) -> bool {
    matches!(
        d.fragment(),
        ir::FragmentProgram::Texture(_) | ir::FragmentProgram::TextureVertexColor(_)
    )
}
#[cfg(test)]
fn shader_source(d:&ir::RenderPipelineDesc)->Result<String,IrSubmitError> {shader_source_with_conversion(d,None)}
fn shader_source_with_conversion(d: &ir::RenderPipelineDesc,conversion:Option<ir::YcbcrConversion>) -> Result<String, IrSubmitError> {
    use ir::{FragmentProgram as F, TextureSampleMode as M, VertexFormat as V};
    let attributes = d.vertex_buffer().attributes();
    let format = |location| {
        attributes
            .iter()
            .find(|a| a.location() == location)
            .map(|a| a.format())
    };
    let (position_ty, position) = match format(0) {
        Some(V::Float32x2) => ("vec2<f32>", "vec4<f32>(p,0.0,1.0)"),
        Some(V::Float32x3) => ("vec3<f32>", "vec4<f32>(p,1.0)"),
        Some(V::Float32x4) => ("vec4<f32>", "p"),
        _ => return Err(ir::Error::InvalidDescriptor.into()),
    };
    let has_color = matches!(d.fragment(), F::VertexColor | F::TextureVertexColor(_));
    let mut parameters = format!("@location(0) p:{position_ty}");
    let color = if has_color {
        let (ty, value) = match format(1) {
            Some(V::Float32x3) => ("vec3<f32>", "vec4<f32>(c,1.0)"),
            Some(V::Float32x4 | V::Unorm8x4) => ("vec4<f32>", "c"),
            _ => return Err(ir::Error::InvalidDescriptor.into()),
        };
        parameters.push_str(&format!(",@location(1) c:{ty}"));
        value
    } else {
        "vec4<f32>(1.0)"
    };
    let uv = if textured(d) {
        let location = if has_color { 2 } else { 1 };
        if format(location) != Some(V::Float32x2) {
            return Err(ir::Error::InvalidDescriptor.into());
        }
        parameters.push_str(&format!(",@location({location}) uv:vec2<f32>"));
        "uv"
    } else {
        "vec2<f32>(0.0)"
    };
    let mut source = String::from(
        "struct Params {transform:mat4x4<f32>,color:vec4<f32>,mask_from_red:u32}; var<push_constant> params:Params; struct Out {@builtin(position) position:vec4<f32>,@location(0) color:vec4<f32>,@location(1) uv:vec2<f32>};\n",
    );
    if textured(d) {
        source.push_str("@group(0) @binding(0) var image:texture_2d<f32>; @group(0) @binding(1) var filtering:sampler;\n");
    }
    source.push_str(&format!("@vertex fn vs({parameters})->Out {{return Out(params.transform*{position},{color},{uv});}}\n"));
    source.push_str("@fragment fn fs(input:Out)->@location(0) vec4<f32>{\n");
    if textured(d) {
        if let Some(conversion)=conversion {
            let coefficients=c::ycbcr_coefficients(conversion);
            source.push_str("let raw=textureSample(image,filtering,input.uv); let yuv=vec4<f32>(raw.xyz,1.0);\n");
            for row in 0..3 {
                let a=&coefficients[row*4..row*4+4];
                source.push_str(&format!("let component{row}=clamp(dot(yuv,vec4<f32>({},{},{},{})),0.0,1.0);\n",a[0],a[1],a[2],a[3]));
            }
            source.push_str("let sampled=vec4<f32>(component0,component1,component2,1.0);\n");
        } else {source.push_str("let sampled=textureSample(image,filtering,input.uv);\n");}
        source.push_str("let coverage=select(sampled.a,sampled.r,params.mask_from_red!=0u);\n");
    }
    let expression = match d.fragment() {
        F::Solid => "params.color",
        F::VertexColor => "input.color*params.color",
        F::Texture(M::Rgba) => "sampled*params.color",
        F::Texture(M::RgbIgnoreAlpha) => "vec4<f32>(sampled.rgb,1.0)*params.color",
        F::Texture(M::AlphaMask) => "vec4<f32>(params.color.rgb,coverage*params.color.a)",
        F::TextureVertexColor(M::Rgba) => "sampled*input.color*params.color",
        F::TextureVertexColor(M::RgbIgnoreAlpha) => {
            "vec4<f32>(sampled.rgb,1.0)*input.color*params.color"
        }
        F::TextureVertexColor(M::AlphaMask) => {
            "vec4<f32>(params.color.rgb*input.color.rgb,coverage*input.color.a*params.color.a)"
        }
    };
    source.push_str(&format!("return {expression};}}\n"));
    Ok(source)
}
fn descriptor(
    d: &ir::RenderPipelineDesc,
    source: &str,
) -> Result<
    (
        ir::ResourceTable,
        ir::ProgrammableRenderPipelineDesc,
        BindingMap,
    ),
    IrSubmitError,
> {
    let table = ir::ResourceTable::new();
    let module = table.define_shader_module(ir::ShaderModuleDesc::wgsl(source.into())?)?;
    let layout = if textured(d) {
        vec![ir::BindGroupLayoutDesc::new(vec![
            ir::BindGroupLayoutEntry::new(
                0,
                ir::ShaderStages::FRAGMENT,
                ir::BindingType::SampledTexture,
            ),
            ir::BindGroupLayoutEntry::new(1, ir::ShaderStages::FRAGMENT, ir::BindingType::Sampler),
        ])?]
    } else {
        vec![]
    };
    let layout = ir::PipelineLayoutDesc::new(layout)?.with_push_constant_ranges(vec![
        ir::PushConstantRange::new(ir::ShaderStages::VERTEX | ir::ShaderStages::FRAGMENT, 0, 96)?,
    ])?;
    let pipeline = ir::ProgrammableRenderPipelineDesc::new(
        ir::ShaderEntryPoint::new(module, ir::ShaderStage::Vertex, "vs".into())?,
        ir::ShaderEntryPoint::new(module, ir::ShaderStage::Fragment, "fs".into())?,
        layout,
        d.target_format(),
        None,
        d.topology(),
        d.blend(),
        d.raster(),
    )?
    .with_vertex_buffers(vec![d.vertex_buffer().clone()])?;
    let pipeline = if let Some(depth) = d.depth_state() {
        pipeline.with_depth_state(depth)?
    } else {
        pipeline
    };
    let map = BindingMap::new(if textured(d) {
        vec![
            BindingAssignment {
                group: 0,
                binding: 0,
                kind: ResourceKind::Texture,
                slot: 0,
            },
            BindingAssignment {
                group: 0,
                binding: 1,
                kind: ResourceKind::Sampler,
                slot: 0,
            },
        ]
    } else {
        vec![]
    })
    .map_err(|e| IrSubmitError::ShaderCompile(e.0))?;
    Ok((table, pipeline, map))
}
fn append(
    record: &Record,
    resources: &mut Vec<c::ResourceMeta>,
    bindings: &mut Vec<BoundObject>,
) -> Result<(), IrSubmitError> {
    for shader in &record.shaders {
        if let Some(private) = &shader.private {
            private.append(resources, bindings)?;
        }
        let id = shader.native.object;
        if resources.iter().any(|r| r.id == id) {
            continue;
        }
        resources
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings
            .try_reserve(2)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        resources.push(c::ResourceMeta {
            id,
            size: shader.allocation.logical_size,
            kind: c::ResourceKind::Buffer {
                usage: ir::BufferUsage::UNIFORM,
            },
        });
        for object in [c::ObjectRef::NativeShader(id), c::ObjectRef::External(id)] {
            bindings.push(BoundObject {
                object,
                attachment_token: shader.allocation.attachment_token,
                allocation_offset: 0,
                size: shader.allocation.logical_size,
            });
        }
    }
    Ok(())
}
impl FixedCache {
    pub(crate) fn retain(
        &self,
        bindings: &[BoundObject],
        buffers: &mut Vec<Arc<RawBuffer>>,
    ) -> Result<(), IrSubmitError> {
        for r in &self.records {
            for shader in &r.shaders {
                if let Some(private) = &shader.private {
                    private.retain(bindings, buffers)?;
                }
                if bindings
                    .iter()
                    .any(|b| b.attachment_token == shader.allocation.attachment_token)
                    && !buffers.iter().any(|b| Arc::ptr_eq(b, &shader.allocation))
                {
                    buffers
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    buffers.push(Arc::clone(&shader.allocation));
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpecialTexture {
    Red,
    Nv12,
    Native,
}
fn texture_ok(d: &ir::RenderPipelineDesc, special: Option<SpecialTexture>) -> bool {
    use ir::{FragmentProgram as F, TextureSampleMode as M};
    match d.fragment() {
        F::Solid | F::VertexColor => true,
        F::Texture(M::AlphaMask) | F::TextureVertexColor(M::AlphaMask) => {
            special == Some(SpecialTexture::Red)
        }
        _ => special.is_none() || special==Some(SpecialTexture::Nv12),
    }
}
fn fill_probe(
    resources: &[c::ResourceMeta],
    probe: &mut Vec<(c::ObjectId, SpecialTexture)>,
) -> Result<(), IrSubmitError> {
    probe.clear();
    for resource in resources.iter() {
        if let c::ResourceKind::Image(image) = &resource.kind {
            let kind = match image.format {
                ir::TextureFormat::R8Unorm => Some(SpecialTexture::Red),
                ir::TextureFormat::Nv12 => Some(SpecialTexture::Nv12),
                ir::TextureFormat::Rg8Unorm
                | ir::TextureFormat::Rgba16Float
                | ir::TextureFormat::Rgba32Float => Some(SpecialTexture::Native),
                _ => None,
            };
            if let Some(kind) = kind {
                probe
                    .try_reserve(1)
                    .map_err(|_| IrSubmitError::OutOfMemory)?;
                probe.push((resource.id, kind));
            }
        }
    }
    Ok(())
}
fn needs_native(
    pipelines: &[c::PipelineMeta],
    special: &[(c::ObjectId, SpecialTexture)],
    operations: &[c::Operation<'_>],
) -> bool {
    use c::Operation as O;
    let mut active = None;
    let mut sample = None;
    let mut border = false;
    for op in operations {
        match op {
            O::SetPipeline(id) => {
                active = pipelines
                    .iter()
                    .find(|p| p.id == *id)
                    .map(|p| (p, c::canonical_fixed_descriptor_supported(&p.descriptor)))
            }
            O::SetTexture(id) => {
                sample = special
                    .iter()
                    .find(|(key, _)| key == id)
                    .map(|(_, kind)| *kind)
            }
            O::SetSampler(s) => border=[s.address_u(),s.address_v()].contains(&ir::AddressMode::ClampToBorder),
            O::SetProgrammablePipeline(_) | O::SetFixedNativePipeline(_) => active = None,
            O::BeginRenderPass(_) | O::BeginNativeRenderPass(_) | O::EndRenderPass => {
                active = None;
                sample = None;
                border = false;
            }
            O::Draw { .. } | O::DrawIndexed { .. } => {
                if let Some((p, canonical)) = active {
                    if !canonical
                        || !texture_ok(&p.descriptor, sample)
                        || (border && sample==Some(SpecialTexture::Nv12) && textured(&p.descriptor))
                        || matches!(op,O::DrawIndexed {base_vertex,..} if *base_vertex<0)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}
fn retain_effective_fixed_pipelines(pipelines:&mut Vec<c::PipelineMeta>,operations:&[c::Operation<'_>]) {
    // The normalizer initially records every fixed descriptor. Converted
    // native programs own their descriptor; passing their old fixed metadata
    // to codegen would incorrectly re-run canonical-pack admission.
    pipelines.retain(|p|operations.iter().any(|op|matches!(op,c::Operation::SetPipeline(id) if *id==p.id)));
}
impl ContextResources {
    fn fixed_program(
        &mut self,
        key: FixedKey,
        d: &ir::RenderPipelineDesc,
        resources: &mut Vec<c::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<Arc<c::NativeRenderPipeline>, IrSubmitError> {
        if !self
            .context
            .device
            .capabilities
            .supports_programmable_graphics()
        {
            return Err(IrSubmitError::Unsupported(
                crate::UnsupportedIrFeature::ProgrammableExecution,
            ));
        }
        if let Some(r) = self.fixed.records.iter().find(|r| r.key == key) {
            append(r, resources, bindings)?;
            return Ok(Arc::clone(&r.pipeline));
        }
        let source = shader_source_with_conversion(d,if key.sampler.is_some() {None}else{key.conversion})?;
        let (definitions, descriptor, map) = descriptor(d, &source)?;
        // Separate bounded object namespace from public buffers and the normal
        // shader cache (0x40000000..0x5fffffff, scratch0x60000000..0x7fffffff).
        let slot =
            u32::try_from(self.fixed.records.len()).map_err(|_| IrSubmitError::OutOfMemory)?;
        if slot >= (ir::MAX_RENDER_PIPELINES *17) as u32 {
            return Err(IrSubmitError::OutOfMemory);
        }
        let mut shaders = Vec::new();
        for (i, stage, entry) in [(0, Stage::Vertex, "vs"), (1, Stage::Fragment, "fs")] {
            let compiled = Arc::new(
                if stage==Stage::Fragment && key.sampler.is_some() {
                    sgfx_shader_ir3::compile_with_bindings_ycbcr_samplers(Source::Wgsl(&source),stage,entry,&map,
                        &[c::nv12_profile(0,key.conversion.ok_or(ir::Error::InvalidDescriptor)?)],
                        &[key.sampler.unwrap()]).map_err(|e|IrSubmitError::ShaderCompile(e.0))?
                } else {
                    sgfx_shader_ir3::compile_with_bindings(Source::Wgsl(&source), stage, entry, &map)
                        .map_err(|e| IrSubmitError::ShaderCompile(e.0))?
                },
            );
            let object = c::ObjectId::new(0x8000_0000 + slot * 2 + i);
            let allocation = Arc::new(RawBuffer::create(
                &self.context,
                compiled.allocation.len() as u64,
            )?);
            allocation.write(0, &compiled.allocation)?;
            let private = crate::native_private::PrivateMemoryBuffer::create(
                &self.context,
                c::ObjectId::new(object.raw() + 0x2000_0000),
                &compiled.metadata,
            )?;
            shaders.push(Shader {
                native: c::NativeShader {
                    object,
                    private_memory: private.as_ref().map(|p| p.native),
                    artifact: compiled,
                },
                allocation,
                private,
            });
        }
        let shaders: [Shader; 2] = shaders.try_into().map_err(|_| IrSubmitError::OutOfMemory)?;
        let pipeline = Arc::new(c::NativeRenderPipeline {
            descriptor,
            vertex: shaders[0].native.clone(),
            fragment: shaders[1].native.clone(),
        });
        c::validate_native_render_pipeline(&pipeline)?;
        let r = Record {
            key,
            pipeline: Arc::clone(&pipeline),
            shaders,
            _definitions: definitions,
        };
        append(&r, resources, bindings)?;
        self.fixed
            .records
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.fixed.records.push(r);
        Ok(pipeline)
    }
    pub(crate) fn lower_fixed<'data>(
        &mut self,
        pipelines: &mut Vec<c::PipelineMeta>,
        resources: &mut Vec<c::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
        operations: &mut Vec<c::Operation<'data>>,
    ) -> Result<(), IrSubmitError> {
        use c::Operation as O;
        // Resource metadata is submission-local, but it can include unrelated
        // transfers. Only an actually affected fixed draw authorizes rewriting.
        fill_probe(resources, &mut self.fixed.probe)?;
        if !needs_native(pipelines, &self.fixed.probe, operations) {
            return Ok(());
        }
        let mut lowered = Vec::new();
        lowered
            .try_reserve(operations.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        let mut fixed: Option<(&c::PipelineMeta, bool)> = None;
        let mut texture = None;
        let mut sampler = None;
        let mut selected: Option<(FixedKey, Arc<c::NativeRenderPipeline>)> = None;
        for op in operations.iter() {
            match op {
                O::BeginRenderPass(_) | O::BeginNativeRenderPass(_) | O::EndRenderPass => {
                    fixed = None;
                    texture = None;
                    sampler = None;
                    lowered.push(op.clone());
                }
                O::SetPipeline(id) => {
                    let p = pipelines
                        .iter()
                        .find(|p| p.id == *id)
                        .ok_or(ir::Error::PipelineNotSet)?;
                    fixed = Some((p, c::canonical_fixed_descriptor_supported(&p.descriptor)));
                    selected = None;
                }
                O::SetProgrammablePipeline(_) | O::SetFixedNativePipeline(_) => {
                    fixed = None;
                    lowered.push(op.clone());
                }
                O::SetSampler(s) => {sampler=Some(*s);lowered.push(op.clone());}
                O::SetTexture(id) => {
                    texture = resources.iter().find(|r| r.id == *id).and_then(|r| {
                        if let c::ResourceKind::Image(i) = &r.kind {
                            Some((i.format,i.conversion))
                        } else {
                            None
                        }
                    });
                    lowered.push(op.clone());
                }
                O::Draw { .. } | O::DrawIndexed { .. } if fixed.is_some() => {
                    let (p, canonical) = fixed.unwrap();
                    let sample = texture.map(|v|v.0);
                    let conversion=if sample==Some(ir::TextureFormat::Nv12)&&textured(&p.descriptor) {
                        Some(texture.and_then(|v|v.1).ok_or(ir::Error::InvalidDescriptor)?)
                    } else {None};
                    let sampler_profile=if conversion.is_some() {sampler.filter(|s|
                        [s.address_u(),s.address_v()].contains(&ir::AddressMode::ClampToBorder))
                        .map(|s|c::nv12_sampler_profile(0,s))} else {None};
                    let key=FixedKey {pipeline:p.id,conversion,sampler:sampler_profile};
                    let canonical_texture = match p.descriptor.fragment() {
                        ir::FragmentProgram::Solid | ir::FragmentProgram::VertexColor => true,
                        ir::FragmentProgram::Texture(ir::TextureSampleMode::AlphaMask)
                        | ir::FragmentProgram::TextureVertexColor(
                            ir::TextureSampleMode::AlphaMask,
                        ) => sample == Some(ir::TextureFormat::R8Unorm),
                        _ => matches!(
                            sample,
                            Some(
                                ir::TextureFormat::Bgra8Unorm
                                    | ir::TextureFormat::Rgba8Unorm
                                    | ir::TextureFormat::Bgra8UnormSrgb
                                    | ir::TextureFormat::Rgba8UnormSrgb
                                    | ir::TextureFormat::Nv12
                            )
                        ),
                    };
                    if canonical
                        && sampler_profile.is_none()
                        && canonical_texture
                        && !matches!(op,O::DrawIndexed {base_vertex,..} if *base_vertex<0)
                    {
                        lowered.push(O::SetPipeline(p.id));
                    } else {
                        let native = if let Some((id, native)) = &selected {
                            if *id == key {
                                Arc::clone(native)
                            } else {
                                self.fixed_program(key, &p.descriptor, resources, bindings)?
                            }
                        } else {
                            self.fixed_program(key, &p.descriptor, resources, bindings)?
                        };
                        selected = Some((key, Arc::clone(&native)));
                        lowered.push(O::SetFixedNativePipeline(native));
                    }
                    lowered.push(op.clone());
                }
                _ => lowered.push(op.clone()),
            }
        }
        *operations = lowered;
        retain_effective_fixed_pipelines(pipelines,operations);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_fixed_program_semantics_compile_real_wgsl_and_spirv_for_general_layouts() {
        use ir::{FragmentProgram as F, TextureSampleMode as M, VertexFormat as V};
        for position in [V::Float32x2, V::Float32x3, V::Float32x4] {
            for color in [V::Float32x3, V::Float32x4, V::Unorm8x4] {
                for fragment in [
                    F::Solid,
                    F::VertexColor,
                    F::Texture(M::Rgba),
                    F::Texture(M::RgbIgnoreAlpha),
                    F::Texture(M::AlphaMask),
                    F::TextureVertexColor(M::Rgba),
                    F::TextureVertexColor(M::RgbIgnoreAlpha),
                    F::TextureVertexColor(M::AlphaMask),
                ] {
                    let colored = matches!(fragment, F::VertexColor | F::TextureVertexColor(_));
                    let mut attrs = vec![ir::VertexAttribute::new(0, position, 8)];
                    if colored {
                        attrs.push(ir::VertexAttribute::new(1, color, 32));
                    }
                    if matches!(fragment, F::Texture(_) | F::TextureVertexColor(_)) {
                        attrs.push(ir::VertexAttribute::new(
                            if colored { 2 } else { 1 },
                            V::Float32x2,
                            56,
                        ));
                    }
                    let d = ir::RenderPipelineDesc::new(
                        ir::TextureFormat::Rgba16Float,
                        ir::PrimitiveTopology::TriangleStrip,
                        ir::VertexBufferLayout::new(80, attrs).unwrap(),
                        fragment,
                        ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                        ir::RasterState::new(ir::CullMode::Front, ir::FrontFace::Clockwise),
                    )
                    .unwrap();
                    let source = shader_source(&d).unwrap();
                    let (_table, descriptor, map) = descriptor(&d, &source).unwrap();
                    let mut compiled = Vec::new();
                    for (stage, entry) in [(Stage::Vertex, "vs"), (Stage::Fragment, "fs")] {
                        let shader = sgfx_shader_ir3::compile_with_bindings(
                            Source::Wgsl(&source),
                            stage,
                            entry,
                            &map,
                        )
                        .unwrap_or_else(|e| panic!("{fragment:?}/{position:?}/{color:?}: {e}"));
                        let normalized = BindingMap::new(if textured(&d) {
                            vec![
                                BindingAssignment {
                                    group: 2,
                                    binding: 0,
                                    kind: ResourceKind::Texture,
                                    slot: 0,
                                },
                                BindingAssignment {
                                    group: 3,
                                    binding: 0,
                                    kind: ResourceKind::Sampler,
                                    slot: 0,
                                },
                            ]
                        } else {
                            vec![]
                        })
                        .unwrap();
                        let round = sgfx_shader_ir3::compile_with_bindings(
                            Source::SpirV(&shader.spirv),
                            stage,
                            entry,
                            &normalized,
                        )
                        .unwrap();
                        assert_eq!(shader.allocation, round.allocation);
                        assert!(shader.push_constant_bytes <= 96);
                        assert!(!shader.metadata.sample_shading);
                        compiled.push(shader);
                    }
                    let fs = compiled.pop().unwrap();
                    let vertex = compiled.pop().unwrap();
                    let p = c::NativeRenderPipeline {
                        descriptor,
                        vertex: c::NativeShader {
                            object: c::ObjectId::new(10),
                            private_memory: None,
                            artifact: Arc::new(vertex),
                        },
                        fragment: c::NativeShader {
                            object: c::ObjectId::new(11),
                            private_memory: None,
                            artifact: Arc::new(fs),
                        },
                    };
                    c::validate_native_render_pipeline(&p).unwrap();
                    if position == V::Float32x3
                        && color == V::Unorm8x4
                        && fragment == F::TextureVertexColor(M::AlphaMask)
                    {
                        if let Some(dir) = std::env::var_os("SGFX_FIXED_NATIVE_FIXTURE_OUT") {
                            let dir = std::path::PathBuf::from(dir);
                            std::fs::create_dir_all(&dir).unwrap();
                            std::fs::write(dir.join("fixed_native.wgsl"), &source).unwrap();
                            for (name, shader) in [
                                ("fixed_native_vs", &p.vertex.artifact),
                                ("fixed_native_fs", &p.fragment.artifact),
                            ] {
                                std::fs::write(dir.join(format!("{name}.bin")), &shader.allocation)
                                    .unwrap();
                                std::fs::write(
                                    dir.join(format!("{name}.json")),
                                    serde_json::to_vec_pretty(&shader.metadata).unwrap(),
                                )
                                .unwrap();
                                std::fs::write(
                                    dir.join(format!("{name}.bindings.json")),
                                    serde_json::to_vec_pretty(&shader.bindings).unwrap(),
                                )
                                .unwrap();
                                std::fs::write(
                                    dir.join(format!("{name}.spv")),
                                    shader
                                        .spirv
                                        .iter()
                                        .flat_map(|w| w.to_le_bytes())
                                        .collect::<Vec<_>>(),
                                )
                                .unwrap();
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod reachability_tests {
    use super::*;
    pub(super) fn pipeline(fragment: ir::FragmentProgram) -> c::PipelineMeta {
        c::PipelineMeta {
            id: c::PipelineId::new(1),
            descriptor: ir::RenderPipelineDesc::new(
                ir::TextureFormat::Bgra8Unorm,
                ir::PrimitiveTopology::TriangleList,
                ir::VertexBufferLayout::new(
                    24,
                    vec![
                        ir::VertexAttribute::new(0, ir::VertexFormat::Float32x4, 0),
                        ir::VertexAttribute::new(1, ir::VertexFormat::Float32x2, 16),
                    ],
                )
                .unwrap(),
                fragment,
                ir::BlendState::REPLACE,
                ir::RasterState::new(ir::CullMode::None, ir::FrontFace::CounterClockwise),
            )
            .unwrap(),
        }
    }
    #[test]
    fn unrelated_or_rebound_exotic_images_do_not_rewrite_canonical_ui_draws() {
        use crate::fixed_test_fixtures as f;
        use c::Operation as O;
        let bgra = c::ObjectId::new(20);
        let red = c::ObjectId::new(21);
        let float = c::ObjectId::new(22);
        let rg = c::ObjectId::new(23);
        let resources = vec![
            f::native_image(
                bgra,
                ir::TextureFormat::Bgra8Unorm,
                ir::TextureUsage::SAMPLED,
            ),
            f::native_image(red, ir::TextureFormat::R8Unorm, ir::TextureUsage::SAMPLED),
            f::native_image(
                float,
                ir::TextureFormat::Rgba16Float,
                ir::TextureUsage::SAMPLED | ir::TextureUsage::COPY_SRC,
            ),
            f::native_image(rg, ir::TextureFormat::Rg8Unorm, ir::TextureUsage::SAMPLED),
        ];
        let mut probe = Vec::new();
        fill_probe(&resources, &mut probe).unwrap();
        let pipelines = [pipeline(ir::FragmentProgram::Texture(
            ir::TextureSampleMode::Rgba,
        ))];
        let ordinary = vec![
            O::SetPipeline(pipelines[0].id),
            O::SetTexture(bgra),
            O::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
        ];
        assert!(!needs_native(&pipelines, &probe, &ordinary));
        let mut changed = ordinary.clone();
        changed[1] = O::SetTexture(float);
        assert!(needs_native(&pipelines, &probe, &changed));
        changed.insert(2, O::SetTexture(bgra));
        assert!(!needs_native(&pipelines, &probe, &changed));
        // Reuse the same capacity/context metadata after a previous float use.
        fill_probe(&resources, &mut probe).unwrap();
        assert!(!needs_native(&pipelines, &probe, &ordinary));
        let alpha = [pipeline(ir::FragmentProgram::Texture(
            ir::TextureSampleMode::AlphaMask,
        ))];
        let glyph = vec![
            O::SetPipeline(alpha[0].id),
            O::SetTexture(red),
            O::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
        ];
        assert!(!needs_native(&alpha, &probe, &glyph));
        let mut actual_nonred = glyph.clone();
        actual_nonred[1] = O::SetTexture(bgra);
        assert!(needs_native(&alpha, &probe, &actual_nonred));
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn public_normalizer_original_fixed_table_is_pruned_before_real_chunk_admission() {
        use crate::fixed_test_fixtures as f;
        let case=f::fixed_native_case(false);
        let native=case.operations.iter().find_map(|o|if let c::Operation::SetFixedNativePipeline(p)=o {Some(p)} else {None}).unwrap();
        let original=ir::RenderPipelineDesc::new(ir::TextureFormat::Rgba16Float,ir::PrimitiveTopology::TriangleStrip,
            native.descriptor.vertex_buffers()[0].clone(),ir::FragmentProgram::TextureVertexColor(ir::TextureSampleMode::AlphaMask),
            ir::BlendState::SOURCE_OVER_STRAIGHT_ALPHA,ir::RasterState::new(ir::CullMode::Front,ir::FrontFace::Clockwise)).unwrap();
        let mut pipelines=vec![c::PipelineMeta {id:c::PipelineId::new(1),descriptor:original}];
        let caps=c::Capabilities::a618(512*1024,4096);
        // This is the precise old public-submit failure: native operations
        // coexist with original fixed metadata, and strict pack admission runs
        // before any operation. A native-only fixture with [] missed it.
        assert!(c::compile(c::CompileInput {capabilities:caps,resources:&case.resources,pipelines:&pipelines,operations:&case.operations}).is_err());
        retain_effective_fixed_pipelines(&mut pipelines,&case.operations);
        assert!(pipelines.is_empty());
        let mut external=Vec::new();
        for r in &case.resources {
            external.push(BoundObject {object:c::ObjectRef::External(r.id),attachment_token:u64::from(r.id.raw())+1,allocation_offset:0,size:r.size});
            if [f::VS,f::FS].contains(&r.id) {external.push(BoundObject {object:c::ObjectRef::NativeShader(r.id),attachment_token:u64::from(r.id.raw())+1,allocation_offset:0,size:r.size});}
        }
        let (prepared,_)=crate::preparation::prepare_chunk(caps,&case.resources,&pipelines,&case.operations,&external,0,2*1024*1024).unwrap();
        assert!(!prepared.compiled.words.is_empty());
        // A still-used canonical descriptor is retained; its validation is
        // neither skipped nor relaxed by the lowering.
        let valid=super::reachability_tests::pipeline(ir::FragmentProgram::Texture(ir::TextureSampleMode::Rgba));
        pipelines.push(valid.clone());
        retain_effective_fixed_pipelines(&mut pipelines,&[c::Operation::SetPipeline(valid.id)]);
        assert_eq!(pipelines.len(),1);
    }
}

#[cfg(test)]
mod nv12_tests {
    use super::*;
    #[test]
    fn nv12_all_conversions_compile_wgsl_and_spirv_and_have_distinct_complete_keys() {
        use ir::{ChromaLocation as C, YcbcrMatrix as M, YcbcrRange as R};
        let d=super::reachability_tests::pipeline(ir::FragmentProgram::Texture(ir::TextureSampleMode::Rgba)).descriptor;
        let mut keys=Vec::new();
        for matrix in [M::Bt601,M::Bt709] {for range in [R::Limited,R::Full] {
            for chroma_x in [C::Cosited,C::Midpoint] {for chroma_y in [C::Cosited,C::Midpoint] {
                let conversion=ir::YcbcrConversion {matrix,range,chroma_x,chroma_y};
                let key=FixedKey {pipeline:c::PipelineId::new(1),conversion:Some(conversion),sampler:None};
                assert!(!keys.contains(&key));keys.push(key);
                let source=shader_source_with_conversion(&d,Some(conversion)).unwrap();
                let (_,descriptor,map)=descriptor(&d,&source).unwrap();
                let mut shaders=Vec::new();
                for (stage,entry) in [(Stage::Vertex,"vs"),(Stage::Fragment,"fs")] {
                    let shader=sgfx_shader_ir3::compile_with_bindings(Source::Wgsl(&source),stage,entry,&map).unwrap();
                    let normalized=BindingMap::new(if stage==Stage::Fragment {vec![
                        BindingAssignment {group:2,binding:0,kind:ResourceKind::Texture,slot:0},
                        BindingAssignment {group:3,binding:0,kind:ResourceKind::Sampler,slot:0},
                    ]} else {vec![]}).unwrap();
                    let round=sgfx_shader_ir3::compile_with_bindings(Source::SpirV(&shader.spirv),stage,entry,&normalized).unwrap();
                    assert_eq!(shader.allocation,round.allocation);
                    shaders.push(Arc::new(shader));
                }
                let pipeline=c::NativeRenderPipeline {descriptor,
                    vertex:c::NativeShader {object:c::ObjectId::new(10),private_memory:None,artifact:shaders[0].clone()},
                    fragment:c::NativeShader {object:c::ObjectId::new(11),private_memory:None,artifact:shaders[1].clone()}};
                c::validate_native_render_pipeline(&pipeline).unwrap();
                if conversion==(ir::YcbcrConversion {matrix:M::Bt709,range:R::Limited,chroma_x:C::Midpoint,chroma_y:C::Midpoint}) {
                    if let Some(dir)=std::env::var_os("SGFX_NV12_FIXTURE_OUT") {
                        let dir=std::path::PathBuf::from(dir);std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("fixed_nv12.wgsl"),&source).unwrap();
                        for (name,shader) in [("fixed_nv12_vs",&pipeline.vertex.artifact),("fixed_nv12_fs",&pipeline.fragment.artifact)] {
                            std::fs::write(dir.join(format!("{name}.bin")),&shader.allocation).unwrap();
                            std::fs::write(dir.join(format!("{name}.json")),serde_json::to_vec_pretty(&shader.metadata).unwrap()).unwrap();
                            std::fs::write(dir.join(format!("{name}.bindings.json")),serde_json::to_vec_pretty(&shader.bindings).unwrap()).unwrap();
                            std::fs::write(dir.join(format!("{name}.spv")),shader.spirv.iter().flat_map(|w|w.to_le_bytes()).collect::<Vec<_>>()).unwrap();
                        }
                    }
                }
            }}
        }}
        assert_eq!(keys.len(),16);
        assert!(!keys.contains(&FixedKey {pipeline:c::PipelineId::new(1),conversion:None}));
        assert!(!keys.contains(&FixedKey {pipeline:c::PipelineId::new(2),conversion:keys[0].conversion}));
    }
}
