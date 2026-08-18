// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 display clock roots consumed by DSI0.

use scarlet::time;

use crate::registers::RegisterWindow;

const PCLK0_BRANCH: usize = 0x2004;
const BYTE0_BRANCH: usize = 0x2028;
const BYTE0_INTERFACE_BRANCH: usize = 0x202c;
const ESC0_BRANCH: usize = 0x2038;

const PCLK0_ROOT: usize = 0x2098;
const BYTE0_ROOT: usize = 0x2110;
const ESC0_ROOT: usize = 0x2148;

const ROOT_COMMAND: usize = 0x0;
const ROOT_CONFIG: usize = 0x4;
const ROOT_M: usize = 0x8;
const ROOT_N: usize = 0xc;
const ROOT_D2: usize = 0x10;

const ROOT_UPDATE: u32 = 1;
const ROOT_SOURCE_SHIFT: u32 = 8;
const ROOT_DIVIDER_SHIFT: u32 = 0;
const ROOT_DUAL_EDGE_MODE: u32 = 2 << 12;
const ROOT_MND_MASK: u32 = 0xffff;

const BRANCH_ENABLE: u32 = 1;
const BRANCH_OFF: u32 = 1 << 31;
const BRANCH_TIMEOUT_US: u64 = 100;

pub(crate) struct DisplayClocks {
    registers: RegisterWindow,
}

impl DisplayClocks {
    pub(crate) const fn new(registers: RegisterWindow) -> Self {
        Self { registers }
    }

    fn configure_root(&self, root: usize, source: u32, divider: u32, m: u32, n: u32, d2: u32) {
        let divider = if divider == 0 { 0 } else { divider * 2 - 1 };
        self.registers.write(
            root + ROOT_CONFIG,
            (source << ROOT_SOURCE_SHIFT) | (divider << ROOT_DIVIDER_SHIFT),
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
                return Err("qcom-sc7180-mdss: display clock failed to start");
            }
            time::udelay(1);
        }
        Ok(())
    }

    /// Select the DSI PHY external byte/pixel clock outputs and start all DSI0
    /// branch clocks. Firmware keeps the MDSS AHB/core domains powered.
    pub(crate) fn enable_dsi0(&self) -> Result<(), &'static str> {
        self.configure_root(ESC0_ROOT, 0, 0, 0, 0, 0);
        self.configure_root(PCLK0_ROOT, 1, 0, 0, 0, 0);
        self.configure_root(BYTE0_ROOT, 1, 0, 0, 0, 0);
        // BYTE0 and BYTE0_INTF share the same RCG. The interface branch uses
        // the divided form selected by the final configuration.
        self.configure_root(BYTE0_ROOT, 1, 1, 0, 0, 0);

        self.enable_branch(ESC0_BRANCH)?;
        self.enable_branch(PCLK0_BRANCH)?;
        self.enable_branch(BYTE0_BRANCH)?;
        self.enable_branch(BYTE0_INTERFACE_BRANCH)
    }
}
