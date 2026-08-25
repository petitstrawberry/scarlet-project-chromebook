//! Context-local GPU resources and immutable image layouts.

use alloc::{rc::Rc, vec::Vec};
use core::ptr;

use gpu_raw::{
    GPU_BUFFER_FLAG_CPU_VISIBLE, GPU_IMAGE_FORMAT_BGRA8_UNORM, GPU_IMAGE_FORMAT_DEPTH32_FLOAT,
    GPU_IMAGE_MODIFIER_LINEAR, GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT,
    GPU_IMAGE_USAGE_PRESENTABLE, GPU_IMAGE_USAGE_RENDER_TARGET, GPU_IMAGE_USAGE_SAMPLED,
    GPU_IMAGE_USAGE_TRANSFER_DST, GpuBuffer, GpuImage, GpuImageLayout,
};
#[cfg(feature = "std")]
use scarlet_os::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};
#[cfg(not(feature = "std"))]
use std::handle::capability::memory_mapping::{MemoryMappingOps, flags, prot};

use crate::Handle;
use crate::{ContextInner, HandleError, HandleResult, Image, IrSubmitError, ir};

pub(crate) struct RawImage {
    pub(crate) raw: GpuImage,
    pub(crate) attachment_token: u64,
    pub(crate) layout: GpuImageLayout,
    pub(crate) logical_format: ir::TextureFormat,
    context_id: i32,
}

impl RawImage {
    pub(crate) fn create_present(
        context: &Rc<ContextInner>,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        if width == 0 || height == 0 {
            return Err(HandleError::InvalidParameter);
        }
        let usage =
            GPU_IMAGE_USAGE_RENDER_TARGET | GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_SAMPLED;
        let raw = context.device.gpu.create_image_with_format_and_usage(
            GPU_IMAGE_FORMAT_BGRA8_UNORM,
            width,
            height,
            usage,
        )?;
        Self::finish_create(context, raw, ir::TextureFormat::Bgra8Unorm, width, height)
    }

    fn create_logical(
        context: &Rc<ContextInner>,
        descriptor: ir::TextureDesc,
    ) -> HandleResult<Self> {
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        let (format, usage) = image_create_parameters(descriptor)?;
        let raw = context
            .device
            .gpu
            .create_image_with_format_and_usage(format, width, height, usage)?;
        Self::finish_create(context, raw, descriptor.format(), width, height)
    }

    fn import_sampled(
        context: &Rc<ContextInner>,
        handle: Handle,
        descriptor: ir::TextureDesc,
    ) -> HandleResult<Self> {
        if descriptor.format() != ir::TextureFormat::Bgra8Unorm
            || !descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::PRESENT)
        {
            return Err(HandleError::InvalidParameter);
        }
        let raw = GpuImage::from_handle(handle)?;
        let info = raw.query()?;
        let width = descriptor.extent().width();
        let height = descriptor.extent().height();
        if info.format != GPU_IMAGE_FORMAT_BGRA8_UNORM
            || info.usage & GPU_IMAGE_USAGE_SAMPLED == 0
            || info.width != width
            || info.height != height
        {
            return Err(HandleError::InvalidParameter);
        }
        Self::finish_create(context, raw, ir::TextureFormat::Bgra8Unorm, width, height)
    }

    fn finish_create(
        context: &Rc<ContextInner>,
        raw: GpuImage,
        logical_format: ir::TextureFormat,
        width: u32,
        height: u32,
    ) -> HandleResult<Self> {
        let layout = raw.query_layout()?;
        validate_linear_layout(&layout, width, height, logical_format)?;
        let attachment_token = context.raw.attach_image(&raw)?;
        if attachment_token == 0 {
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            raw,
            attachment_token,
            layout,
            logical_format,
            context_id: context.raw.as_handle().as_raw(),
        })
    }

    pub(crate) fn allocation_size(&self) -> u64 {
        self.layout.total_size
    }
}

pub(crate) struct RawBuffer {
    pub(crate) raw: GpuBuffer,
    pub(crate) attachment_token: u64,
    pub(crate) logical_size: u64,
}

