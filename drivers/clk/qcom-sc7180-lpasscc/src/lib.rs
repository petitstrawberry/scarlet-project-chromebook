// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 LPASS clock and power-domain controllers.
//!
//! The register layout and sequencing follow Linux 6.6
//! `drivers/clk/qcom/lpasscorecc-sc7180.c`, `clk-alpha-pll.c`, `clk-rcg2.c`,
//! and `gdsc.c`.  The provider exposes the firmware clock IDs used by the
//! direct LPASS CPU DAI and owns the LPASS_HM GDSC independently from the PCM
//! driver.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::{self, mmio},
    device::{
        clk::{Clk, ClkError, ClkHandle, ClkProvider},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        power::{PowerDomain, PowerDomainProvider, PowerManager},
    },
    early_println,
    sync::IrqSpinLock,
    time, vm,
};

const LPASS_CORE_WINDOW_SIZE: usize = 0x5_0000;
const LPASS_AUDIO_WINDOW_SIZE: usize = 0x3_0000;
const LPASS_HM_WINDOW_SIZE: usize = 0x28;

const LPASS_LPAAUDIO_DIG_PLL: u32 = 0;
const LPASS_LPAAUDIO_DIG_PLL_OUT_ODD: u32 = 1;
const CORE_CLK_SRC: u32 = 2;
const EXT_MCLK0_CLK_SRC: u32 = 3;
const LPAIF_PRI_CLK_SRC: u32 = 4;
const LPAIF_SEC_CLK_SRC: u32 = 5;
const LPASS_AUDIO_CORE_CORE_CLK: u32 = 6;
const LPASS_AUDIO_CORE_EXT_MCLK0_CLK: u32 = 7;
const LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK: u32 = 8;
const LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK: u32 = 9;
const LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK: u32 = 10;

const LPASS_CORE_HM_GDSCR: u32 = 0;
const LPASS_AUDIO_HM_GDSCR: u32 = 0;
const LPASS_PDC_HM_GDSCR: u32 = 1;

const PLL_BASE: usize = 0x1000;
const PLL_MODE: usize = 0x00;
const PLL_L_VAL: usize = 0x04;
const PLL_CAL_L_VAL: usize = 0x08;
const PLL_USER_CTL: usize = 0x0c;
const PLL_USER_CTL_U: usize = 0x10;
const PLL_USER_CTL_U1: usize = 0x14;
const PLL_CONFIG_CTL: usize = 0x18;
const PLL_CONFIG_CTL_U: usize = 0x1c;
const PLL_CONFIG_CTL_U1: usize = 0x20;
const PLL_TEST_CTL: usize = 0x24;
const PLL_TEST_CTL_U: usize = 0x28;
const PLL_STATUS: usize = 0x30;
const PLL_OPMODE: usize = 0x38;
const PLL_FRAC: usize = 0x40;

const PLL_OUTCTRL: u32 = 1;
const PLL_RESET_N: u32 = 1 << 2;
const PLL_UPDATE_BYPASS: u32 = 1 << 23;
const PLL_LOCK_DET: u32 = 1 << 31;
const PLL_OUT_MASK: u32 = 0x7;
const PLL_STANDBY: u32 = 0;
const PLL_RUN: u32 = 1;

const CORE_CLK_CMD: usize = 0x1d000;
const EXT_MCLK0_CMD: usize = 0x20000;
const LPAIF_PRI_CMD: usize = 0x10000;
const LPAIF_SEC_CMD: usize = 0x11000;
const RCG_CMD_UPDATE: u32 = 1;
const RCG_CFG: usize = 0x04;
const RCG_M: usize = 0x08;
const RCG_N: usize = 0x0c;
const RCG_D: usize = 0x10;
const RCG_SRC_DIV_MASK: u32 = 0x1f;
const RCG_SRC_SEL_MASK: u32 = 0x7 << 8;
const RCG_MODE_MASK: u32 = 0x3 << 12;
const RCG_MODE_DUAL_EDGE: u32 = 0x2 << 12;
const RCG_HW_CLK_CTRL: u32 = 1 << 20;
const RCG_PLL_ODD_SOURCE: u32 = 5;

const EXT_MCLK0_BRANCH: usize = 0x20014;
const LPAIF_PRI_BRANCH: usize = 0x10018;
const LPAIF_SEC_BRANCH: usize = 0x11018;
const SYSNOC_MPORT_BRANCH: usize = 0x23000;
const SYSNOC_SWAY_BRANCH: usize = 0x24000;
const BRANCH_ENABLE: u32 = 1;
const BRANCH_HW_CLOCK_GATING: u32 = 1 << 1;
const BRANCH_OFF: u32 = 1 << 31;

