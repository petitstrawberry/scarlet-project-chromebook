// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! SC7180 GCC bootstrap and the clock/reset subset used by display, USB, and
//! the CoachZ trackpad I2C serial engine.
//!
//! The driver deliberately covers only the GCC resources consumed by Scarlet's
//! SC7180 display, primary USB path, and CoachZ trackpad. Firmware may leave
//! these resources in an arbitrary enabled state, so all enabling operations
//! preserve unrelated register bits and are safe to repeat during handoff.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use scarlet::{
    arch::{self, mmio},
    device::{
        clk::{Clk, ClkError, ClkHandle, ClkProvider},
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        reset::ResetController,
    },
    early_println, time, vm,
};

// The highest register used by this subset is USB3_PRIM_CLKREF_CBCR.
const REGISTER_WINDOW_SIZE: usize = 0x8d000;

// Register layout and DT IDs follow Linux 6.6 gcc-sc7180.c and
// include/dt-bindings/clock/qcom,gcc-sc7180.h.
const GCC_DISP_AHB_BRANCH: usize = 0x0b00c;
const GCC_DISP_HF_AXI_BRANCH: usize = 0x0b024;
const GCC_DISP_XO_BRANCH: usize = 0x0b030;
const GCC_CLOCK_VOTE: usize = 0x52000;

const GCC_USB30_PRIM_GDSCR: usize = 0x0f004;
const GCC_USB30_PRIM_MASTER_CMD_RCGR: usize = 0x0f01c;
const GCC_USB30_PRIM_MOCK_UTMI_CMD_RCGR: usize = 0x0f034;
const GCC_USB3_PRIM_PHY_AUX_CMD_RCGR: usize = 0x0f060;

const GCC_AGGRE_USB3_PRIM_AXI_CLK: u32 = 8;
const GCC_CFG_NOC_USB3_PRIM_AXI_CLK: u32 = 17;
const GCC_USB30_PRIM_MASTER_CLK: u32 = 109;
const GCC_USB30_PRIM_MOCK_UTMI_CLK: u32 = 111;
const GCC_USB30_PRIM_SLEEP_CLK: u32 = 113;
const GCC_USB3_PRIM_CLKREF_CLK: u32 = 114;
const GCC_USB3_PRIM_PHY_AUX_CLK: u32 = 115;
const GCC_USB3_PRIM_PHY_COM_AUX_CLK: u32 = 117;
const GCC_USB3_PRIM_PHY_PIPE_CLK: u32 = 118;
const GCC_USB_PHY_CFG_AHB2PHY_CLK: u32 = 119;
const GCC_QUPV3_WRAP1_S1_CLK: u32 = 74;

const GCC_QUPV3_WRAP1_S1_CMD_RCGR: usize = 0x18148;
const GCC_QUPV3_WRAP1_S1_HALT: usize = 0x18144;
const GCC_APSS_CLOCK_BRANCH_ENA_VOTE: usize = 0x52008;
const GCC_QUPV3_WRAP1_S1_VOTE: u32 = 1 << 23;

const GCC_QUSB2PHY_PRIM_BCR: u32 = 0;
const GCC_USB30_PRIM_BCR: u32 = 3;
const GCC_USB3_DP_PHY_PRIM_BCR: u32 = 4;
const GCC_USB3_PHY_PRIM_BCR: u32 = 6;

const GCC_DISP_GPLL0_VOTE: u32 = 1 << 18;
const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;
const GDSC_SW_COLLAPSE: u32 = 1;
const GDSC_POWER_ON: u32 = 1 << 31;
const RCG_UPDATE: u32 = 1;
const RCG_CFG_OFFSET: usize = 4;
const RCG_SRC_DIV_MASK: u32 = 0x1f;
const RCG_SRC_SEL_MASK: u32 = 0x7 << 8;
const RCG_MODE_MASK: u32 = 0x3 << 12;
const RCG_HW_CLK_CTRL: u32 = 1 << 20;
const BRANCH_TIMEOUT_US: u64 = 1_000;
const RCG_TIMEOUT_US: u64 = 500;

const USB_MASTER_ASSIGNED_RATE: u64 = 150_000_000;
const USB_MASTER_RATE: u64 = 200_000_000;
const USB_MOCK_UTMI_RATE: u64 = 19_200_000;
const QUP_SERIAL_RATE: u64 = 19_200_000;

