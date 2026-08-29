// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 Venus AR50 register layout.
//!
//! Offsets follow Linux `hfi_venus_io.h` for the non-lite Venus 4xx block.

use scarlet::{
    arch::{self, mmio},
    println, time,
};

const CPU_BASE: usize = 0xc0000;
const CPU_CS_BASE: usize = CPU_BASE + 0x12000;
const CPU_IC_BASE: usize = CPU_BASE + 0x1f000;
const WRAPPER_BASE: usize = 0xe0000;

const CPU_CS_A2HSOFTINTCLR: usize = 0x1c;
const VIDC_CTRL_INIT: usize = 0x48;
const CPU_CS_SCIACMDARG0: usize = 0x4c;
const CPU_CS_SCIACMDARG1: usize = 0x50;
const CPU_CS_SCIACMDARG2: usize = 0x54;
const SFR_ADDR: usize = 0x5c;
const UC_REGION_ADDR: usize = 0x64;
const UC_REGION_SIZE: usize = 0x68;

const CPU_IC_SOFTINT: usize = 0x18;
const CPU_IC_SOFTINT_H2A: u32 = 0x8000;

const WRAPPER_HW_VERSION: usize = 0x00;
const WRAPPER_CLOCK_CONFIG: usize = 0x04;
const WRAPPER_INTR_STATUS: usize = 0x0c;
const WRAPPER_INTR_MASK: usize = 0x10;
const WRAPPER_INTR_CLEAR: usize = 0x14;
const WRAPPER_INTR_MASK_A2HVCODEC: u32 = 0x8;
const WRAPPER_INTR_MASK_ALL: u32 = 0x1c;
const WRAPPER_CPU_CLOCK_CONFIG: usize = 0x2000;
const WRAPPER_CPU_CGC_DIS: usize = 0x2010;
const WRAPPER_CPU_STATUS: usize = 0x2014;
const WRAPPER_POWER_STATUS: usize = 0x44;
const WRAPPER_VCODEC0_POWER_STATUS: usize = 0x90;
const WRAPPER_VCODEC0_POWER_CONTROL: usize = 0x94;
const WRAPPER_CPA_START_ADDR: usize = 0x1020;
const WRAPPER_CPA_END_ADDR: usize = 0x1024;
const WRAPPER_FW_START_ADDR: usize = 0x1028;
const WRAPPER_FW_END_ADDR: usize = 0x102c;
const WRAPPER_NONPIX_START_ADDR: usize = 0x1030;
const WRAPPER_NONPIX_END_ADDR: usize = 0x1034;
const WRAPPER_A9SS_SW_RESET: usize = 0x3000;
const WRAPPER_A9SS_SW_RESET_BIT: u32 = 1 << 4;

const BOOT_TIMEOUT_US: u64 = 100_000;

/// One mapped Venus register window.
#[derive(Clone, Copy)]
pub(crate) struct VenusRegisters {
    base: usize,
}