const AUDIO_PDC_GDSCR: usize = 0x3090;
const AUDIO_HM_GDSCR: usize = 0x9090;
const GDSC_SW_COLLAPSE: u32 = 1;
const GDSC_HW_CONTROL: u32 = 1 << 1;
const GDSC_SW_OVERRIDE: u32 = 1 << 2;
const GDSC_RETAIN_FF_ENABLE: u32 = 1 << 11;
const GDSC_CLK_DIS_WAIT_MASK: u32 = 0xf << 12;
const GDSC_EN_FEW_WAIT_MASK: u32 = 0xf << 16;
const GDSC_EN_REST_WAIT_MASK: u32 = 0xf << 20;
const GDSC_DEFAULT_WAITS: u32 = (2 << 20) | (8 << 16) | (2 << 12);
const GDSC_POWER_ON: u32 = 1 << 31;

const XO_RATE: u64 = 19_200_000;
const AUDIO_PLL_RATE: u64 = 614_400_000;
const AUDIO_PLL_ODD_RATE: u64 = 122_880_000;
const DEFAULT_BIT_CLOCK: u64 = 1_536_000;
const CLOCK_TIMEOUT_US: u64 = 2_000;
const GDSC_TIMEOUT_US: u64 = 5_000;

#[derive(Clone, Copy)]
struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    const fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(self, offset: usize) -> u32 {
        // SAFETY: probe maps the complete register resource containing every
        // offset used by this driver.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        // SAFETY: see `read`; writes target the same mapped MMIO window.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    fn update(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
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

struct EnabledClock {
    clock: Option<ClkHandle>,
}

impl EnabledClock {
    fn acquire(clock: ClkHandle) -> Result<Self, ClkError> {
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
            return Err("qcom-sc7180-lpasscc: hardware transition timed out");
        }
        time::udelay(1);
    }
    Ok(())
}

fn initialize_gdsc(registers: RegisterWindow, offset: usize, retain_ff: bool) {
    let mask = GDSC_HW_CONTROL
        | GDSC_SW_OVERRIDE
        | GDSC_EN_REST_WAIT_MASK
        | GDSC_EN_FEW_WAIT_MASK
        | GDSC_CLK_DIS_WAIT_MASK;
    registers.update(offset, mask, GDSC_DEFAULT_WAITS);
    if retain_ff && registers.read(offset) & GDSC_POWER_ON != 0 {
        registers.update(offset, 0, GDSC_RETAIN_FF_ENABLE);
    }
    arch::io_wmb();
}

fn enable_gdsc(
    registers: RegisterWindow,
    offset: usize,
    retain_ff: bool,
) -> Result<(), &'static str> {
    registers.update(offset, GDSC_HW_CONTROL | GDSC_SW_COLLAPSE, 0);
    arch::io_wmb();
    wait_for(
        registers,
        offset,
        GDSC_POWER_ON,
        GDSC_POWER_ON,
        GDSC_TIMEOUT_US,
    )?;
    time::udelay(1);
    if retain_ff {
        registers.update(offset, 0, GDSC_RETAIN_FF_ENABLE);
        arch::io_wmb();
    }
    Ok(())
}

fn disable_gdsc(
    registers: RegisterWindow,
    offset: usize,
    votable: bool,
) -> Result<(), &'static str> {
    registers.update(offset, GDSC_HW_CONTROL, GDSC_SW_COLLAPSE);
    arch::io_wmb();
    if votable {
        time::udelay(100);
        Ok(())
    } else {
        wait_for(registers, offset, GDSC_POWER_ON, 0, GDSC_TIMEOUT_US)
    }
}

struct LpassHmController {
    registers: RegisterWindow,
    _mapping: MmioMapping,
    _iface: EnabledClock,
    _xo: EnabledClock,
    lock: IrqSpinLock<()>,
}

struct LpassHmDomain {
    controller: Arc<LpassHmController>,
}

impl PowerDomain for LpassHmDomain {
    fn enable(&self) -> Result<(), &'static str> {
        let _guard = self.controller.lock.lock();
        enable_gdsc(self.controller.registers, 0, true)
    }

    fn disable(&self) -> Result<(), &'static str> {
        let _guard = self.controller.lock.lock();
        disable_gdsc(self.controller.registers, 0, false)
    }

    fn is_enabled(&self) -> bool {
        self.controller.registers.read(0) & GDSC_POWER_ON != 0
    }

    fn label(&self) -> &str {
        "sc7180-lpass-core-hm"
    }
}