static GCC_BASE: AtomicUsize = AtomicUsize::new(0);

fn round_usb_master_rate(rate: u64) -> Result<u64, ClkError> {
    match rate {
        USB_MASTER_ASSIGNED_RATE | USB_MASTER_RATE => Ok(USB_MASTER_RATE),
        _ => Err(ClkError::InvalidRate),
    }
}

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

    fn update_bits(self, offset: usize, mask: u32, value: u32) {
        self.write(offset, (self.read(offset) & !mask) | (value & mask));
    }

    fn set_bits(self, offset: usize, bits: u32) {
        self.update_bits(offset, bits, bits);
    }

    fn clear_bits(self, offset: usize, bits: u32) {
        self.update_bits(offset, bits, 0);
    }
}

fn wait_for(
    registers: RegisterWindow,
    offset: usize,
    mask: u32,
    expected: u32,
    timeout_us: u64,
) -> Result<(), &'static str> {
    let start = time::current_time();
    while registers.read(offset) & mask != expected {
        if time::current_time().saturating_sub(start) >= timeout_us {
            return Err("qcom-sc7180-gcc: hardware transition timed out");
        }
        time::udelay(1);
    }
    Ok(())
}

fn enable_branch(
    registers: RegisterWindow,
    branch: usize,
    halt_check: bool,
) -> Result<(), ClkError> {
    registers.set_bits(branch, BRANCH_ENABLE);
    arch::io_wmb();
    if halt_check {
        wait_for(registers, branch, BRANCH_OFF, 0, BRANCH_TIMEOUT_US)
            .map_err(|_| ClkError::HardwareError)?;
    }
    Ok(())
}

fn enable_usb_gdsc(registers: RegisterWindow) -> Result<(), &'static str> {
    // Clearing SW_COLLAPSE requests ON.  If firmware already left the domain
    // on this is a no-op; no reset or power-cycle is introduced.
    registers.clear_bits(GCC_USB30_PRIM_GDSCR, GDSC_SW_COLLAPSE);
    arch::io_wmb();
    wait_for(
        registers,
        GCC_USB30_PRIM_GDSCR,
        GDSC_POWER_ON,
        GDSC_POWER_ON,
        BRANCH_TIMEOUT_US,
    )
}

fn configure_rcg(
    registers: RegisterWindow,
    command: usize,
    source: u32,
    divider: u32,
) -> Result<(), ClkError> {
    let config = command + RCG_CFG_OFFSET;
    registers.update_bits(
        config,
        RCG_SRC_DIV_MASK | RCG_SRC_SEL_MASK | RCG_MODE_MASK | RCG_HW_CLK_CTRL,
        divider | (source << 8),
    );
    registers.set_bits(command, RCG_UPDATE);
    arch::io_wmb();
    wait_for(registers, command, RCG_UPDATE, 0, RCG_TIMEOUT_US).map_err(|_| ClkError::Busy)
}

