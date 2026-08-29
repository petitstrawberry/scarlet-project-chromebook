// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 Venus clock and power-domain controller.
//!
//! # Provenance
//!
//! Register layout, Fabia PLL programming, RCG2 encoding, branch handling,
//! and GDSC sequencing follow Linux `drivers/clk/qcom/videocc-sc7180.c` and
//! the Qualcomm common-clock `clk-alpha-pll.c`, `clk-rcg2.c`, and `gdsc.c`
//! implementations.

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

const REGISTER_WINDOW_SIZE: usize = 0x1_0000;

const VIDEO_PLL0: u32 = 0;
const VIDEO_CC_VCODEC0_AXI_CLK: u32 = 1;
const VIDEO_CC_VCODEC0_CORE_CLK: u32 = 2;
const VIDEO_CC_VENUS_AHB_CLK: u32 = 3;
const VIDEO_CC_VENUS_CLK_SRC: u32 = 4;
const VIDEO_CC_VENUS_CTL_AXI_CLK: u32 = 5;
const VIDEO_CC_VENUS_CTL_CORE_CLK: u32 = 6;
const VIDEO_CC_XO_CLK: u32 = 7;

const VENUS_GDSC: u32 = 0;
const VCODEC0_GDSC: u32 = 1;

const PLL0_BASE: usize = 0x42c;
const PLL_MODE: usize = 0x00;
const PLL_L_VAL: usize = 0x04;
const PLL_USER_CTL: usize = 0x0c;
const PLL_USER_CTL_U: usize = 0x10;
const PLL_STATUS: usize = 0x24;
const PLL_OPMODE: usize = 0x2c;
const PLL_FRAC: usize = 0x38;
const PLL_OUTCTRL: u32 = 1 << 0;
const PLL_RESET_N: u32 = 1 << 2;
const PLL_UPDATE: u32 = 1 << 22;
const PLL_UPDATE_BYPASS: u32 = 1 << 23;
const PLL_ACK_LATCH: u32 = 1 << 29;
const PLL_LOCK_DET: u32 = 1 << 31;
const PLL_OUT_MASK: u32 = 0x7;
const PLL_STANDBY: u32 = 0;
const PLL_RUN: u32 = 1;

const PLL0_BOOT_L: u32 = 0x1f;
const PLL0_BOOT_FRAC: u32 = 0x4000;
const PLL0_1GHZ_L: u32 = 52;
const PLL0_1GHZ_FRAC: u32 = 0x1556;

const VENUS_CLOCK_ROOT: usize = 0x7f0;
const ROOT_COMMAND: usize = 0;
const ROOT_CONFIG: usize = 4;
const ROOT_UPDATE: u32 = 1;
const ROOT_ENABLE: u32 = 1 << 1;
const ROOT_OFF: u32 = 1 << 31;
const ROOT_SOURCE_SHIFT: u32 = 8;
const ROOT_SOURCE_PLL0: u32 = 1;
const ROOT_DIVIDE_BY_TWO: u32 = 3;

const VCODEC0_CORE_BRANCH: usize = 0x890;
const VCODEC0_AXI_BRANCH: usize = 0x9ec;
const VENUS_AHB_BRANCH: usize = 0xa4c;
const VENUS_CTL_AXI_BRANCH: usize = 0x9cc;
const VENUS_CTL_CORE_BRANCH: usize = 0x850;
const XO_BRANCH: usize = 0x984;
const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;

const VENUS_GDSCR: usize = 0x814;
const VCODEC0_GDSCR: usize = 0x874;
const GDSC_SOFTWARE_COLLAPSE: u32 = 1;
const GDSC_HARDWARE_CONTROL: u32 = 1 << 1;
const GDSC_SOFTWARE_OVERRIDE: u32 = 1 << 2;
const GDSC_POWER_ON: u32 = 1 << 31;
const GDSC_ENABLE_REST_WAIT_MASK: u32 = 0xf << 20;
const GDSC_ENABLE_FEW_WAIT_MASK: u32 = 0xf << 16;
const GDSC_CLOCK_DISABLE_WAIT_MASK: u32 = 0xf << 12;
const GDSC_DEFAULT_WAITS: u32 = (2 << 20) | (8 << 16) | (2 << 12);