struct LpassHmDomainProvider {
    controller: Arc<LpassHmController>,
}

impl PowerDomainProvider for LpassHmDomainProvider {
    fn power_domain_cells(&self) -> usize {
        1
    }

    fn get_domain(&self, specifier: &[u32]) -> Result<Arc<dyn PowerDomain>, &'static str> {
        let [LPASS_CORE_HM_GDSCR] = specifier else {
            return Err("qcom-sc7180-lpasscc: invalid LPASS_HM domain");
        };
        Ok(Arc::new(LpassHmDomain {
            controller: Arc::clone(&self.controller),
        }))
    }
}

#[derive(Clone, Copy)]
struct RcgRate {
    rate: u64,
    pre_div: u32,
    m: u32,
    n: u32,
    source: u32,
}

const LPAIF_RATES: [RcgRate; 14] = [
    RcgRate {
        rate: 256_000,
        pre_div: 29,
        m: 1,
        n: 32,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 512_000,
        pre_div: 29,
        m: 1,
        n: 16,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 768_000,
        pre_div: 19,
        m: 1,
        n: 16,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 1_024_000,
        pre_div: 29,
        m: 1,
        n: 8,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 1_536_000,
        pre_div: 19,
        m: 1,
        n: 8,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 2_048_000,
        pre_div: 29,
        m: 1,
        n: 4,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 3_072_000,
        pre_div: 19,
        m: 1,
        n: 4,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 4_096_000,
        pre_div: 29,
        m: 1,
        n: 2,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 6_144_000,
        pre_div: 19,
        m: 1,
        n: 2,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 8_192_000,
        pre_div: 29,
        m: 0,
        n: 0,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 9_600_000,
        pre_div: 3,
        m: 0,
        n: 0,
        source: 0,
    },
    RcgRate {
        rate: 12_288_000,
        pre_div: 19,
        m: 0,
        n: 0,
        source: RCG_PLL_ODD_SOURCE,
    },
    RcgRate {
        rate: 19_200_000,
        pre_div: 1,
        m: 0,
        n: 0,
        source: 0,
    },
    RcgRate {
        rate: 24_576_000,
        pre_div: 9,
        m: 0,
        n: 0,
        source: RCG_PLL_ODD_SOURCE,
    },
];

struct LpassClockState {
    ext_mclk_rate: u64,
    pri_bit_rate: u64,
    sec_bit_rate: u64,
}

struct LpassCcController {
    core: RegisterWindow,
    audio: RegisterWindow,
    _core_mapping: MmioMapping,
    _audio_mapping: MmioMapping,
    _iface: EnabledClock,
    _xo: EnabledClock,
    state: IrqSpinLock<LpassClockState>,
    gdsc_lock: IrqSpinLock<()>,
}

impl LpassCcController {
    fn initialize(&self) {
        self.core.update(SYSNOC_SWAY_BRANCH, 0, BRANCH_ENABLE);
        self.core.write(PLL_BASE + PLL_CAL_L_VAL, 0x20);
        self.core.update(PLL_BASE + PLL_USER_CTL_U1, 0, 1);
        self.core.write(PLL_BASE + PLL_L_VAL, 0x20);
        self.core.write(PLL_BASE + PLL_FRAC, 0);
        self.core.write(PLL_BASE + PLL_CONFIG_CTL, 0x2048_5699);
        self.core.write(PLL_BASE + PLL_CONFIG_CTL_U, 0x0000_2067);
        self.core.write(PLL_BASE + PLL_CONFIG_CTL_U1, 0);
        self.core.write(PLL_BASE + PLL_USER_CTL, 0x0000_5105);
        self.core.write(PLL_BASE + PLL_USER_CTL_U, 0x0000_4805);
        self.core.write(PLL_BASE + PLL_TEST_CTL, 0x4000_0000);
        self.core.write(PLL_BASE + PLL_TEST_CTL_U, 0);
        self.core
            .update(PLL_BASE + PLL_MODE, 0, PLL_UPDATE_BYPASS | PLL_RESET_N);
        initialize_gdsc(self.audio, AUDIO_HM_GDSCR, false);
        initialize_gdsc(self.audio, AUDIO_PDC_GDSCR, false);
        arch::io_wmb();
    }

