//! Pure SGFX-to-Adreno-A6xx compiler.
//!
//! This crate consumes normalized, transport-independent SGFX operations and
//! returns address-free PM4 with symbolic object references. It deliberately
//! has no device, operating-system, queue-submit, attachment-token, or GPU-VA
//! dependency. The concrete backend materializes generated objects and maps
//! symbolic fixups into its negotiated submission transport.

#![no_std]

extern crate alloc;

mod compiler;
mod emit;
mod model;

pub use compiler::compile;
pub use model::{
    Access, AddressEncoding, Capabilities, Chip, CompileError, CompileInput, DepthAttachment,
    GeneratedObject, GeneratedObjectId, GeneratedObjectKind, ImageMeta, ImageModifier, ObjectId,
    ObjectRef, Operation, PipelineId, PipelineMeta, PlaneLayout, RelocatablePm4, RenderPass,
    ResourceAccess, ResourceKind, ResourceMeta, SymbolicAddress,
};
