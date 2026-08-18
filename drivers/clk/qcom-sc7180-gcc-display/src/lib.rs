// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Minimal SC7180 GCC display-clock bootstrap for alternate-firmware boot.
//!
//! Linux models the MDSS parent, DPU, and DSI controller as consumers of GCC
//! display AHB, HF-AXI, and XO clocks. Depthcharge's alternate-firmware path
//! does not guarantee that those GCC gates remain enabled, while the SN65DSI86
//! internal color bar can still work because it bypasses DPU memory scanout.
//!
//! This driver only restores the GCC resources required before the existing
//! DISP_CC and MDSS drivers program the native scanout pipeline.

extern crate alloc;

use alloc::{boxed::Box, vec};

use scarlet::{
    arch::{self, mmio},
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    early_println,
    time, vm,
};

// The highest register used here is the branch-vote register at 0x52000.
const REGISTER_WINDOW_SIZE: usize = 0x53_000;

// Register layout follows Linux drivers/clk/qcom/gcc-sc7180.c.
const GCC_DISP_AHB_BRANCH: usize = 0x0b00c;
const GCC_DISP_HF_AXI_BRANCH: usize = 0x0b024;
const GCC_DISP_XO_BRANCH: usize = 0x0b030;
const GCC_CLOCK_VOTE: usize = 0x52000;

const GCC_DISP_GPLL0_VOTE: u32 = 1 << 18;
const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;
const BRANCH_TIMEOUT_US: u64 = 1_000;

#[derive(Clone, Copy)]
struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(self, offset: usize) -> u32 {
        // SAFETY: `base` is an ioremap'd GCC register window and every offset
        // used by this driver is below `REGISTER_WINDOW_SIZE`.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        // SAFETY: see `read`; writes target the same mapped GCC window.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn set_bits(self, offset: usize, bits: u32) {
        self.write(offset, self.read(offset) | bits);
    }
}

fn wait_for_branch(registers: RegisterWindow, branch: usize) -> Result<(), &'static str> {
    let start = time::current_time();
    while registers.read(branch) & BRANCH_OFF != 0 {
        if time::current_time().saturating_sub(start) >= BRANCH_TIMEOUT_US {
            return Err("qcom-sc7180-gcc-display: HF-AXI clock failed to start");
        }
        time::udelay(1);
    }
    Ok(())
}

fn prepare_display_clocks(registers: RegisterWindow) -> Result<(), &'static str> {
    let inherited_vote = registers.read(GCC_CLOCK_VOTE);
    let inherited_ahb = registers.read(GCC_DISP_AHB_BRANCH);
    let inherited_hf_axi = registers.read(GCC_DISP_HF_AXI_BRANCH);
    let inherited_xo = registers.read(GCC_DISP_XO_BRANCH);

    // DISP_CC selects GCC's GPLL0 display branch as the parent of its AHB and
    // MDP roots. Keep that branch voted on before touching the consumer gates.
    registers.set_bits(GCC_CLOCK_VOTE, GCC_DISP_GPLL0_VOTE);

    // Linux keeps the display AHB and XO gates enabled during GCC probe. They
    // are needed for register access and the low-rate display clock paths.
    registers.set_bits(GCC_DISP_AHB_BRANCH, BRANCH_ENABLE);
    registers.set_bits(GCC_DISP_XO_BRANCH, BRANCH_ENABLE);

    // DPU and DSI both consume GCC_DISP_HF_AXI_CLK. Without it, the bridge can
    // generate its own color bar while DPU framebuffer traffic never arrives.
    registers.set_bits(GCC_DISP_HF_AXI_BRANCH, BRANCH_ENABLE);
    arch::io_wmb();
    wait_for_branch(registers, GCC_DISP_HF_AXI_BRANCH)?;

    early_println!(
        "[qcom-sc7180-gcc-display] inherited: vote={:#010x} ahb={:#010x} hf-axi={:#010x} xo={:#010x}",
        inherited_vote,
        inherited_ahb,
        inherited_hf_axi,
        inherited_xo,
    );
    early_println!(
        "[qcom-sc7180-gcc-display] ready: vote={:#010x} ahb={:#010x} hf-axi={:#010x} xo={:#010x}",
        registers.read(GCC_CLOCK_VOTE),
        registers.read(GCC_DISP_AHB_BRANCH),
        registers.read(GCC_DISP_HF_AXI_BRANCH),
        registers.read(GCC_DISP_XO_BRANCH),
    );
    Ok(())
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-sc7180-gcc-display: missing GCC memory resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-gcc-display: invalid GCC memory resource")?;
    if resource_size < REGISTER_WINDOW_SIZE {
        return Err("qcom-sc7180-gcc-display: GCC register resource is too small");
    }

    let base = vm::ioremap(resource.start, REGISTER_WINDOW_SIZE)
        .map_err(|_| "qcom-sc7180-gcc-display: GCC ioremap failed")?;
    prepare_display_clocks(RegisterWindow::new(base))
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-sc7180-gcc-display",
        probe_fn,
        remove_fn,
        vec!["qcom,gcc-sc7180"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Critical);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_GCC_DISPLAY_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
