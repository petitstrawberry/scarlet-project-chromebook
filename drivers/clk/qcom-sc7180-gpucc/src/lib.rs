// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 GPU clock and CX/GX power-domain provider.
//!
//! Register programming follows Linux `drivers/clk/qcom/gpucc-sc7180.c` and
//! the common Qualcomm Fabia PLL/GDSC implementations.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::{self, mmio},
    device::{
        clk::{Clk, ClkError, ClkHandle, ClkProvider},
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        power::{PowerDomain, PowerDomainProvider, PowerManager},
    },
    early_println,
    sync::IrqSpinLock,
    time, vm,
};

const REGISTER_WINDOW_SIZE: usize = 0x9000;

const GPU_CC_PLL1: u32 = 0;
const GPU_CC_CRC_AHB_CLK: u32 = 2;
const GPU_CC_CX_GMU_CLK: u32 = 3;
const GPU_CC_CX_SNOC_DVM_CLK: u32 = 4;
const GPU_CC_CXO_AON_CLK: u32 = 5;
const GPU_CC_CXO_CLK: u32 = 6;
const GPU_CC_GMU_CLK_SRC: u32 = 7;

const CX_GDSC: u32 = 0;
const GX_GDSC: u32 = 1;

const PLL1_BASE: usize = 0x100;
const PLL_MODE: usize = 0x00;
const PLL_L_VAL: usize = 0x04;
const PLL_USER_CTL: usize = 0x0c;
const PLL_USER_CTL_U: usize = 0x10;
const PLL_CONFIG_CTL: usize = 0x14;
const PLL_CONFIG_CTL_U: usize = 0x18;
const PLL_TEST_CTL_U: usize = 0x20;
const PLL_FRAC: usize = 0x38;
const PLL_RESET_N: u32 = 1 << 2;
const PLL_UPDATE_BYPASS: u32 = 1 << 23;

const GMU_CLOCK_ROOT: usize = 0x1120;
const ROOT_CONFIG: usize = 0x4;
const ROOT_UPDATE: u32 = 1;
const ROOT_SOURCE_SHIFT: u32 = 8;
const ROOT_SOURCE_GPLL0_DIV: u32 = 6;
const ROOT_DIVIDE_BY_1P5: u32 = 2;

const CRC_AHB_BRANCH: usize = 0x107c;
const CX_GMU_BRANCH: usize = 0x1098;
const CX_SNOC_DVM_BRANCH: usize = 0x108c;
const CXO_AON_BRANCH: usize = 0x1004;
const CXO_BRANCH: usize = 0x109c;
const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;
const CX_GMU_WAKE_SLEEP_MASK: u32 = 0xfff0;
const CX_GMU_WAKE_SLEEP_VALUE: u32 = 0xfff0;

const CX_GDSCR: usize = 0x106c;
const GX_GDSCR: usize = 0x100c;
const GX_CLAMP_IO: usize = 0x1508;
const GDSC_SOFTWARE_COLLAPSE: u32 = 1;
const GDSC_POWER_ON: u32 = 1 << 31;
const GDSC_TIMEOUT_US: u64 = 2_000;
const CLOCK_TIMEOUT_US: u64 = 1_000;

#[derive(Clone, Copy)]
struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(self, offset: usize) -> u32 {
        // SAFETY: probe maps the complete GPU_CC resource and every constant is
        // below `REGISTER_WINDOW_SIZE`.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        // SAFETY: see `read`; writes target the same mapped register window.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn update(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }
}

/// Probed SC7180 GPU clock controller.
pub struct Sc7180GpuCc {
    registers: RegisterWindow,
    phandle: u32,
    _parents: EnabledClocks,
    _mmio: MmioMapping,
    lock: IrqSpinLock<()>,
}

impl Sc7180GpuCc {
    fn new(mapping: MmioMapping, phandle: u32, parents: EnabledClocks) -> Self {
        Self {
            registers: RegisterWindow::new(mapping.base),
            phandle,
            _parents: parents,
            _mmio: mapping,
            lock: IrqSpinLock::new(()),
        }
    }