impl RawBuffer {
    fn create(context: &Rc<ContextInner>, logical_size: u64) -> HandleResult<Self> {
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
        let attachment_token = context.raw.attach_buffer(&raw)?;
        if attachment_token == 0 {
            let _ = context.raw.detach_buffer(&raw);
            return Err(HandleError::InvalidParameter);
        }
        Ok(Self {
            raw,
            attachment_token,
            logical_size,
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
        let mapping_len = usize::try_from(self.raw.allocated_size())
            .map_err(|_| HandleError::InvalidParameter)?;
        let destination_offset =
            usize::try_from(offset).map_err(|_| HandleError::InvalidParameter)?;
        let mapping = self.raw.as_handle().as_memory_mapping()?;
        let address = mapping
            .mmap(0, mapping_len, prot::READ | prot::WRITE, flags::SHARED, 0)
            .map_err(|_| HandleError::SystemError(-1))?;

        // SAFETY: the kernel returned a writable mapping of `mapping_len`
        // bytes. The checked logical range above is no larger than the backing
        // allocation and the source slice remains valid for this copy.
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (address as *mut u8).add(destination_offset),
                bytes.len(),
            );
        }
        MemoryMappingOps::munmap(address, mapping_len).map_err(|_| HandleError::SystemError(-1))
    }
}

pub(crate) struct ContextResources {
    pub(crate) resources: Rc<ir::ResourceTable>,
    context: Rc<ContextInner>,
    images: Vec<Option<Rc<RawImage>>>,
    buffers: Vec<Option<RawBuffer>>,
    scratch: Option<RawBuffer>,
}

impl ContextResources {
    pub(crate) fn new(
        resources: Rc<ir::ResourceTable>,
        context: Rc<ContextInner>,
    ) -> Result<Self, IrSubmitError> {
        Ok(Self {
            resources,
            context,
            images: empty_slots(ir::MAX_TEXTURES)?,
            buffers: empty_slots(ir::MAX_BUFFERS)?,
            scratch: None,
        })
    }

    pub(crate) fn map_present_image(
        &mut self,
        texture: ir::TextureId,
        image: Rc<Image>,
    ) -> Result<(), IrSubmitError> {
        let reference = self.resources.texture_ref(texture)?;
        let descriptor = self.resources.texture(reference)?;
        if image.raw.context_id != self.context_id() {
            return Err(IrSubmitError::ContextMismatch);
        }
        if descriptor.extent().width() != image.width
            || descriptor.extent().height() != image.height
        {
            return Err(IrSubmitError::TargetExtentMismatch);
        }
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
            .any(|candidate| Rc::ptr_eq(candidate, &image.raw))
        {
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(Rc::clone(&image.raw));
        Ok(())
    }

    pub(crate) fn import_sampled_image(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
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
        let image = Rc::new(RawImage::import_sampled(&self.context, handle, descriptor)?);
        if self
            .images
            .iter()
            .flatten()
            .any(|candidate| candidate.attachment_token == image.attachment_token)
        {
            let _ = self.context.raw.detach_image(&image.raw);
            return Err(IrSubmitError::ImageAlreadyMapped);
        }
        self.images[slot] = Some(image);
        Ok(())
    }

    pub(crate) fn release_imported_image(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
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
        if let Err(error) = self.context.raw.detach_image(&image.raw) {
            self.images[slot] = Some(image);
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn texture(
        &mut self,
        reference: ir::TextureRef<'_>,
    ) -> Result<Rc<RawImage>, IrSubmitError> {
        if !reference.belongs_to(&self.resources) {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        let slot = reference.slot();
        if let Some(image) = self.images.get(slot).and_then(Option::as_ref) {
            return Ok(Rc::clone(image));
        }
        let descriptor = self.resources.texture(reference)?;
        if descriptor.usage().contains(ir::TextureUsage::PRESENT) {
            return Err(IrSubmitError::ImageNotMapped);
        }
        let image = Rc::new(RawImage::create_logical(&self.context, descriptor)?);
        let entry = self
            .images
            .get_mut(slot)
            .ok_or(IrSubmitError::ResourceTableMismatch)?;
        *entry = Some(Rc::clone(&image));
        Ok(image)
    }

    pub(crate) fn buffer(
        &mut self,
        reference: ir::BufferRef<'_>,
    ) -> Result<&RawBuffer, IrSubmitError> {
        let slot = reference.slot();
        if slot >= self.buffers.len() {
            return Err(IrSubmitError::ResourceTableMismatch);
        }
        if self.buffers[slot].is_none() {
            let descriptor = self.resources.buffer(reference)?;
            self.buffers[slot] = Some(RawBuffer::create(&self.context, descriptor.size())?);
        }
        self.buffers[slot]
            .as_ref()
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
                if let Err(error) = self.context.raw.detach_buffer(&previous.raw) {
                    // Preserve the already-live scratch on failure and discard the
                    // newly attached replacement. The original detachment failure
                    // remains authoritative if cleanup also fails.
                    let _ = self.context.raw.detach_buffer(&replacement.raw);
                    self.scratch = Some(previous);
                    return Err(error.into());
                }
            }
            self.scratch = Some(replacement);
        }
        self.scratch.as_ref().ok_or(IrSubmitError::OutOfMemory)
    }

    pub(crate) fn context_id(&self) -> i32 {
        self.context.raw.as_handle().as_raw()
    }
}

impl Drop for ContextResources {
    fn drop(&mut self) {
        for index in 0..self.images.len() {
            let Some(image) = self.images[index].as_ref() else {
                continue;
            };
            if self.images[..index]
                .iter()
                .flatten()
                .any(|earlier| Rc::ptr_eq(earlier, image))
            {
                continue;
            }
            let _ = self.context.raw.detach_image(&image.raw);
        }
        for buffer in self.buffers.iter().flatten() {
            let _ = self.context.raw.detach_buffer(&buffer.raw);
        }
        if let Some(scratch) = self.scratch.as_ref() {
            let _ = self.context.raw.detach_buffer(&scratch.raw);
        }
    }
}

fn image_create_parameters(descriptor: ir::TextureDesc) -> HandleResult<(u32, u32)> {
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
            usage |= GPU_IMAGE_USAGE_PRESENTABLE;
        }
        if descriptor.usage().contains(ir::TextureUsage::SAMPLED)
            || descriptor.usage().contains(ir::TextureUsage::COPY_SRC)
        {
            usage |= GPU_IMAGE_USAGE_SAMPLED;
        }
        if descriptor.usage().contains(ir::TextureUsage::COPY_DST) {
            usage |= GPU_IMAGE_USAGE_TRANSFER_DST;
        }
    }
    if usage == 0 {
        return Err(HandleError::InvalidParameter);
    }
    let format = if descriptor.format() == ir::TextureFormat::Depth32Float {
        GPU_IMAGE_FORMAT_DEPTH32_FLOAT
    } else {
        GPU_IMAGE_FORMAT_BGRA8_UNORM
    };
    Ok((format, usage))
}

