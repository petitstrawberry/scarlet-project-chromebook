// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 Venus stateful video decoder.
//!
//! # Provenance
//!
//! Clocking, no-TrustZone firmware boot, AR50 registers, HFI 4xx packets,
//! buffer sizing, and decode sequencing follow the upstream Linux Qualcomm
//! Venus driver. Scarlet's video-device integration follows the same backend,
//! DMA-lifetime, and interrupt-source boundaries used by the Apple AVD driver.

extern crate alloc;

mod backend;
mod firmware;
mod hfi;
mod hfi_abi;
mod memory;
mod registers;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch,
    device::{
        clk::{ClkError, ClkHandle},
        iommu::{IommuDomainConfig, IommuDomainType, IommuMapFlags},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        video::{VideoDecodeBackend, register_video_backend, register_video_decode_device},
    },
    interrupt::{InterruptError, InterruptManager, InterruptSource},
    println, vm,
};

use backend::{FirmwareRegion, VenusBackend};
use qcom_sc7180_interconnect::venus_interconnect_paths;
use registers::VenusRegisters;

const VENUS_REGISTER_SIZE: usize = 0x0f_f000;
const MAIN_IOVA_BASE: u64 = 0x1000_0000;
const MAIN_IOVA_SIZE: u64 = 0xc000_0000;
const FIRMWARE_IOVA_SIZE: u64 = 0x1_0000_0000;
const VIDEO_CLOCK_NAMES: [&str; 5] = ["core", "iface", "bus", "vcodec0_core", "vcodec0_bus"];

pub(crate) struct EnabledVideoClocks {
    clocks: Vec<ClkHandle>,
}

impl EnabledVideoClocks {
    fn acquire(device: &PlatformDeviceInfo) -> Result<Self, &'static str> {
        let manager = DeviceManager::get_manager();
        let mut enabled = Self {
            clocks: Vec::with_capacity(VIDEO_CLOCK_NAMES.len()),
        };
        for name in VIDEO_CLOCK_NAMES {
            let clock = match manager.resolve_clk(device, name) {
                Ok(clock) => clock,
                Err("clk: provider not found") | Err("clk: clock not found") => {
                    return probe_defer();
                }
                Err(error) => return Err(error),
            };
            clock.prepare_enable().map_err(|error| match error {
                ClkError::ClockNotFound => scarlet::device::manager::PROBE_DEFER,
                _ => "qcom-venus-sc7180: failed to enable a video clock",
            })?;
            enabled.clocks.push(clock);
        }
        Ok(enabled)
    }
}

impl Drop for EnabledVideoClocks {
    fn drop(&mut self) {
        for clock in self.clocks.iter().rev() {
            clock.disable_unprepare();
        }
    }
}

fn resolve_firmware_region(device: &PlatformDeviceInfo) -> Result<FirmwareRegion, &'static str> {
    let manager = DeviceManager::get_manager();
    let region = manager.resolve_platform_memory_region(device, "memory-region", 0)?;
    let paddr = region.start;
    let size = region
        .end
        .checked_sub(region.start)
        .and_then(|span| span.checked_add(1))
        .ok_or("qcom-venus-sc7180: firmware reserved-memory range overflows")?;
    if paddr == 0 || size == 0 || paddr % 4096 != 0 || size % 4096 != 0 {
        return Err("qcom-venus-sc7180: invalid firmware reserved-memory range");
    }

    let dma = manager.resolve_platform_child_dma_context(
        device,
        "video-firmware",
        IommuDomainConfig {
            domain_type: IommuDomainType::Dma,
            iova_base: 0,
            iova_size: FIRMWARE_IOVA_SIZE,
        },
    )?;
    if dma.iommu.is_none() {
        return Err("qcom-venus-sc7180: firmware requester has no IOMMU");
    }
    let mapping = dma
        .map_phys_at_owned(
            0,
            paddr,
            size,
            IommuMapFlags::READ
                | IommuMapFlags::WRITE
                | IommuMapFlags::EXECUTE
                | IommuMapFlags::PRIVILEGED,
        )
        .map_err(|_| "qcom-venus-sc7180: firmware IOVA-zero mapping failed")?;
    dma.restore_iommu()
        .map_err(|_| "qcom-venus-sc7180: firmware IOMMU flush failed")?;
    let vaddr = vm::memremap_normal(paddr, size)
        .map_err(|_| "qcom-venus-sc7180: firmware reserved-memory mapping failed")?;
    Ok(FirmwareRegion {
        paddr,
        vaddr,
        size,
        dma,
        _mapping: mapping,
    })
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let interconnect_paths = venus_interconnect_paths(device)?;
    interconnect_paths.enable_firmware_boot()?;
    let video_clocks = EnabledVideoClocks::acquire(device)?;

    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-venus-sc7180: missing MMIO resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|span| span.checked_add(1))
        .ok_or("qcom-venus-sc7180: invalid MMIO resource")?;
    if size < VENUS_REGISTER_SIZE {
        return Err("qcom-venus-sc7180: MMIO resource is too small");
    }
    let base = vm::ioremap(resource.start, VENUS_REGISTER_SIZE)
        .map_err(|_| "qcom-venus-sc7180: MMIO ioremap failed")?;
    let registers = VenusRegisters::new(base);
    registers.mask_interrupts();
    registers.clear_pending_interrupts();

    let dma = DeviceManager::get_manager().resolve_platform_dma_context(
        device,
        IommuDomainConfig {
            domain_type: IommuDomainType::Dma,
            iova_base: MAIN_IOVA_BASE,
            iova_size: MAIN_IOVA_SIZE,
        },
    )?;
    let firmware = resolve_firmware_region(device)?;
    let firmware_paddr = firmware.paddr;
    let firmware_size = firmware.size;
    let irq_resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("qcom-venus-sc7180: missing IRQ resource")?;
    let interrupt_id = match scarlet::interrupt::resolve_platform_irq(irq_resource) {
        Ok(interrupt_id) => interrupt_id,
        Err(InterruptError::ControllerNotFound) => return probe_defer(),
        Err(_) => return Err("qcom-venus-sc7180: failed to resolve IRQ"),
    };

    let backend = Arc::new(VenusBackend::new(
        registers,
        dma,
        firmware,
        interconnect_paths,
        video_clocks,
        interrupt_id,
    ));
    let source: Arc<dyn InterruptSource> = backend.clone();
    InterruptManager::global()
        .register_interrupt_source(interrupt_id, source)
        .map_err(|_| "qcom-venus-sc7180: failed to register IRQ source")?;
    InterruptManager::global()
        .enable_external_interrupt(interrupt_id, arch::get_cpu().get_cpuid() as u32)
        .map_err(|_| "qcom-venus-sc7180: failed to enable IRQ")?;

    backend.start_worker();
    let video_backend: Arc<dyn VideoDecodeBackend> = backend;
    let backend_id = register_video_backend(Arc::clone(&video_backend));
    let device_name = register_video_decode_device(video_backend);
    println!(
        "[qcom-venus-sc7180] registered backend={} device={} mmio={:#x}+{:#x} irq={} firmware={:#x}+{:#x}",
        backend_id,
        device_name,
        resource.start,
        VENUS_REGISTER_SIZE,
        interrupt_id,
        firmware_paddr,
        firmware_size
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-venus-sc7180",
            probe,
            remove,
            vec!["qcom,sc7180-venus"],
        )),
        DriverPriority::Standard,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_VENUS_SC7180_ANCHOR: fn() = force_link;

/// Keep the SC7180 Venus driver linked into Scarlet module bundles.
pub fn force_link() {}
