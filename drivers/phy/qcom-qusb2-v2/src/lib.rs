// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm QUSB2 v2 USB 2.0 PHY for SC7180.
//!
//! This driver ports the SC7180 (`qcom,qusb2-v2-phy`) register sequence from
//! ChromeOS Linux 6.6's `drivers/phy/qualcomm/phy-qcom-qusb2.c`.  The PHY is
//! registered as a Scarlet [`PhyProvider`] at its device-tree phandle, so USB
//! controllers can resolve the standard zero-cell `phys = <&phy>` reference.
//!
//! Firmware owns the three required PHY rails (`vdd`, `vdda-pll`, and
//! `vdda-phy-dpdm`) during this early Scarlet bring-up.  Scarlet currently has
//! no regulator-consumer API, therefore this module deliberately leaves those
//! rails untouched and reports that constraint at probe time.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicU8, Ordering};

use scarlet::{
    arch::mmio,
    device::{
        clk::ClkHandle,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        nvmem::NvmemCell,
        phy::{Phy, PhyError, PhyHandle, PhyMode, PhyProvider},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        reset::ResetHandle,
    },
    println, time, vm,
};

const DRIVER_NAME: &str = "qcom-qusb2-v2";

// SC7180 uses the QUSB2 v2 register layout.
const PLL_CORE_INPUT_OVERRIDE: usize = 0x0a8;
const PLL_STATUS: usize = 0x1a0;
const PORT_TUNE1: usize = 0x240;
const PORT_TUNE2: usize = 0x244;
const PORT_TUNE3: usize = 0x248;
const PORT_TUNE4: usize = 0x24c;
const PORT_TUNE5: usize = 0x250;
const PORT_POWERDOWN: usize = 0x210;

const PLL_ANALOG_CONTROLS_TWO: usize = 0x004;
const PLL_CMODE: usize = 0x02c;
const PLL_DIGITAL_TIMERS_TWO: usize = 0x0b4;
const PLL_LOCK_DELAY: usize = 0x184;
const PLL_CLOCK_INVERTERS: usize = 0x18c;
const PLL_BIAS_CONTROL_1: usize = 0x194;
const PLL_BIAS_CONTROL_2: usize = 0x198;
const PWR_CTRL2: usize = 0x214;
const IMP_CTRL1: usize = 0x220;
const IMP_CTRL2: usize = 0x224;
const CHG_CTRL2: usize = 0x23c;

const CORE_READY_STATUS: u32 = 1 << 0;
const CORE_PLL_EN_FROM_RESET: u32 = 1 << 4;
const CORE_RESET: u32 = 1 << 5;
const CORE_RESET_MUX: u32 = 1 << 6;
const POWER_DOWN: u32 = 1;
const PWR_CTRL1_VREF_SUPPLY_TRIM: u32 = 1 << 5;
const PWR_CTRL1_CLAMP_N_EN: u32 = 1 << 1;
const DISABLE_CTRL: u32 = PWR_CTRL1_VREF_SUPPLY_TRIM | PWR_CTRL1_CLAMP_N_EN | POWER_DOWN;

const IMP_RES_OFFSET_MASK: u32 = 0x3f;
const BIAS_CTRL2_RES_OFFSET_MASK: u32 = 0x3f;
const CHG_CTRL2_OFFSET_MASK: u32 = 0x3 << 4;
const HSTX_TRIM_MASK: u32 = 0xf << 4;
const PREEMPH_WIDTH_HALF_BIT: u32 = 1 << 2;
const PREEMPHASIS_EN_MASK: u32 = 0x3;
const HSDISC_TRIM_MASK: u32 = 0x3;

const RESET_HOLD_US: u64 = 100;
const PHY_ENABLE_SETTLE_US: u64 = 150;
const PLL_POLL_TIMEOUT_US: u64 = 1_000;
const OPERATION_TIMEOUT_US: u64 = 10_000;
const POLL_INTERVAL_US: u64 = 10;

