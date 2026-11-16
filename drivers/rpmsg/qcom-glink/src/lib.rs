// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! Qualcomm GLINK v1 with bounded, intent-based datagrams.
extern crate alloc;
#[cfg(target_os = "none")]
mod native;
pub mod protocol;
#[cfg(target_os = "none")]
pub use native::*;