const CLOCK_TIMEOUT_US: u64 = 2_000;
const GDSC_TIMEOUT_US: u64 = 5_000;
const VENUS_RATE_HZ: u64 = 500_000_000;

#[derive(Clone, Copy)]
struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(self, offset: usize) -> u32 {
        // SAFETY: probe maps the complete VIDEO_CC register resource and all
        // constants used by this driver are inside that window.
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

/// Probed SC7180 VIDEO_CC provider.
pub struct Sc7180VideoCc {
    registers: RegisterWindow,
    phandle: u32,
    _parent_xo: EnabledClock,
    _mapping: MmioMapping,
    lock: IrqSpinLock<()>,
}

impl Sc7180VideoCc {
    fn new(mapping: MmioMapping, phandle: u32, parent_xo: EnabledClock) -> Self {
        Self {
            registers: RegisterWindow::new(mapping.base),
            phandle,
            _parent_xo: parent_xo,
            _mapping: mapping,
            lock: IrqSpinLock::new(()),
        }
    }

    /// Return the firmware phandle identifying this provider.
    ///
    /// # Returns
    ///
    /// Non-zero VIDEO_CC phandle.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    fn configure_pll0_defaults(&self) {
        // These are the only non-zero fields supplied by Linux' SC7180
        // VIDEO_CC descriptor. In particular, CONFIG_CTL and TEST_CTL are not
        // GPU_CC values and must retain their hardware defaults here.
        self.registers.write(PLL0_BASE + PLL_L_VAL, PLL0_BOOT_L);
        self.registers.write(PLL0_BASE + PLL_FRAC, PLL0_BOOT_FRAC);
        self.registers.write(PLL0_BASE + PLL_USER_CTL, 1);
        self.registers
            .write(PLL0_BASE + PLL_USER_CTL_U, 0x0000_4805);
        arch::io_wmb();

        self.registers
            .update(PLL0_BASE + PLL_MODE, 0, PLL_UPDATE_BYPASS);
        arch::io_wmb();
        self.registers.update(PLL0_BASE + PLL_MODE, 0, PLL_RESET_N);
        arch::io_wmb();
    }

    fn wait_for_pll_mode(
        &self,
        mask: u32,
        expected_set: bool,
        error: &'static str,
    ) -> Result<(), &'static str> {
        let start = time::current_time();
        loop {
            let set = self.registers.read(PLL0_BASE + PLL_MODE) & mask == mask;
            if set == expected_set {
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err(error);
            }
            time::udelay(1);
        }
    }

    fn latch_pll0_update(&self) -> Result<(), &'static str> {
        let update_bypass = self.registers.read(PLL0_BASE + PLL_MODE) & PLL_UPDATE_BYPASS != 0;
        self.registers.update(PLL0_BASE + PLL_MODE, 0, PLL_UPDATE);
        arch::io_wmb();

        // Fabia requires two reference cycles before ACK may be sampled.
        time::udelay(1);
        if update_bypass {
            self.wait_for_pll_mode(
                PLL_ACK_LATCH,
                true,
                "qcom-sc7180-videocc: PLL update ACK timed out",
            )?;
            self.registers.update(PLL0_BASE + PLL_MODE, PLL_UPDATE, 0);
            arch::io_wmb();
        } else {
            self.wait_for_pll_mode(
                PLL_UPDATE,
                false,
                "qcom-sc7180-videocc: PLL update timed out",
            )?;
        }
        self.wait_for_pll_mode(
            PLL_ACK_LATCH,
            false,
            "qcom-sc7180-videocc: PLL update ACK failed to clear",
        )?;
        time::udelay(10);
        Ok(())
    }

    fn set_pll0_1ghz(&self) -> Result<(), &'static str> {
        if self.registers.read(PLL0_BASE + PLL_L_VAL) == PLL0_1GHZ_L
            && self.registers.read(PLL0_BASE + PLL_FRAC) == PLL0_1GHZ_FRAC
        {
            return Ok(());
        }

        // 19.2 MHz * (52 + 0x1556 / 2^16) = 1,000,000,195 Hz. The RCG
        // divide-by-two encoding below therefore matches Linux' 500,000,097
        // Hz top OPP after integer clock rounding.
        self.registers.write(PLL0_BASE + PLL_L_VAL, PLL0_1GHZ_L);
        self.registers.write(PLL0_BASE + PLL_FRAC, PLL0_1GHZ_FRAC);
        arch::io_wmb();
        self.latch_pll0_update()
    }

    fn enable_pll0(&self) -> Result<(), &'static str> {
        let mode = self.registers.read(PLL0_BASE + PLL_MODE);
        let opmode = self.registers.read(PLL0_BASE + PLL_OPMODE);
        if opmode & PLL_RUN != 0 && mode & PLL_OUTCTRL != 0 {
            return Ok(());
        }

        self.registers.update(PLL0_BASE + PLL_MODE, PLL_OUTCTRL, 0);
        self.registers.write(PLL0_BASE + PLL_OPMODE, PLL_STANDBY);
        self.registers.update(PLL0_BASE + PLL_MODE, 0, PLL_RESET_N);
        self.registers.write(PLL0_BASE + PLL_OPMODE, PLL_RUN);
        arch::io_wmb();
        self.wait_for_pll_mode(
            PLL_LOCK_DET,
            true,
            "qcom-sc7180-videocc: PLL failed to lock",
        )?;
        self.registers
            .update(PLL0_BASE + PLL_USER_CTL, 0, PLL_OUT_MASK);
        self.registers.update(PLL0_BASE + PLL_MODE, 0, PLL_OUTCTRL);
        arch::io_wmb();
        Ok(())
    }

    fn prepare_pll0_1ghz(&self) -> Result<(), &'static str> {
        self.set_pll0_1ghz()?;
        self.enable_pll0()
    }

    fn configure_venus_root(&self) -> Result<(), &'static str> {
        // `clk_rcg2_shared_ops` force-enables an otherwise parked RCG while
        // installing its cached configuration. Without this step the update
        // can acknowledge even though the new PLL source never reaches the
        // downstream Venus branches.
        self.registers
            .update(VENUS_CLOCK_ROOT + ROOT_COMMAND, 0, ROOT_ENABLE);
        let start = time::current_time();
        while self.registers.read(VENUS_CLOCK_ROOT + ROOT_COMMAND) & ROOT_OFF != 0 {
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err("qcom-sc7180-videocc: Venus clock root failed to start");
            }
            time::udelay(1);
        }
        self.registers.write(
            VENUS_CLOCK_ROOT + ROOT_CONFIG,
            (ROOT_SOURCE_PLL0 << ROOT_SOURCE_SHIFT) | ROOT_DIVIDE_BY_TWO,
        );
        self.registers
            .update(VENUS_CLOCK_ROOT + ROOT_COMMAND, 0, ROOT_UPDATE);
        let start = time::current_time();
        while self.registers.read(VENUS_CLOCK_ROOT + ROOT_COMMAND) & ROOT_UPDATE != 0 {
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err("qcom-sc7180-videocc: Venus clock root update timed out");
            }
            time::udelay(1);
        }
        self.registers
            .update(VENUS_CLOCK_ROOT + ROOT_COMMAND, ROOT_ENABLE, 0);
        arch::io_wmb();
        Ok(())
    }

    fn configure_domain_defaults(&self) {
        let mask = GDSC_HARDWARE_CONTROL
            | GDSC_SOFTWARE_OVERRIDE
            | GDSC_ENABLE_REST_WAIT_MASK
            | GDSC_ENABLE_FEW_WAIT_MASK
            | GDSC_CLOCK_DISABLE_WAIT_MASK;
        for offset in [VENUS_GDSCR, VCODEC0_GDSCR] {
            self.registers.update(offset, mask, GDSC_DEFAULT_WAITS);
        }
        arch::io_wmb();
    }

    fn log_state(&self, reason: &str) {
        early_println!(
            "[qcom-sc7180-videocc] state={} pll={:#010x}/{:#010x}/{:#010x} l={:#x} frac={:#x} root={:#010x}/{:#010x} branches={:#010x},{:#010x},{:#010x},{:#010x},{:#010x},{:#010x} gdsc={:#010x},{:#010x}",
            reason,
            self.registers.read(PLL0_BASE + PLL_MODE),
            self.registers.read(PLL0_BASE + PLL_OPMODE),
            self.registers.read(PLL0_BASE + PLL_STATUS),
            self.registers.read(PLL0_BASE + PLL_L_VAL),
            self.registers.read(PLL0_BASE + PLL_FRAC),
            self.registers.read(VENUS_CLOCK_ROOT + ROOT_COMMAND),
            self.registers.read(VENUS_CLOCK_ROOT + ROOT_CONFIG),
            self.registers.read(VENUS_CTL_CORE_BRANCH),
            self.registers.read(VENUS_AHB_BRANCH),
            self.registers.read(VENUS_CTL_AXI_BRANCH),
            self.registers.read(VCODEC0_CORE_BRANCH),
            self.registers.read(VCODEC0_AXI_BRANCH),
            self.registers.read(XO_BRANCH),
            self.registers.read(VENUS_GDSCR),
            self.registers.read(VCODEC0_GDSCR),
        );
    }

    fn enable_branch(&self, offset: usize, voted: bool) -> Result<(), &'static str> {
        self.registers.update(offset, 0, BRANCH_ENABLE);
        arch::io_wmb();
        let start = time::current_time();
        while self.registers.read(offset) & BRANCH_OFF != 0 {
            // A voted branch may retain its halt indication briefly while the
            // shared vote propagates, but still has the same finite contract.
            if time::current_time().saturating_sub(start) >= CLOCK_TIMEOUT_US {
                return Err(if voted {
                    "qcom-sc7180-videocc: voted clock branch failed to start"
                } else {
                    "qcom-sc7180-videocc: clock branch failed to start"
                });
            }
            time::udelay(1);
        }
        Ok(())
    }

    fn enable_domain_locked(&self, id: u32) -> Result<(), &'static str> {
        let offset = match id {
            VENUS_GDSC => VENUS_GDSCR,
            VCODEC0_GDSC => VCODEC0_GDSCR,
            _ => return Err("qcom-sc7180-videocc: invalid power domain"),
        };

        if id == VCODEC0_GDSC {
            self.enable_domain_locked(VENUS_GDSC)?;
        }

        // Keep the domain under explicit software control while Scarlet owns
        // the codec. Linux later permits HW trigger control for VCODEC0, but
        // doing so before runtime-PM exists can collapse it between submits.
        self.registers
            .update(offset, GDSC_SOFTWARE_COLLAPSE | GDSC_HARDWARE_CONTROL, 0);
        arch::io_wmb();
        let start = time::current_time();
        while self.registers.read(offset) & GDSC_POWER_ON == 0 {
            if time::current_time().saturating_sub(start) >= GDSC_TIMEOUT_US {
                return Err("qcom-sc7180-videocc: GDSC power-up timed out");
            }
            time::udelay(1);
        }
        // Linux leaves one microsecond between memory power-up and clocks.
        time::udelay(1);
        self.log_state(if id == VENUS_GDSC {
            "venus-domain"
        } else {
            "vcodec0-domain"
        });
        Ok(())
    }

    fn enable_domain(&self, id: u32) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.enable_domain_locked(id)
    }

    /// Enable the complete 500 MHz Venus clock path.
    ///
    /// # Returns
    ///
    /// Success after both GDSCs, the PLL/RCG, and all consumer branches are
    /// hardware-visible.
    pub fn prepare_for_video(&self) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        self.enable_domain_locked(VENUS_GDSC)?;
        self.enable_domain_locked(VCODEC0_GDSC)?;
        self.prepare_pll0_1ghz()?;
        self.configure_venus_root()?;
        self.enable_branch(XO_BRANCH, false)?;
        self.enable_branch(VENUS_AHB_BRANCH, false)?;
        self.enable_branch(VENUS_CTL_AXI_BRANCH, false)?;
        self.enable_branch(VENUS_CTL_CORE_BRANCH, false)?;
        self.enable_branch(VCODEC0_AXI_BRANCH, false)?;
        self.enable_branch(VCODEC0_CORE_BRANCH, true)?;
        Ok(())
    }
}