fn prepare_display_clocks(registers: RegisterWindow) -> Result<(), &'static str> {
    let inherited_vote = registers.read(GCC_CLOCK_VOTE);
    let inherited_ahb = registers.read(GCC_DISP_AHB_BRANCH);
    let inherited_hf_axi = registers.read(GCC_DISP_HF_AXI_BRANCH);
    let inherited_xo = registers.read(GCC_DISP_XO_BRANCH);

    registers.set_bits(GCC_CLOCK_VOTE, GCC_DISP_GPLL0_VOTE);
    registers.set_bits(GCC_DISP_AHB_BRANCH, BRANCH_ENABLE);
    registers.set_bits(GCC_DISP_XO_BRANCH, BRANCH_ENABLE);
    enable_branch(registers, GCC_DISP_HF_AXI_BRANCH, true)
        .map_err(|_| "qcom-sc7180-gcc: display HF-AXI clock failed to start")?;

    early_println!(
        "[qcom-sc7180-gcc] display inherited: vote={:#010x} ahb={:#010x} hf-axi={:#010x} xo={:#010x}",
        inherited_vote,
        inherited_ahb,
        inherited_hf_axi,
        inherited_xo,
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RateControl {
    Fixed,
    Master,
    MockUtmi,
    PhyAux,
}

#[derive(Clone, Copy)]
struct UsbClockDescriptor {
    id: u32,
    name: &'static str,
    branch: usize,
    rate: u64,
    rate_control: RateControl,
    halt_check: bool,
}

const USB_CLOCKS: [UsbClockDescriptor; 10] = [
    UsbClockDescriptor {
        id: GCC_CFG_NOC_USB3_PRIM_AXI_CLK,
        name: "gcc_cfg_noc_usb3_prim_axi_clk",
        branch: 0x0502c,
        rate: USB_MASTER_RATE,
        rate_control: RateControl::Master,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB30_PRIM_MASTER_CLK,
        name: "gcc_usb30_prim_master_clk",
        branch: 0x0f010,
        rate: USB_MASTER_RATE,
        rate_control: RateControl::Master,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_AGGRE_USB3_PRIM_AXI_CLK,
        name: "gcc_aggre_usb3_prim_axi_clk",
        branch: 0x8201c,
        rate: USB_MASTER_RATE,
        rate_control: RateControl::Master,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB30_PRIM_SLEEP_CLK,
        name: "gcc_usb30_prim_sleep_clk",
        branch: 0x0f014,
        rate: 32_000,
        rate_control: RateControl::Fixed,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB30_PRIM_MOCK_UTMI_CLK,
        name: "gcc_usb30_prim_mock_utmi_clk",
        branch: 0x0f018,
        rate: USB_MOCK_UTMI_RATE,
        rate_control: RateControl::MockUtmi,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB_PHY_CFG_AHB2PHY_CLK,
        name: "gcc_usb_phy_cfg_ahb2phy_clk",
        branch: 0x6a004,
        rate: USB_MOCK_UTMI_RATE,
        rate_control: RateControl::Fixed,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB3_PRIM_PHY_AUX_CLK,
        name: "gcc_usb3_prim_phy_aux_clk",
        branch: 0x0f050,
        rate: USB_MOCK_UTMI_RATE,
        rate_control: RateControl::PhyAux,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB3_PRIM_CLKREF_CLK,
        name: "gcc_usb3_prim_clkref_clk",
        branch: 0x8c010,
        rate: USB_MOCK_UTMI_RATE,
        rate_control: RateControl::Fixed,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB3_PRIM_PHY_COM_AUX_CLK,
        name: "gcc_usb3_prim_phy_com_aux_clk",
        branch: 0x0f054,
        rate: USB_MOCK_UTMI_RATE,
        // Linux parents COM_AUX to the same source as AUX. The QMP binding
        // enables AUX first, so do not reprogram that live RCG for COM_AUX.
        rate_control: RateControl::Fixed,
        halt_check: true,
    },
    UsbClockDescriptor {
        id: GCC_USB3_PRIM_PHY_PIPE_CLK,
        name: "gcc_usb3_prim_phy_pipe_clk",
        branch: 0x0f058,
        rate: 125_000_000,
        rate_control: RateControl::Fixed,
        halt_check: false,
    },
];

struct Sc7180UsbClock {
    registers: RegisterWindow,
    descriptor: UsbClockDescriptor,
    master_rate: Arc<AtomicU64>,
    mock_utmi_rate: Arc<AtomicU64>,
}

impl Clk for Sc7180UsbClock {
    fn name(&self) -> &'static str {
        self.descriptor.name
    }

    fn enable(&self) -> Result<(), ClkError> {
        if self.descriptor.rate_control == RateControl::PhyAux {
            // AUX and COM_AUX share gcc_usb3_prim_phy_aux_clk_src. Linux's
            // only SC7180 table entry selects BI_TCXO with HID /1.
            configure_rcg(self.registers, GCC_USB3_PRIM_PHY_AUX_CMD_RCGR, 0, 1)?;
            early_println!(
                "[qcom-sc7180-gcc] USB3 PHY AUX source: cmd={:#010x} cfg={:#010x}",
                self.registers.read(GCC_USB3_PRIM_PHY_AUX_CMD_RCGR),
                self.registers
                    .read(GCC_USB3_PRIM_PHY_AUX_CMD_RCGR + RCG_CFG_OFFSET),
            );
        }
        enable_branch(
            self.registers,
            self.descriptor.branch,
            self.descriptor.halt_check,
        )
    }

    fn disable(&self) {
        self.registers
            .clear_bits(self.descriptor.branch, BRANCH_ENABLE);
        arch::io_wmb();
    }

    fn is_enabled(&self) -> bool {
        self.registers.read(self.descriptor.branch) & BRANCH_ENABLE != 0
    }

    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        match self.descriptor.rate_control {
            RateControl::Master => self.master_rate.load(Ordering::Relaxed),
            RateControl::MockUtmi => self.mock_utmi_rate.load(Ordering::Relaxed),
            RateControl::Fixed | RateControl::PhyAux => self.descriptor.rate,
        }
    }

    fn round_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        match self.descriptor.rate_control {
            RateControl::Master => round_usb_master_rate(rate),
            RateControl::MockUtmi if rate == USB_MOCK_UTMI_RATE => Ok(rate),
            RateControl::Fixed | RateControl::PhyAux if rate == self.descriptor.rate => Ok(rate),
            _ => Err(ClkError::InvalidRate),
        }
    }

    fn set_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        match self.descriptor.rate_control {
            RateControl::Master if rate == USB_MASTER_ASSIGNED_RATE || rate == USB_MASTER_RATE => {
                // Linux CEIL-rounds the DT's 150 MHz request to the next
                // SC7180 table entry: GPLL0_MAIN 600 MHz /3 = 200 MHz.
                // Parent selector 1 and HID encoding 5 match F(..., 3, ...).
                configure_rcg(self.registers, GCC_USB30_PRIM_MASTER_CMD_RCGR, 1, 5)?;
                self.master_rate.store(USB_MASTER_RATE, Ordering::Relaxed);
                early_println!(
                    "[qcom-sc7180-gcc] USB master source: requested={} actual={} cmd={:#010x} cfg={:#010x}",
                    rate,
                    USB_MASTER_RATE,
                    self.registers.read(GCC_USB30_PRIM_MASTER_CMD_RCGR),
                    self.registers
                        .read(GCC_USB30_PRIM_MASTER_CMD_RCGR + RCG_CFG_OFFSET),
                );
                Ok(USB_MASTER_RATE)
            }
            RateControl::MockUtmi if rate == USB_MOCK_UTMI_RATE => {
                // BI_TCXO at 19.2 MHz: source selector 0, HID divider /1.
                configure_rcg(self.registers, GCC_USB30_PRIM_MOCK_UTMI_CMD_RCGR, 0, 1)?;
                self.mock_utmi_rate.store(rate, Ordering::Relaxed);
                Ok(rate)
            }
            RateControl::PhyAux if rate == USB_MOCK_UTMI_RATE => {
                configure_rcg(self.registers, GCC_USB3_PRIM_PHY_AUX_CMD_RCGR, 0, 1)?;
                Ok(rate)
            }
            RateControl::Fixed if rate == self.descriptor.rate => Ok(rate),
            _ => Err(ClkError::InvalidRate),
        }
    }
}

