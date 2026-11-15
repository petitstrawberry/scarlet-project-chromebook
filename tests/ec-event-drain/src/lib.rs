// Exercise the production EC queue policy without target-only kernel types.
#![allow(dead_code)]
extern crate self as scarlet;
pub mod device {
    pub mod power_supply {
        pub use scarlet_abi::power_supply::{ChargeState, PowerSupplyState};
    }
}
#[path = "../../../drivers/platform/chromeos/cros-ec-spi/src/event_drain.rs"]
mod event_drain;
#[path = "../../../drivers/platform/chromeos/cros-ec-spi/src/power_decode.rs"]
mod power_decode;
