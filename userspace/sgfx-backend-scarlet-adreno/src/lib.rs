//! Scarlet GPU ABI backend for the implemented Qualcomm Adreno A618 SGFX subset.
//!
//! This crate owns the GPU connection, exact backend/dialect negotiation,
//! context-local resource attachments, physical image layouts, command lowering,
//! A6xx submit-wire encoding, and synchronous or tracked queue submission. The pure A6xx
//! code generator remains transport-independent.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate scarlet_std as std;

use alloc::{rc::Rc, sync::Arc, vec::Vec};

use gpu_raw::{
    GPU_DEVICE_STATE_READY, GPU_EXECUTION_SUPPORT_IMAGE_READBACK,
    GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD, GPU_EXECUTION_SUPPORT_MEMORY,
    GPU_EXECUTION_SUPPORT_PRESENTATION, GPU_EXECUTION_SUPPORT_QUEUE, GPU_MAX_IMAGE_UPLOAD_SIZE,
    GPU_RESULT_SUCCESS, Gpu, GpuImageBgraRect, GpuQueryInfo,
};
#[cfg(feature = "std")]
pub use scarlet_os::handle::{Handle, HandleError, HandleResult};
#[cfg(not(feature = "std"))]
pub use std::handle::{Handle, HandleError, HandleResult};

pub use sgfx_core::ir;

mod asynchronous;
mod completion;
mod dispatch;
mod execute;
mod preparation;
mod programmable;
#[cfg(feature = "programmable")]
mod fixed_native;
#[cfg(feature = "programmable")]
mod native_private;
#[cfg(feature = "programmable")]
mod shader_artifact_cache;
mod resource;
mod scheduler;
#[cfg(feature = "profile-submissions")]
mod submission_profile;
mod wire;
mod timestamp;

pub use completion::Submission;

use resource::{ContextResources, RawImage};

/// Exact backend identifier advertised by the Qualcomm Adreno kernel backend.
pub const BACKEND_ID: &[u8] = b"qcom-adreno";

/// Exact A6xx command dialect required by this backend.
pub const DIALECT_ID: &[u8] = b"adreno-a6xx-pm4-reloc-v1";

// Negotiated kernel contract: wire v1.2 with isolated native shader execution.
// The opaque dialect ID is shared with older kernels; neither that ID nor our
// locally compiled wire version establishes native execution support.
const ISOLATED_NATIVE_DIALECT_TOKEN: u64 = adreno_a6xx_submit_wire::DIALECT_TOKEN;

/// Return whether a backend identifier selects this Adreno backend.
///
/// # Arguments
///
/// * `backend_id` - Exact identifier returned by [`GpuQueryInfo`].
///
/// # Returns
///
/// `true` only for [`BACKEND_ID`].
pub fn matches_backend_id(backend_id: &[u8]) -> bool {
    backend_id == BACKEND_ID
}

/// Return whether dialect information selects the required A6xx submit format.
///
/// # Arguments
///
/// * `dialect_info` - Exact opaque bytes returned by `GPU_QUERY_DIALECT`.
///
/// # Returns
///
/// `true` only for [`DIALECT_ID`].
pub fn matches_dialect(dialect_info: &[u8]) -> bool {
    dialect_info == DIALECT_ID
}

/// Device capabilities expressed in portable SGFX terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    rendering: bool,
    presentation: bool,
    image_upload: bool,
    image_readback: bool,
    depth: bool,
    image_mips: bool,
    texture_arrays: bool,
    native_programs: bool,
    timestamps: bool,
}

impl Capabilities {
    const fn from_negotiated_support(execution_support: u32, dialect_token: u64) -> Self {
        Self {
            rendering: true,
            presentation: execution_support & GPU_EXECUTION_SUPPORT_PRESENTATION != 0,
            image_upload: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD != 0,
            image_readback: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_READBACK != 0,
            depth: true,
            image_mips: execution_support & gpu_raw::GPU_EXECUTION_SUPPORT_IMAGE_MIPS != 0,
            texture_arrays: execution_support & gpu_raw::GPU_EXECUTION_SUPPORT_TEXTURE_ARRAYS != 0,
            native_programs: dialect_token == ISOLATED_NATIVE_DIALECT_TOKEN,
            timestamps: execution_support & gpu_raw::GPU_EXECUTION_SUPPORT_TIMESTAMPS != 0,
        }
    }

    /// Return whether command execution is available.
    pub const fn supports_rendering(&self) -> bool {
        self.rendering
    }

    /// Return whether mapped images may be presented.
    pub const fn supports_presentation(&self) -> bool {
        self.presentation
    }

    /// Return whether texture upload is available.
    pub const fn supports_image_upload(&self) -> bool {
        self.image_upload
    }

    /// Return whether rendered BGRA images can be read back synchronously.
    pub const fn supports_image_readback(&self) -> bool {
        self.image_readback
    }

    /// Return whether depth attachments are available.
    pub const fn supports_depth(&self) -> bool {
        self.depth
    }

    /// Return whether the kernel allocates complete mip chains.
    pub const fn supports_image_mips(&self) -> bool {
        self.image_mips
    }

    /// Return whether the kernel allocates independent array/cube layers.
    pub const fn supports_texture_arrays(&self) -> bool {
        self.texture_arrays
    }