struct VideoClock {
    controller: Arc<Sc7180VideoCc>,
    id: u32,
}

impl VideoClock {
    fn branch(&self) -> Option<(usize, bool)> {
        match self.id {
            VIDEO_CC_VCODEC0_AXI_CLK => Some((VCODEC0_AXI_BRANCH, false)),
            VIDEO_CC_VCODEC0_CORE_CLK => Some((VCODEC0_CORE_BRANCH, true)),
            VIDEO_CC_VENUS_AHB_CLK => Some((VENUS_AHB_BRANCH, false)),
            VIDEO_CC_VENUS_CTL_AXI_CLK => Some((VENUS_CTL_AXI_BRANCH, false)),
            VIDEO_CC_VENUS_CTL_CORE_CLK => Some((VENUS_CTL_CORE_BRANCH, false)),
            VIDEO_CC_XO_CLK => Some((XO_BRANCH, false)),
            _ => None,
        }
    }
}

impl Clk for VideoClock {
    fn name(&self) -> &'static str {
        match self.id {
            VIDEO_PLL0 => "video_pll0",
            VIDEO_CC_VCODEC0_AXI_CLK => "video_cc_vcodec0_axi_clk",
            VIDEO_CC_VCODEC0_CORE_CLK => "video_cc_vcodec0_core_clk",
            VIDEO_CC_VENUS_AHB_CLK => "video_cc_venus_ahb_clk",
            VIDEO_CC_VENUS_CLK_SRC => "video_cc_venus_clk_src",
            VIDEO_CC_VENUS_CTL_AXI_CLK => "video_cc_venus_ctl_axi_clk",
            VIDEO_CC_VENUS_CTL_CORE_CLK => "video_cc_venus_ctl_core_clk",
            VIDEO_CC_XO_CLK => "video_cc_xo_clk",
            _ => "video_cc_unknown",
        }
    }

    fn enable(&self) -> Result<(), ClkError> {
        let _guard = self.controller.lock.lock();
        let result = match self.id {
            VIDEO_PLL0 => self
                .controller
                .prepare_pll0_1ghz()
                .map_err(|_| ClkError::HardwareError),
            VIDEO_CC_VENUS_CLK_SRC => {
                self.controller
                    .prepare_pll0_1ghz()
                    .map_err(|_| ClkError::HardwareError)?;
                self.controller
                    .configure_venus_root()
                    .map_err(|_| ClkError::HardwareError)
            }
            _ => {
                let (branch, voted) = self.branch().ok_or(ClkError::Unsupported)?;
                if matches!(
                    self.id,
                    VIDEO_CC_VCODEC0_CORE_CLK | VIDEO_CC_VENUS_CTL_CORE_CLK
                ) {
                    self.controller
                        .prepare_pll0_1ghz()
                        .map_err(|_| ClkError::HardwareError)?;
                    self.controller
                        .configure_venus_root()
                        .map_err(|_| ClkError::HardwareError)?;
                }
                self.controller
                    .enable_branch(branch, voted)
                    .map_err(|_| ClkError::HardwareError)
            }
        };
        if result.is_ok() {
            self.controller.log_state(self.name());
        }
        result
    }

    fn disable(&self) {
        if let Some((branch, _)) = self.branch() {
            self.controller.registers.update(branch, BRANCH_ENABLE, 0);
            arch::io_wmb();
        }
    }

    fn is_enabled(&self) -> bool {
        match self.id {
            VIDEO_PLL0 => {
                self.controller.registers.read(PLL0_BASE + PLL_MODE) & (PLL_OUTCTRL | PLL_LOCK_DET)
                    == (PLL_OUTCTRL | PLL_LOCK_DET)
            }
            VIDEO_CC_VENUS_CLK_SRC => true,
            _ => self
                .branch()
                .is_some_and(|(branch, _)| self.controller.registers.read(branch) & 1 != 0),
        }
    }

    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        match self.id {
            VIDEO_PLL0 => VENUS_RATE_HZ * 2,
            VIDEO_CC_VENUS_CLK_SRC | VIDEO_CC_VCODEC0_CORE_CLK | VIDEO_CC_VENUS_CTL_CORE_CLK => {
                VENUS_RATE_HZ
            }
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
        if matches!(
            self.id,
            VIDEO_PLL0
                | VIDEO_CC_VENUS_CLK_SRC
                | VIDEO_CC_VCODEC0_CORE_CLK
                | VIDEO_CC_VENUS_CTL_CORE_CLK
        ) {
            self.controller
                .prepare_pll0_1ghz()
                .map_err(|_| ClkError::HardwareError)?;
            self.controller
                .configure_venus_root()
                .map_err(|_| ClkError::HardwareError)?;
        }
        Ok(rounded)
    }
}

