// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! SC7180 MSS PIL: staged MBA authentication and non-LTE modem firmware.
extern crate alloc;
pub mod mdt;
#[cfg(target_os = "none")]
mod native;
#[cfg(target_os = "none")]
pub use native::{Mss, controller};

pub fn force_link() {}
