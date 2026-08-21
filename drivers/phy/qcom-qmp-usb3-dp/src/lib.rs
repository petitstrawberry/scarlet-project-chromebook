// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 QMP USB3/DisplayPort combo PHY.
//!
//! This driver exposes the USB3 lane of the combo block to Scarlet's PHY
//! subsystem. DisplayPort programming remains owned by the display path; the
//! shared mode register is kept in USB3+DP mode throughout USB operation.
//! Hardware sequencing is adapted from the GPL-2.0 ChromeOS Linux 6.6 QMP
//! combo PHY driver, expressed through Scarlet's clock, reset, and PHY APIs.

extern crate alloc;

mod registers;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use registers::*;
use scarlet::{
    arch::{self, mmio},
    device::{
        clk::ClkHandle,
        manager::{DeviceManager, DriverPriority, probe_defer},
        phy::{Phy, PhyError, PhyHandle, PhyMode, PhyOrientation, PhyProvider},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        reset::ResetHandle,
    },
    early_println,
    sync::IrqSpinLock,
    time, vm,
};

const QMP_USB43DP_USB3_PHY: u32 = 0;
const COMMON_CLOCK_NAMES: [&str; 4] = ["aux", "cfg_ahb", "ref", "com_aux"];
const PIPE_CLOCK_NAME: &str = "usb3_pipe";
const PHY_RESET_NAME: &str = "phy";
const PHY_READY_TIMEOUT_US: u64 = 10_000;
const PHY_READY_POLL_US: u64 = 200;

#[derive(Clone, Copy)]
struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(self, offset: usize) -> u32 {
        // SAFETY: the base is an ioremap'd 0x3000-byte QMP resource and every
        // offset used by this driver is bounded by REGISTER_WINDOW_SIZE.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        // SAFETY: see `read`; the same mapped QMP register window is used.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn update_bits(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
        let _ = self.read(offset);
    }

    fn write_table(self, base: usize, table: &[(usize, u32)]) {
        for &(offset, value) in table {
            self.write(base + offset, value);
        }
    }
}

struct Usb3State {
    powered: bool,
    mode: Option<PhyMode>,
    orientation: PhyOrientation,
}

struct Sc7180QmpUsb3 {
    registers: RegisterWindow,
    common_clocks: Vec<ClkHandle>,
    pipe_clock: ClkHandle,
    phy_reset: ResetHandle,
    state: IrqSpinLock<Usb3State>,
}

impl Sc7180QmpUsb3 {
    fn log_clocks(&self, stage: &str) {
        for clock in &self.common_clocks {
            early_println!(
                "[qcom-qmp-usb3-dp] {} clock={} enabled={} rate={}",
                stage,
                clock.name(),
                clock.is_enabled(),
                clock.rate(),
            );
        }
        early_println!(
            "[qcom-qmp-usb3-dp] {} clock={} enabled={} rate={}",
            stage,
            self.pipe_clock.name(),
            self.pipe_clock.is_enabled(),
            self.pipe_clock.rate(),
        );
    }

    fn log_snapshot(&self, stage: &str) {
        early_println!(
            "[qcom-qmp-usb3-dp] {} com mode={:#010x} power={:#010x} reset={:#010x} swi={:#010x} typec={:#010x}",
            stage,
            self.registers.read(DP_COM_PHY_MODE_CTRL),
            self.registers.read(DP_COM_POWER_DOWN_CTRL),
            self.registers.read(DP_COM_RESET_OVRD_CTRL),
            self.registers.read(DP_COM_SWI_CTRL),
            self.registers.read(DP_COM_TYPEC_CTRL),
        );
        early_println!(
            "[qcom-qmp-usb3-dp] {} serdes cmn={:#010x} reset-sm={:#010x} c-ready={:#010x} ivco={:#010x}",
            stage,
            self.registers.read(SERDES_CMN_STATUS),
            self.registers.read(SERDES_RESET_SM_STATUS),
            self.registers.read(SERDES_C_READY_STATUS),
            self.registers.read(SERDES_PLL_IVCO),
        );
        early_println!(
            "[qcom-qmp-usb3-dp] {} pcs reset={:#010x} power={:#010x} start={:#010x} status={:#010x} fll={:#010x} tx-highz={:#010x} rx-fastlock={:#010x}",
            stage,
            self.registers.read(PCS_SW_RESET),
            self.registers.read(PCS_POWER_DOWN_CONTROL),
            self.registers.read(PCS_START_CONTROL),
            self.registers.read(PCS_STATUS),
            self.registers.read(PCS_FLL_CNTRL2),
            self.registers.read(TXA_HIGHZ_DRVR_EN),
            self.registers.read(RXA_UCDR_FASTLOCK_FO_GAIN),
        );
    }