struct VideoClockProvider {
    clocks: Vec<(u32, ClkHandle)>,
}

impl VideoClockProvider {
    fn new(controller: &Arc<Sc7180VideoCc>) -> Self {
        let ids = [
            VIDEO_PLL0,
            VIDEO_CC_VCODEC0_AXI_CLK,
            VIDEO_CC_VCODEC0_CORE_CLK,
            VIDEO_CC_VENUS_AHB_CLK,
            VIDEO_CC_VENUS_CLK_SRC,
            VIDEO_CC_VENUS_CTL_AXI_CLK,
            VIDEO_CC_VENUS_CTL_CORE_CLK,
            VIDEO_CC_XO_CLK,
        ];
        let clocks = ids
            .into_iter()
            .map(|id| {
                (
                    id,
                    ClkHandle::new(Arc::new(VideoClock {
                        controller: Arc::clone(controller),
                        id,
                    })),
                )
            })
            .collect();
        Self { clocks }
    }
}

impl ClkProvider for VideoClockProvider {
    fn name(&self) -> &'static str {
        "qcom-sc7180-videocc"
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

struct VideoPowerDomain {
    controller: Arc<Sc7180VideoCc>,
    id: u32,
}

impl PowerDomain for VideoPowerDomain {
    fn enable(&self) -> Result<(), &'static str> {
        self.controller.enable_domain(self.id)
    }

