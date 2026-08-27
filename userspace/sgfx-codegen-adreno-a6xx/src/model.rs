//! Transport-independent input and output artifacts for A6xx compilation.

use adreno_a6xx_shader_pack::ShaderVariant;
use alloc::vec::Vec;

use sgfx_core::ir::{
    BufferUsage, DepthLoadOp, DrawUniforms, Extent2D, IndexFormat, LoadOp, PixelRect,
    RenderPipelineDesc, SamplerDesc, StoreOp, TextureFormat, TextureUsage,
};

/// Identity of an externally materialized object within one compilation.
///
/// An `ObjectId` is a dense compiler-local key. It is neither a capability,
/// an operating-system handle, an attachment token, nor a GPU address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(u32);

impl ObjectId {
    /// Construct a compiler-local object identity.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Return the caller-assigned compiler-local value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Identity of a pipeline within one compilation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PipelineId(u32);

impl PipelineId {
    /// Construct a compiler-local pipeline identity.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Return the caller-assigned compiler-local value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Identity of an object whose bytes are produced by this compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GeneratedObjectId(u32);

impl GeneratedObjectId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Return the compiler-assigned value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A618-family target selected by the backend after device discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// Qualcomm Adreno 618.
    A618,
}

/// Immutable device limits used while compiling one command buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Concrete A6xx target.
    pub chip: Chip,
    /// On-chip GMEM capacity reported by the device.
    pub gmem_size: u32,
    /// Backend transport budget, expressed as PM4 dwords.
    pub max_pm4_words: u32,
    /// Maximum linear image pitch accepted by the target.
    pub max_linear_pitch: u32,
}

impl Capabilities {
    /// Construct the initial A618 capability profile.
    pub const fn a618(gmem_size: u32, max_pm4_words: u32) -> Self {
        Self {
            chip: Chip::A618,
            gmem_size,
            max_pm4_words,
            max_linear_pitch: 0x003f_ffc0,
        }
    }
}

/// Image memory layout understood by this compiler revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageModifier {
    /// One or more uncompressed linear planes.
    Linear,
}

/// Immutable layout of one image plane in an external allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Byte offset from the beginning of the allocation.
    pub offset: u64,
    /// Bytes between adjacent rows.
    pub stride: u32,
    /// Number of allocation bytes occupied by this plane.
    pub size: u64,
}

/// Image metadata selected before compilation by the concrete backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMeta {
    /// Portable SGFX pixel format visible to shaders and upload commands.
    pub format: TextureFormat,
    /// Format of the materialized linear allocation.
    ///
    /// Scarlet backs `R8Unorm` and `Rgba8Unorm` sampled images with BGRA8
    /// allocations. Keeping both formats prevents logical upload row sizes
    /// from being confused with the physical GPU row layout.
    pub storage_format: TextureFormat,
    /// Logical pixel extent.
    pub extent: Extent2D,
    /// Operations authorized by the logical resource.
    pub usage: TextureUsage,
    /// Physical memory organization.
    pub modifier: ImageModifier,
    /// Plane layouts in format order.
    pub planes: Vec<PlaneLayout>,
}

/// Kind and portable authority of one external object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// Byte-addressed SGFX buffer.
    Buffer {
        /// Operations authorized by its SGFX descriptor.
        usage: BufferUsage,
    },
    /// Backend-materialized SGFX image.
    Image(ImageMeta),
}

/// Metadata for one externally materialized object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceMeta {
    /// Compiler-local identity.
    pub id: ObjectId,
    /// Total allocation size.
    pub size: u64,
    /// Object kind and SGFX usage.
    pub kind: ResourceKind,
}

/// A normalized pipeline and its validated fixed shader artifacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineMeta {
    /// Compiler-local pipeline identity.
    pub id: PipelineId,
    /// Complete portable SGFX pipeline descriptor.
    pub descriptor: RenderPipelineDesc,
}