    fn enable_common(&self, orientation: PhyOrientation) -> Result<(), PhyError> {
        if self.phy_reset.assert().is_err() {
            early_println!("[qcom-qmp-usb3-dp] external PHY reset assert failed");
            return Err(PhyError::ResetFailed);
        }
        if self.phy_reset.deassert().is_err() {
            early_println!("[qcom-qmp-usb3-dp] external PHY reset deassert failed");
            return Err(PhyError::ResetFailed);
        }

        let mut enabled = 0usize;
        for clock in &self.common_clocks {
            if clock.prepare_enable().is_err() {
                early_println!(
                    "[qcom-qmp-usb3-dp] common clock {} failed to enable",
                    clock.name()
                );
                for previous in self.common_clocks[..enabled].iter().rev() {
                    previous.disable_unprepare();
                }
                let _ = self.phy_reset.assert();
                return Err(PhyError::PowerOnFailed);
            }
            enabled += 1;
        }

        let reset_override = DP_RESET_OVERRIDE | USB3_RESET_OVERRIDE;
        self.registers
            .update_bits(DP_COM_POWER_DOWN_CTRL, 0, SW_POWER_DOWN);
        self.registers
            .update_bits(DP_COM_RESET_OVRD_CTRL, 0, reset_override);

        let orientation_value = SOFTWARE_PORT_SELECT_MUX
            | if orientation == PhyOrientation::Reverse {
                SOFTWARE_PORT_SELECT_VALUE
            } else {
                0
            };
        self.registers.write(DP_COM_TYPEC_CTRL, orientation_value);
        // Keep the DisplayPort half enabled while bringing up USB3.
        self.registers.write(DP_COM_PHY_MODE_CTRL, USB3_AND_DP_MODE);
        self.registers
            .update_bits(DP_COM_RESET_OVRD_CTRL, reset_override, 0);
        self.registers.update_bits(DP_COM_SWI_CTRL, 0x03, 0);
        self.registers.update_bits(DP_COM_SW_RESET, SW_RESET, 0);
        self.registers
            .update_bits(PCS_POWER_DOWN_CONTROL, 0, SW_POWER_DOWN);
        self.log_clocks("common-enabled");
        self.log_snapshot("common-enabled");
        Ok(())
    }

    fn disable_common(&self) {
        let _ = self.phy_reset.assert();
        for clock in self.common_clocks.iter().rev() {
            clock.disable_unprepare();
        }
    }

    fn configure_usb3(&self) -> Result<(), PhyError> {
        self.registers
            .write_table(USB3_SERDES_BASE, USB3_SERDES_TABLE);

        if self.pipe_clock.prepare_enable().is_err() {
            early_println!("[qcom-qmp-usb3-dp] usb3_pipe failed to enable");
            return Err(PhyError::PowerOnFailed);
        }

        self.registers.write_table(TXA_BASE, USB3_TX_TABLE);
        self.registers.write_table(TXB_BASE, USB3_TX_TABLE);
        self.registers.write_table(RXA_BASE, USB3_RX_TABLE);
        self.registers.write_table(RXB_BASE, USB3_RX_TABLE);
        self.registers.write_table(USB3_PCS_BASE, USB3_PCS_TABLE);

        // SC7180 requires the post-power-down delay before releasing PCS reset.
        time::udelay(10);
        self.registers.update_bits(PCS_SW_RESET, SW_RESET, 0);
        self.registers
            .update_bits(PCS_START_CONTROL, 0, SERDES_START | PCS_START);
        arch::io_wmb();
        self.log_snapshot("started");

        let started = time::current_time();
        loop {
            let status = self.registers.read(PCS_STATUS);
            if status & PHY_STATUS == 0 {
                early_println!(
                    "[qcom-qmp-usb3-dp] PHY ready after {} us (PCS_STATUS={:#010x})",
                    time::current_time().saturating_sub(started),
                    status,
                );
                break;
            }
            if time::current_time().saturating_sub(started) >= PHY_READY_TIMEOUT_US {
                early_println!(
                    "[qcom-qmp-usb3-dp] PHY ready timeout after {} us (PCS_STATUS={:#010x})",
                    PHY_READY_TIMEOUT_US,
                    status,
                );
                self.log_clocks("timeout");
                self.log_snapshot("timeout");
                return Err(PhyError::Timeout);
            }
            time::udelay(PHY_READY_POLL_US);
        }
        Ok(())
    }

    fn stop_usb3(&self, stage: &str) {
        // Keep Linux's qmp_combo_usb_power_off() order. This is also the
        // rollback path for failed power-on because PhyHandle deliberately
        // does not acquire a power reference when power_on() returns Err.
        self.pipe_clock.disable_unprepare();
        self.registers.update_bits(PCS_SW_RESET, 0, SW_RESET);
        self.registers
            .update_bits(PCS_START_CONTROL, SERDES_START | PCS_START, 0);
        self.registers
            .update_bits(PCS_POWER_DOWN_CONTROL, SW_POWER_DOWN, 0);
        arch::io_wmb();
        self.log_snapshot(stage);
        self.log_clocks(stage);
    }
}