struct Sc7180GccProvider {
    clocks: [ClkHandle; USB_CLOCKS.len()],
    trackpad_i2c_clock: ClkHandle,
}

impl Sc7180GccProvider {
    fn new(registers: RegisterWindow) -> Self {
        let master_rate = Arc::new(AtomicU64::new(USB_MASTER_RATE));
        let mock_utmi_rate = Arc::new(AtomicU64::new(USB_MOCK_UTMI_RATE));
        let clocks = core::array::from_fn(|index| {
            ClkHandle::new(Arc::new(Sc7180UsbClock {
                registers,
                descriptor: USB_CLOCKS[index],
                master_rate: Arc::clone(&master_rate),
                mock_utmi_rate: Arc::clone(&mock_utmi_rate),
            }))
        });
        let trackpad_i2c_clock = ClkHandle::new(Arc::new(Sc7180QupSerialClock { registers }));
        Self {
            clocks,
            trackpad_i2c_clock,
        }
    }
}

struct Sc7180QupSerialClock {
    registers: RegisterWindow,
}

impl Clk for Sc7180QupSerialClock {
    fn name(&self) -> &'static str {
        "gcc_qupv3_wrap1_s1_clk"
    }

    fn enable(&self) -> Result<(), ClkError> {
        // SE1 runs from BI_TCXO at 19.2 MHz. Program the source while taking
        // ownership because U-Boot deliberately leaves the trackpad bus off.
        configure_rcg(self.registers, GCC_QUPV3_WRAP1_S1_CMD_RCGR, 0, 1)?;
        self.registers
            .set_bits(GCC_APSS_CLOCK_BRANCH_ENA_VOTE, GCC_QUPV3_WRAP1_S1_VOTE);
        arch::io_wmb();
        wait_for(
            self.registers,
            GCC_QUPV3_WRAP1_S1_HALT,
            BRANCH_OFF,
            0,
            BRANCH_TIMEOUT_US,
        )
        .map_err(|_| ClkError::HardwareError)?;
        early_println!(
            "[qcom-sc7180-gcc] QUPv3 wrap1 SE1 ready: rate={} cmd={:#010x} vote={:#010x} halt={:#010x}",
            QUP_SERIAL_RATE,
            self.registers.read(GCC_QUPV3_WRAP1_S1_CMD_RCGR),
            self.registers.read(GCC_APSS_CLOCK_BRANCH_ENA_VOTE),
            self.registers.read(GCC_QUPV3_WRAP1_S1_HALT),
        );
        Ok(())
    }

    fn disable(&self) {
        self.registers
            .clear_bits(GCC_APSS_CLOCK_BRANCH_ENA_VOTE, GCC_QUPV3_WRAP1_S1_VOTE);
        arch::io_wmb();
    }

    fn is_enabled(&self) -> bool {
        self.registers.read(GCC_APSS_CLOCK_BRANCH_ENA_VOTE) & GCC_QUPV3_WRAP1_S1_VOTE != 0
            && self.registers.read(GCC_QUPV3_WRAP1_S1_HALT) & BRANCH_OFF == 0
    }

    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        QUP_SERIAL_RATE
    }

    fn round_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        if rate == QUP_SERIAL_RATE {
            Ok(rate)
        } else {
            Err(ClkError::InvalidRate)
        }
    }

    fn set_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        if rate != QUP_SERIAL_RATE {
            return Err(ClkError::InvalidRate);
        }
        configure_rcg(self.registers, GCC_QUPV3_WRAP1_S1_CMD_RCGR, 0, 1)?;
        Ok(rate)
    }
}

