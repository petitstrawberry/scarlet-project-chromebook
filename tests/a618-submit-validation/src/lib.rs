// SPDX-License-Identifier: GPL-2.0-only

//! Host-runnable boundary tests for the A618 userspace emitter and kernel validator.

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

extern crate alloc;

// Keep one validator implementation: tests compile the exact source used by
// the kernel driver without pulling the target-only Scarlet driver graph into
// a host test binary.
#[path = "../../../drivers/gpu/qcom-adreno-a618/src/submit.rs"]
mod submit;

#[path = "../../../drivers/gpu/qcom-adreno-a618/src/isolation.rs"]
mod isolation;

#[path = "../../../drivers/gpu/qcom-adreno-a618/src/completion_wait.rs"]
mod completion_wait;

#[path = "../../../drivers/gpu/qcom-adreno-a618/src/readback.rs"]
mod readback;

#[cfg(test)]
mod native;

#[cfg(all(test, feature = "programmable"))]
mod codegen_native;