/// Normalized color and optional depth attachment state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderPass {
    /// Color attachment.
    pub target: ObjectId,
    /// Render area.
    pub area: PixelRect,
    /// Color attachment initialization.
    pub load: LoadOp,
    /// Color attachment finalization.
    pub store: StoreOp,
    /// Optional depth attachment.
    pub depth: Option<DepthAttachment>,
}

/// Normalized depth attachment state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepthAttachment {
    /// Depth image.
    pub target: ObjectId,
    /// Depth attachment initialization.
    pub load: DepthLoadOp,
    /// Depth attachment finalization.
    pub store: StoreOp,
}

/// One backend-private normalized SGFX operation.
///
/// This represents the current SGFX command vocabulary while replacing
/// lifetime-branded SGFX resource references with compile-local identities.
/// It is an in-process compiler input, not a kernel/userspace protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum Operation<'data> {
    /// Upload bytes to a logical buffer.
    WriteBuffer {
        /// Destination buffer.
        destination: ObjectId,
        /// Destination byte offset.
        offset: u64,
        /// Borrowed upload bytes.
        data: &'data [u8],
    },
    /// Upload pixels to a logical image.
    WriteTexture {
        /// Destination image.
        destination: ObjectId,
        /// Destination rectangle.
        area: PixelRect,
        /// Source bytes between adjacent rows.
        bytes_per_row: u32,
        /// Borrowed upload bytes.
        data: &'data [u8],
    },
    /// Copy an equal-sized image rectangle.
    CopyTextureToTexture {
        /// Source image.
        source: ObjectId,
        /// Source rectangle.
        source_rect: PixelRect,
        /// Destination image.
        destination: ObjectId,
        /// Destination rectangle.
        destination_rect: PixelRect,
    },
    /// Begin a render pass.
    BeginRenderPass(RenderPass),
    /// End the active render pass.
    EndRenderPass,
    /// Bind a normalized pipeline.
    SetPipeline(PipelineId),
    /// Bind the interleaved vertex buffer.
    SetVertexBuffer {
        /// Vertex buffer.
        buffer: ObjectId,
        /// Byte offset of vertex zero.
        offset: u64,
    },
    /// Bind the index buffer.
    SetIndexBuffer {
        /// Index buffer.
        buffer: ObjectId,
        /// Byte offset of index zero.
        offset: u64,
        /// Encoded index width.
        format: IndexFormat,
    },
    /// Bind one sampled image.
    SetTexture(ObjectId),
    /// Bind a fully normalized sampler value.
    SetSampler(SamplerDesc),
    /// Bind transform and draw color constants.
    SetUniforms(DrawUniforms),
    /// Set or reset the draw scissor.
    SetScissor(Option<PixelRect>),
    /// Draw non-indexed vertices.
    Draw {
        /// Vertex count.
        vertex_count: u32,
        /// First vertex.
        first_vertex: u32,
    },
    /// Draw indexed vertices.
    DrawIndexed {
        /// Index count.
        index_count: u32,
        /// First index.
        first_index: u32,
        /// Signed base vertex.
        base_vertex: i32,
    },
}

/// Complete pure input for one compilation.
#[derive(Clone, Copy, Debug)]
pub struct CompileInput<'a> {
    /// Immutable A6xx capabilities.
    pub capabilities: Capabilities,
    /// Materialized-resource metadata.
    pub resources: &'a [ResourceMeta],
    /// Normalized pipeline table.
    pub pipelines: &'a [PipelineMeta],
    /// Ordered normalized commands.
    pub operations: &'a [Operation<'a>],
}

/// Read/write requirements independent of any submission ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access(u8);

impl Access {
    /// GPU reads object contents.
    pub const READ: Self = Self(1 << 0);
    /// GPU writes object contents.
    pub const WRITE: Self = Self(1 << 1);

    /// Return whether all bits in `other` are present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Combine two access sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Return the compiler-private bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl core::ops::BitOr for Access {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

/// An object referenced by relocatable PM4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectRef {
    /// Object materialized before compilation.
    External(ObjectId),
    /// Object whose contents are returned by this compilation.
    Generated(GeneratedObjectId),
    /// Immutable program selected from the shared canonical shader pack.
    CanonicalShader(ShaderVariant),
}