const PHASE_OFF: u8 = 0;
const PHASE_TRANSITION: u8 = 1;
const PHASE_ON: u8 = 2;

// The writes below deliberately keep Linux's SC7180 QUSB2 v2 ordering.
const INIT_SEQUENCE: &[(usize, u32)] = &[
    (PLL_ANALOG_CONTROLS_TWO, 0x03),
    (PLL_CLOCK_INVERTERS, 0x7c),
    (PLL_CMODE, 0x80),
    (PLL_LOCK_DELAY, 0x0a),
    (PLL_DIGITAL_TIMERS_TWO, 0x19),
    (PLL_BIAS_CONTROL_1, 0x40),
    (PLL_BIAS_CONTROL_2, 0x20),
    (PWR_CTRL2, 0x21),
    (IMP_CTRL1, 0x00),
    (IMP_CTRL2, 0x58),
    (PORT_TUNE1, 0x30),
    (PORT_TUNE2, 0x29),
    (PORT_TUNE3, 0xca),
    (PORT_TUNE4, 0x04),
    (PORT_TUNE5, 0x03),
    (CHG_CTRL2, 0x00),
];

#[derive(Clone, Copy, Default)]
struct Tuning {
    imp_res_offset: Option<u8>,
    hstx_trim: Option<u8>,
    preemphasis: Option<u8>,
    preemphasis_width: Option<u8>,
    bias_ctrl: Option<u8>,
    charge_ctrl: Option<u8>,
    hsdisc_trim: Option<u8>,
}

/// One SC7180 QUSB2 v2 PHY and its zero-cell provider.
pub struct Sc7180Qusb2V2Phy {
    base: usize,
    cfg_ahb_clk: ClkHandle,
    ref_clk: Option<ClkHandle>,
    iface_clk: Option<ClkHandle>,
    phy_reset: ResetHandle,
    hstx_trim: Option<NvmemCell>,
    tuning: Tuning,
    phase: AtomicU8,
    mode: scarlet::sync::IrqSpinLock<Option<PhyMode>>,
}

impl Sc7180Qusb2V2Phy {
    fn new(
        base: usize,
        cfg_ahb_clk: ClkHandle,
        ref_clk: Option<ClkHandle>,
        iface_clk: Option<ClkHandle>,
        phy_reset: ResetHandle,
        hstx_trim: Option<NvmemCell>,
        tuning: Tuning,
    ) -> Self {
        Self {
            base,
            cfg_ahb_clk,
            ref_clk,
            iface_clk,
            phy_reset,
            hstx_trim,
            tuning,
            phase: AtomicU8::new(PHASE_OFF),
            mode: scarlet::sync::IrqSpinLock::new(None),
        }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: `base` is a mapped QUSB2 PHY register window, and all
        // offsets are from the SC7180 QUSB2 v2 layout (within its 0x400-byte
        // resource).
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: see `Self::read`; this is a 32-bit QUSB2 PHY register write.
        unsafe { mmio::write32(self.base + offset, value) };
        // Complete posted writes before dependent register operations.
        let _ = self.read(offset);
    }

