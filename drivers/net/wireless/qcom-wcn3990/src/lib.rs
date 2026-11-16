// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
extern crate alloc;
pub mod board;
#[cfg(target_os = "none")]
mod ce;
pub mod channel;
#[cfg(target_os = "none")]
mod datapath;
pub mod htc;
pub mod htt;
#[cfg(target_os = "none")]
mod mlme;
#[cfg(target_os = "none")]
mod native;
#[cfg(target_os = "none")]
mod power;
pub mod qmi;
pub mod qrtr;
#[cfg(target_os = "none")]
mod radio;
pub mod rmtfs;
mod secret;
pub mod sta;
pub mod wlfw;
pub mod wmi;
pub fn force_link() {}