    /// Return the firmware phandle of this provider.
    ///
    /// # Returns
    ///
    /// Non-zero GPU_CC phandle.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    fn configure_pll1(&self) {
        let base = PLL1_BASE;
        self.registers.write(base + PLL_L_VAL, 0x12);
        self.registers.write(base + PLL_FRAC, 0xc000);
        self.registers.write(base + PLL_CONFIG_CTL, 0x2048_5699);
        self.registers.write(base + PLL_CONFIG_CTL_U, 0x0000_2067);
        self.registers.write(base + PLL_USER_CTL, 0x0000_0001);
        self.registers.write(base + PLL_USER_CTL_U, 0x0000_4805);
        self.registers.write(base + PLL_TEST_CTL_U, 0x4000_0000);
        self.registers
            .update(base + PLL_MODE, 0, PLL_UPDATE_BYPASS | PLL_RESET_N);
    }

    fn configure_gmu_root(&self) -> Result<(), &'static str> {
        self.registers.write(
            GMU_CLOCK_ROOT + ROOT_CONFIG,
            (ROOT_SOURCE_GPLL0_DIV << ROOT_SOURCE_SHIFT) | ROOT_DIVIDE_BY_1P5,
        );
        self.registers.update(GMU_CLOCK_ROOT, 0, ROOT_UPDATE);
        let start = time::current_time();
        while self.registers.read(GMU_CLOCK_ROOT) & ROOT_UPDATE != 0 {
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err("qcom-sc7180-gpucc: GMU clock root update timed out");
            }
            time::udelay(1);
        }
        Ok(())
    }

    fn enable_branch(&self, offset: usize, wait_for_halt: bool) -> Result<(), &'static str> {
        self.registers.update(offset, 0, BRANCH_ENABLE);
        arch::io_wmb();
        if !wait_for_halt {
            return Ok(());
        }
        let start = time::current_time();
        while self.registers.read(offset) & BRANCH_OFF != 0 {
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err("qcom-sc7180-gpucc: clock branch failed to start");
            }
            time::udelay(1);
        }
        Ok(())
    }

    fn enable_cx(&self) -> Result<(), &'static str> {
        self.registers.update(CX_GDSCR, GDSC_SOFTWARE_COLLAPSE, 0);
        arch::io_wmb();
        let start = time::current_time();
        while self.registers.read(CX_GDSCR) & GDSC_POWER_ON == 0 {
            if time::current_time().saturating_sub(start) >= GDSC_TIMEOUT_US {
                return Err("qcom-sc7180-gpucc: CX GDSC power-up timed out");
            }
            time::udelay(1);
        }
        Ok(())
    }

    /// Enable the fixed 200 MHz GMU clock path and foundational GPU_CC branches.
    ///
    /// # Returns
    ///
    /// Success after CX and all GMU-visible clocks are live.
    pub fn prepare_for_gmu(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.enable_cx()?;
        self.configure_gmu_root()?;
        self.registers.update(
            CX_GMU_BRANCH,
            CX_GMU_WAKE_SLEEP_MASK,
            CX_GMU_WAKE_SLEEP_VALUE,
        );
        self.enable_branch(CXO_AON_BRANCH, false)?;
        self.enable_branch(CXO_BRANCH, true)?;
        self.enable_branch(CRC_AHB_BRANCH, false)?;
        self.enable_branch(CX_SNOC_DVM_BRANCH, false)?;
        self.enable_branch(CX_GMU_BRANCH, true)?;
        Ok(())
    }
}

struct GpuClock {
    controller: Arc<Sc7180GpuCc>,
    id: u32,
}