impl VenusRegisters {
    pub(crate) const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: probe maps the complete 0xff000 Venus resource and every
        // offset used here is within that resource.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: see `read`; the same bounded mapping backs this write.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn cpu_cs_read(&self, offset: usize) -> u32 {
        self.read(CPU_CS_BASE + offset)
    }

    fn cpu_cs_write(&self, offset: usize, value: u32) {
        self.write(CPU_CS_BASE + offset, value)
    }

    fn wrapper_read(&self, offset: usize) -> u32 {
        self.read(WRAPPER_BASE + offset)
    }

    fn wrapper_write(&self, offset: usize, value: u32) {
        self.write(WRAPPER_BASE + offset, value)
    }

    pub(crate) fn assert_arm9_reset(&self) {
        let reset = self.wrapper_read(WRAPPER_A9SS_SW_RESET);
        self.wrapper_write(WRAPPER_A9SS_SW_RESET, reset | WRAPPER_A9SS_SW_RESET_BIT);
        arch::io_mb();
    }

    pub(crate) fn release_arm9(&self, firmware_size: usize) -> Result<(), &'static str> {
        let firmware_size = u32::try_from(firmware_size)
            .map_err(|_| "qcom-venus-sc7180: firmware region exceeds 32-bit wrapper range")?;
        self.wrapper_write(WRAPPER_FW_START_ADDR, 0);
        self.wrapper_write(WRAPPER_FW_END_ADDR, firmware_size);
        self.wrapper_write(WRAPPER_CPA_START_ADDR, 0);
        self.wrapper_write(WRAPPER_CPA_END_ADDR, firmware_size);
        self.wrapper_write(WRAPPER_NONPIX_START_ADDR, firmware_size);
        self.wrapper_write(WRAPPER_NONPIX_END_ADDR, firmware_size);
        self.wrapper_write(WRAPPER_CPU_CGC_DIS, 0);
        self.wrapper_write(WRAPPER_CPU_CLOCK_CONFIG, 0);
        self.wrapper_write(WRAPPER_A9SS_SW_RESET, 0);
        arch::io_mb();
        Ok(())
    }

    pub(crate) fn initialize_hfi(
        &self,
        queue_dma: u32,
        shared_region_size: u32,
        sfr_dma: u32,
    ) -> Result<u32, &'static str> {
        self.cpu_cs_write(UC_REGION_ADDR, queue_dma);
        self.cpu_cs_write(UC_REGION_SIZE, shared_region_size);
        self.cpu_cs_write(CPU_CS_SCIACMDARG2, queue_dma);
        self.cpu_cs_write(CPU_CS_SCIACMDARG1, 1);
        if sfr_dma != 0 {
            self.cpu_cs_write(SFR_ADDR, sfr_dma);
        }

        // Unmask ARM-to-host CPU messages while retaining the VCODEC source
        // mask exactly as Linux does for non-lite Venus 4xx.
        self.wrapper_write(WRAPPER_INTR_MASK, WRAPPER_INTR_MASK_A2HVCODEC);
        self.cpu_cs_write(VIDC_CTRL_INIT, 1);
        arch::io_mb();

        let start = time::current_time();
        loop {
            let status = self.cpu_cs_read(CPU_CS_SCIACMDARG0);
            if status & 1 != 0 {
                return Ok(status);
            }
            if status & 0xfe == 4 {
                return Err("qcom-venus-sc7180: firmware rejected UC region");
            }
            if time::current_time().saturating_sub(start) >= BOOT_TIMEOUT_US {
                self.log_boot_state();
                return Err("qcom-venus-sc7180: HFI core initialization timed out");
            }
            time::udelay(50);
        }
    }

    pub(crate) fn raise_host_interrupt(&self) {
        self.write(CPU_IC_BASE + CPU_IC_SOFTINT, CPU_IC_SOFTINT_H2A);
        arch::io_mb();
    }

    pub(crate) fn acknowledge_interrupt(&self) -> u32 {
        let status = self.wrapper_read(WRAPPER_INTR_STATUS);
        self.cpu_cs_write(CPU_CS_A2HSOFTINTCLR, 1);
        self.wrapper_write(WRAPPER_INTR_CLEAR, status);
        arch::io_mb();
        status
    }

    pub(crate) fn interrupt_status(&self) -> u32 {
        self.wrapper_read(WRAPPER_INTR_STATUS)
    }

    pub(crate) fn mask_interrupts(&self) {
        self.wrapper_write(WRAPPER_INTR_MASK, WRAPPER_INTR_MASK_ALL);
        arch::io_mb();
    }

    pub(crate) fn unmask_interrupts(&self) {
        self.wrapper_write(WRAPPER_INTR_MASK, WRAPPER_INTR_MASK_A2HVCODEC);
        arch::io_mb();
    }

    pub(crate) fn clear_pending_interrupts(&self) {
        let _ = self.acknowledge_interrupt();
    }

    pub(crate) fn control_status(&self) -> u32 {
        self.cpu_cs_read(CPU_CS_SCIACMDARG0)
    }

    pub(crate) fn hardware_version(&self) -> u32 {
        self.wrapper_read(WRAPPER_HW_VERSION)
    }

    fn log_boot_state(&self) {
        println!(
            "[qcom-venus-sc7180] boot-timeout hfi init={:#010x} arg={:#010x},{:#010x},{:#010x} uc={:#010x}+{:#010x} sfr={:#010x}",
            self.cpu_cs_read(VIDC_CTRL_INIT),
            self.cpu_cs_read(CPU_CS_SCIACMDARG0),
            self.cpu_cs_read(CPU_CS_SCIACMDARG1),
            self.cpu_cs_read(CPU_CS_SCIACMDARG2),
            self.cpu_cs_read(UC_REGION_ADDR),
            self.cpu_cs_read(UC_REGION_SIZE),
            self.cpu_cs_read(SFR_ADDR),
        );
        println!(
            "[qcom-venus-sc7180] boot-timeout wrapper hw={:#010x} clock={:#010x} cgc={:#010x} reset={:#010x} cpu={:#010x} power={:#010x} vcodec={:#010x}/{:#010x} irq={:#010x}/{:#010x}",
            self.wrapper_read(WRAPPER_HW_VERSION),
            self.wrapper_read(WRAPPER_CLOCK_CONFIG),
            self.wrapper_read(WRAPPER_CPU_CGC_DIS),
            self.wrapper_read(WRAPPER_A9SS_SW_RESET),
            self.wrapper_read(WRAPPER_CPU_STATUS),
            self.wrapper_read(WRAPPER_POWER_STATUS),
            self.wrapper_read(WRAPPER_VCODEC0_POWER_STATUS),
            self.wrapper_read(WRAPPER_VCODEC0_POWER_CONTROL),
            self.wrapper_read(WRAPPER_INTR_STATUS),
            self.wrapper_read(WRAPPER_INTR_MASK),
        );
        println!(
            "[qcom-venus-sc7180] boot-timeout fw={:#010x}..{:#010x} cpa={:#010x}..{:#010x} nonpix={:#010x}..{:#010x}",
            self.wrapper_read(WRAPPER_FW_START_ADDR),
            self.wrapper_read(WRAPPER_FW_END_ADDR),
            self.wrapper_read(WRAPPER_CPA_START_ADDR),
            self.wrapper_read(WRAPPER_CPA_END_ADDR),
            self.wrapper_read(WRAPPER_NONPIX_START_ADDR),
            self.wrapper_read(WRAPPER_NONPIX_END_ADDR),
        );
    }
}
