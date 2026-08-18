// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 display clock and MDSS power controller.
//!
//! The controller preserves a live firmware handoff when possible. It records
//! the inherited GDSC state, only requests power when MDSS is off, and then
//! starts the clock roots and branches needed by the native DPU/DSI path.
//!
//! # Provenance
//!
//! Register layout and GDSC sequencing follow Linux
//! `drivers/clk/qcom/dispcc-sc7180.c` and `drivers/clk/qcom/gdsc.c`. Clock-root
//! programming also follows coreboot's SC7180 display clock implementation.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::mmio,
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        power::{PowerDomain, PowerManager},
    },
    early_println,
    sync::IrqSpinLock,
    time, vm,
};

const REGISTER_WINDOW_SIZE: usize = 0x1_0000;

const GDSC_CONTROL: usize = 0x3000;
const GDSC_STATUS: usize = 0x3004;
const GDSC_POWER_ON: u32 = 1 << 31;
const GDSC_POWER_UP_COMPLETE: u32 = 1 << 16;
const GDSC_POWER_DOWN_COMPLETE: u32 = 1 << 15;
const GDSC_HARDWARE_CONTROL: u32 = 1 << 1;
const GDSC_SOFTWARE_OVERRIDE: u32 = 1 << 2;
const GDSC_SOFTWARE_COLLAPSE: u32 = 1;
const GDSC_RESTORE_WAIT_MASK: u32 = 0xf << 20;
const GDSC_FEW_WAIT_MASK: u32 = 0xf << 16;
const GDSC_CLOCK_DISABLE_WAIT_MASK: u32 = 0xf << 12;
const GDSC_WAIT_MASK: u32 = GDSC_RESTORE_WAIT_MASK
    | GDSC_FEW_WAIT_MASK
    | GDSC_CLOCK_DISABLE_WAIT_MASK
    | GDSC_HARDWARE_CONTROL
    | GDSC_SOFTWARE_OVERRIDE;
const GDSC_WAIT_VALUES: u32 = (2 << 20) | (2 << 16) | (0xf << 12);
const GDSC_TIMEOUT_US: u64 = 2_000;

const MDP_BRANCH: usize = 0x200c;
const VSYNC_BRANCH: usize = 0x2024;
const PCLK0_BRANCH: usize = 0x2004;
const BYTE0_BRANCH: usize = 0x2028;
const BYTE0_INTERFACE_BRANCH: usize = 0x202c;
const ESC0_BRANCH: usize = 0x2038;
const AHB_BRANCH: usize = 0x2080;
const NON_GDSC_AHB_BRANCH: usize = 0x4004;

const PCLK0_ROOT: usize = 0x2098;
const MDP_ROOT: usize = 0x20c8;
const VSYNC_ROOT: usize = 0x20f8;
const BYTE0_ROOT: usize = 0x2110;
const BYTE0_INTERFACE_DIVIDER: usize = 0x2128;
const ESC0_ROOT: usize = 0x2148;
const AHB_ROOT: usize = 0x22bc;

const ROOT_COMMAND: usize = 0;
const ROOT_CONFIG: usize = 4;
const ROOT_M: usize = 8;
const ROOT_N: usize = 0xc;
const ROOT_D2: usize = 0x10;
const ROOT_UPDATE: u32 = 1;
const ROOT_SOURCE_SHIFT: u32 = 8;
const ROOT_DUAL_EDGE_MODE: u32 = 2 << 12;
const ROOT_MND_MASK: u32 = 0xffff;

const SOURCE_XO: u32 = 0;
const SOURCE_DSI_PHY: u32 = 1;
const SOURCE_GPLL0: u32 = 4;

const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;
const BRANCH_TIMEOUT_US: u64 = 100;

/// Read-only DSI clock state used for hardware bring-up diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct Sc7180DsiClockSnapshot {
    pub pclk0_command: u32,
    pub pclk0_config: u32,
    pub byte0_command: u32,
    pub byte0_config: u32,
    pub byte0_interface_divider: u32,
    pub esc0_command: u32,
    pub esc0_config: u32,
    pub pclk0_branch: u32,
    pub byte0_branch: u32,
    pub byte0_interface_branch: u32,
    pub esc0_branch: u32,
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
        // SAFETY: the constructor receives an ioremap'd DISP_CC window and
        // all offsets in this driver are bounded by `REGISTER_WINDOW_SIZE`.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        // SAFETY: see `read`; the same mapped register window is used here.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn update(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }
}

/// One SC7180 display clock controller.
pub struct Sc7180DispCc {
    registers: RegisterWindow,
    phandle: u32,
    lock: IrqSpinLock<()>,
}

impl Sc7180DispCc {
    fn new(base: usize, phandle: u32) -> Self {
        Self {
            registers: RegisterWindow::new(base),
            phandle,
            lock: IrqSpinLock::new(()),
        }
    }

