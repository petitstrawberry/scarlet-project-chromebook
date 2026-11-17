// SPDX-License-Identifier: GPL-2.0-only
//! Read-only PLL outputs. The PHY power/configure operations own PLL changes.
use super::*;
use scarlet::device::clk::{Clk, ClkError, ClkHandle, ClkProvider};
struct Output {
    combo: Arc<Sc7180QmpUsb3>,
    index: u32,
}
impl Clk for Output {
    fn name(&self) -> &'static str {
        match self.index {
            0 => "sc7180-usb3-pipe",
            1 => "sc7180-dp-link",
            _ => "sc7180-dp-vco-div",
        }
    }
    fn enable(&self) -> Result<(), ClkError> {
        if self.is_enabled() {
            Ok(())
        } else {
            Err(ClkError::Busy)
        }
    }
    fn disable(&self) {} // Shared PLL gates belong to the PHY's power lifetime.
    fn is_enabled(&self) -> bool {
        let state = self.combo.state.lock();
        if self.index == 0 {
            state.powered
        } else {
            state.dp_powered
        }
    }
    fn recalc_rate(&self, _parent_rate: u64) -> u64 {
        if self.index == 0 {
            return 125_000_000;
        }
        let rate = u64::from(self.combo.state.lock().dp_config.link_rate_mbps);
        if self.index == 1 {
            rate * 100_000
        } else {
            rate * 1_000_000 / if rate == 5400 { 4 } else { 2 }
        }
    }
}
struct Provider([ClkHandle; 3]);
impl ClkProvider for Provider {
    fn name(&self) -> &'static str {
        "sc7180-qmp-combo"
    }
    fn clock_cells(&self) -> usize {
        1
    }
    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id] = spec else {
            return Err(ClkError::InvalidSpecifier);
        };
        self.0
            .get(*id as usize)
            .cloned()
            .ok_or(ClkError::ClockNotFound)
    }
}
pub(super) fn register(phandle: u32, combo: Arc<Sc7180QmpUsb3>) {
    let outputs = core::array::from_fn(|index| {
        ClkHandle::new(Arc::new(Output {
            combo: combo.clone(),
            index: index as u32,
        }))
    });
    DeviceManager::get_manager().register_clk_provider(phandle, Arc::new(Provider(outputs)));
}