    fn disable(&self) -> Result<(), &'static str> {
        let offset = match self.id {
            VENUS_GDSC => VENUS_GDSCR,
            VCODEC0_GDSC => VCODEC0_GDSCR,
            _ => return Err("qcom-sc7180-videocc: invalid power domain"),
        };
        self.controller
            .registers
            .update(offset, GDSC_HARDWARE_CONTROL, GDSC_SOFTWARE_COLLAPSE);
        arch::io_wmb();
        Ok(())
    }

    fn is_enabled(&self) -> bool {
        let offset = match self.id {
            VENUS_GDSC => VENUS_GDSCR,
            VCODEC0_GDSC => VCODEC0_GDSCR,
            _ => return false,
        };
        self.controller.registers.read(offset) & GDSC_POWER_ON != 0
    }

    fn label(&self) -> &str {
        match self.id {
            VENUS_GDSC => "sc7180-venus",
            VCODEC0_GDSC => "sc7180-vcodec0",
            _ => "sc7180-video-invalid",
        }
    }
}

struct VideoPowerDomainProvider {
    controller: Arc<Sc7180VideoCc>,
}

impl PowerDomainProvider for VideoPowerDomainProvider {
    fn power_domain_cells(&self) -> usize {
        1
    }

    fn get_domain(&self, specifier: &[u32]) -> Result<Arc<dyn PowerDomain>, &'static str> {
        let [id @ (VENUS_GDSC | VCODEC0_GDSC)] = specifier else {
            return Err("qcom-sc7180-videocc: unsupported power-domain specifier");
        };
        Ok(Arc::new(VideoPowerDomain {
            controller: Arc::clone(&self.controller),
            id: *id,
        }))
    }
}