    /// Firmware phandle identifying this DISP_CC provider.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    fn gdsc_is_on(&self) -> bool {
        self.registers.read(GDSC_CONTROL) & GDSC_POWER_ON != 0
            || self.registers.read(GDSC_STATUS) & GDSC_POWER_UP_COMPLETE != 0
    }

    fn wait_for_gdsc(&self, on: bool) -> Result<(), &'static str> {
        let start = time::current_time();
        loop {
            let control = self.registers.read(GDSC_CONTROL);
            let status = self.registers.read(GDSC_STATUS);
            let reached = if on {
                control & GDSC_POWER_ON != 0 || status & GDSC_POWER_UP_COMPLETE != 0
            } else {
                control & GDSC_POWER_ON == 0 || status & GDSC_POWER_DOWN_COMPLETE != 0
            };
            if reached {
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= GDSC_TIMEOUT_US {
                return Err("qcom-sc7180-dispcc: MDSS GDSC transition timed out");
            }
            time::udelay(1);
        }
    }

    fn enable_gdsc_locked(&self) -> Result<(), &'static str> {
        let inherited_control = self.registers.read(GDSC_CONTROL);
        let inherited_status = self.registers.read(GDSC_STATUS);
        let inherited_on = self.gdsc_is_on();
        early_println!(
            "[qcom-sc7180-dispcc] MDSS GDSC handoff: control={:#010x} status={:#010x} on={}",
            inherited_control,
            inherited_status,
            inherited_on,
        );

        // Keep the domain under software control while Scarlet reconstructs
        // the display pipeline. This avoids a hardware-triggered collapse in
        // the interval between power-up and enabling the consumer branches.
        self.registers
            .update(GDSC_CONTROL, GDSC_WAIT_MASK, GDSC_WAIT_VALUES);
        if !inherited_on {
            self.registers
                .update(GDSC_CONTROL, GDSC_SOFTWARE_COLLAPSE, 0);
            self.wait_for_gdsc(true)?;
            time::udelay(1);
        }

        early_println!(
            "[qcom-sc7180-dispcc] MDSS GDSC ready: control={:#010x} status={:#010x}",
            self.registers.read(GDSC_CONTROL),
            self.registers.read(GDSC_STATUS),
        );
        Ok(())
    }

    fn configure_root(&self, root: usize, source: u32, divider: u32, m: u32, n: u32, d2: u32) {
        let encoded_divider = if divider == 0 { 0 } else { divider * 2 - 1 };
        self.registers.write(
            root + ROOT_CONFIG,
            (source << ROOT_SOURCE_SHIFT) | encoded_divider,
        );
        if m != 0 {
            self.registers
                .update(root + ROOT_CONFIG, 0, ROOT_DUAL_EDGE_MODE);
            self.registers.write(root + ROOT_M, m & ROOT_MND_MASK);
            self.registers
                .write(root + ROOT_N, (!(n - m)) & ROOT_MND_MASK);
            self.registers.write(root + ROOT_D2, (!d2) & ROOT_MND_MASK);
        }
        self.registers.update(root + ROOT_COMMAND, 0, ROOT_UPDATE);
    }

    fn enable_branch(&self, branch: usize) -> Result<(), &'static str> {
        self.registers.update(branch, 0, BRANCH_ENABLE);
        let start = time::current_time();
        while self.registers.read(branch) & BRANCH_OFF != 0 {
            if time::current_time().saturating_sub(start) >= BRANCH_TIMEOUT_US {
                return Err("qcom-sc7180-dispcc: display clock failed to start");
            }
            time::udelay(1);
        }
        Ok(())
    }

    fn branch_is_running(&self, branch: usize) -> bool {
        let value = self.registers.read(branch);
        value & BRANCH_ENABLE != 0 && value & BRANCH_OFF == 0
    }

    fn preserve_or_enable_branch(&self, branch: usize) -> Result<(), &'static str> {
        let inherited = self.registers.read(branch);
        if self.branch_is_running(branch) {
            early_println!(
                "[qcom-sc7180-dispcc] preserving live clock branch {:#x} value={:#010x}",
                branch,
                inherited,
            );
            Ok(())
        } else {
            self.enable_branch(branch)
        }
    }

    fn preserve_or_start_branch(
        &self,
        root: usize,
        branch: usize,
        source: u32,
        divider: u32,
    ) -> Result<(), &'static str> {
        if self.branch_is_running(branch) {
            self.preserve_or_enable_branch(branch)?;
            return Ok(());
        }

        self.configure_root(root, source, divider, 0, 0, 0);
        self.enable_branch(branch)
    }

    /// Restore the MDSS power domain and foundational DPU bus/core clocks.
    ///
    /// This operation is handoff-safe: already-running resources remain on,
    /// while disabled resources are configured and enabled in dependency order.
    pub fn prepare_for_scanout(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();

        // The non-GDSC AHB path must be usable before manipulating the domain.
        if !self.branch_is_running(NON_GDSC_AHB_BRANCH) && !self.branch_is_running(AHB_BRANCH) {
            self.configure_root(AHB_ROOT, SOURCE_GPLL0, 8, 0, 0, 0);
        }
        self.preserve_or_enable_branch(NON_GDSC_AHB_BRANCH)?;
        self.enable_gdsc_locked()?;

        // DPU scanout uses the MDSS AHB and MDP core. Keep the pixel-rate
        // independent VSYNC root alive as well.
        self.preserve_or_enable_branch(AHB_BRANCH)?;
        self.preserve_or_start_branch(MDP_ROOT, MDP_BRANCH, SOURCE_GPLL0, 2)?;
        self.preserve_or_start_branch(VSYNC_ROOT, VSYNC_BRANCH, SOURCE_XO, 1)?;

        early_println!(
            "[qcom-sc7180-dispcc] foundational clocks ready: ahb={:#010x} mdp={:#010x} vsync={:#010x}",
            self.registers.read(AHB_BRANCH),
            self.registers.read(MDP_BRANCH),
            self.registers.read(VSYNC_BRANCH),
        );
        Ok(())
    }

    /// Select DSI PHY byte/pixel outputs and enable the DSI0 branches.
    pub fn enable_dsi0(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.configure_root(ESC0_ROOT, SOURCE_XO, 1, 0, 0, 0);
        self.configure_root(PCLK0_ROOT, SOURCE_DSI_PHY, 1, 0, 0, 0);
        self.configure_root(BYTE0_ROOT, SOURCE_DSI_PHY, 1, 0, 0, 0);
        // The 10 nm D-PHY exposes byte_intf_clk at half the byte clock.
        // Linux's clk_regmap_div encoding is divisor - 1, so /2 is value 1.
        self.registers.update(BYTE0_INTERFACE_DIVIDER, 0x3, 1);

        self.enable_branch(ESC0_BRANCH)?;
        self.enable_branch(PCLK0_BRANCH)?;
        self.enable_branch(BYTE0_BRANCH)?;
        self.enable_branch(BYTE0_INTERFACE_BRANCH)
    }

    /// Capture the complete DISP_CC state feeding DSI0 without modifying it.
    pub fn dsi0_clock_snapshot(&self) -> Sc7180DsiClockSnapshot {
        Sc7180DsiClockSnapshot {
            pclk0_command: self.registers.read(PCLK0_ROOT + ROOT_COMMAND),
            pclk0_config: self.registers.read(PCLK0_ROOT + ROOT_CONFIG),
            byte0_command: self.registers.read(BYTE0_ROOT + ROOT_COMMAND),
            byte0_config: self.registers.read(BYTE0_ROOT + ROOT_CONFIG),
            byte0_interface_divider: self.registers.read(BYTE0_INTERFACE_DIVIDER),
            esc0_command: self.registers.read(ESC0_ROOT + ROOT_COMMAND),
            esc0_config: self.registers.read(ESC0_ROOT + ROOT_CONFIG),
            pclk0_branch: self.registers.read(PCLK0_BRANCH),
            byte0_branch: self.registers.read(BYTE0_BRANCH),
            byte0_interface_branch: self.registers.read(BYTE0_INTERFACE_BRANCH),
            esc0_branch: self.registers.read(ESC0_BRANCH),
        }
    }
}