    fn enable_pll(&self) -> Result<(), &'static str> {
        let mode = self.core.read(PLL_BASE + PLL_MODE);
        let opmode = self.core.read(PLL_BASE + PLL_OPMODE);
        if mode & PLL_OUTCTRL != 0 && opmode & PLL_RUN != 0 {
            return Ok(());
        }
        self.core.update(PLL_BASE + PLL_MODE, PLL_OUTCTRL, 0);
        self.core.write(PLL_BASE + PLL_OPMODE, PLL_STANDBY);
        self.core.update(PLL_BASE + PLL_MODE, 0, PLL_RESET_N);
        self.core.write(PLL_BASE + PLL_OPMODE, PLL_RUN);
        arch::io_wmb();
        wait_for(
            self.core,
            PLL_BASE + PLL_MODE,
            PLL_LOCK_DET,
            PLL_LOCK_DET,
            CLOCK_TIMEOUT_US,
        )?;
        self.core.update(PLL_BASE + PLL_USER_CTL, 0, PLL_OUT_MASK);
        self.core.update(PLL_BASE + PLL_MODE, 0, PLL_OUTCTRL);
        arch::io_wmb();
        Ok(())
    }

    fn configure_rcg(&self, command: usize, entry: RcgRate) -> Result<(), &'static str> {
        if entry.source == RCG_PLL_ODD_SOURCE {
            self.enable_pll()?;
        }
        if entry.n != 0 {
            let mask = 0xffff;
            self.core.update(command + RCG_M, mask, entry.m & mask);
            self.core
                .update(command + RCG_N, mask, (!(entry.n - entry.m)) & mask);
            let d = entry.n.clamp(entry.m, (entry.n - entry.m) * 2);
            self.core.update(command + RCG_D, mask, (!d) & mask);
        }
        let mask = RCG_SRC_DIV_MASK | RCG_SRC_SEL_MASK | RCG_MODE_MASK | RCG_HW_CLK_CTRL;
        let mut cfg = entry.pre_div | (entry.source << 8);
        if entry.n != 0 && entry.m != entry.n {
            cfg |= RCG_MODE_DUAL_EDGE;
        }
        self.core.update(command + RCG_CFG, mask, cfg);
        self.core.update(command, 0, RCG_CMD_UPDATE);
        arch::io_wmb();
        wait_for(self.core, command, RCG_CMD_UPDATE, 0, CLOCK_TIMEOUT_US)
    }

    fn set_audio_rate(&self, id: u32, rate: u64) -> Result<u64, ClkError> {
        let mut state = self.state.lock();
        let result = match id {
            EXT_MCLK0_CLK_SRC | LPASS_AUDIO_CORE_EXT_MCLK0_CLK => {
                let entry = match rate {
                    9_600_000 => RcgRate {
                        rate,
                        pre_div: 3,
                        m: 0,
                        n: 0,
                        source: 0,
                    },
                    19_200_000 => RcgRate {
                        rate,
                        pre_div: 1,
                        m: 0,
                        n: 0,
                        source: 0,
                    },
                    _ => return Err(ClkError::InvalidRate),
                };
                self.configure_rcg(EXT_MCLK0_CMD, entry)
                    .map_err(|_| ClkError::HardwareError)?;
                state.ext_mclk_rate = rate;
                rate
            }
            LPAIF_PRI_CLK_SRC | LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => {
                let entry = LPAIF_RATES
                    .iter()
                    .find(|entry| entry.rate == rate)
                    .copied()
                    .ok_or(ClkError::InvalidRate)?;
                self.configure_rcg(LPAIF_PRI_CMD, entry)
                    .map_err(|_| ClkError::HardwareError)?;
                state.pri_bit_rate = rate;
                rate
            }
            LPAIF_SEC_CLK_SRC | LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => {
                let entry = LPAIF_RATES
                    .iter()
                    .find(|entry| entry.rate == rate)
                    .copied()
                    .ok_or(ClkError::InvalidRate)?;
                self.configure_rcg(LPAIF_SEC_CMD, entry)
                    .map_err(|_| ClkError::HardwareError)?;
                state.sec_bit_rate = rate;
                rate
            }
            CORE_CLK_SRC | LPASS_AUDIO_CORE_CORE_CLK | LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK
                if rate == XO_RATE =>
            {
                rate
            }
            LPASS_LPAAUDIO_DIG_PLL if rate == AUDIO_PLL_RATE => rate,
            LPASS_LPAAUDIO_DIG_PLL_OUT_ODD if rate == AUDIO_PLL_ODD_RATE => rate,
            _ => return Err(ClkError::InvalidRate),
        };
        Ok(result)
    }

    fn rate(&self, id: u32) -> u64 {
        let state = self.state.lock();
        match id {
            LPASS_LPAAUDIO_DIG_PLL => AUDIO_PLL_RATE,
            LPASS_LPAAUDIO_DIG_PLL_OUT_ODD => AUDIO_PLL_ODD_RATE,
            CORE_CLK_SRC | LPASS_AUDIO_CORE_CORE_CLK | LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK => {
                XO_RATE
            }
            EXT_MCLK0_CLK_SRC | LPASS_AUDIO_CORE_EXT_MCLK0_CLK => state.ext_mclk_rate,
            LPAIF_PRI_CLK_SRC | LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => state.pri_bit_rate,
            LPAIF_SEC_CLK_SRC | LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => state.sec_bit_rate,
            _ => 0,
        }
    }

    fn enable_clock(&self, id: u32) -> Result<(), ClkError> {
        match id {
            LPASS_LPAAUDIO_DIG_PLL | LPASS_LPAAUDIO_DIG_PLL_OUT_ODD => {
                self.enable_pll().map_err(|_| ClkError::HardwareError)
            }
            CORE_CLK_SRC | LPASS_AUDIO_CORE_CORE_CLK => Ok(()),
            EXT_MCLK0_CLK_SRC => self.set_audio_rate(id, self.rate(id)).map(|_| ()),
            LPAIF_PRI_CLK_SRC => self.set_audio_rate(id, self.rate(id)).map(|_| ()),
            LPAIF_SEC_CLK_SRC => self.set_audio_rate(id, self.rate(id)).map(|_| ()),
            LPASS_AUDIO_CORE_EXT_MCLK0_CLK
            | LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK
            | LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK
            | LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK => {
                if id == LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK {
                    self.configure_rcg(
                        CORE_CLK_CMD,
                        RcgRate {
                            rate: XO_RATE,
                            pre_div: 1,
                            m: 0,
                            n: 0,
                            source: 0,
                        },
                    )
                    .map_err(|_| ClkError::HardwareError)?;
                } else {
                    self.set_audio_rate(id, self.rate(id))?;
                }
                let branch = match id {
                    LPASS_AUDIO_CORE_EXT_MCLK0_CLK => EXT_MCLK0_BRANCH,
                    LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => LPAIF_PRI_BRANCH,
                    LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => LPAIF_SEC_BRANCH,
                    _ => SYSNOC_MPORT_BRANCH,
                };
                self.core.update(branch, 0, BRANCH_ENABLE);
                arch::io_wmb();
                // Qualcomm branch clocks may autonomously gate their output
                // while HWCG is selected.  Linux deliberately skips the
                // CLK_OFF poll in that mode because bit 31 is then a demand
                // indication rather than proof that the software vote failed.
                if self.core.read(branch) & BRANCH_HW_CLOCK_GATING != 0 {
                    Ok(())
                } else {
                    wait_for(self.core, branch, BRANCH_OFF, 0, CLOCK_TIMEOUT_US)
                        .map_err(|_| ClkError::HardwareError)
                }
            }
            _ => Err(ClkError::ClockNotFound),
        }
    }

    fn disable_clock(&self, id: u32) {
        let branch = match id {
            LPASS_AUDIO_CORE_EXT_MCLK0_CLK => Some(EXT_MCLK0_BRANCH),
            LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => Some(LPAIF_PRI_BRANCH),
            LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => Some(LPAIF_SEC_BRANCH),
            LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK => Some(SYSNOC_MPORT_BRANCH),
            _ => None,
        };
        if let Some(branch) = branch {
            self.core.update(branch, BRANCH_ENABLE, 0);
            arch::io_wmb();
        }
    }

    fn is_clock_enabled(&self, id: u32) -> bool {
        match id {
            LPASS_LPAAUDIO_DIG_PLL | LPASS_LPAAUDIO_DIG_PLL_OUT_ODD => {
                self.core.read(PLL_BASE + PLL_MODE) & (PLL_OUTCTRL | PLL_LOCK_DET)
                    == (PLL_OUTCTRL | PLL_LOCK_DET)
            }
            LPASS_AUDIO_CORE_EXT_MCLK0_CLK => self.core.read(EXT_MCLK0_BRANCH) & 1 != 0,
            LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => self.core.read(LPAIF_PRI_BRANCH) & 1 != 0,
            LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => self.core.read(LPAIF_SEC_BRANCH) & 1 != 0,
            LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK => self.core.read(SYSNOC_MPORT_BRANCH) & 1 != 0,
            _ => true,
        }
    }
}