    /// Return whether this build and the negotiated kernel support native graphics.
    /// Unknown dialect versions remain disabled until their contract is reviewed.
    pub const fn supports_programmable_graphics(&self) -> bool {
        cfg!(feature = "programmable") && self.native_programs
    }
}

struct DeviceInner {
    gpu: Gpu,
    dialect: gpu_raw::GpuDialect,
    capabilities: Capabilities,
    codegen_capabilities: sgfx_codegen_adreno_a6xx::Capabilities,
}

/// An owning connection to a compatible Scarlet Adreno GPU device.
pub struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    /// Test whether already-queried GPU information is compatible.
    ///
    /// This check is side-effect free. Dialect compatibility is checked later by
    /// [`Device::from_gpu`] because it requires a control request on the owning
    /// connection.
    ///
    /// # Arguments
    ///
    /// * `info` - Information returned from the same `Gpu` connection.
    ///
    /// # Returns
    ///
    /// `true` only for a ready exact-match backend with queue and memory support.
    pub fn supports(info: &GpuQueryInfo) -> bool {
        info.result == GPU_RESULT_SUCCESS
            && info.device_state == GPU_DEVICE_STATE_READY
            && matches_backend_id(info.backend_id_bytes())
            && info.execution_support & GPU_EXECUTION_SUPPORT_QUEUE != 0
            && info.execution_support & GPU_EXECUTION_SUPPORT_MEMORY != 0
            && info.max_opaque_command_size != 0
    }

    /// Adopt an already-opened GPU connection after exact backend negotiation.
    ///
    /// # Arguments
    ///
    /// * `gpu` - Owning connection used to obtain `info`.
    /// * `info` - Query result from that same connection.
    ///
    /// # Returns
    ///
    /// A compatible device, or [`HandleError::Unsupported`] when either the
    /// backend or dialect differs byte-for-byte from this implementation.
    pub fn from_gpu(gpu: Gpu, info: GpuQueryInfo) -> HandleResult<Self> {
        if !Self::supports(&info) {
            return Err(HandleError::Unsupported);
        }

        let dialect = gpu.query_dialect(0)?;
        if !matches_dialect(dialect.opaque_info()) {
            return Err(HandleError::Unsupported);
        }

        let capabilities =
            Capabilities::from_negotiated_support(info.execution_support, dialect.token());
        let transport_bytes = usize::try_from(info.max_opaque_command_size)
            .unwrap_or(adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE)
            .min(adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE);
        let max_pm4_words = transport_bytes
            .saturating_sub(adreno_a6xx_submit_wire::HEADER_SIZE)
            .checked_div(core::mem::size_of::<u32>())
            .and_then(|words| u32::try_from(words).ok())
            .ok_or(HandleError::Unsupported)?;
        if max_pm4_words == 0 {
            return Err(HandleError::Unsupported);
        }
        // A618 has a fixed 512 KiB GMEM. The backend is intentionally limited
        // to this concrete chip until the kernel's opaque backend-info record
        // grows an explicit chip/GMEM descriptor.
        let codegen_capabilities =
            sgfx_codegen_adreno_a6xx::Capabilities::a618(512 * 1024, max_pm4_words);
        Ok(Self {
            inner: Arc::new(DeviceInner {
                gpu,
                dialect,
                capabilities,
                codegen_capabilities,
            }),
        })
    }

    /// Open and negotiate a Scarlet GPU device.
    ///
    /// # Arguments
    ///
    /// * `path` - GPU device path such as `/dev/gpu0`.
    ///
    /// # Returns
    ///
    /// An owning compatible device or a handle error.
    pub fn open(path: &str) -> HandleResult<Self> {
        let gpu = Gpu::open(path)?;
        let info = gpu.query_info()?;
        Self::from_gpu(gpu, info)
    }

    /// Return portable capabilities for this negotiated device.
    pub fn capabilities(&self) -> Capabilities {
        self.inner.capabilities
    }

    /// Create an Adreno execution context for the negotiated dialect.
    pub fn create_context(&self) -> HandleResult<Context> {
        let raw = self.inner.gpu.create_context(&self.inner.dialect)?;
        if raw.effective_dialect_index() != self.inner.dialect.index()
            || raw.effective_dialect_token() != self.inner.dialect.token()
        {
            return Err(HandleError::Unsupported);
        }
        Ok(Context {
            inner: Arc::new(ContextInner {
                device: Arc::clone(&self.inner),
                raw,
                dispatcher: dispatch::NativeScheduler::new()?,
            }),
        })
    }
}

struct ContextInner {
    device: Arc<DeviceInner>,
    raw: gpu_raw::GpuContext,
    dispatcher: dispatch::NativeScheduler,
}

/// An owning Adreno execution context.
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

impl Context {
    /// Create an independent logical resource cache for this context.
    pub fn create_ir_resources(
        &self,
        table: Rc<ir::ResourceTable>,
    ) -> Result<IrResources, IrSubmitError> {
        Ok(IrResources {
            inner: ContextResources::new(table, Arc::clone(&self.inner))?,
            images: Vec::new(),
        })
    }

    /// Create a queue sharing this context's ordered dispatcher.
    pub fn create_queue(&self) -> HandleResult<Queue> {
        Ok(Queue {
            raw: Arc::new(self.inner.raw.create_queue()?),
            context: Arc::clone(&self.inner),
        })
    }