    fn update(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | (set & clear));
    }

    fn set_bits(&self, offset: usize, bits: u32) {
        self.write(offset, self.read(offset) | bits);
    }

    fn clear_bits(&self, offset: usize, bits: u32) {
        self.write(offset, self.read(offset) & !bits);
    }

    fn enable_programming_clocks(&self) -> Result<(), PhyError> {
        if let Some(clock) = &self.iface_clk {
            if clock.prepare_enable().is_err() {
                println!("[{}] power_on: iface clock enable failed", DRIVER_NAME);
                return Err(PhyError::PowerOnFailed);
            }
        }
        if self.cfg_ahb_clk.prepare_enable().is_err() {
            println!("[{}] power_on: cfg_ahb clock enable failed", DRIVER_NAME);
            if let Some(clock) = &self.iface_clk {
                clock.disable_unprepare();
            }
            return Err(PhyError::PowerOnFailed);
        }
        Ok(())
    }

    fn disable_programming_clocks(&self) {
        self.cfg_ahb_clk.disable_unprepare();
        if let Some(clock) = &self.iface_clk {
            clock.disable_unprepare();
        }
    }

    fn apply_tuning(&self) {
        if let Some(value) = self.tuning.imp_res_offset {
            self.update(IMP_CTRL1, IMP_RES_OFFSET_MASK, u32::from(value));
        }
        if let Some(value) = self.tuning.bias_ctrl {
            self.update(
                PLL_BIAS_CONTROL_2,
                BIAS_CTRL2_RES_OFFSET_MASK,
                u32::from(value),
            );
        }
        if let Some(value) = self.tuning.charge_ctrl {
            self.update(CHG_CTRL2, CHG_CTRL2_OFFSET_MASK, u32::from(value) << 4);
        }
        if let Some(value) = self.tuning.hstx_trim {
            self.update(PORT_TUNE1, HSTX_TRIM_MASK, u32::from(value) << 4);
        }
        if let Some(value) = self.tuning.preemphasis {
            self.update(PORT_TUNE1, PREEMPHASIS_EN_MASK, u32::from(value));
        }
        if let Some(value) = self.tuning.preemphasis_width {
            if value == 1 {
                self.set_bits(PORT_TUNE1, PREEMPH_WIDTH_HALF_BIT);
            } else {
                self.clear_bits(PORT_TUNE1, PREEMPH_WIDTH_HALF_BIT);
            }
        }
        if let Some(value) = self.tuning.hsdisc_trim {
            self.update(PORT_TUNE2, HSDISC_TRIM_MASK, u32::from(value));
        }
    }

    fn apply_nvmem_hstx_trim(&self) {
        let Some(cell) = &self.hstx_trim else {
            return;
        };
        if cell.size() != 1 {
            println!(
                "[{}] ignoring hstx trim cell with size {}",
                DRIVER_NAME,
                cell.size()
            );
            return;
        }

        let mut trim = [0u8; 1];
        if cell.read(&mut trim).is_err() || trim[0] == 0 {
            println!(
                "[{}] no valid fused hstx trim; retaining default",
                DRIVER_NAME
            );
            return;
        }

        // QUSB2 v2 uses the efuse value as TUNE1's HSTX field.
        self.update(PORT_TUNE1, HSTX_TRIM_MASK, u32::from(trim[0]) << 4);
    }

    fn wait_for_pll(&self) -> Result<(), PhyError> {
        let deadline = time::current_time().saturating_add(PLL_POLL_TIMEOUT_US);
        loop {
            let status = self.read(PLL_STATUS);
            if status & CORE_READY_STATUS != 0 {
                println!(
                    "[{}] power_on: PLL ready (status={:#x})",
                    DRIVER_NAME, status
                );
                return Ok(());
            }
            if time::current_time() >= deadline {
                println!(
                    "[{}] power_on: PLL core-ready timeout after {} us (status={:#x})",
                    DRIVER_NAME,
                    PLL_POLL_TIMEOUT_US,
                    self.read(PLL_STATUS)
                );
                return Err(PhyError::Timeout);
            }
            time::udelay(POLL_INTERVAL_US);
        }
    }

    fn initialize(&self) -> Result<(), PhyError> {
        // Rails are intentionally retained from the boot firmware: Scarlet
        // currently lacks a regulator API to acquire or sequence them.
        println!(
            "[{}] power_on: enable cfg_ahb{}; firmware owns rails and single-ended CXO{}",
            DRIVER_NAME,
            if self.iface_clk.is_some() {
                " + iface"
            } else {
                ""
            },
            if self.ref_clk.is_some() {
                " (DT ref left unprepared)"
            } else {
                ""
            }
        );
        self.enable_programming_clocks()?;

        println!("[{}] power_on: assert PHY reset", DRIVER_NAME);
        if self.phy_reset.assert().is_err() {
            println!("[{}] power_on: PHY reset assert failed", DRIVER_NAME);
            self.disable_programming_clocks();
            return Err(PhyError::ResetFailed);
        }
        time::udelay(RESET_HOLD_US);
        println!("[{}] power_on: deassert PHY reset", DRIVER_NAME);
        if self.phy_reset.deassert().is_err() {
            println!("[{}] power_on: PHY reset deassert failed", DRIVER_NAME);
            let _ = self.phy_reset.assert();
            self.disable_programming_clocks();
            return Err(PhyError::ResetFailed);
        }

        self.set_bits(PORT_POWERDOWN, DISABLE_CTRL);
        for &(offset, value) in INIT_SEQUENCE {
            self.write(offset, value);
        }
        self.apply_tuning();
        self.apply_nvmem_hstx_trim();
        self.clear_bits(PORT_POWERDOWN, POWER_DOWN);
        time::udelay(PHY_ENABLE_SETTLE_US);

        // Linux's SC7180 QUSB2 v2 configuration defaults to the
        // single-ended reference scheme when no TCSR clock-scheme syscon is
        // supplied.  It therefore does not prepare `ref`; boot firmware keeps
        // CXO running.  Preparing a present `ref` here would diverge from that
        // sequence and is unsafe for the OS handoff DTB's optional RPMh clock.

        if let Err(error) = self.wait_for_pll() {
            self.set_bits(
                PLL_CORE_INPUT_OVERRIDE,
                CORE_PLL_EN_FROM_RESET | CORE_RESET | CORE_RESET_MUX,
            );
            let _ = self.phy_reset.assert();
            self.disable_programming_clocks();
            return Err(error);
        }
        Ok(())
    }

    fn shutdown(&self) -> Result<(), PhyError> {
        self.set_bits(PORT_POWERDOWN, DISABLE_CTRL);
        self.set_bits(
            PLL_CORE_INPUT_OVERRIDE,
            CORE_PLL_EN_FROM_RESET | CORE_RESET | CORE_RESET_MUX,
        );
        let reset_result = self.phy_reset.assert();
        self.disable_programming_clocks();
        reset_result.map_err(|_| PhyError::PowerOffFailed)
    }

    fn claim_transition(&self, allowed: u8) -> Result<u8, PhyError> {
        let deadline = time::current_time().saturating_add(OPERATION_TIMEOUT_US);
        loop {
            let phase = self.phase.load(Ordering::Acquire);
            if phase == allowed
                && self
                    .phase
                    .compare_exchange(
                        allowed,
                        PHASE_TRANSITION,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return Ok(allowed);
            }
            if phase != PHASE_TRANSITION {
                return Ok(phase);
            }
            if time::current_time() >= deadline {
                return Err(PhyError::Timeout);
            }
            time::udelay(POLL_INTERVAL_US);
        }
    }

    fn complete_transition(&self, phase: u8) {
        self.phase.store(phase, Ordering::Release);
    }
}

