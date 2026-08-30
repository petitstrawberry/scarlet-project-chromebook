//! Scarlet GPU ABI backend for the implemented Qualcomm Adreno A618 SGFX subset.
//!
//! This crate owns the GPU connection, exact backend/dialect negotiation,
//! context-local resource attachments, physical image layouts, command lowering,
//! A6xx submit-wire encoding, and synchronous queue submission. The pure A6xx
//! code generator remains transport-independent.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate scarlet_std as std;

use alloc::{rc::Rc, vec::Vec};

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

mod execute;
mod resource;
mod wire;

use resource::{ContextResources, RawImage};

/// Exact backend identifier advertised by the Qualcomm Adreno kernel backend.
pub const BACKEND_ID: &[u8] = b"qcom-adreno";

/// Exact A6xx command dialect required by this backend.
pub const DIALECT_ID: &[u8] = b"adreno-a6xx-pm4-reloc-v1";

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
}

impl Capabilities {
    const fn from_execution_support(execution_support: u32) -> Self {
        Self {
            rendering: true,
            presentation: execution_support & GPU_EXECUTION_SUPPORT_PRESENTATION != 0,
            image_upload: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD != 0,
            image_readback: execution_support & GPU_EXECUTION_SUPPORT_IMAGE_READBACK != 0,
            depth: true,
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
}

struct DeviceInner {
    gpu: Gpu,
    dialect: gpu_raw::GpuDialect,
    capabilities: Capabilities,
    codegen_capabilities: sgfx_codegen_adreno_a6xx::Capabilities,
}

/// An owning connection to a compatible Scarlet Adreno GPU device.
pub struct Device {
    inner: Rc<DeviceInner>,
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

        let capabilities = Capabilities::from_execution_support(info.execution_support);
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
            inner: Rc::new(DeviceInner {
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
            inner: Rc::new(ContextInner {
                device: Rc::clone(&self.inner),
                raw,
            }),
        })
    }
}

struct ContextInner {
    device: Rc<DeviceInner>,
    raw: gpu_raw::GpuContext,
}

/// An owning Adreno execution context.
pub struct Context {
    inner: Rc<ContextInner>,
}

impl Context {
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
        let queue = self.inner.raw.create_queue()?;
        let mut cache = ContextResources::new(Rc::clone(&resources), Rc::clone(&self.inner))?;
        let mut images = Vec::new();
        images
            .try_reserve_exact(targets.len())
            .map_err(|_| IrSubmitError::OutOfMemory)?;

        for &target in targets {
            if images
                .iter()
                .any(|(candidate, _): &(ir::TextureId, Rc<Image>)| *candidate == target)
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
                Rc::new(self.create_shared_image(
                    descriptor.extent().width(),
                    descriptor.extent().height(),
                )?);
            cache.map_present_image(target, Rc::clone(&image))?;
            images.push((target, image));
        }

        Ok(MappedTargetSession {
            images,
            resources: cache,
            queue,
            context: Context {
                inner: Rc::clone(&self.inner),
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
            raw: Rc::new(raw),
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
    /// The canonical A6xx submit-wire encoder rejected its input.
    SubmitWire(adreno_a6xx_submit_wire::Error),
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
}

/// Session owning mapped presentation images and all execution state.
pub struct MappedTargetSession {
    // These owners must drop before the queue/context that authorized them.
    images: Vec<(ir::TextureId, Rc<Image>)>,
    resources: ContextResources,
    queue: gpu_raw::GpuQueue,
    context: Context,
}

impl MappedTargetSession {
    /// Import a transferred shared BGRA image into a logical sampled texture.
    pub fn import_shared_bgra_texture(
        &mut self,
        texture: ir::TextureId,
        handle: Handle,
    ) -> Result<(), IrSubmitError> {
        self.resources.import_sampled_image(texture, handle)
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
    raw: Rc<RawImage>,
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
    queue: &'a gpu_raw::GpuQueue,
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
    fn capabilities_report_only_advertised_image_readback() {
        assert!(
            !Capabilities::from_execution_support(0).supports_image_readback(),
            "readback must not be inferred from rendering support"
        );
        assert!(
            Capabilities::from_execution_support(GPU_EXECUTION_SUPPORT_IMAGE_READBACK)
                .supports_image_readback()
        );
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