    /// Observe quiescence without waiting or interpreting failure as idle.
    pub fn is_idle(&self) -> HandleResult<bool> {
        self.inner.dispatcher.is_idle()
    }

    /// Read one texture's base level into tightly packed logical-format bytes.
    /// Unsupported formats/layered images are rejected before output is written.
    pub fn read_texture_into(
        &self,
        resources: &mut IrResources,
        id: ir::TextureId,
        output: &mut [u8],
    ) -> Result<(), IrSubmitError> {
        if !Arc::ptr_eq(&self.inner, &resources.inner.context) {
            return Err(IrSubmitError::ContextMismatch);
        }
        let table = Rc::clone(&resources.inner.resources);
        let reference = table.texture_ref(id)?;
        let descriptor = table.texture(reference)?;
        validate_texture_read(descriptor, output.len())?;
        resources.inner.drain_async()?;
        let image = Image {
            raw: resources.inner.texture(reference)?,
            width: descriptor.extent().width(),
            height: descriptor.extent().height(),
        };
        if matches!(
            descriptor.format(),
            ir::TextureFormat::Depth32Float
                | ir::TextureFormat::Rgba16Float
                | ir::TextureFormat::Rgba32Float
        ) {
            self.inner.dispatcher.wait_idle()?;
            let stride = image
                .width
                .checked_mul(descriptor.format().bytes_per_pixel().unwrap())
                .ok_or(ir::Error::Overflow)?;
            let max_rows = max_readback_rows(stride)?;
            let mut y = 0;
            while y < image.height {
                let rows = (image.height - y).min(max_rows);
                self.inner.raw.readback_image(
                    &image.raw.raw,
                    output,
                    stride,
                    gpu_raw::GpuImageRect::new(0, y, image.width, rows),
                )?;
                y += rows;
            }
            return Ok(());
        }
        let stride = image.width.checked_mul(4).ok_or(ir::Error::Overflow)?;
        let rect = ir::PixelRect::new(0, 0, image.width, image.height)?;
        if matches!(
            descriptor.format(),
            ir::TextureFormat::Bgra8Unorm | ir::TextureFormat::Bgra8UnormSrgb
        ) {
            self.readback_image_bgra(&image, output, stride, rect)?;
        } else if matches!(
            descriptor.format(),
            ir::TextureFormat::Rgba8Unorm | ir::TextureFormat::Rgba8UnormSrgb
        ) {
            self.readback_image_bgra(&image, output, stride, rect)?;
            for pixel in output.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        } else {
            // R8 occupies physical alpha; RG8 occupies physical red/green.
            let components = descriptor.format().bytes_per_pixel().unwrap() as usize;
            let length = (output.len() / components)
                .checked_mul(4)
                .ok_or(ir::Error::Overflow)?;
            let mut bgra = Vec::new();
            bgra.try_reserve_exact(length)
                .map_err(|_| IrSubmitError::OutOfMemory)?;
            bgra.resize(length, 0);
            self.readback_image_bgra(&image, &mut bgra, stride, rect)?;
            unpack_expanded_texture(descriptor.format(), &bgra, output);
        }
        Ok(())
    }