impl Phy for Sc7180Qusb2V2Phy {
    fn name(&self) -> &'static str {
        "qcom,sc7180-qusb2-phy"
    }

    fn power_on(&self) -> Result<(), PhyError> {
        match self.claim_transition(PHASE_OFF)? {
            PHASE_ON => return Ok(()),
            PHASE_OFF => {}
            _ => return Err(PhyError::Busy),
        }

        println!("[{}] power_on: begin", DRIVER_NAME);
        let result = self.initialize();
        if let Err(error) = &result {
            println!("[{}] power_on: failed ({:?})", DRIVER_NAME, error);
        }
        self.complete_transition(if result.is_ok() { PHASE_ON } else { PHASE_OFF });
        result
    }

    fn power_off(&self) -> Result<(), PhyError> {
        match self.claim_transition(PHASE_ON)? {
            PHASE_OFF => return Ok(()),
            PHASE_ON => {}
            _ => return Err(PhyError::Busy),
        }

        let result = self.shutdown();
        self.complete_transition(PHASE_OFF);
        result
    }

    fn reset(&self) -> Result<(), PhyError> {
        match self.claim_transition(PHASE_ON)? {
            PHASE_ON => {}
            PHASE_OFF => return Err(PhyError::InvalidMode),
            _ => return Err(PhyError::Busy),
        }

        let result = self.shutdown().and_then(|_| self.initialize());
        self.complete_transition(if result.is_ok() { PHASE_ON } else { PHASE_OFF });
        result
    }

    fn set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        match mode {
            PhyMode::UsbHost | PhyMode::UsbDevice | PhyMode::UsbOtg => {
                *self.mode.lock() = Some(mode);
                Ok(())
            }
            _ => Err(PhyError::NotSupported),
        }
    }

    fn get_mode(&self) -> Option<PhyMode> {
        *self.mode.lock()
    }
}

