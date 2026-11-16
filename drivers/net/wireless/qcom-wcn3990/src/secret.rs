// SPDX-License-Identifier: GPL-2.0-only
//! Erase transport copies of key material without permitting dead-store removal.
pub fn erase(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe {
            core::ptr::write_volatile(byte, 0);
        }
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}