    /// Create and map physical images for logical presentation targets.
    ///
    /// # Arguments
    ///
    /// * `resources` - Logical SGFX resource table retained by the session.
    /// * `targets` - Distinct `PRESENT` texture identities to materialize.
    ///
    /// # Returns
    ///
    /// A session owning its queue, cache, context, and mapped images. Each
    /// logical target has one stable physical image for the session lifetime;
    /// callers implement buffering with multiple target identities, matching
    /// the VirGL mapped-target contract and shared-image registration model.
    pub fn create_mapped_target_session(
        &self,
        resources: Rc<ir::ResourceTable>,
        targets: &[ir::TextureId],
    ) -> Result<MappedTargetSession, IrSubmitError> {
        let queue = Arc::new(self.inner.raw.create_queue()?);
        let mut cache = ContextResources::new(Rc::clone(&resources), Arc::clone(&self.inner))?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(targets.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;

        for &target in targets {
            if images
                .iter()
                .any(|(candidate, _): &(ir::TextureId, Arc<Image>)| *candidate == target)
            {
                return Err(IrSubmitError::TextureAlreadyMapped);
            }
            let reference = resources.texture_ref(target)?;
            let descriptor = resources.texture(reference)?;
            let required = ir::TextureUsage::RENDER_ATTACHMENT | ir::TextureUsage::PRESENT;
            if descriptor.format() != ir::TextureFormat::Bgra8Unorm
                || !descriptor.usage().contains(required)
            {
                return Err(IrSubmitError::Unsupported(
                    UnsupportedIrFeature::TargetUsage,
                ));
            }
            let image =
                Arc::new(self.create_shared_image(
                    descriptor.extent().width(),
                    descriptor.extent().height(),
                )?);
            cache.map_present_image(target, Arc::clone(&image))?;
            images.push((target, image));
        }

        Ok(MappedTargetSession {
            images,
            resources: cache,
            queue,
            context: Context {
                inner: Arc::clone(&self.inner),
            },
        })
    }

    /// Create a presentation-capable linear BGRA render target.
    ///
    /// # Arguments
    ///
    /// * `width` - Non-zero image width.
    /// * `height` - Non-zero image height.
    ///
    /// # Returns
    ///
    /// An attached image whose queried layout is retained by the backend.
    pub fn create_shared_image(&self, width: u32, height: u32) -> HandleResult<Image> {
        let raw = RawImage::create_present(&self.inner, width, height)?;
        Ok(Image {
            raw: Arc::new(raw),
            width,
            height,
        })
    }

    /// Read one render-target rectangle into a complete BGRA destination buffer.
    ///
    /// # Arguments
    ///
    /// * `image` - Render-target image created by this context.
    /// * `destination` - Complete writable BGRA destination buffer.
    /// * `destination_stride` - Bytes between destination rows.
    /// * `rect` - Source image rectangle written at identical destination coordinates.
    ///
    /// # Returns
    ///
    /// Success after synchronous GPU readback, or a handle error.
    pub fn readback_image_bgra(
        &self,
        image: &Image,
        destination: &mut [u8],
        destination_stride: u32,
        rect: ir::PixelRect,
    ) -> HandleResult<()> {
        // A direct Context readback must also cover logical chunks which have
        // not yet reached the kernel FIFO. All sessions share this dispatcher.
        self.inner.dispatcher.wait_idle()?;
        if !image.raw.belongs_to(&self.inner)
            || !self.inner.device.capabilities.supports_image_readback()
        {
            return Err(HandleError::InvalidParameter);
        }
        validate_readback_destination(
            image.width,
            image.height,
            destination.len(),
            destination_stride,
            rect,
        )?;

        let row_bytes = rect
            .width()
            .checked_mul(4)
            .ok_or(HandleError::InvalidParameter)?;
        let max_rows = max_readback_rows(row_bytes)?;
        let mut y = rect.y();
        let mut remaining = rect.height();
        while remaining != 0 {
            let height = remaining.min(max_rows);
            self.inner.raw.readback_image_bgra(
                &image.raw.raw,
                destination,
                destination_stride,
                GpuImageBgraRect::new(rect.x(), y, rect.width(), height),
            )?;
            y = y.checked_add(height).ok_or(HandleError::InvalidParameter)?;
            remaining -= height;
        }
        Ok(())
    }
}

fn max_readback_rows(row_bytes: u32) -> HandleResult<u32> {
    GPU_MAX_IMAGE_UPLOAD_SIZE
        .checked_div(row_bytes)
        .filter(|rows| *rows != 0)
        .ok_or(HandleError::InvalidParameter)
}

fn validate_readback_destination(
    image_width: u32,
    image_height: u32,
    destination_length: usize,
    destination_stride: u32,
    rect: ir::PixelRect,
) -> HandleResult<()> {
    let x_end = rect
        .x()
        .checked_add(rect.width())
        .ok_or(HandleError::InvalidParameter)?;
    let y_end = rect
        .y()
        .checked_add(rect.height())
        .ok_or(HandleError::InvalidParameter)?;
    if x_end > image_width || y_end > image_height {
        return Err(HandleError::InvalidParameter);
    }

    let destination_row_end = x_end.checked_mul(4).ok_or(HandleError::InvalidParameter)?;
    if destination_stride < destination_row_end {
        return Err(HandleError::InvalidParameter);
    }
    let destination_end = u64::from(y_end - 1)
        .checked_mul(u64::from(destination_stride))
        .and_then(|offset| offset.checked_add(u64::from(destination_row_end)))
        .ok_or(HandleError::InvalidParameter)?;
    let destination_length =
        u64::try_from(destination_length).map_err(|_| HandleError::InvalidParameter)?;
    if destination_end > destination_length {
        return Err(HandleError::InvalidParameter);
    }
    Ok(())
}

/// Failure while materializing or submitting a logical SGFX command buffer.
#[derive(Debug)]
pub enum IrSubmitError {
    /// The SGFX resource or command buffer failed validation.
    InvalidIr(ir::Error),
    /// The command buffer and persistent cache use different tables.
    ResourceTableMismatch,
    /// A context or queue differs from the cache's creating context.
    ContextMismatch,
    /// The mapped target extent differs from its logical texture.
    TargetExtentMismatch,
    /// A logical target has no mapped physical image.
    ImageNotMapped,
    /// A logical texture already has a physical image mapping.
    TextureAlreadyMapped,
    /// A physical image is mapped to another logical texture.
    ImageAlreadyMapped,
    /// A valid SGFX feature cannot be represented faithfully.
    Unsupported(UnsupportedIrFeature),
    /// Host-side bounded allocation failed.
    OutOfMemory,
    /// The Scarlet GPU ABI rejected an operation.
    Backend(HandleError),
    /// The pure A6xx code generator rejected the normalized command stream.
    Codegen(sgfx_codegen_adreno_a6xx::CompileError),
    /// Native shader validation/compilation failed with a retained diagnostic.
    ShaderCompile(alloc::string::String),
    /// The canonical A6xx submit-wire encoder rejected its input.
    SubmitWire(adreno_a6xx_submit_wire::Error),
    /// The negotiated kernel queue does not implement asynchronous admission.
    AsyncUnsupported,
    /// A logical submission exceeds the bounded staging or command capacity.
    SubmissionTooLarge,
    /// Native completion or acceptance cannot be observed authoritatively.
    CompletionUnavailable,
    /// The kernel reported a terminal GPU completion failure.
    CompletionFailed(u32),
}

impl From<ir::Error> for IrSubmitError {
    fn from(error: ir::Error) -> Self {
        Self::InvalidIr(error)
    }
}

impl From<HandleError> for IrSubmitError {
    fn from(error: HandleError) -> Self {
        Self::Backend(error)
    }
}

impl From<sgfx_codegen_adreno_a6xx::CompileError> for IrSubmitError {
    fn from(error: sgfx_codegen_adreno_a6xx::CompileError) -> Self {
        Self::Codegen(error)
    }
}

impl From<adreno_a6xx_submit_wire::Error> for IrSubmitError {
    fn from(error: adreno_a6xx_submit_wire::Error) -> Self {
        Self::SubmitWire(error)
    }
}

/// Portable feature rejected before backend submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedIrFeature {
    /// The presentation target format or usage is not supported.
    TargetUsage,
    /// A physical image layout is incompatible with A6xx execution.
    ImageLayout,
    /// A texture upload format cannot be converted without loss.
    TextureUpload,
    /// A command references an unsupported resource state.
    ResourceState,
    /// Programmable pipelines and compute need an extended backend.
    ProgrammableExecution,
}