struct Qusb2PhyProvider {
    phy: Arc<Sc7180Qusb2V2Phy>,
}

impl PhyProvider for Qusb2PhyProvider {
    fn name(&self) -> &'static str {
        self.phy.name()
    }

    fn phy_cells(&self) -> usize {
        0
    }

    fn get_phy(&self, spec: &[u32]) -> Result<PhyHandle, PhyError> {
        if !spec.is_empty() {
            return Err(PhyError::NotFound);
        }
        Ok(PhyHandle::new(self.phy.clone()))
    }
}

fn property_u8(
    device: &PlatformDeviceInfo,
    name: &str,
    max: u8,
) -> Result<Option<u8>, &'static str> {
    let Some(property) = device.property(name) else {
        return Ok(None);
    };
    let value = property
        .as_usize()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or("qcom-qusb2-v2: malformed tuning property")?;
    if value > max {
        return Err("qcom-qusb2-v2: tuning value outside documented range");
    }
    Ok(Some(value))
}

fn tuning_from_dt(device: &PlatformDeviceInfo) -> Result<Tuning, &'static str> {
    Ok(Tuning {
        imp_res_offset: property_u8(device, "qcom,imp-res-offset-value", 0x3f)?,
        bias_ctrl: property_u8(device, "qcom,bias-ctrl-value", 0x3f)?,
        charge_ctrl: property_u8(device, "qcom,charge-ctrl-value", 0x3)?,
        hstx_trim: property_u8(device, "qcom,hstx-trim-value", 0xf)?,
        preemphasis: property_u8(device, "qcom,preemphasis-level", 0x3)?,
        preemphasis_width: property_u8(device, "qcom,preemphasis-width", 0x1)?,
        hsdisc_trim: property_u8(device, "qcom,hsdisc-trim-value", 0x3)?,
    })
}

fn has_named_item(device: &PlatformDeviceInfo, property: &str, name: &str) -> bool {
    device
        .property(property)
        .and_then(|property| property.as_string_list())
        .is_some_and(|names| names.contains(&name))
}

fn resolve_optional_iface_clock(
    device: &PlatformDeviceInfo,
) -> Result<Option<ClkHandle>, &'static str> {
    if has_named_item(device, "clock-names", "iface") {
        DeviceManager::get_manager()
            .resolve_clk(device, "iface")
            .map(Some)
    } else {
        Ok(None)
    }
}