impl ClkProvider for Sc7180GccProvider {
    fn name(&self) -> &'static str {
        "qcom-sc7180-gcc"
    }

    fn clock_cells(&self) -> usize {
        1
    }

    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id] = spec else {
            return Err(ClkError::InvalidSpecifier);
        };
        if *id == GCC_QUPV3_WRAP1_S1_CLK {
            return Ok(self.trackpad_i2c_clock.clone());
        }
        USB_CLOCKS
            .iter()
            .position(|clock| clock.id == *id)
            .map(|index| self.clocks[index].clone())
            .ok_or(ClkError::ClockNotFound)
    }
}

struct Sc7180GccResetController {
    registers: RegisterWindow,
}

impl Sc7180GccResetController {
    fn reset_offset(spec: &[u32]) -> Result<usize, &'static str> {
        match spec {
            [GCC_QUSB2PHY_PRIM_BCR] => Ok(0x26000),
            [GCC_USB30_PRIM_BCR] => Ok(0x0f000),
            [GCC_USB3_DP_PHY_PRIM_BCR] => Ok(0x50008),
            [GCC_USB3_PHY_PRIM_BCR] => Ok(0x50000),
            [_] => Err("qcom-sc7180-gcc: unsupported reset ID"),
            _ => Err("qcom-sc7180-gcc: invalid reset specifier"),
        }
    }
}

impl ResetController for Sc7180GccResetController {
    fn name(&self) -> &'static str {
        "qcom-sc7180-gcc"
    }

    fn reset_cells(&self) -> usize {
        1
    }

    fn assert_reset(&self, spec: &[u32]) -> Result<(), &'static str> {
        self.registers
            .set_bits(Self::reset_offset(spec)?, BRANCH_ENABLE);
        arch::io_wmb();
        Ok(())
    }

    fn deassert_reset(&self, spec: &[u32]) -> Result<(), &'static str> {
        self.registers
            .clear_bits(Self::reset_offset(spec)?, BRANCH_ENABLE);
        arch::io_wmb();
        Ok(())
    }
}

fn node_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-sc7180-gcc: missing DT phandle")
}