/// Context-local logical resources for the low-level driver extension.
pub struct IrResources {
    inner: ContextResources,
    // Explicit mappings retain their public image owner across unmap/remap.
    images: Vec<(ir::TextureId, Arc<Image>)>,
}

impl IrResources {
    /// Validate and compile a shader using the native programmable cache.
    pub fn validate_shader_module(&mut self, id: ir::ShaderModuleId) -> Result<(), IrSubmitError> {
        self.inner.validate_shader_module(id)
    }

    /// Validate and compile a compute pipeline, or report unsupported execution.
    pub fn validate_compute_pipeline(
        &mut self,
        id: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        self.inner.validate_compute_pipeline(id)
    }

    /// Validate and compile a programmable graphics pipeline.
    pub fn validate_programmable_render_pipeline(
        &mut self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        self.inner.validate_programmable_render_pipeline(id)
    }

    /// Retire shader cache entries after accepted work has completed.
    pub fn release_shader_module(&mut self, id: ir::ShaderModuleId) -> Result<(), IrSubmitError> {
        self.inner.drain_async()?;
        self.inner.release_shader_module(id)
    }

    /// Retire compute cache entries after accepted work has completed.
    pub fn release_compute_pipeline(
        &mut self,
        id: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        self.inner.drain_async()?;
        self.inner.release_compute_pipeline(id)
    }

    /// Retire graphics cache entries after accepted work has completed.
    pub fn release_programmable_render_pipeline(
        &mut self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        self.inner.drain_async()?;
        self.inner.release_programmable_render_pipeline(id)
    }

    /// Validate retirement of an uncached logical bind group.
    pub fn release_bind_group(&mut self, id: ir::BindGroupId) -> Result<(), IrSubmitError> {
        self.inner.resources.bind_group_ref(id)?;
        Ok(())
    }

    /// Read coherent buffer bytes after all deferred CPU/GPU writes complete.
    pub fn read_buffer_into(
        &mut self,
        id: ir::BufferId,
        offset: u64,
        output: &mut [u8],
    ) -> Result<(), IrSubmitError> {
        self.inner.read_buffer_into(id, offset, output)
    }

    /// Release physical buffer storage before retiring its logical identity.
    pub fn release_buffer(&mut self, id: ir::BufferId) -> Result<(), IrSubmitError> {
        self.inner.resources.buffer_ref(id)?;
        self.inner.drain_async()?;
        self.inner.release_buffer(id)
    }

    /// Release cached texture storage, preserving externally owned images.
    pub fn release_texture(&mut self, id: ir::TextureId) -> Result<(), IrSubmitError> {
        self.inner.resources.texture_ref(id)?;
        if self.images.iter().any(|(candidate, _)| *candidate == id) {
            return self.unmap_image(id);
        }
        self.inner.drain_async()?;
        self.inner.release_texture(id)
    }

    /// Associate a compatible shared BGRA image with a logical render target.
    pub fn map_image(&mut self, id: ir::TextureId, image: Arc<Image>) -> Result<(), IrSubmitError> {
        self.images
            .try_reserve(1)
            .map_err(|_| IrSubmitError::OutOfMemory)?;
        self.inner.map_present_image(id, Arc::clone(&image))?;
        self.images.push((id, image));
        Ok(())
    }

    /// Remove an explicit mapping once accepted uses of it have completed.
    pub fn unmap_image(&mut self, id: ir::TextureId) -> Result<(), IrSubmitError> {
        self.inner.resources.texture_ref(id)?;
        let index = self
            .images
            .iter()
            .position(|(candidate, _)| *candidate == id)
            .ok_or(IrSubmitError::ImageNotMapped)?;
        self.inner.drain_async()?;
        self.inner.unmap_present_image(id)?;
        self.images.remove(index);
        Ok(())
    }
}

/// Queue and retained context authority for tracked low-level submissions.
pub struct Queue {
    raw: Arc<gpu_raw::GpuQueue>,
    context: Arc<ContextInner>,
}

