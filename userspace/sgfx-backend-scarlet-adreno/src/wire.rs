//! Translation from pure codegen artifacts to the kernel submit wire.

use alloc::{vec, vec::Vec};

use adreno_a6xx_submit_wire as submit_wire;
use sgfx_codegen_adreno_a6xx::{
    Access, AddressEncoding as CodegenAddressEncoding, ObjectRef, RelocatablePm4,
};

use crate::{IrSubmitError, UnsupportedIrFeature};

#[derive(Clone, Copy)]
pub(crate) struct BoundObject {
    pub(crate) object: ObjectRef,
    pub(crate) attachment_token: u64,
    pub(crate) allocation_offset: u64,
    pub(crate) size: u64,
}

pub(crate) fn encode(
    compiled: &RelocatablePm4,
    bindings: &[BoundObject],
) -> Result<Vec<u8>, IrSubmitError> {
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(compiled.accesses.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;

    for access in &compiled.accesses {
        let bound = binding(bindings, access.object)?;
        let access_end =
            access
                .offset
                .checked_add(access.size)
                .ok_or(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::ResourceState,
                ))?;
        if access.size == 0 || access_end > bound.size {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        resources.push(submit_wire::Resource {
            attachment_token: bound.attachment_token,
            range_offset: bound.allocation_offset.checked_add(access.offset).ok_or(
                IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
            )?,
            range_size: access.size,
            access: wire_access(access.access)?,
        });
    }

    let mut relocations = Vec::new();
    relocations
        .try_reserve_exact(compiled.fixups.len())
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    for fixup in &compiled.fixups {
        if let ObjectRef::CanonicalShader(variant) = fixup.object {
            relocations.push(submit_wire::Relocation {
                pm4_word_offset: fixup.word_offset,
                source: submit_wire::RelocationSource::CanonicalShader(variant),
                resource_offset: fixup.object_offset,
                required_size: fixup.required_size,
                access: wire_access(fixup.access)?,
                encoding: match fixup.encoding {
                    CodegenAddressEncoding::GpuVa64 => submit_wire::AddressEncoding::GpuVa64,
                    CodegenAddressEncoding::GpuVa49TexDescriptor => {
                        return Err(IrSubmitError::Unsupported(
                            UnsupportedIrFeature::ResourceState,
                        ));
                    }
                },
            });
            continue;
        }
        let bound = binding(bindings, fixup.object)?;
        let required_end = fixup.object_offset.checked_add(fixup.required_size).ok_or(
            IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
        )?;
        if fixup.required_size == 0 || required_end > bound.size {
            return Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ));
        }
        let fixup_access = wire_access(fixup.access)?;
        let resource_index = compiled
            .accesses
            .iter()
            .enumerate()
            .find(|(_, access)| {
                if access.object != fixup.object || !access.access.contains(fixup.access) {
                    return false;
                }
                let Some(end) = access.offset.checked_add(access.size) else {
                    return false;
                };
                access.offset <= fixup.object_offset && required_end <= end
            })
            .map(|(index, _)| index)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let resource = resources
            .get(resource_index)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let absolute_offset = bound
            .allocation_offset
            .checked_add(fixup.object_offset)
            .ok_or(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState,
            ))?;
        let resource_offset = absolute_offset.checked_sub(resource.range_offset).ok_or(
            IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState),
        )?;
        relocations.push(submit_wire::Relocation {
            pm4_word_offset: fixup.word_offset,
            source: submit_wire::RelocationSource::Attachment(
                u32::try_from(resource_index)
                    .map_err(|_| IrSubmitError::Unsupported(UnsupportedIrFeature::ResourceState))?,
            ),
            resource_offset,
            required_size: fixup.required_size,
            access: fixup_access,
            encoding: match fixup.encoding {
                CodegenAddressEncoding::GpuVa64 => submit_wire::AddressEncoding::GpuVa64,
                CodegenAddressEncoding::GpuVa49TexDescriptor => {
                    submit_wire::AddressEncoding::GpuVa49TexDescriptor
                }
            },
        });
    }

    let submit = submit_wire::Submit {
        pm4: &compiled.words,
        resources: &resources,
        relocations: &relocations,
    };
    let encoded_len = submit_wire::encoded_len(submit)?;
    let mut output = vec![0; encoded_len];
    submit_wire::encode(submit, &mut output)?;
    Ok(output)
}

fn binding(bindings: &[BoundObject], object: ObjectRef) -> Result<BoundObject, IrSubmitError> {
    bindings
        .iter()
        .copied()
        .find(|binding| binding.object == object)
        .ok_or(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ))
}