struct LpassClock {
    controller: Arc<LpassCcController>,
    id: u32,
}

impl Clk for LpassClock {
    fn name(&self) -> &'static str {
        match self.id {
            LPASS_LPAAUDIO_DIG_PLL => "lpass_lpaaudio_dig_pll",
            LPASS_LPAAUDIO_DIG_PLL_OUT_ODD => "lpass_lpaaudio_dig_pll_out_odd",
            CORE_CLK_SRC => "core_clk_src",
            EXT_MCLK0_CLK_SRC => "ext_mclk0_clk_src",
            LPAIF_PRI_CLK_SRC => "lpaif_pri_clk_src",
            LPAIF_SEC_CLK_SRC => "lpaif_sec_clk_src",
            LPASS_AUDIO_CORE_CORE_CLK => "lpass_audio_core_core_clk",
            LPASS_AUDIO_CORE_EXT_MCLK0_CLK => "lpass_audio_core_ext_mclk0_clk",
            LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK => "lpass_audio_core_lpaif_pri_ibit_clk",
            LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => "lpass_audio_core_lpaif_sec_ibit_clk",
            LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK => "lpass_audio_core_sysnoc_mport_core_clk",
            _ => "lpass_unknown_clk",
        }
    }

    fn enable(&self) -> Result<(), ClkError> {
        self.controller.enable_clock(self.id)
    }

    fn disable(&self) {
        self.controller.disable_clock(self.id);
    }

    fn is_enabled(&self) -> bool {
        self.controller.is_clock_enabled(self.id)
    }

    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        self.controller.rate(self.id)
    }

    fn round_rate(&self, rate: u64, _parent_rate: u64) -> Result<u64, ClkError> {
        match self.id {
            EXT_MCLK0_CLK_SRC | LPASS_AUDIO_CORE_EXT_MCLK0_CLK => match rate {
                9_600_000 | 19_200_000 => Ok(rate),
                _ => Err(ClkError::InvalidRate),
            },
            LPAIF_PRI_CLK_SRC
            | LPAIF_SEC_CLK_SRC
            | LPASS_AUDIO_CORE_LPAIF_PRI_IBIT_CLK
            | LPASS_AUDIO_CORE_LPAIF_SEC_IBIT_CLK => LPAIF_RATES
                .iter()
                .find(|entry| entry.rate == rate)
                .map(|entry| entry.rate)
                .ok_or(ClkError::InvalidRate),
            _ if self.controller.rate(self.id) == rate => Ok(rate),
            _ => Err(ClkError::InvalidRate),
        }
    }

    fn set_rate(&self, rate: u64, parent_rate: u64) -> Result<u64, ClkError> {
        self.round_rate(rate, parent_rate)?;
        self.controller.set_audio_rate(self.id, rate)
    }
}

