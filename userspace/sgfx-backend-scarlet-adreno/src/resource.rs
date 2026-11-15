//! Context-local GPU resources and immutable image layouts.

use alloc::{rc::Rc, sync::Arc, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use adreno_a6xx_layout::{
    DEPTH32_LAYER_ALIGNMENT, IMAGE_MODIFIER_TILE6_3_DEPTH, is_depth32_tile6_3_layout,
};
use gpu_raw::{
    GPU_BUFFER_FLAG_CPU_VISIBLE, GPU_IMAGE_FORMAT_BGRA8_UNORM, GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
    GPU_IMAGE_FORMAT_RGBA16_FLOAT, GPU_IMAGE_FORMAT_RGBA32_FLOAT, GPU_IMAGE_MODIFIER_LINEAR,
    GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT, GPU_IMAGE_USAGE_PRESENTABLE,
    GPU_IMAGE_USAGE_RENDER_TARGET, GPU_IMAGE_USAGE_SAMPLED, GPU_IMAGE_USAGE_TRANSFER_DST,
    GPU_IMAGE_USAGE_TRANSFER_SRC, GpuBuffer, GpuImage, GpuImageLayout,
};
#[cfg(feature = "std")]
use scarlet_os::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};
#[cfg(not(feature = "std"))]
use std::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};

use crate::Handle;
use crate::{ContextInner, HandleError, HandleResult, Image, IrSubmitError, ir};
use sgfx_core::backend::{Completion, CompletionStatus};

struct BufferMirror {
    id: ir::BufferId,
    bytes: Vec<u8>,
}

enum MirrorUpdate<'data> {
    Write {
        slot: usize,
        offset: usize,
        data: &'data [u8],
    },
    Copy {
        source: usize,
        destination: usize,
        source_offset: usize,
        destination_offset: usize,
        size: usize,
    },
}

/// Prepared before admission. Dropping this plan publishes no logical writes.
pub(crate) struct BufferPublication<'data> {
    updates: Vec<MirrorUpdate<'data>>,
    written: Vec<usize>,
    gpu_writes: bool,
}

impl<'data> BufferPublication<'data> {
    fn prepare<'r>(
        commands: &ir::CommandBuffer<'r, 'data>,
        mut ensure: impl FnMut(ir::BufferRef<'r>) -> Result<(), IrSubmitError>,
    ) -> Result<Self, IrSubmitError> {
        let table = commands.resources();
        let mut plan = Self {
            updates: Vec::new(),
            written: Vec::new(),
            gpu_writes: false,
        };
        for command in commands.commands() {
            match command {
                ir::Command::WriteBuffer {
                    buffer,
                    offset,
                    data,
                } => {
                    let descriptor = table.buffer(*buffer)?;
                    let offset = read_range(descriptor.size(), *offset, data.len())?;
                    ensure(*buffer)?;
                    plan.mark_written(buffer.slot())?;
                    plan.updates
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    plan.updates.push(MirrorUpdate::Write {
                        slot: buffer.slot(),
                        offset,
                        data,
                    });
                }
                ir::Command::CopyBufferToBuffer {
                    source,
                    source_offset,
                    destination,
                    destination_offset,
                    size,
                } => {
                    let size = usize::try_from(*size).map_err(|_| HandleError::InvalidParameter)?;
                    let source_offset =
                        read_range(table.buffer(*source)?.size(), *source_offset, size)?;
                    let destination_offset = read_range(
                        table.buffer(*destination)?.size(),
                        *destination_offset,
                        size,
                    )?;
                    ensure(*source)?;
                    ensure(*destination)?;
                    plan.mark_written(destination.slot())?;
                    plan.updates
                        .try_reserve(1)
                        .map_err(|_| IrSubmitError::OutOfMemory)?;
                    plan.updates.push(MirrorUpdate::Copy {
                        source: source.slot(),
                        destination: destination.slot(),
                        source_offset,
                        destination_offset,
                        size,
                    });
                }
                ir::Command::SetBindGroup { bind_group, .. } => {
                    let group = table.bind_group_shared(*bind_group)?;
                    for entry in group.entries() {
                        let layout = group
                            .layout()
                            .entries()
                            .iter()
                            .find(|layout| layout.binding() == entry.binding())
                            .ok_or(ir::Error::BindingLayoutMismatch)?;
                        if !matches!(
                            layout.ty(),
                            ir::BindingType::StorageBuffer { read_only: false }
                        ) {
                            continue;
                        }
                        let ir::BindingResource::Buffer { buffer, .. } = entry.resource() else {
                            return Err(ir::Error::BindingLayoutMismatch.into());
                        };
                        let buffer = table.buffer_ref(buffer)?;
                        ensure(buffer)?;
                        plan.mark_written(buffer.slot())?;
                        // Conservative for unused/rebound writable groups. Uniform,
                        // vertex and read-only storage bindings never cause a wait.
                        plan.gpu_writes = true;
                    }
                }
                _ => {}
            }
        }
        Ok(plan)
    }

    fn mark_written(&mut self, slot: usize) -> Result<(), IrSubmitError> {
        if !self.written.contains(&slot) {
            self.written
                .try_reserve(1)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            self.written.push(slot);
        }
        Ok(())
    }