fn validate_linear_layout(
    layout: &GpuImageLayout,
    width: u32,
    height: u32,
    logical_format: ir::TextureFormat,
) -> HandleResult<()> {
    if layout.modifier != GPU_IMAGE_MODIFIER_LINEAR
        || layout.plane_count != 1
        || layout.total_size == 0
        || layout.alignment == 0
        || !layout.alignment.is_power_of_two()
    {
        return Err(HandleError::Unsupported);
    }
    let plane = layout.planes[0];
    let physical_bytes_per_pixel = 4u32;
    let minimum_pitch = width
        .checked_mul(physical_bytes_per_pixel)
        .ok_or(HandleError::InvalidParameter)?;
    let minimum_size = u64::from(plane.row_pitch)
        .checked_mul(u64::from(height))
        .ok_or(HandleError::InvalidParameter)?;
    let plane_end = plane
        .offset
        .checked_add(plane.size)
        .ok_or(HandleError::InvalidParameter)?;
    if plane.row_pitch < minimum_pitch
        || plane.size < minimum_size
        || plane_end > layout.total_size
        || plane.block_width != 1
        || plane.block_height != 1
        || u32::from(plane.bytes_per_block) != physical_bytes_per_pixel
    {
        return Err(HandleError::Unsupported);
    }
    if logical_format == ir::TextureFormat::Depth32Float && plane.bytes_per_block != 4 {
        return Err(HandleError::Unsupported);
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
    use gpu_raw::{GPU_IMAGE_MODIFIER_LINEAR, GpuImageLayout, GpuImagePlaneLayout};

    use super::validate_linear_layout;
    use crate::{HandleError, ir};

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

    #[test]
    fn accepts_queried_linear_layout_with_padded_pitch() {
        assert_eq!(
            validate_linear_layout(&sample_layout(), 8, 16, ir::TextureFormat::Bgra8Unorm),
            Ok(())
        );
    }

    #[test]
    fn rejects_layout_whose_plane_exceeds_allocation() {
        let mut layout = sample_layout();
        layout.planes[0].offset = 64;
        assert_eq!(
            validate_linear_layout(&layout, 8, 16, ir::TextureFormat::Bgra8Unorm),
            Err(HandleError::Unsupported)
        );
    }
}