struct EnabledClock {
    clock: Option<ClkHandle>,
}

impl EnabledClock {
    fn prepare(clock: ClkHandle) -> Result<Self, ClkError> {
        clock.prepare_enable()?;
        Ok(Self { clock: Some(clock) })
    }
}

impl Drop for EnabledClock {
    fn drop(&mut self) {
        if let Some(clock) = self.clock.take() {
            clock.disable_unprepare();
        }
    }
}

struct MmioMapping {
    base: usize,
}

impl Drop for MmioMapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

static CONTROLLERS: IrqSpinLock<Vec<Arc<Sc7180VideoCc>>> = IrqSpinLock::new(Vec::new());

/// Find a probed SC7180 VIDEO_CC provider by firmware phandle.
///
/// # Arguments
///
/// * `phandle` - VIDEO_CC provider phandle.
///
/// # Returns
///
/// Matching controller, or `None` before probe.
pub fn get_sc7180_videocc_by_phandle(phandle: u32) -> Option<Arc<Sc7180VideoCc>> {
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
        .ok_or("qcom-sc7180-videocc: missing phandle")
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    PowerManager::init();
    let manager = DeviceManager::get_manager();
    let parent_xo = match manager.resolve_clk(device, "bi_tcxo") {
        Ok(clock) => clock,
        Err("clk: provider not found") | Err("clk: clock not found") => {
            return scarlet::device::manager::probe_defer();
        }
        Err(error) => return Err(error),
    };
    let parent_xo = EnabledClock::prepare(parent_xo)
        .map_err(|_| "qcom-sc7180-videocc: failed to enable XO parent")?;
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-sc7180-videocc: missing register resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-videocc: invalid register resource")?;
    if resource_size < REGISTER_WINDOW_SIZE {
        return Err("qcom-sc7180-videocc: register resource is too small");
    }
    let mapping = MmioMapping {
        base: vm::ioremap(resource.start, REGISTER_WINDOW_SIZE)
            .map_err(|_| "qcom-sc7180-videocc: ioremap failed")?,
    };
    let phandle = read_phandle(device)?;
    let controller = Arc::new(Sc7180VideoCc::new(mapping, phandle, parent_xo));

    controller.configure_pll0_defaults();
    controller.configure_domain_defaults();
    controller.enable_branch(XO_BRANCH, false)?;
    manager.register_clk_provider(phandle, Arc::new(VideoClockProvider::new(&controller)));
    PowerManager::register_provider(
        phandle,
        Arc::new(VideoPowerDomainProvider {
            controller: Arc::clone(&controller),
        }),
    );
    CONTROLLERS.lock().push(Arc::clone(&controller));
    early_println!(
        "[qcom-sc7180-videocc] registered phandle={:#x} paddr={:#x} pll={:#010x}",
        phandle,
        resource.start,
        controller.registers.read(PLL0_BASE + PLL_MODE),
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-videocc",
            probe,
            remove,
            vec!["qcom,sc7180-videocc"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_VIDEOCC_ANCHOR: fn() = force_link;

/// Keep this external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