impl PowerDomain for Sc7180DispCc {
    fn enable(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.enable_gdsc_locked()
    }

    fn disable(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.registers
            .update(GDSC_CONTROL, GDSC_HARDWARE_CONTROL, GDSC_SOFTWARE_COLLAPSE);
        self.wait_for_gdsc(false)
    }

    fn is_enabled(&self) -> bool {
        self.gdsc_is_on()
    }

    fn label(&self) -> &str {
        "sc7180-mdss-gdsc"
    }
}

static CONTROLLERS: IrqSpinLock<Vec<Arc<Sc7180DispCc>>> = IrqSpinLock::new(Vec::new());

/// Find a probed SC7180 DISP_CC provider by firmware phandle.
pub fn get_sc7180_dispcc_by_phandle(phandle: u32) -> Option<Arc<Sc7180DispCc>> {
    CONTROLLERS
        .lock()
        .iter()
        .find(|controller| controller.phandle() == phandle)
        .cloned()
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-sc7180-dispcc: missing phandle")
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    // Match the Apple PMGR provider: the platform power registry is owned by
    // the first concrete provider rather than initialized globally.
    PowerManager::init();

    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-sc7180-dispcc: missing register resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-dispcc: invalid register resource")?;
    if resource_size < REGISTER_WINDOW_SIZE {
        return Err("qcom-sc7180-dispcc: register resource is too small");
    }

    let base = vm::ioremap(resource.start, REGISTER_WINDOW_SIZE)
        .map_err(|_| "qcom-sc7180-dispcc: ioremap failed")?;
    let phandle = read_phandle(device)?;
    let controller = Arc::new(Sc7180DispCc::new(base, phandle));
    PowerManager::register_domain(phandle, Arc::clone(&controller) as Arc<dyn PowerDomain>);
    CONTROLLERS.lock().push(Arc::clone(&controller));

    early_println!(
        "[qcom-sc7180-dispcc] registered phandle={:#x} paddr={:#x}",
        phandle,
        resource.start,
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-sc7180-dispcc",
        probe_fn,
        remove_fn,
        vec!["qcom,sc7180-dispcc"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Critical);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_DISPCC_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
