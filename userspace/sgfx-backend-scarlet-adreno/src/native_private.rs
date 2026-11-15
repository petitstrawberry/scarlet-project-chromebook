//! Cache-owned scratch storage for an immutable compiled shader/layout.
//! Integrated by programmable::ShaderRecord; never allocated per draw or placed
//! in the upload arena. The submission retains the Arc after cache retirement.
use crate::{ContextInner, IrSubmitError, ir, resource::RawBuffer, wire::BoundObject};
use alloc::{sync::Arc, vec::Vec};
use sgfx_codegen_adreno_a6xx as codegen;

pub(crate) struct PrivateMemoryBuffer {
    pub(crate) native: codegen::NativePrivateMemory,
    allocation: Arc<RawBuffer>,
}
impl PrivateMemoryBuffer {
    /// `object` must be a newly reserved compiler-local identity, different
    /// from all code/user allocations. The attachment allocator is page-aligned.
    pub(crate) fn create(
        context: &Arc<ContextInner>,
        object: codegen::ObjectId,
        metadata: &sgfx_shader_ir3::Metadata,
    ) -> Result<Option<Self>, IrSubmitError> {
        let Some(layout) = codegen::PrivateMemoryLayout::for_shader(metadata)? else {
            return Ok(None);
        };
        let allocation = Arc::new(RawBuffer::create(context, layout.allocation_bytes)?);
        // GPU private-memory stores/loads are ordered by the compiler. Scratch
        // is not a public buffer mirror and needs no per-submit CPU upload.
        Ok(Some(Self {
            native: codegen::NativePrivateMemory { object },
            allocation,
        }))
    }

    pub(crate) fn append(
        &self,
        resources: &mut Vec<codegen::ResourceMeta>,
        bindings: &mut Vec<BoundObject>,
    ) -> Result<(), IrSubmitError> {
        if resources.iter().any(|r| r.id == self.native.object) {
            return Ok(());
        }
        resources
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        bindings
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        resources.push(codegen::ResourceMeta {
            id: self.native.object,
            size: self.allocation.logical_size,
            kind: codegen::ResourceKind::Buffer {
                usage: ir::BufferUsage::STORAGE,
            },
        });
        bindings.push(BoundObject {
            object: codegen::ObjectRef::External(self.native.object),
            attachment_token: self.allocation.attachment_token,
            allocation_offset: 0,
            size: self.allocation.logical_size,
        });
        Ok(())
    }

    pub(crate) fn retain(
        &self,
        bindings: &[BoundObject],
        output: &mut Vec<Arc<RawBuffer>>,
    ) -> Result<(), IrSubmitError> {
        if bindings
            .iter()
            .any(|b| b.attachment_token == self.allocation.attachment_token)
            && !output.iter().any(|b| Arc::ptr_eq(b, &self.allocation))
        {
            output
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            output.push(Arc::clone(&self.allocation));
        }
        Ok(())
    }
}