    /// No allocation after acceptance. Shader writes require final physical
    /// values for every destination, including copies following those writes.
    fn publish(
        self,
        mirrors: &mut [Option<BufferMirror>],
        wait: impl FnOnce() -> Result<(), IrSubmitError>,
        mut refresh: impl FnMut(usize, &mut [u8]) -> Result<(), IrSubmitError>,
    ) -> Result<(), IrSubmitError> {
        if self.gpu_writes {
            wait()?;
            for slot in self.written {
                let mirror = mirrors
                    .get_mut(slot)
                    .and_then(Option::as_mut)
                    .ok_or(IrSubmitError::ResourceTableMismatch)?;
                refresh(slot, &mut mirror.bytes)?;
            }
            return Ok(());
        }
        for update in self.updates {
            match update {
                MirrorUpdate::Write { slot, offset, data } => {
                    let mirror = mirrors
                        .get_mut(slot)
                        .and_then(Option::as_mut)
                        .ok_or(IrSubmitError::ResourceTableMismatch)?;
                    let end = offset
                        .checked_add(data.len())
                        .ok_or(HandleError::InvalidParameter)?;
                    mirror
                        .bytes
                        .get_mut(offset..end)
                        .ok_or(HandleError::InvalidParameter)?
                        .copy_from_slice(data);
                }
                MirrorUpdate::Copy {
                    source,
                    destination,
                    source_offset,
                    destination_offset,
                    size,
                } => {
                    if source == destination {
                        let mirror = mirrors
                            .get_mut(source)
                            .and_then(Option::as_mut)
                            .ok_or(IrSubmitError::ResourceTableMismatch)?;
                        read_range(mirror.bytes.len() as u64, source_offset as u64, size)?;
                        read_range(mirror.bytes.len() as u64, destination_offset as u64, size)?;
                        mirror
                            .bytes
                            .copy_within(source_offset..source_offset + size, destination_offset);
                    } else {
                        let (source, destination) = if source < destination {
                            let (left, right) = mirrors.split_at_mut(destination);
                            (left[source].as_ref(), right[0].as_mut())
                        } else {
                            let (left, right) = mirrors.split_at_mut(source);
                            (right[0].as_ref(), left[destination].as_mut())
                        };
                        let source = source.ok_or(IrSubmitError::ResourceTableMismatch)?;
                        let destination =
                            destination.ok_or(IrSubmitError::ResourceTableMismatch)?;
                        read_range(source.bytes.len() as u64, source_offset as u64, size)?;
                        read_range(
                            destination.bytes.len() as u64,
                            destination_offset as u64,
                            size,
                        )?;
                        destination.bytes[destination_offset..destination_offset + size]
                            .copy_from_slice(&source.bytes[source_offset..source_offset + size]);
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) struct RawImage {
    pub(crate) conversion: Option<ir::YcbcrConversion>,
    pub(crate) texture: Option<adreno_a6xx_layout::LinearTextureLayout>,
    pub(crate) raw: GpuImage,
    pub(crate) attachment_token: u64,
    pub(crate) layout: GpuImageLayout,
    pub(crate) logical_format: ir::TextureFormat,
    context_id: i32,
    context: Arc<ContextInner>,
    attached: AtomicBool,
}

impl RawImage {
    pub(crate) fn create_present(
        context: &Arc<ContextInner>,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        if width == 0 || height == 0 {
            return Err(HandleError::InvalidParameter);
        }
        let usage = GPU_IMAGE_USAGE_RENDER_TARGET
            | GPU_IMAGE_USAGE_PRESENTABLE
            | GPU_IMAGE_USAGE_SAMPLED
            | GPU_IMAGE_USAGE_TRANSFER_SRC;
        let raw = context.device.gpu.create_image_with_format_and_usage(
            GPU_IMAGE_FORMAT_BGRA8_UNORM,
            width,
            height,
            usage,
        )?;
        Self::finish_create(context, raw, ir::TextureFormat::Bgra8Unorm, width, height)
    }

    fn create_logical(
        context: &Arc<ContextInner>,
        descriptor: ir::TextureDesc,
    ) -> HandleResult<Self> {
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        let (format, usage) = image_create_parameters(descriptor)?;
        let raw = context
            .device
            .gpu
            .create_layered_image_with_format_and_usage(
                format,
                width,
                height,
                usage,
                descriptor.mip_level_count(),
                descriptor.array_layer_count(),
                // A6xx uses the same layer-first storage for arrays and cubes.
                // Scarlet's cube allocation flag describes exactly six faces;
                // larger cube-compatible arrays use ordinary layered storage.
                descriptor.cube_compatible() && descriptor.array_layer_count() == 6,
            )?;
        Self::finish_create(context, raw, descriptor.format(), width, height)
    }

    fn import_sampled(
        context: &Arc<ContextInner>,
        handle: Handle,
        descriptor: ir::TextureDesc,
        conversion: Option<ir::YcbcrConversion>,
    ) -> HandleResult<Self> {
        let logical = if conversion.is_some() {
            ir::TextureFormat::Nv12
        } else {
            ir::TextureFormat::Bgra8Unorm
        };
        let physical = if conversion.is_some() {
            gpu_raw::GPU_IMAGE_FORMAT_NV12
        } else {
            GPU_IMAGE_FORMAT_BGRA8_UNORM
        };
        if descriptor.format() != logical
            || !descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::PRESENT)
        {
            return Err(HandleError::InvalidParameter);
        }
        let raw = if let Some(c) = conversion {
            use gpu_raw::shared_image::*;
            context.device.gpu.import_shared_image(
                &handle,
                ImageColor {
                    matrix: match c.matrix {
                        ir::YcbcrMatrix::Bt601 => COLOR_MATRIX_BT601,
                        ir::YcbcrMatrix::Bt709 => COLOR_MATRIX_BT709,
                    },
                    range: match c.range {
                        ir::YcbcrRange::Limited => COLOR_RANGE_LIMITED,
                        ir::YcbcrRange::Full => COLOR_RANGE_FULL,
                    },
                    chroma_x: match c.chroma_x {
                        ir::ChromaLocation::Cosited => CHROMA_COSITED,
                        ir::ChromaLocation::Midpoint => CHROMA_MIDPOINT,
                    },
                    chroma_y: match c.chroma_y {
                        ir::ChromaLocation::Cosited => CHROMA_COSITED,
                        ir::ChromaLocation::Midpoint => CHROMA_MIDPOINT,
                    },
                    ..Default::default()
                },
            )?
        } else {
            GpuImage::from_handle(handle)?
        };
        let info = raw.query()?;
        let shape = raw.query_texture()?;
        if shape.mip_levels != descriptor.mip_level_count()
            || shape.array_layers != descriptor.array_layer_count()
        {
            return Err(HandleError::InvalidParameter);
        }
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        if info.format != physical
            || info.usage & GPU_IMAGE_USAGE_SAMPLED == 0
            || info.width != width
            || info.height != height
        {
            return Err(HandleError::InvalidParameter);
        }
        let mut image = Self::finish_create(context, raw, logical, width, height)?;
        image.conversion = conversion;
        Ok(image)
    }

    fn finish_create(
        context: &Arc<ContextInner>,
        raw: GpuImage,
        logical_format: ir::TextureFormat,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        let layout = raw.query_layout()?;
        let shape = raw.query_texture()?;
        let texture = validate_texture_storage(
            &layout,
            width,
            height,
            shape.mip_levels,
            shape.array_layers,
            logical_format,
        )?;
        let attachment_token = context.raw.attach_image(&raw)?;
        if attachment_token == 0 {
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            conversion: None,
            texture,
            raw,
            attachment_token,
            layout,
            logical_format,
            context_id: context.raw.as_handle().as_raw(),
            context: Arc::clone(context),
            attached: AtomicBool::new(true),
        })
    }

    pub(crate) fn allocation_size(&self) -> u64 {
        self.layout.total_size
    }

    pub(crate) fn belongs_to(&self, context: &ContextInner) -> bool {
        self.context_id == context.raw.as_handle().as_raw()
    }

    fn detach(&self) -> HandleResult<()> {
        if self.attached.swap(false, Ordering::AcqRel) {
            if let Err(error) = self.context.raw.detach_image(&self.raw) {
                self.attached.store(true, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for RawImage {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

pub(crate) struct RawBuffer {
    pub(crate) raw: GpuBuffer,
    pub(crate) attachment_token: u64,
    pub(crate) logical_size: u64,
    context: Arc<ContextInner>,
    attached: AtomicBool,
    mapping_address: usize,
    mapping_len: usize,
}

impl RawBuffer {
    pub(crate) fn create(context: &Arc<ContextInner>, logical_size: u64) -> HandleResult<Self> {
        if logical_size == 0 {
            return Err(HandleError::InvalidParameter);
        }
        let raw = context
            .device
            .gpu
            .create_buffer(logical_size, GPU_BUFFER_FLAG_CPU_VISIBLE)?;
        if !raw.cpu_visible() || raw.allocated_size() < logical_size {
            return Err(HandleError::Unsupported);
        }
        let mapping_len =
            usize::try_from(raw.allocated_size()).map_err(|_| HandleError::InvalidParameter)?;
        let mapping = raw.as_handle().as_memory_mapping()?;
        // SAFETY: This creates a fresh mapping. RawBuffer retains the allocation,
        // bounds CPU accesses to it and owns the mapping until teardown.
        let mapping_address =
            unsafe { mapping.mmap(0, mapping_len, prot::READ | prot::WRITE, flags::SHARED, 0) }
                .map_err(|_| HandleError::SystemError(-1))?;
        let attachment_token = match context.raw.attach_buffer(&raw) {
            Ok(token) => token,
            Err(error) => {
                // SAFETY: Attachment failed before any buffer access was exposed.
                let _ = unsafe { MemoryMappingOps::munmap(mapping_address, mapping_len) };
                return Err(error);
            }
        };
        if attachment_token == 0 {
            let _ = context.raw.detach_buffer(&raw);
            // SAFETY: The failed attachment was detached and no CPU view escaped.
            let _ = unsafe { MemoryMappingOps::munmap(mapping_address, mapping_len) };
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            raw,
            attachment_token,
            logical_size,
            context: Arc::clone(context),
            attached: AtomicBool::new(true),
            mapping_address,
            mapping_len,
        })
    }

    pub(crate) fn write(&self, offset: u64, bytes: &[u8]) -> HandleResult<()> {
        let byte_len = u64::try_from(bytes.len()).map_err(|_| HandleError::InvalidParameter)?;
        let end = offset
            .checked_add(byte_len)
            .ok_or(HandleError::InvalidParameter)?;
        if bytes.is_empty() || end > self.logical_size {
            return Err(HandleError::InvalidParameter);
        }
        let destination_offset =
            usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)?;

        // SAFETY: `create` retains a writable mapping of `mapping_len` bytes
        // for the complete RawBuffer lifetime. The checked logical range above
        // is no larger than that backing allocation and the source slice
        // remains valid for this copy.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.mapping_address as *mut u8).add(destination_offset),
                bytes.len(),
            );
        }
        Ok(())
    }

    pub(crate) fn read_u32(&self, offset: u64) -> HandleResult<u32> {
        let end = offset
            .checked_add(core::mem::size_of::<u32>() as u64)
            .ok_or(HandleError::InvalidParameter)?;
        if !offset.is_multiple_of(core::mem::align_of::<u32>() as u64) || end > self.logical_size {
            return Err(HandleError::InvalidParameter);
        }
        let source_offset = usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)?;

        // SAFETY: the retained mapping is readable, the checked range lies
        // within the backing allocation, and generated CCU sequence objects
        // are at least four-byte aligned.
        let value = unsafe {
            u32::from_le(ptr::read_volatile(
                (self.mapping_address as *const u8)
                    .add(source_offset)
                    .cast::<u32>(),
            ))
        };
        Ok(value)
    }

    /// The caller must observe completion before reading the coherent mapping.
    fn read_into(&self, offset: u64, output: &mut [u8]) -> HandleResult<()> {
        let start = read_range(self.logical_size, offset, output.len())?;
        if output.is_empty() {
            return Ok(());
        }
        core::sync::atomic::fence(Ordering::Acquire);
        // SAFETY: the checked range is inside this retained mapping; output is
        // a distinct, exclusively borrowed caller allocation. GPU/worker writes
        // have completed before this method is called.
        unsafe {
            ptr::copy_nonoverlapping(
                (self.mapping_address as *const u8).add(start),
                output.as_mut_ptr(),
                output.len(),
            );
        }
        Ok(())
    }

    fn detach(&self) -> HandleResult<()> {
        if self.attached.swap(false, Ordering::AcqRel) {
            if let Err(error) = self.context.raw.detach_buffer(&self.raw) {
                self.attached.store(true, Ordering::Release);
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        let _ = self.detach();
        // The mapping must be retired before the capability-backed allocation
        // is dropped. Teardown cannot report an error, and the kernel still
        // revokes the address space when the process exits.
        // SAFETY: RawBuffer owns this CPU mapping and no borrowed CPU view can
        // outlive it. Kernel-held GPU backing is independent of the CPU mapping.
        let _ = unsafe { MemoryMappingOps::munmap(self.mapping_address, self.mapping_len) };
    }
}

pub(crate) struct ContextResources {
    pub(crate) resources: Rc<ir::ResourceTable>,
    pub(crate) context: Arc<ContextInner>,
    pub(crate) images: Vec<Option<Arc<RawImage>>>,
    pub(crate) buffers: Vec<Option<Arc<RawBuffer>>>,
    buffer_mirrors: Vec<Option<BufferMirror>>,
    buffer_publication_failed: bool,
    buffer_publication_receipt: Option<crate::Submission>,
    scratch: Option<RawBuffer>,
    pub(crate) next_async_arena: usize,
    pub(crate) async_arenas: Vec<Arc<crate::asynchronous::UploadArena>>,
    pub(crate) programs: crate::programmable::ProgramCache,
    #[cfg(feature = "programmable")]
    pub(crate) fixed: crate::fixed_native::FixedCache,
}

impl ContextResources {
    pub(crate) fn release_texture(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        let slot = self.resources.texture_ref(texture)?.slot();
        if let Some(image) = self.images[slot].as_ref() {
            image.detach()?;
        }
        self.images[slot] = None;
        Ok(())
    }

    pub(crate) fn release_buffer(&mut self, buffer: ir::BufferId) -> Result<(), IrSubmitError> {
        let slot = self.resources.buffer_ref(buffer)?.slot();
        if let Some(buffer) = self.buffers[slot].as_ref() {
            buffer.detach()?;
        }
        self.buffers[slot] = None;
        self.buffer_mirrors[slot] = None;
        Ok(())
    }

    pub(crate) fn new(
        resources: Rc<ir::ResourceTable>,
        context: Arc<ContextInner>,
    ) -> Result<Self, IrSubmitError> {
        Ok(Self {
            resources,
            context,
            images: empty_slots(ir::MAX_TEXTURES)?,
            buffers: empty_slots(ir::MAX_BUFFERS)?,
            buffer_mirrors: empty_slots(ir::MAX_BUFFERS)?,
            buffer_publication_failed: false,
            buffer_publication_receipt: None,
            scratch: None,
            next_async_arena: 0,
            async_arenas: Vec::new(),
            programs: crate::programmable::ProgramCache::default(),
            #[cfg(feature = "programmable")]
            fixed: crate::fixed_native::FixedCache::default(),
        })
    }

    pub(crate) fn map_present_image(
        &mut self,
        texture: ir::TextureId,
        image: Arc<Image>,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        self.validate_present_image(reference, &image)?;
        let slot = reference.slot();
        if self
            .images
            .get(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .is_some()
        {
            return Err(IrSubmitError::TextureAlreadyMapped);
        }
        if self
            .images
            .iter()
            .flatten()
            .any(|candidate| Arc::ptr_eq(candidate, &image.raw))
        {
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(Arc::clone(&image.raw));
        Ok(())
    }

    fn validate_present_image(
        &self,
        reference: ir::TextureRef<'_>,
        image: &Image,
    ) -> Result<(), IrSubmitError> {
        let descriptor = self.resources.texture(reference)?;
        let allowed = ir::TextureUsage::RENDER_ATTACHMENT
            | ir::TextureUsage::PRESENT
            | ir::TextureUsage::SAMPLED
            | ir::TextureUsage::COPY_SRC
            | ir::TextureUsage::COPY_DST;
        if descriptor.format() != ir::TextureFormat::Bgra8Unorm
            || descriptor.mip_level_count() != 1
            || descriptor.array_layer_count() != 1
            || !descriptor
                .usage()
                .contains(ir::TextureUsage::RENDER_ATTACHMENT)
            || !allowed.contains(descriptor.usage())
        {
            return Err(IrSubmitError::Unsupported(
                crate::UnsupportedIrFeature::ResourceState,
            ));
        }
        if image.raw.context_id != self.context_id() {
            return Err(IrSubmitError::ContextMismatch);
        }
        if descriptor.extent().width() != image.width
            || descriptor.extent().height() != image.height
        {
            return Err(IrSubmitError::TargetExtentMismatch);
        }
        Ok(())
    }

    /// Remove a shared-image mapping without revoking the external image owner.
    /// The public wrapper checks that this slot was explicitly mapped and waits
    /// for accepted work before calling. Arc lifetime owns the attachment.
    pub(crate) fn unmap_present_image(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
        let slot = self.resources.texture_ref(texture)?.slot();
        self.images[slot]
            .take()
            .ok_or(IrSubmitError::ImageNotMapped)?;
        Ok(())
    }

    pub(crate) fn import_sampled_image(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
        conversion: Option<ir::YcbcrConversion>,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        let descriptor = self.resources.texture(reference)?;
        let slot = reference.slot();
        if self
            .images
            .get(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .is_some()
        {
            return Err(IrSubmitError::TextureAlreadyMapped);
        }
        let image = Arc::new(RawImage::import_sampled(
            &self.context,
            handle,
            descriptor,
            conversion,
        )?);
        if self
            .images
            .iter()
            .flatten()
            .any(|candidate| candidate.attachment_token == image.attachment_token)
        {
            let _ = image.detach();
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(image);
        Ok(())
    }

    pub(crate) fn release_imported_image(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
        self.drain_async()?;
        let reference = self.resources.texture_ref(texture)?;
        let descriptor = self.resources.texture(reference)?;
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            return Err(IrSubmitError::Unsupported(
                crate::UnsupportedIrFeature::ResourceState,
            ));
        }
        let slot = reference.slot();
        let image = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?
            .take()
            .ok_or(IrSubmitError::ImageNotMapped)?;
        if let Err(error) = image.detach() {
            self.images[slot] = Some(image);
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn texture(
        &mut self,
        reference: ir::TextureRef<'_>,
    ) -> Result<Arc<RawImage>, IrSubmitError> {
        if !reference.belongs_to(&self.resources) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        let slot = reference.slot();
        if let Some(image) = self.images.get(slot).and_then(Option::as_ref) {
            return Ok(Arc::clone(image));
        }
        let descriptor = self.resources.texture(reference)?;
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            return Err(IrSubmitError::ImageNotMapped);
        }
        let image = Arc::new(RawImage::create_logical(&self.context, descriptor)?);
        let entry = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        *entry = Some(Arc::clone(&image));
        Ok(image)
    }

    pub(crate) fn buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
    ) -> Result<&RawBuffer, IrSubmitError> {
        // Resolve even on a cache hit: BufferRef has no belongs_to method, and
        // descriptor lookup checks both table ownership and current generation.
        let descriptor = self.resources.buffer(reference)?;
        let slot = reference.slot();
        if slot >= self.buffers.len() {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        if self.buffers[slot].is_none() {
            let buffer = Arc::new(RawBuffer::create(&self.context, descriptor.size())?);
            // Snapshot fresh storage before it can be referenced by GPU work.
            // Do not assume an allocator-specific initial byte pattern.
            let size =
                usize::try_from(descriptor.size()).map_err(|_| IrSubmitError::OutOfMemory)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(size)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            bytes.resize(size, 0);
            buffer.read_into(0, &mut bytes)?;
            self.buffer_mirrors[slot] = Some(BufferMirror {
                id: reference.id(),
                bytes,
            });
            self.buffers[slot] = Some(buffer);
        }
        if self.buffer_mirrors[slot]
            .as_ref()
            .is_none_or(|mirror| mirror.id != reference.id())
        {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        self.buffers[slot]
            .as_deref()
            .ok_or(IrSubmitError::ResourceTableMismatch)
    }

    pub(crate) fn write_buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), IrSubmitError> {
        self.buffer(reference)?.write(offset, bytes)?;
        Ok(())
    }

    pub(crate) fn read_buffer_into(
        &mut self,
        id: ir::BufferId,
        offset: u64,
        output: &mut [u8],
    ) -> Result<(), IrSubmitError> {
        let table = Rc::clone(&self.resources);
        let reference = table.buffer_ref(id)?;
        let descriptor = table.buffer(reference)?;
        read_range(descriptor.size(), offset, output.len())?;
        self.check_buffer_publication()?;
        if output.is_empty() {
            return Ok(());
        }
        self.buffer(reference)?;
        let mirror = self.buffer_mirrors[reference.slot()]
            .as_ref()
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        let start = read_range(mirror.bytes.len() as u64, offset, output.len())?;
        output.copy_from_slice(&mirror.bytes[start..start + output.len()]);
        Ok(())
    }

    fn check_buffer_publication(&self) -> Result<(), IrSubmitError> {
        // Includes failures from other resource caches sharing this context.
        // This is an atomic health check, not an idle/retirement wait.
        self.context.dispatcher.check()?;
        if self.buffer_publication_failed {
            return Err(IrSubmitError::CompletionUnavailable);
        }
        if let Some(receipt) = &self.buffer_publication_receipt {
            // Pending is permitted: CPU mirrors describe accepted writes while
            // unrelated rendering may still be executing. Never wait here.
            receipt.poll()?;
        }
        Ok(())
    }

    /// Call after stream validation/materialization and before enqueue.
    pub(crate) fn prepare_buffer_publication<'r, 'data>(
        &mut self,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<BufferPublication<'data>, IrSubmitError> {
        self.check_buffer_publication()?;
        if !core::ptr::eq(commands.resources(), self.resources.as_ref()) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        BufferPublication::prepare(commands, |reference| self.buffer(reference).map(|_| ()))
    }

    /// Call only after successful enqueue. An error is a failed accepted prefix:
    /// return SubmitError::Failed with this same receipt, never Rejected/Busy.
    pub(crate) fn publish_buffer_submission(
        &mut self,
        publication: BufferPublication<'_>,
        receipt: &crate::Submission,
    ) -> Result<(), IrSubmitError> {
        self.buffer_publication_receipt = Some(receipt.clone());
        let buffers = &self.buffers;
        let result = publication.publish(
            &mut self.buffer_mirrors,
            || match receipt.wait(None)? {
                CompletionStatus::Complete => Ok(()),
                _ => Err(IrSubmitError::CompletionUnavailable),
            },
            |slot, bytes| {
                buffers
                    .get(slot)
                    .and_then(Option::as_ref)
                    .ok_or(IrSubmitError::ResourceTableMismatch)?
                    .read_into(0, bytes)?;
                Ok(())
            },
        );
        if result.is_err() {
            self.buffer_publication_failed = true;
        }
        result
    }

    /// Synchronous execution must route every success/error exit here after
    /// preparing its plan. On success all physical writes already retired.
    pub(crate) fn finish_buffer_execution(
        &mut self,
        mut publication: BufferPublication<'_>,
        execution: Result<(), IrSubmitError>,
    ) -> Result<(), IrSubmitError> {
        let buffers = &self.buffers;
        let result = execution.and_then(|()| {
            // Read final physical destinations rather than replaying uploads
            // over buffers which synchronous execution has already changed.
            publication.gpu_writes = true;
            publication.publish(
                &mut self.buffer_mirrors,
                || Ok(()),
                |slot, bytes| {
                    buffers
                        .get(slot)
                        .and_then(Option::as_ref)
                        .ok_or(IrSubmitError::ResourceTableMismatch)?
                        .read_into(0, bytes)?;
                    Ok(())
                },
            )
        });
        if result.is_err() {
            self.buffer_publication_failed = true;
        }
        result
    }

    pub(crate) fn scratch(&mut self, required_size: u64) -> Result<&RawBuffer, IrSubmitError> {
        let needs_replacement = self
            .scratch
            .as_ref()
            .is_none_or(|scratch| scratch.logical_size < required_size);
        if needs_replacement {
            let allocation = required_size
                .checked_next_power_of_two()
                .ok_or(IrSubmitError::OutOfMemory)?;
            let replacement = RawBuffer::create(&self.context, allocation)?;
            if let Some(previous) = self.scratch.take() {
                if let Err(error) = previous.detach() {
                    // Preserve the already-live scratch on failure and discard the
                    // newly attached replacement. The original detachment failure
                    // remains authoritative if cleanup also fails.
                    let _ = replacement.detach();
                    self.scratch = Some(previous);
                    return Err(error.into());
                }
            }
            self.scratch = Some(replacement);
        }
        self.scratch.as_ref().ok_or(IrSubmitError::OutOfMemory)
    }

    pub(crate) fn read_scratch_u32(&self, offset: u64) -> HandleResult<u32> {
        self.scratch
            .as_ref()
            .ok_or(HandleError::InvalidParameter)?
            .read_u32(offset)
    }

    pub(crate) fn context_id(&self) -> i32 {
        self.context.raw.as_handle().as_raw()
    }
}

fn read_range(size: u64, offset: u64, length: usize) -> HandleResult<usize> {
    let end = offset
        .checked_add(length as u64)
        .ok_or(HandleError::InvalidParameter)?;
    if end > size {
        return Err(HandleError::InvalidParameter);
    }
    usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)
}

fn image_create_parameters(descriptor: ir::TextureDesc) -> HandleResult<(u32, u32)> {
    let format = physical_image_format(descriptor.format())?;
    let mut usage = 0;
    if descriptor.format() == ir::TextureFormat::Depth32Float {
        if !descriptor
            .usage()
            .contains(ir::TextureUsage::RENDER_ATTACHMENT)
        {
            return Err(HandleError::InvalidParameter);
        }
        usage |= GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT;
    } else {
        if descriptor
            .usage()
            .contains(ir::TextureUsage::RENDER_ATTACHMENT)
        {
            usage |= GPU_IMAGE_USAGE_RENDER_TARGET;
        }
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            usage |= GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_TRANSFER_SRC;
        }
        if descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::COPY_SRC)
        {
            usage |= GPU_IMAGE_USAGE_SAMPLED;
        }
    }
    // Transfer intent applies to both color and depth, independently of the
    // attachment type. The kernel remains responsible for the backing layout.
    if descriptor.usage().contains(ir::TextureUsage::COPY_SRC) {
        usage |= GPU_IMAGE_USAGE_TRANSFER_SRC;
    }
    if descriptor.usage().contains(ir::TextureUsage::COPY_DST) {
        usage |= GPU_IMAGE_USAGE_TRANSFER_DST;
    }
    if usage == 0 {
        return Err(HandleError::InvalidParameter);
    }
    Ok((format, usage))
}

/// Allocation format, not logical channel order. Narrow normalized textures
/// retain the existing four-byte BGRA expansion; floats must never use it.
fn physical_image_format(format: ir::TextureFormat) -> HandleResult<u32> {
    match format {
        ir::TextureFormat::Bgra8Unorm
        | ir::TextureFormat::Bgra8UnormSrgb
        | ir::TextureFormat::Rgba8Unorm
        | ir::TextureFormat::Rgba8UnormSrgb
        | ir::TextureFormat::R8Unorm
        | ir::TextureFormat::Rg8Unorm => Ok(GPU_IMAGE_FORMAT_BGRA8_UNORM),
        ir::TextureFormat::Depth32Float => Ok(GPU_IMAGE_FORMAT_DEPTH32_FLOAT),
        ir::TextureFormat::Rgba16Float => Ok(GPU_IMAGE_FORMAT_RGBA16_FLOAT),
        ir::TextureFormat::Rgba32Float => Ok(GPU_IMAGE_FORMAT_RGBA32_FLOAT),
        // NV12 remains import-only and uses its separate two-plane validator.
        ir::TextureFormat::Nv12 => Err(HandleError::Unsupported),
    }
}

fn physical_texel_bytes(format: ir::TextureFormat) -> HandleResult<u32> {
    gpu_raw::image_format_bytes_per_texel(physical_image_format(format)?)
        .ok_or(HandleError::Unsupported)
}

fn validate_texture_storage(
    layout: &GpuImageLayout,
    width: u32,
    height: u32,
    mip_levels: u32,
    array_layers: u32,
    logical_format: ir::TextureFormat,
) -> HandleResult<Option<adreno_a6xx_layout::LinearTextureLayout>> {
    if mip_levels == 0 || array_layers == 0 {
        return Err(HandleError::InvalidParameter);
    }
    let texture = if logical_format != ir::TextureFormat::Depth32Float
        && (mip_levels > 1 || array_layers > 1)
    {
        let texture = adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(
            width,
            height,
            mip_levels,
            array_layers,
            physical_texel_bytes(logical_format)?,
        )
        .ok_or(HandleError::InvalidParameter)?;
        if layout.modifier == GPU_IMAGE_MODIFIER_LINEAR
            && (layout.planes[0].row_pitch != texture.row_pitch
                || layout.planes[0].array_pitch != texture.layer_stride
                || layout.planes[0].size != texture.total_size
                || layout.total_size != texture.total_size)
        {
            return Err(HandleError::InvalidParameter);
        }
        Some(texture)
    } else {
        None
    };
    if logical_format == ir::TextureFormat::Depth32Float && array_layers > 1 {
        let mut layer = *layout;
        let expected = adreno_a6xx_layout::depth32_tile6_3_layout(width, height)
            .ok_or(HandleError::InvalidParameter)?
            .layer_size;
        if layout.planes[0].size
            != expected
                .checked_mul(u64::from(array_layers))
                .ok_or(HandleError::InvalidParameter)?
            || layout.total_size < layout.planes[0].size
        {
            return Err(HandleError::InvalidParameter);
        }
        layer.planes[0].size = expected;
        validate_image_layout(&layer, width, height, logical_format)?;
    } else {
        validate_image_layout(layout, width, height, logical_format)?;
    }
    Ok(texture)
}

fn validate_image_layout(
    layout: &GpuImageLayout,
    width: u32,
    height: u32,
    logical_format: ir::TextureFormat,
) -> HandleResult<()> {
    if width == 0 || height == 0 {
        return Err(HandleError::InvalidParameter);
    }
    if logical_format == ir::TextureFormat::Nv12 {
        if layout.plane_count != 2 || layout.modifier != 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(HandleError::Unsupported);
        }
        for (index, p) in layout.planes[..2].iter().enumerate() {
            let rows = if index == 0 { height } else { height / 2 };
            if p.row_pitch < width
                || p.row_pitch % 64 != 0
                || p.size != u64::from(p.row_pitch) * u64::from(rows)
                || p.offset
                    .checked_add(p.size)
                    .is_none_or(|end| end > layout.total_size)
            {
                return Err(HandleError::InvalidParameter);
            }
        }
        return Ok(());
    }
    if layout.plane_count != 1
        || layout.total_size == 0
        || layout.alignment == 0
        || !layout.alignment.is_power_of_two()
    {
        return Err(HandleError::Unsupported);
    }
    let plane = layout.planes[0];
    let physical_bytes_per_pixel = physical_texel_bytes(logical_format)?;
    // Allocation metadata is ready for float storage, not tiled float execution.
    // Keep this rejection explicit until the kernel and code generator agree.
    if matches!(
        logical_format,
        ir::TextureFormat::Rgba16Float | ir::TextureFormat::Rgba32Float
    ) && layout.modifier != GPU_IMAGE_MODIFIER_LINEAR
    {
        return Err(HandleError::Unsupported);
    }
    let minimum_pitch = width
        .checked_mul(physical_bytes_per_pixel)
        .ok_or(HandleError::InvalidParameter)?;
    let plane_end = plane
        .offset
        .checked_add(plane.size)
        .ok_or(HandleError::InvalidParameter)?;
    if plane_end > layout.total_size
        || plane.block_width != 1
        || plane.block_height != 1
        || u32::from(plane.bytes_per_block) != physical_bytes_per_pixel
    {
        return Err(HandleError::Unsupported);
    }
    if logical_format == ir::TextureFormat::Depth32Float
        || layout.modifier == adreno_a6xx_layout::IMAGE_MODIFIER_TILE6_3_COLOR
    {
        let expected_modifier = if logical_format == ir::TextureFormat::Depth32Float {
            IMAGE_MODIFIER_TILE6_3_DEPTH
        } else if logical_format == ir::TextureFormat::Bgra8Unorm && width >= 64 {
            adreno_a6xx_layout::IMAGE_MODIFIER_TILE6_3_COLOR
        } else {
            return Err(HandleError::Unsupported);
        };
        if layout.modifier != expected_modifier
            || layout.alignment != DEPTH32_LAYER_ALIGNMENT
            || plane.offset & (DEPTH32_LAYER_ALIGNMENT - 1) != 0
            || !is_depth32_tile6_3_layout(width, height, plane.row_pitch, plane.size)
            || u64::from(plane.array_pitch) != plane.size
        {
            return Err(HandleError::Unsupported);
        }
    } else {
        let minimum_size = u64::from(plane.row_pitch)
            .checked_mul(u64::from(height))
            .ok_or(HandleError::InvalidParameter)?;
        if layout.modifier != GPU_IMAGE_MODIFIER_LINEAR
            || plane.row_pitch < minimum_pitch
            || plane.row_pitch % physical_bytes_per_pixel != 0
            || plane.size < minimum_size
        {
            return Err(HandleError::Unsupported);
        }
    }
    Ok(())
}

fn empty_slots<T>(length: usize) -> Result<Vec<Option<T>>, IrSubmitError> {
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(length)
        .map_err(|_| IrSubmitError::OutOfMemory)?;
    slots.resize_with(length, || None);
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use adreno_a6xx_layout::IMAGE_MODIFIER_TILE6_3_DEPTH;
    use alloc::vec;
    use core::cell::Cell;
    use gpu_raw::{GPU_IMAGE_MODIFIER_LINEAR, GpuImageLayout, GpuImagePlaneLayout};

    use super::{
        BufferMirror, BufferPublication, image_create_parameters, physical_texel_bytes, read_range,
        validate_image_layout, validate_texture_storage,
    };
    use crate::{HandleError, ir};

    fn mirror(id: ir::BufferId, bytes: &[u8]) -> Option<BufferMirror> {
        Some(BufferMirror {
            id,
            bytes: bytes.to_vec(),
        })
    }

    #[test]
    fn accepted_cpu_writes_and_copy_chains_publish_without_waiting() {
        let table = ir::ResourceTable::new();
        let desc =
            ir::BufferDesc::new(16, ir::BufferUsage::COPY_SRC | ir::BufferUsage::COPY_DST).unwrap();
        let a = table.define_buffer(desc).unwrap();
        let b = table.define_buffer(desc).unwrap();
        let mut encoder = ir::CommandEncoder::new(&table);
        encoder.write_buffer(a, 0, &[1, 2, 3, 4]).unwrap();
        encoder
            .resource_barrier(ir::ResourceBarrier::Global)
            .unwrap();
        encoder.copy_buffer_to_buffer(a, 0, b, 4, 4).unwrap();
        encoder
            .resource_barrier(ir::ResourceBarrier::Global)
            .unwrap();
        encoder.write_buffer(a, 0, &[9, 8, 7, 6]).unwrap();
        encoder
            .resource_barrier(ir::ResourceBarrier::Global)
            .unwrap();
        encoder.copy_buffer_to_buffer(b, 4, a, 8, 4).unwrap();
        let commands = encoder.finish().unwrap();
        let mut mirrors = vec![mirror(a.id(), &[0xcc; 16]), mirror(b.id(), &[0xdd; 16])];
        let plan = BufferPublication::prepare(&commands, |_| Ok(())).unwrap();
        assert!(!plan.gpu_writes);
        // Preparation and Busy/rejection (dropping the plan) cannot publish.
        drop(plan);
        assert_eq!(mirrors[0].as_ref().unwrap().bytes, [0xcc; 16]);
        let plan = BufferPublication::prepare(&commands, |_| Ok(())).unwrap();
        plan.publish(
            &mut mirrors,
            || panic!("CPU mirrors must not wait"),
            |_, _| panic!("no GPU refresh"),
        )
        .unwrap();
        assert_eq!(&mirrors[0].as_ref().unwrap().bytes[..4], &[9, 8, 7, 6]);
        assert_eq!(
            &mirrors[0].as_ref().unwrap().bytes[8..],
            &[1, 2, 3, 4, 0xcc, 0xcc, 0xcc, 0xcc]
        );
        assert_eq!(&mirrors[1].as_ref().unwrap().bytes[4..8], &[1, 2, 3, 4]);
        assert_eq!(&mirrors[1].as_ref().unwrap().bytes[..4], &[0xdd; 4]);
    }

    #[test]
    fn shader_writes_refresh_final_destinations_including_trailing_copies() {
        let table = ir::ResourceTable::new();
        let desc = ir::BufferDesc::new(
            16,
            ir::BufferUsage::STORAGE | ir::BufferUsage::COPY_SRC | ir::BufferUsage::COPY_DST,
        )
        .unwrap();
        let a = table.define_buffer(desc).unwrap();
        let b = table.define_buffer(desc).unwrap();
        let shader = table
            .define_shader_module(
                ir::ShaderModuleDesc::wgsl("@compute @workgroup_size(1) fn cs() {}".into())
                    .unwrap(),
            )
            .unwrap();
        for read_only in [true, false] {
            let layout = ir::BindGroupLayoutDesc::new(vec![ir::BindGroupLayoutEntry::new(
                0,
                ir::ShaderStages::COMPUTE,
                ir::BindingType::StorageBuffer { read_only },
            )])
            .unwrap();
            let group = table
                .define_bind_group(
                    ir::BindGroupDesc::new(
                        &table,
                        layout.clone(),
                        vec![ir::BindGroupEntry::new(
                            0,
                            ir::BindingResource::Buffer {
                                buffer: a.id(),
                                offset: 0,
                                size: 16,
                            },
                        )],
                    )
                    .unwrap(),
                )
                .unwrap();
            let pipeline = table
                .define_compute_pipeline(
                    ir::ComputePipelineDesc::new(
                        ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Compute, "cs".into())
                            .unwrap(),
                        ir::PipelineLayoutDesc::new(vec![layout]).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut encoder = ir::CommandEncoder::new(&table);
            encoder.write_buffer(a, 0, &[1; 16]).unwrap();
            encoder
                .resource_barrier(ir::ResourceBarrier::Global)
                .unwrap();
            let mut pass = encoder.begin_compute_pass().unwrap();
            pass.set_pipeline(pipeline).unwrap();
            pass.set_bind_group(0, group).unwrap();
            pass.dispatch(1, 1, 1).unwrap();
            pass.end().unwrap();
            encoder
                .resource_barrier(ir::ResourceBarrier::Global)
                .unwrap();
            encoder.copy_buffer_to_buffer(a, 0, b, 0, 16).unwrap();
            let commands = encoder.finish().unwrap();
            let plan = BufferPublication::prepare(&commands, |_| Ok(())).unwrap();
            assert_eq!(plan.gpu_writes, !read_only);
            let mut mirrors = vec![mirror(a.id(), &[0; 16]), mirror(b.id(), &[0; 16])];
            let waited = Cell::new(false);
            plan.publish(
                &mut mirrors,
                || {
                    waited.set(true);
                    Ok(())
                },
                |_, bytes| {
                    assert!(waited.get(), "physical reads must follow retirement");
                    bytes.fill(7); // final shader result AND its trailing copy
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(waited.get(), !read_only);
            for mirror in mirrors.iter().flatten() {
                assert_eq!(mirror.bytes, vec![if read_only { 1 } else { 7 }; 16]);
            }
            if !read_only {
                let plan = BufferPublication::prepare(&commands, |_| Ok(())).unwrap();
                let result = plan.publish(
                    &mut mirrors,
                    || Err(crate::IrSubmitError::CompletionUnavailable),
                    |_, _| panic!("failed completion must not authorize a read"),
                );
                assert!(matches!(
                    result,
                    Err(crate::IrSubmitError::CompletionUnavailable)
                ));
                assert!(mirrors.iter().flatten().all(|m| m.bytes == [7; 16]));
            }
        }
    }

    #[test]
    fn empty_graphics_checkpoint_needs_no_mirror_work() {
        let table = ir::ResourceTable::new();
        let commands = ir::CommandEncoder::new(&table).finish().unwrap();
        let plan = BufferPublication::prepare(&commands, |_| panic!("no buffers")).unwrap();
        assert!(plan.written.is_empty());
        plan.publish(
            &mut [],
            || panic!("must stay async"),
            |_, _| panic!("no refresh"),
        )
        .unwrap();
    }

    #[test]
    fn coherent_read_ranges_allow_empty_end_and_reject_overflow_or_overrun() {
        assert_eq!(read_range(16, 0, 16), Ok(0));
        assert_eq!(read_range(16, 3, 5), Ok(3));
        assert_eq!(read_range(16, 16, 0), Ok(16));
        assert_eq!(read_range(16, 16, 1), Err(HandleError::InvalidParameter));
        assert_eq!(read_range(16, 17, 0), Err(HandleError::InvalidParameter));
        assert_eq!(
            read_range(u64::MAX, u64::MAX, 1),
            Err(HandleError::InvalidParameter)
        );
    }

    fn sample_layout() -> GpuImageLayout {
        let mut layout = GpuImageLayout::new();
        layout.modifier = GPU_IMAGE_MODIFIER_LINEAR;
        layout.total_size = 64 * 16;
        layout.alignment = 64;
        layout.plane_count = 1;
        layout.planes[0] = GpuImagePlaneLayout {
            offset: 0,
            size: 64 * 16,
            row_pitch: 64,
            array_pitch: 64 * 16,
            block_width: 1,
            block_height: 1,
            bytes_per_block: 4,
            reserved: 0,
        };
        layout
    }

    fn linear_storage(bytes: u32, mips: u32, layers: u32) -> GpuImageLayout {
        let texture = adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(
            65, 17, mips, layers, bytes,
        )
        .unwrap();
        let mut layout = sample_layout();
        layout.total_size = texture.total_size;
        layout.alignment = 4096;
        layout.planes[0].row_pitch = texture.row_pitch;
        layout.planes[0].array_pitch = texture.layer_stride;
        layout.planes[0].size = texture.total_size;
        layout.planes[0].bytes_per_block = bytes as _;
        layout
    }

    #[test]
    fn allocation_parameters_preserve_expansion_and_select_native_float_formats() {
        for (format, physical, bytes) in [
            (
                ir::TextureFormat::Bgra8Unorm,
                gpu_raw::GPU_IMAGE_FORMAT_BGRA8_UNORM,
                4,
            ),
            (
                ir::TextureFormat::Rgba8Unorm,
                gpu_raw::GPU_IMAGE_FORMAT_BGRA8_UNORM,
                4,
            ),
            (
                ir::TextureFormat::R8Unorm,
                gpu_raw::GPU_IMAGE_FORMAT_BGRA8_UNORM,
                4,
            ),
            (ir::TextureFormat::Rgba16Float, 6, 8),
            (ir::TextureFormat::Rgba32Float, 7, 16),
        ] {
            let desc = ir::TextureDesc::new(
                format,
                ir::Extent2D::new(65, 17).unwrap(),
                ir::TextureUsage::RENDER_ATTACHMENT
                    | ir::TextureUsage::SAMPLED
                    | ir::TextureUsage::COPY_SRC
                    | ir::TextureUsage::COPY_DST,
            )
            .unwrap();
            assert_eq!(
                image_create_parameters(desc),
                Ok((
                    physical,
                    gpu_raw::GPU_IMAGE_USAGE_RENDER_TARGET
                        | gpu_raw::GPU_IMAGE_USAGE_SAMPLED
                        | gpu_raw::GPU_IMAGE_USAGE_TRANSFER_SRC
                        | gpu_raw::GPU_IMAGE_USAGE_TRANSFER_DST
                ))
            );
            assert_eq!(physical_texel_bytes(format), Ok(bytes));
        }
    }

    #[test]
    fn float_linear_validation_uses_full_texel_bytes_and_rejects_truncation() {
        for (format, bytes) in [
            (ir::TextureFormat::Rgba16Float, 8),
            (ir::TextureFormat::Rgba32Float, 16),
        ] {
            let layout = linear_storage(bytes, 1, 1);
            assert_eq!(validate_image_layout(&layout, 65, 17, format), Ok(()));
            let mut wrong = layout;
            wrong.planes[0].bytes_per_block = 4;
            assert!(validate_image_layout(&wrong, 65, 17, format).is_err());
            for pitch in [65 * 4, 65 * bytes - 1, 65 * bytes + 1] {
                let mut wrong = layout;
                wrong.planes[0].row_pitch = pitch;
                assert!(validate_image_layout(&wrong, 65, 17, format).is_err());
            }
            let mut short = layout;
            short.planes[0].size = u64::from(layout.planes[0].row_pitch) * 17 - 1;
            assert!(validate_image_layout(&short, 65, 17, format).is_err());
            let mut overrun = layout;
            overrun.planes[0].offset = layout.total_size;
            assert!(validate_image_layout(&overrun, 65, 17, format).is_err());
            overrun.planes[0].offset = u64::MAX;
            assert!(validate_image_layout(&overrun, 65, 17, format).is_err());
            assert!(validate_image_layout(&layout, u32::MAX, 17, format).is_err());
            assert!(validate_image_layout(&layout, 65, 0, format).is_err());
        }
    }

    #[test]
    fn layered_float_metadata_must_match_every_byte_extent() {
        for (format, bytes) in [
            (ir::TextureFormat::Rgba16Float, 8),
            (ir::TextureFormat::Rgba32Float, 16),
        ] {
            let layout = linear_storage(bytes, 4, 6);
            let texture = validate_texture_storage(&layout, 65, 17, 4, 6, format)
                .unwrap()
                .unwrap();
            assert_eq!(texture.row_pitch, layout.planes[0].row_pitch);
            assert_eq!(texture.layer_stride, layout.planes[0].array_pitch);
            assert_eq!(texture.total_size, layout.total_size);
            assert!(
                texture.subresource(3, 5).unwrap().offset >= u64::from(texture.layer_stride) * 5
            );
            for field in 0..4 {
                let mut wrong = layout;
                match field {
                    0 => wrong.planes[0].row_pitch -= 64,
                    1 => wrong.planes[0].array_pitch -= 4096,
                    2 => wrong.planes[0].size -= 1,
                    _ => wrong.total_size -= 1,
                }
                assert!(validate_texture_storage(&wrong, 65, 17, 4, 6, format).is_err());
            }
            assert!(
                validate_texture_storage(&linear_storage(4, 4, 6), 65, 17, 4, 6, format).is_err()
            );
            for (mips, layers) in [(0, 6), (4, 0), (8, 6), (4, 2049)] {
                assert!(validate_texture_storage(&layout, 65, 17, mips, layers, format).is_err());
            }
        }
    }

    #[test]
    fn float_tiling_is_explicitly_unsupported_for_base_and_layered_images() {
        for (format, bytes) in [
            (ir::TextureFormat::Rgba16Float, 8),
            (ir::TextureFormat::Rgba32Float, 16),
        ] {
            for (mips, layers) in [(1, 1), (4, 6)] {
                for modifier in [
                    IMAGE_MODIFIER_TILE6_3_DEPTH,
                    adreno_a6xx_layout::IMAGE_MODIFIER_TILE6_3_COLOR,
                    u64::MAX,
                ] {
                    let mut layout = linear_storage(bytes, mips, layers);
                    layout.modifier = modifier;
                    assert_eq!(
                        validate_texture_storage(&layout, 65, 17, mips, layers, format),
                        Err(HandleError::Unsupported)
                    );
                }
            }
        }
    }

    #[test]
    fn normalized_layered_images_keep_four_byte_physical_storage() {
        let layout = linear_storage(4, 4, 6);
        let expected = adreno_a6xx_layout::LinearTextureLayout::new(65, 17, 4, 6).unwrap();
        for format in [
            ir::TextureFormat::Bgra8Unorm,
            ir::TextureFormat::Rgba8Unorm,
            ir::TextureFormat::R8Unorm,
        ] {
            assert_eq!(
                validate_texture_storage(&layout, 65, 17, 4, 6, format),
                Ok(Some(expected))
            );
        }
    }

    #[test]
    fn accepts_queried_linear_layout_with_padded_pitch() {
        assert_eq!(
            validate_image_layout(&sample_layout(), 8, 16, ir::TextureFormat::Bgra8Unorm),
            Ok(())
        );
    }

    #[test]
    fn rejects_layout_whose_plane_exceeds_allocation() {
        let mut layout = sample_layout();
        layout.planes[0].offset = 64;
        assert_eq!(
            validate_image_layout(&layout, 8, 16, ir::TextureFormat::Bgra8Unorm),
            Err(HandleError::Unsupported)
        );
    }

    #[test]
    fn accepts_canonical_tile6_3_depth_layout() {
        let mut layout = sample_layout();
        layout.modifier = IMAGE_MODIFIER_TILE6_3_DEPTH;
        layout.total_size = 4096;
        layout.alignment = 4096;
        layout.planes[0].size = 4096;
        layout.planes[0].row_pitch = 256;
        layout.planes[0].array_pitch = 4096;
        assert_eq!(
            validate_image_layout(&layout, 16, 16, ir::TextureFormat::Depth32Float),
            Ok(())
        );
    }

    #[test]
    fn copy_source_usage_is_preserved_for_layout_selection() {
        let desc = ir::TextureDesc::new(
            ir::TextureFormat::Bgra8Unorm,
            ir::Extent2D::new(64, 64).unwrap(),
            ir::TextureUsage::RENDER_ATTACHMENT
                | ir::TextureUsage::SAMPLED
                | ir::TextureUsage::COPY_SRC,
        )
        .unwrap();
        let (_, usage) = super::image_create_parameters(desc).unwrap();
        assert_ne!(usage & gpu_raw::GPU_IMAGE_USAGE_TRANSFER_SRC, 0);
    }

    #[test]
    fn depth_allocation_preserves_each_requested_transfer_usage() {
        for (transfer, expected) in [
            (ir::TextureUsage::RENDER_ATTACHMENT, 0),
            (
                ir::TextureUsage::COPY_SRC,
                gpu_raw::GPU_IMAGE_USAGE_TRANSFER_SRC,
            ),
            (
                ir::TextureUsage::COPY_DST,
                gpu_raw::GPU_IMAGE_USAGE_TRANSFER_DST,
            ),
            (
                ir::TextureUsage::COPY_SRC | ir::TextureUsage::COPY_DST,
                gpu_raw::GPU_IMAGE_USAGE_TRANSFER_SRC | gpu_raw::GPU_IMAGE_USAGE_TRANSFER_DST,
            ),
        ] {
            let desc = ir::TextureDesc::new(
                ir::TextureFormat::Depth32Float,
                ir::Extent2D::new(64, 64).unwrap(),
                ir::TextureUsage::RENDER_ATTACHMENT | transfer,
            )
            .unwrap();
            assert_eq!(
                image_create_parameters(desc),
                Ok((
                    gpu_raw::GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
                    gpu_raw::GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT | expected,
                )),
            );
        }
    }

    #[test]
    fn tiled_color_requires_exact_pitch_size_and_color_format() {
        let mut layout = sample_layout();
        layout.modifier = adreno_a6xx_layout::IMAGE_MODIFIER_TILE6_3_COLOR;
        layout.total_size = 8192;
        layout.alignment = 4096;
        layout.planes[0].size = 8192;
        layout.planes[0].row_pitch = 256;
        layout.planes[0].array_pitch = 8192;
        assert_eq!(
            validate_image_layout(&layout, 64, 17, ir::TextureFormat::Bgra8Unorm),
            Ok(())
        );
        assert!(validate_image_layout(&layout, 64, 17, ir::TextureFormat::R8Unorm).is_err());
        assert!(validate_image_layout(&layout, 64, 17, ir::TextureFormat::Depth32Float).is_err());
        layout.planes[0].size = 256 * 17;
        assert!(validate_image_layout(&layout, 64, 17, ir::TextureFormat::Bgra8Unorm).is_err());
        layout.planes[0].size = 8192;
        layout.planes[0].row_pitch = 320;
        assert!(validate_image_layout(&layout, 64, 17, ir::TextureFormat::Bgra8Unorm).is_err());
    }

    #[test]
    fn rejects_linear_depth_layout() {
        assert_eq!(
            validate_image_layout(&sample_layout(), 8, 16, ir::TextureFormat::Depth32Float),
            Err(HandleError::Unsupported)
        );
    }
}