impl Queue {
    /// Submit through the same bounded dispatcher as mapped-target sessions.
    pub fn submit_ir_async<'r, 'data>(
        &self,
        context: &Context,
        resources: &mut IrResources,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, sgfx_core::backend::SubmitError<IrSubmitError, Submission>> {
        if !Arc::ptr_eq(&self.context, &context.inner)
            || !Arc::ptr_eq(&self.context, &resources.inner.context)
        {
            return Err(sgfx_core::backend::SubmitError::Rejected(
                IrSubmitError::ContextMismatch,
            ));
        }
        resources
            .inner
            .submit_async(&context.inner, &self.raw, commands)
    }
}

fn unpack_expanded_texture(format: ir::TextureFormat, bgra: &[u8], output: &mut [u8]) {
    match format {
        ir::TextureFormat::R8Unorm => {
            for (red, pixel) in output.iter_mut().zip(bgra.chunks_exact(4)) {
                *red = pixel[3];
            }
        }
        ir::TextureFormat::Rg8Unorm => {
            for (rg, pixel) in output.chunks_exact_mut(2).zip(bgra.chunks_exact(4)) {
                rg.copy_from_slice(&[pixel[2], pixel[1]]);
            }
        }
        _ => unreachable!("only expanded one/two-component textures reach this path"),
    }
}

fn validate_texture_read(descriptor: ir::TextureDesc, length: usize) -> Result<(), IrSubmitError> {
    if !descriptor.usage().contains(ir::TextureUsage::COPY_SRC) {
        return Err(ir::Error::InvalidUsage.into());
    }
    if descriptor.array_layer_count() != 1
        || !matches!(
            descriptor.format(),
            ir::TextureFormat::Bgra8Unorm
                | ir::TextureFormat::Rgba8Unorm
                | ir::TextureFormat::R8Unorm
                | ir::TextureFormat::Rg8Unorm
                | ir::TextureFormat::Bgra8UnormSrgb
                | ir::TextureFormat::Rgba8UnormSrgb
                | ir::TextureFormat::Depth32Float
                | ir::TextureFormat::Rgba16Float
                | ir::TextureFormat::Rgba32Float
        )
    {
        return Err(IrSubmitError::Unsupported(
            UnsupportedIrFeature::ImageLayout,
        ));
    }
    let expected = u64::from(descriptor.extent().width())
        .checked_mul(u64::from(descriptor.extent().height()))
        .and_then(|pixels| {
            pixels.checked_mul(u64::from(descriptor.format().bytes_per_pixel().unwrap()))
        })
        .ok_or(ir::Error::Overflow)?;
    if expected != length as u64 {
        return Err(ir::Error::OutOfBounds.into());
    }
    Ok(())
}

/// Session owning mapped presentation images and all execution state.
pub struct MappedTargetSession {
    // These owners must drop before the queue/context that authorized them.
    images: Vec<(ir::TextureId, Arc<Image>)>,
    resources: ContextResources,
    queue: Arc<gpu_raw::GpuQueue>,
    context: Context,
}

impl MappedTargetSession {
    /// Retire a shader cache before its mirrored logical slot changes generation.
    pub fn release_shader_module(&mut self, id: ir::ShaderModuleId) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        self.resources.release_shader_module(id)
    }

    /// Retire compute cache entries before replacing their logical generation.
    pub fn release_compute_pipeline(
        &mut self,
        id: ir::ComputePipelineId,
    ) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        self.resources.release_compute_pipeline(id)
    }

    /// Retire graphics cache entries before replacing their logical generation.
    pub fn release_programmable_render_pipeline(
        &mut self,
        id: ir::ProgrammableRenderPipelineId,
    ) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        self.resources.release_programmable_render_pipeline(id)
    }

    /// Observe quiescence without waiting during dynamic metadata retirement.
    pub fn is_idle(&self) -> Result<bool, IrSubmitError> {
        Ok(self.context.inner.dispatcher.is_idle()?)
    }

    /// Retire a texture before its logical slot is reused by the frontend.
    pub fn release_texture(&mut self, texture: ir::TextureId) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        self.resources.release_texture(texture)?;
        self.images.retain(|(id, _)| *id != texture);
        Ok(())
    }

    /// Retire a buffer before its logical slot is reused by the frontend.
    pub fn release_buffer(&mut self, buffer: ir::BufferId) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        self.resources.release_buffer(buffer)
    }

    /// Import a transferred shared BGRA image into a logical sampled texture.
    pub fn import_shared_bgra_texture(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
    ) -> Result<(), IrSubmitError> {
        self.resources.import_sampled_image(texture, handle, None)
    }

    /// Import an immutable two-plane image without CPU color conversion.
    pub fn import_ycbcr_texture(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
        conversion: ir::YcbcrConversion,
    ) -> Result<(), IrSubmitError> {
        self.resources
            .import_sampled_image(texture, handle, Some(conversion))
    }

    /// Detach and release a previously imported sampled texture.
    pub fn release_imported_texture(
        &mut self,
        texture: ir::TextureId,
    ) -> Result<(), IrSubmitError> {
        self.resources.release_imported_image(texture)
    }

    /// Borrow the stable image mapped to a logical presentation target.
    pub fn image(&self, target: ir::TextureId) -> Result<ImageRef<'_>, IrSubmitError> {
        self.images
            .iter()
            .find(|(candidate, _)| *candidate == target)
            .map(|(_, image)| image.as_ref())
            .ok_or(IrSubmitError::ImageNotMapped)
    }

    /// Read one mapped presentation-image rectangle into a BGRA buffer.
    ///
    /// # Arguments
    ///
    /// * `target` - Logical presentation texture identity.
    /// * `destination` - Complete writable BGRA destination buffer.
    /// * `destination_stride` - Bytes between destination rows.
    /// * `rect` - Source target rectangle written at identical destination coordinates.
    ///
    /// # Returns
    ///
    /// Success after synchronous GPU readback, or an execution error.
    pub fn readback_bgra(
        &self,
        target: ir::TextureId,
        destination: &mut [u8],
        destination_stride: u32,
        rect: ir::PixelRect,
    ) -> Result<(), IrSubmitError> {
        self.resources.drain_async()?;
        let image = self.image(target)?;
        self.context
            .readback_image_bgra(image, destination, destination_stride, rect)?;
        Ok(())
    }

    /// Bind this session's queue and resource cache for command execution.
    pub fn executor(&mut self) -> Executor<'_> {
        Executor {
            queue: &self.queue,
            context: &self.context,
            resources: &mut self.resources,
        }
    }
}