fn wire_access(access: Access) -> Result<u32, IrSubmitError> {
    let mut result = 0;
    if access.contains(Access::READ) {
        result |= submit_wire::ACCESS_READ;
    }
    if access.contains(Access::WRITE) {
        result |= submit_wire::ACCESS_WRITE;
    }
    if result == 0 {
        Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ResourceState,
        ))
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use sgfx_codegen_adreno_a6xx::{
        Access, AddressEncoding, Capabilities, CompileInput, ImageMeta, ImageModifier, ObjectId,
        ObjectRef, Operation, PipelineId, PipelineMeta, PlaneLayout, RelocatablePm4, RenderPass,
        ResourceAccess, ResourceKind, ResourceMeta, SymbolicAddress, compile,
    };
    use sgfx_core::ir::{
        AddressMode, BlendState, BufferUsage, Color, CullMode, DrawUniforms, Extent2D, FilterMode,
        FragmentProgram, FrontFace, LoadOp, PixelRect, PrimitiveTopology, RasterState,
        RenderPipelineDesc, SamplerDesc, StoreOp, TextureFormat, TextureSampleMode, TextureUsage,
        Transform, VertexAttribute, VertexBufferLayout, VertexFormat,
    };

    use super::{BoundObject, encode};
    use crate::{IrSubmitError, UnsupportedIrFeature};

    fn one_read_fixup() -> (ObjectRef, RelocatablePm4) {
        let object = ObjectRef::External(ObjectId::new(7));
        let compiled = RelocatablePm4 {
            words: vec![0x7000_8026, 0, 0],
            fixups: vec![SymbolicAddress {
                word_offset: 1,
                object,
                object_offset: 64,
                required_size: 16,
                access: Access::READ,
                encoding: AddressEncoding::GpuVa64,
            }],
            accesses: vec![ResourceAccess {
                object,
                offset: 32,
                size: 128,
                access: Access::READ,
            }],
            generated_objects: vec![],
        };
        (object, compiled)
    }

    #[test]
    fn maps_symbolic_object_to_attachment_token() {
        let (object, compiled) = one_read_fixup();
        let encoded = encode(
            &compiled,
            &[BoundObject {
                object,
                attachment_token: 99,
                allocation_offset: 4_096,
                size: 512,
            }],
        )
        .expect("wire encoding");
        let decoded = adreno_a6xx_submit_wire::decode(&encoded).expect("canonical wire");
        let resource = decoded.resource(0).expect("resource");
        let relocation = decoded.relocation(0).expect("relocation");
        assert_eq!(resource.attachment_token, 99);
        assert_eq!(resource.range_offset, 4_128);
        assert_eq!(relocation.resource_offset, 32);
    }

    #[test]
    fn rejects_unbound_symbolic_object() {
        let (_, compiled) = one_read_fixup();
        assert!(matches!(
            encode(&compiled, &[]),
            Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState
            ))
        ));
    }

    #[test]
    fn rejects_fixup_outside_bound_allocation() {
        let (object, compiled) = one_read_fixup();
        assert!(matches!(
            encode(
                &compiled,
                &[BoundObject {
                    object,
                    attachment_token: 99,
                    allocation_offset: 0,
                    size: 72,
                }],
            ),
            Err(IrSubmitError::Unsupported(
                UnsupportedIrFeature::ResourceState
            ))
        ));
    }

    #[test]
    fn production_wire_encodes_textured_draw_shader_and_descriptor_sources() {
        let target = ObjectId::new(0);
        let texture = ObjectId::new(1);
        let vertices = ObjectId::new(2);
        let pipeline_id = PipelineId::new(0);
        let image = |id, usage| ResourceMeta {
            id,
            size: 1024,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(16, 16).unwrap(),
                usage,
                modifier: ImageModifier::Linear,
                planes: vec![PlaneLayout {
                    offset: 0,
                    stride: 64,
                    size: 1024,
                }],
            }),
        };
        let resources = [
            image(target, TextureUsage::RENDER_ATTACHMENT),
            image(texture, TextureUsage::SAMPLED),
            ResourceMeta {
                id: vertices,
                size: 48,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
        ];
        let pipeline = PipelineMeta {
            id: pipeline_id,
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    16,
                    vec![
                        VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x2, 8),
                    ],
                )
                .unwrap(),
                FragmentProgram::Texture(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        };
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(pipeline_id),
            Operation::SetVertexBuffer {
                buffer: vertices,
                offset: 0,
            },
            Operation::SetTexture(texture),
            Operation::SetSampler(SamplerDesc::new(
                FilterMode::Nearest,
                FilterMode::Nearest,
                AddressMode::ClampToEdge,
                AddressMode::ClampToEdge,
            )),
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let compiled = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: core::slice::from_ref(&pipeline),
            operations: &operations,
        })
        .unwrap();
        let bytes = encode(
            &compiled,
            &[
                BoundObject {
                    object: ObjectRef::External(target),
                    attachment_token: 10,
                    allocation_offset: 0,
                    size: 1024,
                },
                BoundObject {
                    object: ObjectRef::External(texture),
                    attachment_token: 11,
                    allocation_offset: 0,
                    size: 1024,
                },
                BoundObject {
                    object: ObjectRef::External(vertices),
                    attachment_token: 12,
                    allocation_offset: 0,
                    size: 48,
                },
            ],
        )
        .unwrap();
        let decoded = adreno_a6xx_submit_wire::decode(&bytes).unwrap();
        let relocations: alloc::vec::Vec<_> = (0..decoded.relocation_len())
            .map(|index| decoded.relocation(index).unwrap())
            .collect();
        assert_eq!(
            relocations
                .iter()
                .filter(|relocation| matches!(
                    relocation.source,
                    adreno_a6xx_submit_wire::RelocationSource::CanonicalShader(_)
                ))
                .count(),
            2
        );
        assert_eq!(
            relocations
                .iter()
                .filter(|relocation| relocation.encoding
                    == adreno_a6xx_submit_wire::AddressEncoding::GpuVa49TexDescriptor)
                .count(),
            1
        );
        assert!(relocations.iter().any(|relocation| matches!(
            relocation.source,
            adreno_a6xx_submit_wire::RelocationSource::Attachment(_)
        ) && relocation.required_size == 1024));
    }
}