struct LpassClockProvider {
    clocks: Vec<ClkHandle>,
}

impl LpassClockProvider {
    fn new(controller: &Arc<LpassCcController>) -> Self {
        let clocks = (LPASS_LPAAUDIO_DIG_PLL..=LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK)
            .map(|id| {
                ClkHandle::new(Arc::new(LpassClock {
                    controller: Arc::clone(controller),
                    id,
                }))
            })
            .collect();
        Self { clocks }
    }
}

impl ClkProvider for LpassClockProvider {
    fn name(&self) -> &'static str {
        "qcom-sc7180-lpasscc"
    }

    fn clock_cells(&self) -> usize {
        1
    }

    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id] = spec else {
            return Err(ClkError::InvalidSpecifier);
        };
        self.clocks
            .get(*id as usize)
            .cloned()
            .ok_or(ClkError::ClockNotFound)
    }
}

struct LpassAudioDomain {
    controller: Arc<LpassCcController>,
    id: u32,
}

impl PowerDomain for LpassAudioDomain {
    fn enable(&self) -> Result<(), &'static str> {
        let offset = match self.id {
            LPASS_AUDIO_HM_GDSCR => AUDIO_HM_GDSCR,
            LPASS_PDC_HM_GDSCR => AUDIO_PDC_GDSCR,
            _ => return Err("qcom-sc7180-lpasscc: invalid audio domain"),
        };
        let _guard = self.controller.gdsc_lock.lock();
        enable_gdsc(self.controller.audio, offset, false)
    }

    fn disable(&self) -> Result<(), &'static str> {
        let (offset, votable) = match self.id {
            LPASS_AUDIO_HM_GDSCR => (AUDIO_HM_GDSCR, false),
            LPASS_PDC_HM_GDSCR => (AUDIO_PDC_GDSCR, true),
            _ => return Err("qcom-sc7180-lpasscc: invalid audio domain"),
        };
        let _guard = self.controller.gdsc_lock.lock();
        disable_gdsc(self.controller.audio, offset, votable)
    }

    fn is_enabled(&self) -> bool {
        let offset = match self.id {
            LPASS_AUDIO_HM_GDSCR => AUDIO_HM_GDSCR,
            LPASS_PDC_HM_GDSCR => AUDIO_PDC_GDSCR,
            _ => return false,
        };
        self.controller.audio.read(offset) & GDSC_POWER_ON != 0
    }

    fn label(&self) -> &str {
        match self.id {
            LPASS_AUDIO_HM_GDSCR => "sc7180-lpass-audio-hm",
            LPASS_PDC_HM_GDSCR => "sc7180-lpass-pdc-hm",
            _ => "sc7180-lpass-invalid",
        }
    }
}

