// SPDX-License-Identifier: GPL-2.0-only
//! Read-only Type-C mux observations; EC firmware owns alternate-mode policy.
use super::{CrosEcError, CrosEcSpi};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrosEcTypecState {
    pub port: u8,
    pub usb_enabled: bool,
    pub displayport_enabled: bool,
    pub reverse: bool,
    pub hpd_high: bool,
    pub hpd_irq: bool,
    pub safe_mode: bool,
}

impl CrosEcSpi {
    pub fn typec_port_count(&self) -> Result<u8, CrosEcError> {
        let bytes = self.command(0x0102, 0, &[])?;
        match bytes.as_slice() {
            [ports] if *ports <= 8 => Ok(*ports),
            _ => Err(CrosEcError::InvalidResponse),
        }
    }

    pub fn typec_state(&self, port: u8) -> Result<CrosEcTypecState, CrosEcError> {
        if port >= 8 {
            return Err(CrosEcError::InvalidArgument);
        }
        let bytes = self.command(0x011a, 0, &[port])?;
        let [flags] = bytes.as_slice() else {
            return Err(CrosEcError::InvalidResponse);
        };
        Ok(CrosEcTypecState {
            port,
            usb_enabled: flags & 1 != 0,
            displayport_enabled: flags & 2 != 0,
            reverse: flags & 4 != 0,
            hpd_irq: flags & 8 != 0,
            hpd_high: flags & 16 != 0,
            safe_mode: flags & 32 != 0,
        })
    }
}