impl Clk for GpuClock {
    fn name(&self) -> &'static str {
        match self.id {
            GPU_CC_PLL1 => "gpu_cc_pll1",
            GPU_CC_CRC_AHB_CLK => "gpu_cc_crc_ahb_clk",
            GPU_CC_CX_GMU_CLK => "gpu_cc_cx_gmu_clk",
            GPU_CC_CX_SNOC_DVM_CLK => "gpu_cc_cx_snoc_dvm_clk",
            GPU_CC_CXO_AON_CLK => "gpu_cc_cxo_aon_clk",
            GPU_CC_CXO_CLK => "gpu_cc_cxo_clk",
            GPU_CC_GMU_CLK_SRC => "gpu_cc_gmu_clk_src",
            _ => "gpu_cc_unknown",
        }
    }

    fn enable(&self) -> Result<(), ClkError> {
        let result = match self.id {
            GPU_CC_PLL1 => Ok(()),
            GPU_CC_CRC_AHB_CLK => self.controller.enable_branch(CRC_AHB_BRANCH, false),
            GPU_CC_CX_GMU_CLK => self.controller.configure_gmu_root().and_then(|()| {
                self.controller.registers.update(
                    CX_GMU_BRANCH,
                    CX_GMU_WAKE_SLEEP_MASK,
                    CX_GMU_WAKE_SLEEP_VALUE,
                );
                self.controller.enable_branch(CX_GMU_BRANCH, true)
            }),
            GPU_CC_CX_SNOC_DVM_CLK => self.controller.enable_branch(CX_SNOC_DVM_BRANCH, false),
            GPU_CC_CXO_AON_CLK => self.controller.enable_branch(CXO_AON_BRANCH, false),
            GPU_CC_CXO_CLK => self.controller.enable_branch(CXO_BRANCH, true),
            GPU_CC_GMU_CLK_SRC => self.controller.configure_gmu_root(),
            _ => Err("qcom-sc7180-gpucc: unsupported clock"),
        };
        result.map_err(|_| ClkError::HardwareError)
    }

    fn disable(&self) {
        let branch = match self.id {
            GPU_CC_CRC_AHB_CLK => Some(CRC_AHB_BRANCH),
            GPU_CC_CX_GMU_CLK => Some(CX_GMU_BRANCH),
            GPU_CC_CX_SNOC_DVM_CLK => Some(CX_SNOC_DVM_BRANCH),
            GPU_CC_CXO_AON_CLK => Some(CXO_AON_BRANCH),
            GPU_CC_CXO_CLK => Some(CXO_BRANCH),
            _ => None,
        };
        if let Some(branch) = branch {
            self.controller.registers.update(branch, BRANCH_ENABLE, 0);
            arch::io_wmb();
        }
    }

    fn is_enabled(&self) -> bool {
        match self.id {
            GPU_CC_PLL1 => self.controller.registers.read(PLL1_BASE + PLL_MODE) & PLL_RESET_N != 0,
            GPU_CC_CRC_AHB_CLK => self.controller.registers.read(CRC_AHB_BRANCH) & 1 != 0,
            GPU_CC_CX_GMU_CLK => self.controller.registers.read(CX_GMU_BRANCH) & 1 != 0,
            GPU_CC_CX_SNOC_DVM_CLK => self.controller.registers.read(CX_SNOC_DVM_BRANCH) & 1 != 0,
            GPU_CC_CXO_AON_CLK => self.controller.registers.read(CXO_AON_BRANCH) & 1 != 0,
            GPU_CC_CXO_CLK => self.controller.registers.read(CXO_BRANCH) & 1 != 0,
            GPU_CC_GMU_CLK_SRC => true,
            _ => false,
        }
    }

    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        match self.id {
            GPU_CC_PLL1 => 360_000_000,
            GPU_CC_CX_GMU_CLK | GPU_CC_GMU_CLK_SRC => 200_000_000,
            _ => 19_200_000,
        }
    }

    fn round_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        if rate == self.recalc_rate(0) {
            Ok(rate)
        } else {
            Err(ClkError::InvalidRate)
        }
    }

    fn set_rate(&self, rate: u64, parent_rate: u64) -> Result<u64, ClkError> {
        let rounded = self.round_rate(rate, parent_rate)?;
        if matches!(self.id, GPU_CC_CX_GMU_CLK | GPU_CC_GMU_CLK_SRC) {
            self.controller
                .configure_gmu_root()
                .map_err(|_| ClkError::HardwareError)?;
        }
        Ok(rounded)
    }
}