/// Borrowed view of a mapped Scarlet Adreno presentation image.
///
/// This alias keeps the backend surface directly usable by the SGFX frontend
/// without weakening the session's ownership of the underlying GPU image.
pub type ImageRef<'a> = &'a Image;

/// Renderable image that can be presented through a Scarlet display surface.
pub struct Image {
    raw: Arc<RawImage>,
    width: u32,
    height: u32,
}

impl Image {
    /// Return image width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Return image height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the owning GPU image capability used for presentation.
    pub fn shared_handle(&self) -> &Handle {
        self.raw.raw.as_handle()
    }
}

/// Command executor bound to one context, queue, and persistent resource cache.
pub struct Executor<'a> {
    queue: &'a Arc<gpu_raw::GpuQueue>,
    context: &'a Context,
    resources: &'a mut ContextResources,
}

impl sgfx_core::backend::CommandExecutor for Executor<'_> {
    type Error = IrSubmitError;

    fn execute<'r, 'data>(
        &mut self,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<(), Self::Error> {
        let result = self
            .resources
            .execute(&self.context.inner, self.queue, commands);
        if let Err(error) = &result {
            std::println!("[a618-userspace] command execution failed: {:?}", error);
        }
        result
    }
}

impl sgfx_core::backend::CommandSubmitter for Executor<'_> {
    type Submission = Submission;

    fn supports_async_submission(&self) -> bool {
        self.context
            .inner
            .dispatcher
            .async_command_limit(self.queue)
            .is_ok()
    }

    /// Submit owned GPU work without waiting for its completion or capacity.
    ///
    /// Tracked calls grow at most four persistent upload arenas as needed; first-use
    /// logical resources can also synchronize while their SMMU mappings are
    /// installed. Subsequent calls reuse arenas only after successful native
    /// completion. Aligned buffer updates use owned arena storage and GPU copies;
    /// byte-unaligned updates use the coherent mapping after prior GPU work retires.
    /// Kernel ownership survives session or receipt destruction. All updates are
    /// validated before admission. Writable shader storage additionally waits on
    /// this submission's receipt to publish its final CPU mirror; ordinary
    /// uploads and read-only bindings do not add that publication wait.
    fn submit<'r, 'data>(
        &mut self,
        commands: &ir::CommandBuffer<'r, 'data>,
    ) -> Result<Submission, sgfx_core::backend::SubmitError<IrSubmitError, Submission>> {
        self.resources
            .submit_async(&self.context.inner, self.queue, commands)
    }
}

#[cfg(test)]
mod tests {
    use gpu_raw::{
        GPU_DEVICE_STATE_READY, GPU_EXECUTION_SUPPORT_IMAGE_READBACK, GPU_EXECUTION_SUPPORT_MEMORY,
        GPU_EXECUTION_SUPPORT_QUEUE, GPU_MAX_IMAGE_UPLOAD_SIZE, GPU_RESULT_SUCCESS, GpuQueryInfo,
    };

    use super::{
        BACKEND_ID, Capabilities, DIALECT_ID, Device, HandleError, matches_backend_id,
        matches_dialect, max_readback_rows, validate_readback_destination,
    };

    #[test]
    fn low_level_texture_read_checks_packing_usage_format_and_layers() {
        use super::ir;
        let descriptor = |format, usage| {
            ir::TextureDesc::new(format, ir::Extent2D::new(3, 2).unwrap(), usage).unwrap()
        };
        for (format, bytes) in [
            (ir::TextureFormat::Bgra8Unorm, 24),
            (ir::TextureFormat::Rgba8Unorm, 24),
            (ir::TextureFormat::R8Unorm, 6),
            (ir::TextureFormat::Rg8Unorm, 12),
            (ir::TextureFormat::Bgra8UnormSrgb, 24),
            (ir::TextureFormat::Rgba8UnormSrgb, 24),
            (ir::TextureFormat::Depth32Float, 24),
            (ir::TextureFormat::Rgba16Float, 48),
            (ir::TextureFormat::Rgba32Float, 96),
        ] {
            let desc = descriptor(format, ir::TextureUsage::COPY_SRC);
            assert!(super::validate_texture_read(desc, bytes).is_ok());
            assert!(super::validate_texture_read(desc, bytes - 1).is_err());
            assert!(super::validate_texture_read(desc, bytes + 1).is_err());
            assert!(
                super::validate_texture_read(desc.with_array_layer_count(2).unwrap(), bytes)
                    .is_err()
            );
        }
        assert!(
            super::validate_texture_read(
                descriptor(ir::TextureFormat::Bgra8Unorm, ir::TextureUsage::COPY_DST),
                24
            )
            .is_err()
        );
    }

