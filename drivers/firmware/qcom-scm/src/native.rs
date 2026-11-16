// SPDX-License-Identifier: GPL-2.0-only
// SCM call/descriptor ABI follows Linux drivers/firmware/qcom_scm{,-smc}.c.
// TF-A's QTI SiP handler implements service 0x0c, command 0x16 with arginfo
// 0x1117 and an indirect array of SMCCC64 arguments 3..6.
extern crate alloc;
use crate::{Host, Permission};
use alloc::{boxed::Box, vec};
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::{
    arch,
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    mem::page::ContiguousPages,
    sync::Mutex,
};

static READY: AtomicBool = AtomicBool::new(false);
static ASSIGN_AVAILABLE: AtomicBool = AtomicBool::new(false);
// A yielded call retains its descriptor page and poisons this service if it
// cannot be resumed. Never recycle storage still potentially used by firmware.
static CALL_STATE: Mutex<bool> = Mutex::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotReady,
    Unsupported,
    InvalidRange,
    InvalidOwners,
    NoMemory,
    Monitor(i32),
    Incomplete,
}

fn invoke(function: u32, arginfo: u32, args: [u64; 4]) -> Result<[u64; 3], Error> {
    let mut function = u64::from(function);
    let mut resume = 0u64;
    for _ in 0..64 {
        let mut a0 = function;
        let mut a1 = u64::from(arginfo);
        let mut a2 = args[0];
        let mut a3 = args[1];
        // SAFETY: only the enabled SCM platform driver calls this wrapper.
        // Preserve QCOM_A6 continuation state and declare SMCCC clobbers.
        unsafe {
            core::arch::asm!("smc #0",
                inout("x0") a0, inout("x1") a1,
                inout("x2") a2, inout("x3") a3,
                in("x4") args[2], in("x5") args[3],
                inout("x6") resume, in("x7") 0u64,
                clobber_abi("C"), options(nostack));
        }
        match a0 as i32 {
            0 => return Ok([a1, a2, a3]),
            1 => function = 1, // interrupted call: resume, do not resubmit
            status if status < 0 => return Err(Error::Monitor(status)),
            _ => return Err(Error::Incomplete),
        }
    }
    Err(Error::Incomplete)
}

fn available(service: u32, command: u32) -> Result<bool, Error> {
    // Atomic availability query; target ID omits FAST/SMC64 convention bits.
    let result = invoke(
        0xc200_0601,
        1,
        [u64::from(0x0200_0000 | service << 8 | command), 0, 0, 0],
    )?;
    Ok(result[0] == 1)
}

pub fn memory_assignment_available() -> bool {
    READY.load(Ordering::Acquire) && ASSIGN_AVAILABLE.load(Ordering::Acquire)
}

/// Transfer exclusive/shared ownership of a page-aligned physical allocation.
/// `owners` is updated only after SCM confirms the complete operation.
///
/// # Safety
/// The caller owns this complete reserved/DMA range and its current VM mask.
/// It must stop every CPU/device accessor before removing that accessor's VM,
/// retain backing pages until ownership is recovered, and quarantine on error
/// rather than freeing memory based on a cancelled software request.
pub unsafe fn assign_memory(
    paddr: u64,
    size: usize,
    owners: &mut u64,
    destination: &[Permission],
) -> Result<(), Error> {
    if !READY.load(Ordering::Acquire) {
        return Err(Error::NotReady);
    }
    if !ASSIGN_AVAILABLE.load(Ordering::Acquire) {
        return Err(Error::Unsupported);
    }
    if paddr == 0
        || paddr % 4096 != 0
        || size == 0
        || size % 4096 != 0
        || paddr.checked_add(size as u64).is_none()
    {
        return Err(Error::InvalidRange);
    }
    let hosts = [Host::Hlos, Host::Mss, Host::Wlan, Host::WlanCe];
    let allowed = hosts
        .iter()
        .fold(0u64, |mask, host| mask | (1u64 << *host as u32));
    if *owners == 0 || *owners & !allowed != 0 || destination.is_empty() || destination.len() > 4 {
        return Err(Error::InvalidOwners);
    }
    let mut next = 0u64;
    for permission in destination {
        let bit = 1u64 << permission.host as u32;
        if next & bit != 0 || permission.bits == 0 || permission.bits & !7 != 0 {
            return Err(Error::InvalidOwners);
        }
        next |= bit;
    }
    let mut poisoned = CALL_STATE.lock();
    if *poisoned {
        return Err(Error::Incomplete);
    }
    let pages = ContiguousPages::new(1).ok_or(Error::NoMemory)?;
    let address = pages.as_vaddr();
    let physical = pages.as_paddr() as u64;
    // A normal cached page matches the dma-coherent TF-A mapping. Clean before
    // entering firmware as well, so older handoffs do not depend on AP cache
    // residency. No pointers to kernel virtual addresses cross the SMC ABI.
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, 4096);
    }
    let put32 = |offset: usize, value: u32| unsafe {
        core::ptr::write((address + offset) as *mut u32, value.to_le());
    };
    let put64 = |offset: usize, value: u64| unsafe {
        core::ptr::write((address + offset) as *mut u64, value.to_le());
    };
    put64(0, paddr);
    put64(8, size as u64);
    let mut sources = 0u64;
    for host in hosts {
        if *owners & (1u64 << host as u32) != 0 {
            put32(64 + sources as usize * 4, host as u32);
            sources += 1;
        }
    }
    for (index, permission) in destination.iter().enumerate() {
        // vmid:u32, perm:u32, ctx:u64, ctx_size:u32, reserved:u32
        put32(128 + index * 24, permission.host as u32);
        put32(132 + index * 24, permission.bits);
    }
    for (index, value) in [
        sources * 4,
        physical + 128,
        destination.len() as u64 * 24,
        0,
    ]
    .into_iter()
    .enumerate()
    {
        put64(256 + index * 8, value);
    }
    arch::clean_dcache_to_poc_range(address, 4096);
    arch::io_mb();
    let result = invoke(
        0x4200_0c16,
        0x1117,
        [physical, 16, physical + 64, physical + 256],
    );
    match result {
        Ok(result) if result[0] == 0 => {
            *owners = next;
            Ok(())
        }
        Ok(result) => Err(Error::Monitor(result[0] as i32)),
        Err(Error::Incomplete) => {
            *poisoned = true;
            core::mem::forget(pages);
            Err(Error::Incomplete)
        }
        Err(error) => Err(error),
    }
}

fn probe(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if READY.load(Ordering::Acquire) {
        return Err("qcom-scm: already registered");
    }
    let mut poisoned = CALL_STATE.lock();
    if *poisoned {
        return Err("qcom-scm: unretired firmware call");
    }
    let assign = match available(0x0c, 0x16) {
        Ok(value) => value,
        Err(Error::Incomplete) => {
            *poisoned = true;
            return Err("qcom-scm: incomplete availability query");
        }
        Err(_) => return Err("qcom-scm: SMCCC64 unavailable"),
    };
    ASSIGN_AVAILABLE.store(assign, Ordering::Release);
    READY.store(true, Ordering::Release);
    scarlet::println!("[qcom-scm] SMCCC64 memory-assignment={}", assign);
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    // Clients may have firmware-owned ranges. Unregistering the only recovery
    // service while those clients exist is not a safe device removal operation.
    Err("qcom-scm: firmware service is permanent")
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-scm",
            probe,
            remove,
            vec!["qcom,scm-sc7180"],
        )),
        DriverPriority::Critical,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_SCM_ANCHOR: fn() = crate::force_link;