struct GpuClockProvider {
    clocks: Vec<(u32, ClkHandle)>,
}

impl GpuClockProvider {
    fn new(controller: &Arc<Sc7180GpuCc>) -> Self {
        let ids = [
            GPU_CC_PLL1,
            GPU_CC_CRC_AHB_CLK,
            GPU_CC_CX_GMU_CLK,
            GPU_CC_CX_SNOC_DVM_CLK,
            GPU_CC_CXO_AON_CLK,
            GPU_CC_CXO_CLK,
            GPU_CC_GMU_CLK_SRC,
        ];
        let clocks = ids
            .into_iter()
            .map(|id| {
                (
                    id,
                    ClkHandle::new(Arc::new(GpuClock {
                        controller: Arc::clone(controller),
                        id,
                    })),
                )
            })
            .collect();
        Self { clocks }
    }
}

impl ClkProvider for GpuClockProvider {
    fn name(&self) -> &'static str {
        "qcom-sc7180-gpucc"
    }

    fn clock_cells(&self) -> usize {
        1
    }

    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id] = spec else {
            return Err(ClkError::InvalidSpecifier);
        };
        self.clocks
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, clock)| clock.clone())
            .ok_or(ClkError::ClockNotFound)
    }
}

struct GpuPowerDomain {
    controller: Arc<Sc7180GpuCc>,
    id: u32,
}

impl PowerDomain for GpuPowerDomain {
    fn enable(&self) -> Result<(), &'static str> {
        match self.id {
            CX_GDSC => self.controller.enable_cx(),
            // On SC7180 the GMU owns GX sequencing. Linux deliberately uses a
            // no-op GX GDSC enable hook and lets GMU firmware vote the rail.
            GX_GDSC => Ok(()),
            _ => Err("qcom-sc7180-gpucc: invalid power domain"),
        }
    }

    fn disable(&self) -> Result<(), &'static str> {
        match self.id {
            CX_GDSC => {
                self.controller
                    .registers
                    .update(CX_GDSCR, 0, GDSC_SOFTWARE_COLLAPSE);
                Ok(())
            }
            GX_GDSC => Ok(()),
            _ => Err("qcom-sc7180-gpucc: invalid power domain"),
        }
    }

    fn is_enabled(&self) -> bool {
        match self.id {
            CX_GDSC => self.controller.registers.read(CX_GDSCR) & GDSC_POWER_ON != 0,
            GX_GDSC => {
                self.controller.registers.read(GX_GDSCR) & GDSC_POWER_ON != 0
                    || self.controller.registers.read(GX_CLAMP_IO) == 0
            }
            _ => false,
        }
    }

    fn label(&self) -> &str {
        match self.id {
            CX_GDSC => "sc7180-gpu-cx",
            GX_GDSC => "sc7180-gpu-gx",
            _ => "sc7180-gpu-invalid",
        }
    }
}

struct GpuPowerDomainProvider {
    controller: Arc<Sc7180GpuCc>,
}

impl PowerDomainProvider for GpuPowerDomainProvider {
    fn power_domain_cells(&self) -> usize {
        1
    }

    fn get_domain(&self, specifier: &[u32]) -> Result<Arc<dyn PowerDomain>, &'static str> {
        let [id @ (CX_GDSC | GX_GDSC)] = specifier else {
            return Err("qcom-sc7180-gpucc: unsupported power-domain specifier");
        };
        Ok(Arc::new(GpuPowerDomain {
            controller: Arc::clone(&self.controller),
            id: *id,
        }))
    }
}

static CONTROLLERS: IrqSpinLock<Vec<Arc<Sc7180GpuCc>>> = IrqSpinLock::new(Vec::new());

struct EnabledClocks(Vec<ClkHandle>);

impl Drop for EnabledClocks {
    fn drop(&mut self) {
        for clock in self.0.iter().rev() {
            clock.disable_unprepare();
        }
    }
}

/// Owns the complete GPU_CC ioremap allocation while the provider is live.
struct MmioMapping {
    base: usize,
}