struct LpassAudioDomainProvider {
    controller: Arc<LpassCcController>,
}

impl PowerDomainProvider for LpassAudioDomainProvider {
    fn power_domain_cells(&self) -> usize {
        1
    }

    fn get_domain(&self, specifier: &[u32]) -> Result<Arc<dyn PowerDomain>, &'static str> {
        let [id @ (LPASS_AUDIO_HM_GDSCR | LPASS_PDC_HM_GDSCR)] = specifier else {
            return Err("qcom-sc7180-lpasscc: invalid audio domain specifier");
        };
        Ok(Arc::new(LpassAudioDomain {
            controller: Arc::clone(&self.controller),
            id: *id,
        }))
    }
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-sc7180-lpasscc: missing phandle")
}

fn require_parent_power_domain(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let property = device
        .property("power-domains")
        .ok_or("qcom-sc7180-lpasscc: parent power domain is missing")?;
    let bytes = property.value();
    if bytes.len() < 8 || bytes.len() % 4 != 0 {
        return Err("qcom-sc7180-lpasscc: malformed parent power domain");
    }
    let phandle = u32::from_be_bytes(
        bytes[0..4]
            .try_into()
            .map_err(|_| "qcom-sc7180-lpasscc: malformed parent phandle")?,
    );
    let domain_id = u32::from_be_bytes(
        bytes[4..8]
            .try_into()
            .map_err(|_| "qcom-sc7180-lpasscc: malformed parent domain id")?,
    );
    if !PowerManager::has_provider(phandle) {
        early_println!(
            "[qcom-sc7180-lpasscc] waiting for parent power provider phandle={:#x}",
            phandle,
        );
        return probe_defer();
    }
    let domain = PowerManager::resolve_domain(phandle, &[domain_id])?;
    if !domain.is_enabled() {
        early_println!(
            "[qcom-sc7180-lpasscc] waiting for parent domain '{}' vote",
            domain.label(),
        );
        return probe_defer();
    }
    Ok(())
}

fn resolve_parent_clocks(
    device: &PlatformDeviceInfo,
) -> Result<(EnabledClock, EnabledClock), &'static str> {
    let manager = DeviceManager::get_manager();
    let iface = match manager.resolve_clk(device, "iface") {
        Err("clk: provider not found") | Err("clk: clock not found") => return probe_defer(),
        result => result?,
    };
    let xo = match manager.resolve_clk(device, "bi_tcxo") {
        Err("clk: provider not found") | Err("clk: clock not found") => return probe_defer(),
        result => result?,
    };
    let iface = EnabledClock::acquire(iface)
        .map_err(|_| "qcom-sc7180-lpasscc: failed to enable interface clock")?;
    let xo =
        EnabledClock::acquire(xo).map_err(|_| "qcom-sc7180-lpasscc: failed to enable XO clock")?;
    Ok((iface, xo))
}

fn map_resource(
    device: &PlatformDeviceInfo,
    index: usize,
    minimum_size: usize,
) -> Result<(MmioMapping, usize), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .nth(index)
        .ok_or("qcom-sc7180-lpasscc: missing MMIO resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|span| span.checked_add(1))
        .ok_or("qcom-sc7180-lpasscc: invalid MMIO resource")?;
    if size < minimum_size {
        return Err("qcom-sc7180-lpasscc: MMIO resource is too small");
    }
    let base = vm::ioremap(resource.start, minimum_size)
        .map_err(|_| "qcom-sc7180-lpasscc: ioremap failed")?;
    Ok((MmioMapping { base }, resource.start))
}

