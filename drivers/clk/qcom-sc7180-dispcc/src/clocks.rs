// SPDX-License-Identifier: GPL-2.0-only
//! DISP_CC clocks needed by the native DP controller and firmware assignments.
use super::*;
use scarlet::device::clk::{Clk, ClkError, ClkFlags, ClkHandle, ClkProvider};
struct DisplayClock {
    controller: Arc<Sc7180DispCc>,
    id: u32,
    parent: IrqSpinLock<Option<ClkHandle>>,
}
impl DisplayClock {
    fn branch(&self) -> Option<usize> {
        match self.id {
            2 => Some(AHB_BRANCH),
            8 => Some(0x2054),
            12 => Some(0x2040),
            15 => Some(0x2044),
            16 => Some(0x204c),
            _ => None,
        }
    }
    fn root(&self) -> Option<usize> {
        match self.id {
            3 => Some(AHB_ROOT),
            9 => Some(0x21dc),
            13 => Some(0x2178),
            17 => Some(0x21ac),
            _ => None,
        }
    }
}
impl Clk for DisplayClock {
    fn name(&self) -> &'static str {
        match self.id {
            2 => "dispcc-mdss-ahb",
            3 => "dispcc-mdss-ahb-src",
            8 => "dispcc-dp-aux",
            9 => "dispcc-dp-aux-src",
            12 => "dispcc-dp-link",
            13 => "dispcc-dp-link-src",
            14 => "dispcc-dp-link-div",
            15 => "dispcc-dp-link-intf",
            16 => "dispcc-dp-pixel",
            17 => "dispcc-dp-pixel-src",
            _ => "dispcc-unknown",
        }
    }
    fn flags(&self) -> ClkFlags {
        if matches!(self.id, 2 | 3) {
            ClkFlags::IS_CRITICAL
        } else {
            ClkFlags::NONE
        }
    }
    fn enable(&self) -> Result<(), ClkError> {
        if let Some(branch) = self.branch() {
            let _guard = self.controller.lock.lock();
            self.controller
                .enable_branch(branch)
                .map_err(|_| ClkError::HardwareError)?;
        }
        Ok(())
    }
    fn disable(&self) {
        if self.id == 2 {
            return;
        }
        if let Some(branch) = self.branch() {
            let _guard = self.controller.lock.lock();
            self.controller.registers.update(branch, BRANCH_ENABLE, 0);
        }
    }
    fn is_enabled(&self) -> bool {
        self.branch()
            .is_none_or(|b| self.controller.branch_is_running(b))
    }
    fn parent(&self) -> Option<ClkHandle> {
        self.parent.lock().clone()
    }
    fn set_parent(&self, parent: ClkHandle) -> Result<(), ClkError> {
        let source = match (self.id, parent.name()) {
            (13, "sc7180-dp-link") => 1,
            (17, "sc7180-dp-vco-div") => 2,
            _ => return Err(ClkError::InvalidParent),
        };
        let root = self.root().ok_or(ClkError::InvalidParent)?;
        let _guard = self.controller.lock.lock();
        self.controller.registers.update(
            root + ROOT_CONFIG,
            7 << ROOT_SOURCE_SHIFT,
            source << ROOT_SOURCE_SHIFT,
        );
        self.controller
            .registers
            .update(root + ROOT_COMMAND, 0, ROOT_UPDATE);
        *self.parent.lock() = Some(parent);
        Ok(())
    }
    fn recalc_rate(&self, parent_rate: u64) -> u64 {
        if self.id == 14 {
            return parent_rate / u64::from((self.controller.registers.read(0x2190) & 3) + 1);
        }
        let Some(root) = self.root() else {
            return parent_rate;
        };
        let cfg = self.controller.registers.read(root + ROOT_CONFIG);
        let mut rate = if parent_rate != 0 {
            parent_rate
        } else if self.id == 3 {
            600_000_000
        } else {
            19_200_000
        };
        let hid = cfg & 31;
        if hid != 0 {
            rate = rate.saturating_mul(2) / u64::from(hid + 1);
        }
        if cfg & ROOT_DUAL_EDGE_MODE != 0 {
            let m = self.controller.registers.read(root + ROOT_M) & 0xffff;
            let n = ((!self.controller.registers.read(root + ROOT_N)) & 0xffff) + m;
            if n != 0 {
                rate = rate.saturating_mul(u64::from(m)) / u64::from(n);
            }
        }
        rate
    }
}
struct Provider(Vec<(u32, ClkHandle)>);
impl ClkProvider for Provider {
    fn name(&self) -> &'static str {
        "sc7180-dispcc"
    }
    fn clock_cells(&self) -> usize {
        1
    }
    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id] = spec else {
            return Err(ClkError::InvalidSpecifier);
        };
        self.0
            .iter()
            .find(|(n, _)| n == id)
            .map(|(_, c)| c.clone())
            .ok_or(ClkError::ClockNotFound)
    }
}
pub(super) fn register(controller: Arc<Sc7180DispCc>) {
    let mut clocks: Vec<(u32, ClkHandle)> = Vec::new();
    for (id, parent_id) in [
        (3, None),
        (2, Some(3)),
        (9, None),
        (8, Some(9)),
        (13, None),
        (12, Some(13)),
        (14, Some(13)),
        (15, Some(14)),
        (17, None),
        (16, Some(17)),
    ] {
        let parent = parent_id.and_then(|id| {
            clocks
                .iter()
                .find(|(n, _)| *n == id)
                .map(|(_, c)| c.clone())
        });
        clocks.push((
            id,
            ClkHandle::new(Arc::new(DisplayClock {
                controller: controller.clone(),
                id,
                parent: IrqSpinLock::new(parent),
            })),
        ));
    }
    DeviceManager::get_manager()
        .register_clk_provider(controller.phandle(), Arc::new(Provider(clocks)));
}
