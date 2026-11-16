// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! SC7180's SMCCC64 Qualcomm secure monitor interface.
//!
//! MSS PIL boots through MBA/RMB, but its memory ownership changes still go
//! through SCM. This is deliberately separate from Venus's direct MDT loader.

#[cfg(target_os = "none")]
mod native;
#[cfg(target_os = "none")]
pub use native::{Error, assign_memory, memory_assignment_available};

/// Qualcomm VM IDs used by SC7180 MSS and WLAN shared memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Host {
    Hlos = 3,
    Mss = 15,
    Wlan = 24,
    WlanCe = 25,
}

#[derive(Clone, Copy, Debug)]
pub struct Permission {
    pub host: Host,
    /// Qualcomm permission bits: read=4, write=2, execute=1.
    pub bits: u32,
}

pub fn force_link() {}
