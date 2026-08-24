// SPDX-License-Identifier: GPL-2.0-only

//! Firmware loading through Scarlet's mounted VFS.

use alloc::{vec, vec::Vec};

use scarlet::{fs::manager::get_global_vfs_manager_safe, object::KernelObject};

// Kernel drivers deliberately resolve firmware through the global VFS. That
// namespace retains the initramfs root and does not apply Scarlet ABI aliases
// such as `/lib` -> `/system/scarlet/lib` from a caller's task-local VFS.
pub(crate) const GMU_FIRMWARE_PATH: &str = "/system/scarlet/lib/firmware/qcom/a630_gmu.bin";
pub(crate) const SQE_FIRMWARE_PATH: &str = "/system/scarlet/lib/firmware/qcom/a630_sqe.fw";

pub(crate) fn load(path: &str, maximum_size: usize) -> Result<Vec<u8>, &'static str> {
    let vfs = get_global_vfs_manager_safe().ok_or("qcom-adreno-a618: VFS is not ready")?;
    let object = vfs
        .open(path, 0)
        .map_err(|_| "qcom-adreno-a618: firmware file is unavailable")?;
    let KernelObject::File(file) = object else {
        return Err("qcom-adreno-a618: firmware path is not a file");
    };
    let size = file
        .metadata()
        .map_err(|_| "qcom-adreno-a618: firmware metadata read failed")?
        .size;
    if size == 0 || size > maximum_size {
        return Err("qcom-adreno-a618: firmware size is invalid");
    }
    let mut bytes = vec![0; size];
    let mut offset = 0;
    while offset < size {
        let read = file
            .read(&mut bytes[offset..])
            .map_err(|_| "qcom-adreno-a618: firmware read failed")?;
        if read == 0 {
            return Err("qcom-adreno-a618: firmware ended early");
        }
        offset = offset
            .checked_add(read)
            .ok_or("qcom-adreno-a618: firmware read offset overflows")?;
    }
    Ok(bytes)
}