/// Enable only the SC7180 primary USB power domain.
///
/// The DWC3 wrapper and PHY consumers enable their own clocks in Linux order;
/// in particular, the QMP driver enables PIPE only after programming SerDes.
/// This handoff entry point is safe to call repeatedly and never disables or
/// resets inherited hardware.
///
/// # Returns
///
/// `Ok(())` when the primary USB resources are usable, or an error when GCC has
/// not probed or a hardware transition times out.
pub fn enable_usb30_prim_gdsc() -> Result<(), &'static str> {
    let base = GCC_BASE.load(Ordering::Acquire);
    if base == 0 {
        return Err("qcom-sc7180-gcc: provider has not probed");
    }
    let registers = RegisterWindow::new(base);
    enable_usb_gdsc(registers)?;
    early_println!(
        "[qcom-sc7180-gcc] USB30_PRIM_GDSC enabled: gdscr={:#010x} pipe={:#010x}",
        registers.read(GCC_USB30_PRIM_GDSCR),
        registers.read(0x0f058),
    );
    Ok(())
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-sc7180-gcc: missing GCC memory resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-gcc: invalid GCC memory resource")?;
    if resource_size < REGISTER_WINDOW_SIZE {
        return Err("qcom-sc7180-gcc: GCC register resource is too small");
    }

    let phandle = node_phandle(device)?;
    let base = vm::ioremap(resource.start, REGISTER_WINDOW_SIZE)
        .map_err(|_| "qcom-sc7180-gcc: GCC ioremap failed")?;
    let registers = RegisterWindow::new(base);
    prepare_display_clocks(registers)?;

    let manager = DeviceManager::get_manager();
    manager.register_clk_provider(phandle, Arc::new(Sc7180GccProvider::new(registers)));
    manager.register_reset_controller(phandle, Arc::new(Sc7180GccResetController { registers }));
    GCC_BASE.store(base, Ordering::Release);

    early_println!(
        "[qcom-sc7180-gcc] registered display/USB/QUP clocks and USB resets for phandle {:#x}",
        phandle
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_clock_ids_match_sc7180_bindings() {
        let ids = USB_CLOCKS.map(|clock| clock.id);
        assert_eq!(ids, [17, 109, 8, 113, 111, 119, 115, 114, 117, 118]);
    }

    #[test]
    fn trackpad_i2c_clock_id_matches_sc7180_binding() {
        assert_eq!(GCC_QUPV3_WRAP1_S1_CLK, 74);
        assert_eq!(GCC_QUPV3_WRAP1_S1_CMD_RCGR, 0x18148);
        assert_eq!(GCC_QUPV3_WRAP1_S1_HALT, 0x18144);
        assert_eq!(GCC_QUPV3_WRAP1_S1_VOTE, 1 << 23);
    }

    #[test]
    fn required_usb_resets_resolve_to_linux_offsets() {
        assert_eq!(Sc7180GccResetController::reset_offset(&[0]), Ok(0x26000));
        assert_eq!(Sc7180GccResetController::reset_offset(&[3]), Ok(0x0f000));
        assert_eq!(Sc7180GccResetController::reset_offset(&[4]), Ok(0x50008));
        assert_eq!(Sc7180GccResetController::reset_offset(&[6]), Ok(0x50000));
    }

    #[test]
    fn com_aux_uses_aux_clock_source_rate() {
        let aux = USB_CLOCKS
            .iter()
            .find(|clock| clock.id == GCC_USB3_PRIM_PHY_AUX_CLK)
            .expect("USB3 PHY AUX clock descriptor");
        let com_aux = USB_CLOCKS
            .iter()
            .find(|clock| clock.id == GCC_USB3_PRIM_PHY_COM_AUX_CLK)
            .expect("USB3 PHY COM_AUX clock descriptor");
        assert_eq!(aux.rate_control, RateControl::PhyAux);
        assert_eq!(com_aux.rate_control, RateControl::Fixed);
        assert_eq!(aux.rate, USB_MOCK_UTMI_RATE);
        assert_eq!(com_aux.rate, aux.rate);
    }

    #[test]
    fn master_assigned_rate_rounds_like_linux() {
        assert_eq!(
            round_usb_master_rate(USB_MASTER_ASSIGNED_RATE),
            Ok(USB_MASTER_RATE)
        );
        assert_eq!(round_usb_master_rate(USB_MASTER_RATE), Ok(USB_MASTER_RATE));
        assert_eq!(
            round_usb_master_rate(192_000_000),
            Err(ClkError::InvalidRate)
        );
    }
}