fn resolve_reference_clock(device: &PlatformDeviceInfo) -> Option<ClkHandle> {
    // `DeviceManager::resolve_clk` intentionally falls back to clock index 0
    // when a requested name is absent.  The OS handoff DTB removes the
    // unmanaged RPMh-CXO `ref` specifier, so do not accidentally reinterpret
    // the remaining `cfg_ahb` clock as the PHY reference clock.
    if !has_named_item(device, "clock-names", "ref") {
        println!(
            "[{}] reference clock omitted by OS handoff; preserving firmware CXO handoff",
            DRIVER_NAME
        );
        return None;
    }

    match DeviceManager::get_manager().resolve_clk(device, "ref") {
        Ok(clock) => Some(clock),
        Err(error) => {
            // The SC7180 binding sources this clock from RPMh CXO. Scarlet
            // does not yet expose an RPMh clock provider, while Depthcharge
            // and U-Boot leave the 19.2 MHz reference running for QUSB2.
            // Retain that firmware handoff instead of deferring forever.
            println!(
                "[{}] reference clock unavailable ({}); preserving firmware CXO handoff",
                DRIVER_NAME, error
            );
            None
        }
    }
}

fn resolve_phy_reset(device: &PlatformDeviceInfo) -> Result<ResetHandle, &'static str> {
    if has_named_item(device, "reset-names", "phy") {
        DeviceManager::get_manager().resolve_reset(device, "phy")
    } else {
        // SC7180's upstream DT has one unnamed reset, matching Linux's
        // reset_control_get_by_index(..., 0).
        DeviceManager::get_manager().resolve_reset_by_index(device, 0)
    }
}

fn resolve_optional_hstx_trim(
    device: &PlatformDeviceInfo,
) -> Result<Option<NvmemCell>, &'static str> {
    let Some(cells) = device.property("nvmem-cells") else {
        return Ok(None);
    };
    let first = cells
        .value()
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or("qcom-qusb2-v2: malformed nvmem-cells")?;

    match DeviceManager::get_manager().resolve_nvmem_cell_by_phandle(first, "qusb2-hstx-trim") {
        Ok(cell) => Ok(Some(cell)),
        Err(PROBE_DEFER) => Err(PROBE_DEFER),
        Err(error) => {
            println!(
                "[{}] hstx trim inaccessible ({}); using default",
                DRIVER_NAME, error
            );
            Ok(None)
        }
    }
}

fn device_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-qusb2-v2: missing phandle")
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-qusb2-v2: no memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-qusb2-v2: invalid memory resource")?;
    if size < 0x400 {
        return Err("qcom-qusb2-v2: PHY resource is smaller than 0x400 bytes");
    }
    let base = vm::ioremap(resource.start, size).map_err(|_| "qcom-qusb2-v2: ioremap failed")?;

    let manager = DeviceManager::get_manager();
    let cfg_ahb_clk = manager.resolve_clk(device, "cfg_ahb")?;
    let ref_clk = resolve_reference_clock(device);
    let iface_clk = resolve_optional_iface_clock(device)?;
    let phy_reset = resolve_phy_reset(device)?;
    let hstx_trim = resolve_optional_hstx_trim(device)?;
    let tuning = tuning_from_dt(device)?;
    let phandle = device_phandle(device)?;
    let cfg_ahb_name = cfg_ahb_clk.name();
    let ref_source = if ref_clk.is_some() {
        "DT-present/unprepared"
    } else {
        "firmware-handoff"
    };

    let phy = Arc::new(Sc7180Qusb2V2Phy::new(
        base,
        cfg_ahb_clk,
        ref_clk,
        iface_clk,
        phy_reset,
        hstx_trim,
        tuning,
    ));
    manager.register_phy_provider(phandle, Arc::new(Qusb2PhyProvider { phy }));
    println!(
        "[{}] registered SC7180 QUSB2 v2 PHY (phandle={:#x}, cfg_ahb={}, ref={}); firmware rails/CXO retained",
        DRIVER_NAME, phandle, cfg_ahb_name, ref_source,
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        DRIVER_NAME,
        probe_fn,
        remove_fn,
        vec!["qcom,sc7180-qusb2-phy", "qcom,qusb2-v2-phy"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_QUSB2_V2_ANCHOR: fn() = force_link;

#[inline(never)]
pub fn force_link() {}
