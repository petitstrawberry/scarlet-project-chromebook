// SPDX-License-Identifier: GPL-2.0-only
#![no_std]

//! Qualcomm SMEM v12 partitions for AP-to-coprocessor transports.
//!
//! The layout engine has no kernel dependency. The native adapter owns the
//! noncached mapping and TCSR remote lock; no shared RAM is exposed as a Rust
//! reference. Allocations persist for the lifetime of the remote firmware.

pub mod layout;
#[cfg(target_os = "none")]
mod native;
#[cfg(target_os = "none")]
pub use native::{Error, Item, allocate, get, physical_region};

/// Keep the external module linked into the BSP.
#[inline(never)]
pub fn force_link() {}
