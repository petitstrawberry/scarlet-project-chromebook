// SPDX-License-Identifier: GPL-2.0-only

//! Portable tests compile production chunk preparation and submit encoding.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

use sgfx_core::ir;

// The production modules need only these error variants; the Scarlet handle
// graph is intentionally excluded from this host-runnable boundary harness.
#[derive(Debug)]
enum IrSubmitError {
    InvalidIr(ir::Error),
    Unsupported(UnsupportedIrFeature),
    OutOfMemory,
    Codegen(sgfx_codegen_adreno_a6xx::CompileError),
    SubmitWire(adreno_a6xx_submit_wire::Error),
    SubmissionTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsupportedIrFeature {
    ResourceState,
    TextureUpload,
}

impl From<ir::Error> for IrSubmitError {
    fn from(error: ir::Error) -> Self {
        Self::InvalidIr(error)
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

#[path = "../../../userspace/sgfx-backend-scarlet-adreno/src/preparation.rs"]
mod preparation;
#[path = "../../../userspace/sgfx-backend-scarlet-adreno/src/scheduler.rs"]
mod scheduler;
#[path = "../../../userspace/sgfx-backend-scarlet-adreno/src/wire.rs"]
mod wire;