impl Phy for Sc7180QmpUsb3 {
    fn name(&self) -> &'static str {
        "sc7180-qmp-usb3"
    }

    fn power_on(&self) -> Result<(), PhyError> {
        let mut state = self.state.lock();
        if state.powered {
            return Ok(());
        }

        early_println!(
            "[qcom-qmp-usb3-dp] power_on begin mode={:?} orientation={:?}",
            state.mode,
            state.orientation,
        );
        self.enable_common(state.orientation)?;
        if let Err(error) = self.configure_usb3() {
            early_println!("[qcom-qmp-usb3-dp] power_on failed: {:?}", error);
            self.stop_usb3("rollback");
            self.disable_common();
            early_println!(
                "[qcom-qmp-usb3-dp] rollback complete: PCS reset asserted, PCS/SerDes stopped, PCS power-down asserted, common disabled"
            );
            return Err(error);
        }
        state.powered = true;
        early_println!("[qcom-qmp-usb3-dp] power_on complete");
        Ok(())
    }

    fn power_off(&self) -> Result<(), PhyError> {
        let mut state = self.state.lock();
        if !state.powered {
            return Ok(());
        }
        self.stop_usb3("power-off");
        self.disable_common();
        state.powered = false;
        Ok(())
    }

    fn reset(&self) -> Result<(), PhyError> {
        let state = self.state.lock();
        if state.powered {
            return Err(PhyError::Busy);
        }
        self.phy_reset.reset().map_err(|_| PhyError::ResetFailed)
    }

    fn set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        match mode {
            PhyMode::UsbHost | PhyMode::UsbDevice | PhyMode::UsbOtg => {
                self.state.lock().mode = Some(mode);
                Ok(())
            }
            _ => Err(PhyError::InvalidMode),
        }
    }

    fn get_mode(&self) -> Option<PhyMode> {
        self.state.lock().mode
    }

    fn set_orientation(&self, orientation: PhyOrientation) -> Result<(), PhyError> {
        let mut state = self.state.lock();
        if state.powered && state.orientation != orientation {
            // A live port-select change requires resetting the shared combo
            // block. Require the USB consumer to quiesce first so an active DP
            // link is never reset as a side effect of changing USB orientation.
            return Err(PhyError::Busy);
        }
        state.orientation = orientation;
        Ok(())
    }

    fn get_orientation(&self) -> Option<PhyOrientation> {
        Some(self.state.lock().orientation)
    }
}

struct Sc7180QmpProvider {
    usb3: PhyHandle,
}

impl PhyProvider for Sc7180QmpProvider {
    fn name(&self) -> &'static str {
        "sc7180-qmp-usb3-dp"
    }

    fn phy_cells(&self) -> usize {
        1
    }

    fn get_phy(&self, spec: &[u32]) -> Result<PhyHandle, PhyError> {
        match spec {
            [QMP_USB43DP_USB3_PHY] => Ok(self.usb3.clone()),
            _ => Err(PhyError::NotFound),
        }
    }
}

fn device_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-qmp-usb3-dp: missing phandle")
}

fn resolve_clock(
    manager: &DeviceManager,
    device: &PlatformDeviceInfo,
    name: &str,
) -> Result<ClkHandle, &'static str> {
    match manager.resolve_clk(device, name) {
        Err("clk: provider not found") => probe_defer(),
        result => result,
    }
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-qmp-usb3-dp: missing memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|value| value.checked_add(1))
        .ok_or("qcom-qmp-usb3-dp: invalid memory resource")?;
    if size < REGISTER_WINDOW_SIZE {
        return Err("qcom-qmp-usb3-dp: memory resource is too small");
    }

    let manager = DeviceManager::get_manager();
    let mut common_clocks = Vec::with_capacity(COMMON_CLOCK_NAMES.len());
    for name in COMMON_CLOCK_NAMES {
        common_clocks.push(resolve_clock(manager, device, name)?);
    }
    let pipe_clock = resolve_clock(manager, device, PIPE_CLOCK_NAME)?;
    let phy_reset = manager.resolve_reset(device, PHY_RESET_NAME)?;
    let phandle = device_phandle(device)?;

    let base = vm::ioremap(resource.start, REGISTER_WINDOW_SIZE)
        .map_err(|_| "qcom-qmp-usb3-dp: ioremap failed")?;
    let usb3: Arc<dyn Phy> = Arc::new(Sc7180QmpUsb3 {
        registers: RegisterWindow::new(base),
        common_clocks,
        pipe_clock,
        phy_reset,
        state: IrqSpinLock::new(Usb3State {
            powered: false,
            mode: None,
            orientation: PhyOrientation::Normal,
        }),
    });
    let provider = Arc::new(Sc7180QmpProvider {
        usb3: PhyHandle::new(usb3),
    });
    manager.register_phy_provider(phandle, provider);

    early_println!(
        "[qcom-qmp-usb3-dp] preserving firmware-managed vdda-phy and vdda-pll supplies (Scarlet regulator API unavailable)"
    );
    early_println!(
        "[qcom-qmp-usb3-dp] registered SC7180 USB3 PHY provider phandle={:#x} #phy-cells=1",
        phandle
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-qmp-usb3-dp",
        probe_fn,
        remove_fn,
        vec!["qcom,sc7180-qmp-usb3-dp-phy"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_QMP_USB3_DP_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