impl Drop for MmioMapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

/// Find a probed SC7180 GPU_CC provider by firmware phandle.
///
/// # Arguments
///
/// * `phandle` - GPU_CC provider phandle.
///
/// # Returns
///
/// Matching controller, or `None` before probe.
pub fn get_sc7180_gpucc_by_phandle(phandle: u32) -> Option<Arc<Sc7180GpuCc>> {
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
        .filter(|value| *value != 0)
        .ok_or("qcom-sc7180-gpucc: missing phandle")
}

fn enable_parent_clocks(
    manager: &DeviceManager,
    device: &PlatformDeviceInfo,
) -> Result<EnabledClocks, &'static str> {
    let Some(property) = device.property("clock-names") else {
        return if device.property("clocks").is_some() {
            Err("qcom-sc7180-gpucc: clocks property is missing clock-names")
        } else {
            Ok(EnabledClocks(Vec::new()))
        };
    };
    let names = property
        .as_string_list()
        .ok_or("qcom-sc7180-gpucc: malformed clock-names")?;
    let mut parents = EnabledClocks(Vec::new());
    for name in names {
        let clock = match manager.resolve_clk(device, name) {
            Ok(clock) => clock,
            // CoachZ firmware hands the always-on BI_TCXO reference to the
            // kernel, while Scarlet does not yet expose the parent RPMh clock
            // controller.  This is the same handoff contract used by the GCC
            // driver.  Keep every programmable GCC parent strict below; only
            // the immutable XO reference may be inherited.
            Err("clk: provider not found" | "clk: clock not found") if name == "bi_tcxo" => {
                early_println!("[qcom-sc7180-gpucc] inheriting firmware-enabled bi_tcxo parent");
                continue;
            }
            Err("clk: provider not found") | Err("clk: clock not found") => {
                return scarlet::device::manager::probe_defer();
            }
            Err(error) => return Err(error),
        };
        match clock.prepare_enable() {
            Ok(()) => parents.0.push(clock),
            Err(ClkError::ProviderNotFound | ClkError::ClockNotFound) => {
                return scarlet::device::manager::probe_defer();
            }
            Err(_) => return Err("qcom-sc7180-gpucc: failed to enable parent clock"),
        }
    }
    Ok(parents)
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    PowerManager::init();
    let manager = DeviceManager::get_manager();
    let phandle = read_phandle(device)?;
    // This guard owns every successful prepare/enable and unwinds in reverse
    // order if a later parent cannot resolve or enable.
    let parents = enable_parent_clocks(manager, device)?;
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-sc7180-gpucc: missing register resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-gpucc: invalid register resource")?;
    if resource_size < REGISTER_WINDOW_SIZE {
        return Err("qcom-sc7180-gpucc: register resource is too small");
    }
    let mapping = MmioMapping {
        base: vm::ioremap(resource.start, resource_size)
            .map_err(|_| "qcom-sc7180-gpucc: ioremap failed")?,
    };
    let controller = Arc::new(Sc7180GpuCc::new(mapping, phandle, parents));
    controller.configure_pll1();
    controller.registers.update(
        CX_GMU_BRANCH,
        CX_GMU_WAKE_SLEEP_MASK,
        CX_GMU_WAKE_SLEEP_VALUE,
    );

    manager.register_clk_provider(phandle, Arc::new(GpuClockProvider::new(&controller)));
    PowerManager::register_provider(
        phandle,
        Arc::new(GpuPowerDomainProvider {
            controller: Arc::clone(&controller),
        }),
    );
    CONTROLLERS.lock().push(Arc::clone(&controller));
    early_println!(
        "[qcom-sc7180-gpucc] registered phandle={:#x} paddr={:#x} cx={:#010x} gx={:#010x}",
        phandle,
        resource.start,
        controller.registers.read(CX_GDSCR),
        controller.registers.read(GX_GDSCR),
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-gpucc",
            probe,
            remove,
            vec!["qcom,sc7180-gpucc"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_GPUCC_ANCHOR: fn() = force_link;

/// Keep this external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