    #[test]
    fn backend_id_match_is_exact() {
        assert!(matches_backend_id(BACKEND_ID));
        assert!(!matches_backend_id(b"qcom-adreno-a618"));
        assert!(!matches_backend_id(b"qcom-adreno-extra"));
        assert!(!matches_backend_id(b"virtio-gpu"));
        assert!(!matches_backend_id(b""));
    }

    #[test]
    fn dialect_match_is_exact() {
        assert!(matches_dialect(DIALECT_ID));
        assert!(!matches_dialect(b"adreno-a6xx-pm4-reloc"));
        assert!(!matches_dialect(b"adreno-a6xx-pm4-reloc-v1-extra"));
        assert!(!matches_dialect(b""));
    }

    #[test]
    fn supports_requires_ready_queue_memory_and_exact_id() {
        let mut info = GpuQueryInfo::new();
        info.result = GPU_RESULT_SUCCESS;
        info.device_state = GPU_DEVICE_STATE_READY;
        info.execution_support = GPU_EXECUTION_SUPPORT_QUEUE | GPU_EXECUTION_SUPPORT_MEMORY;
        info.max_opaque_command_size = 64 * 1024;
        info.backend_id_len = BACKEND_ID.len() as u32;
        info.backend_id[..BACKEND_ID.len()].copy_from_slice(BACKEND_ID);
        assert!(Device::supports(&info));

        info.backend_id[BACKEND_ID.len()] = b'x';
        info.backend_id_len += 1;
        assert!(!Device::supports(&info));
        info.backend_id_len -= 1;
        info.execution_support &= !GPU_EXECUTION_SUPPORT_MEMORY;
        assert!(!Device::supports(&info));
    }

    #[test]
    fn expanded_readback_uses_physical_alpha_for_r8_and_red_green_for_rg8() {
        let mut r = [0; 2];
        super::unpack_expanded_texture(
            crate::ir::TextureFormat::R8Unorm,
            &[0, 0, 0, 17, 0, 0, 0, 231],
            &mut r,
        );
        assert_eq!(r, [17, 231]);
        let mut rg = [0; 4];
        super::unpack_expanded_texture(
            crate::ir::TextureFormat::Rg8Unorm,
            &[0, 19, 17, 255, 0, 213, 231, 255],
            &mut rg,
        );
        assert_eq!(rg, [17, 19, 231, 213]);
    }

    #[test]
    fn capabilities_report_only_advertised_image_readback() {
        assert!(
            !Capabilities::from_negotiated_support(0, 0).supports_image_readback(),
            "readback must not be inferred from rendering support"
        );
        assert!(
            Capabilities::from_negotiated_support(GPU_EXECUTION_SUPPORT_IMAGE_READBACK, 0)
                .supports_image_readback()
        );
    }

    #[test]
    fn native_graphics_requires_known_negotiated_token_and_compiler_feature() {
        assert_eq!(super::ISOLATED_NATIVE_DIALECT_TOKEN, 0x4136_5858_0001_0002);
        for token in [
            0,
            0x4136_5858_0001_0000,
            0x4136_5858_0001_0001,
            0x4136_5858_0001_0003,
            0x4136_5858_0002_0002,
            0x0000_0000_0001_0002,
            u64::MAX,
        ] {
            let caps = Capabilities::from_negotiated_support(u32::MAX, token);
            assert!(!caps.native_programs);
            assert!(!caps.supports_programmable_graphics());
        }
        let caps = Capabilities::from_negotiated_support(0, super::ISOLATED_NATIVE_DIALECT_TOKEN);
        assert!(caps.native_programs);
        assert_eq!(
            caps.supports_programmable_graphics(),
            cfg!(feature = "programmable")
        );
        assert!(!caps.supports_image_readback());
        assert!(!caps.supports_image_mips());
        assert!(!caps.supports_texture_arrays());
    }

    #[test]
    fn readback_chunk_rows_respect_the_abi_payload_limit() {
        assert_eq!(max_readback_rows(4), Ok(GPU_MAX_IMAGE_UPLOAD_SIZE / 4));
        assert_eq!(
            max_readback_rows(GPU_MAX_IMAGE_UPLOAD_SIZE + 4),
            Err(HandleError::InvalidParameter)
        );
    }

    #[test]
    fn readback_destination_uses_identical_rectangle_coordinates() {
        let rect = super::ir::PixelRect::new(2, 1, 3, 2).unwrap();
        assert_eq!(validate_readback_destination(8, 4, 84, 32, rect), Ok(()));
        assert_eq!(
            validate_readback_destination(8, 4, 83, 32, rect),
            Err(HandleError::InvalidParameter)
        );
        assert_eq!(
            validate_readback_destination(8, 4, 80, 19, rect),
            Err(HandleError::InvalidParameter)
        );
    }

    #[test]
    fn readback_destination_rejects_rectangles_outside_the_image() {
        let rect = super::ir::PixelRect::new(7, 0, 2, 1).unwrap();
        assert_eq!(
            validate_readback_destination(8, 4, 36, 36, rect),
            Err(HandleError::InvalidParameter)
        );
    }
}

#[cfg(all(test,feature="programmable"))]
#[path = "../../sgfx-codegen-adreno-a6xx/tests/native_support/mod.rs"]
mod fixed_test_fixtures;