/// Address representation required at one PM4 field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressEncoding {
    /// Two adjacent zero dwords patched with one 64-bit GPU virtual address.
    GpuVa64,
    /// A6xx TEX_MEMOBJ words 4/5: VA bits 5..48 while preserving word 5
    /// bits 17..31 (including the texture depth field).
    GpuVa49TexDescriptor,
}

impl AddressEncoding {
    /// Number of zero placeholder dwords occupied by this encoding.
    pub const fn word_count(self) -> u32 {
        match self {
            Self::GpuVa64 | Self::GpuVa49TexDescriptor => 2,
        }
    }

    pub(crate) fn placeholder_is_valid(self, words: &[u32]) -> bool {
        match self {
            Self::GpuVa64 => words == [0, 0],
            Self::GpuVa49TexDescriptor => {
                words.len() == 2 && words[0] == 0 && words[1] & 0x1ffff == 0
            }
        }
    }
}

/// One symbolic address in the address-free PM4 template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolicAddress {
    /// First zero placeholder in [`RelocatablePm4::words`].
    pub word_offset: u32,
    /// Referenced compiler-local object.
    pub object: ObjectRef,
    /// Byte offset within that object.
    pub object_offset: u64,
    /// Minimum valid bytes beginning at `object_offset`.
    pub required_size: u64,
    /// Access performed through this address.
    pub access: Access,
    /// Hardware address representation.
    pub encoding: AddressEncoding,
}

/// Consolidated object range touched by the generated program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceAccess {
    /// Referenced compiler-local object.
    pub object: ObjectRef,
    /// First byte touched.
    pub offset: u64,
    /// Number of bytes made accessible.
    pub size: u64,
    /// Read/write access.
    pub access: Access,
}

/// Purpose of a compiler-generated object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeneratedObjectKind {
    /// Immutable upload staging bytes.
    Upload,
    /// Writable sequence word completed by addressed CCU clean events.
    CcuSequence,
    /// Writable descriptor storage consumed by the FS texture unit.
    TextureState,
}

/// Caller-materialized bytes generated by the pure compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedObject {
    /// Compiler-assigned identity used by PM4 fixups.
    pub id: GeneratedObjectId,
    /// Allocation alignment required by the hardware.
    pub alignment: u64,
    /// Initial contents copied into the caller-owned allocation.
    pub bytes: Vec<u8>,
    /// GPU access after initialization.
    pub access: Access,
    /// Semantic purpose of this allocation.
    pub kind: GeneratedObjectKind,
}

/// Address-free PM4 plus compiler-owned symbolic metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelocatablePm4 {
    /// Logical PM4 dwords. Every address field covered by `fixups` is zero.
    pub words: Vec<u32>,
    /// Strictly word-offset-sorted symbolic addresses.
    pub fixups: Vec<SymbolicAddress>,
    /// Consolidated external and generated object ranges.
    pub accesses: Vec<ResourceAccess>,
    /// Compiler-owned objects the backend must allocate and initialize.
    pub generated_objects: Vec<GeneratedObject>,
}

/// Pure compilation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// Input tables contain duplicate or missing compiler-local identities.
    InvalidIdentity,
    /// A resource kind, usage, format, or layout does not match an operation.
    InvalidResource,
    /// An offset, rectangle, or draw range exceeds its resource.
    OutOfBounds,
    /// Integer arithmetic overflowed.
    Overflow,
    /// Normalized command ordering or required state is invalid.
    InvalidState,
    /// This compiler revision cannot lower a represented SGFX feature yet.
    UnsupportedFeature,
    /// The output would exceed the queried PM4 budget.
    CommandBudgetExceeded,
    /// Allocation for a compiler-owned artifact failed.
    OutOfMemory,
    /// Shared PM4 grammar rejected a field.
    InvalidPm4,
}