fn probe_lpass_hm(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    PowerManager::init();
    let (iface, xo) = resolve_parent_clocks(device)?;
    let (mapping, paddr) = map_resource(device, 0, LPASS_HM_WINDOW_SIZE)?;
    let phandle = read_phandle(device)?;
    let controller = Arc::new(LpassHmController {
        registers: RegisterWindow::new(mapping.base),
        _mapping: mapping,
        _iface: iface,
        _xo: xo,
        lock: IrqSpinLock::new(()),
    });
    initialize_gdsc(controller.registers, 0, true);
    PowerManager::register_provider(
        phandle,
        Arc::new(LpassHmDomainProvider {
            controller: Arc::clone(&controller),
        }),
    );
    early_println!(
        "[qcom-sc7180-lpasscc] LPASS_HM registered phandle={:#x} paddr={:#x} gdscr={:#010x}",
        phandle,
        paddr,
        controller.registers.read(0),
    );
    Ok(())
}

fn probe_lpass_cc(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    PowerManager::init();
    // The controller's MMIO windows are inside LPASS_CORE_HM.  The provider
    // node follows this node in the CoachZ firmware, so the first probe pass
    // must defer rather than relying on the boot loader's power handoff.
    require_parent_power_domain(device)?;
    let (iface, xo) = resolve_parent_clocks(device)?;
    let (core_mapping, core_paddr) = map_resource(device, 0, LPASS_CORE_WINDOW_SIZE)?;
    let (audio_mapping, audio_paddr) = map_resource(device, 1, LPASS_AUDIO_WINDOW_SIZE)?;
    let phandle = read_phandle(device)?;
    let controller = Arc::new(LpassCcController {
        core: RegisterWindow::new(core_mapping.base),
        audio: RegisterWindow::new(audio_mapping.base),
        _core_mapping: core_mapping,
        _audio_mapping: audio_mapping,
        _iface: iface,
        _xo: xo,
        state: IrqSpinLock::new(LpassClockState {
            ext_mclk_rate: XO_RATE,
            pri_bit_rate: DEFAULT_BIT_CLOCK,
            sec_bit_rate: DEFAULT_BIT_CLOCK,
        }),
        gdsc_lock: IrqSpinLock::new(()),
    });
    controller.initialize();
    DeviceManager::get_manager()
        .register_clk_provider(phandle, Arc::new(LpassClockProvider::new(&controller)));
    PowerManager::register_provider(
        phandle,
        Arc::new(LpassAudioDomainProvider {
            controller: Arc::clone(&controller),
        }),
    );
    early_println!(
        "[qcom-sc7180-lpasscc] LPASS_CORE_CC registered phandle={:#x} core={:#x} audio={:#x} pll={:#010x}/{:#010x}",
        phandle,
        core_paddr,
        audio_paddr,
        controller.core.read(PLL_BASE + PLL_MODE),
        controller.core.read(PLL_BASE + PLL_STATUS),
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_drivers() {
    let manager = DeviceManager::get_manager();
    manager.register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-lpasshm",
            probe_lpass_hm,
            remove,
            vec!["qcom,sc7180-lpasshm"],
        )),
        DriverPriority::Critical,
    );
    manager.register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-lpasscorecc",
            probe_lpass_cc,
            remove,
            vec!["qcom,sc7180-lpasscorecc"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_drivers);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_LPASSCC_ANCHOR: fn() = force_link;

/// Keep the external SC7180 LPASS clock driver linked into module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_i2s_48khz_clock_matches_linux_table() {
        let entry = LPAIF_RATES
            .iter()
            .find(|entry| entry.rate == DEFAULT_BIT_CLOCK)
            .unwrap();
        assert_eq!(entry.pre_div, 19);
        assert_eq!(entry.m, 1);
        assert_eq!(entry.n, 8);
        assert_eq!(entry.source, 5);
    }

    #[test]
    fn binding_ids_are_contiguous() {
        assert_eq!(LPASS_LPAAUDIO_DIG_PLL, 0);
        assert_eq!(LPASS_AUDIO_CORE_SYSNOC_MPORT_CORE_CLK, 10);
        assert_eq!(LPASS_CORE_HM_GDSCR, 0);
    }
}
