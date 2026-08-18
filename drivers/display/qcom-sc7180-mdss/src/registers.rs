// SPDX-License-Identifier: GPL-2.0-only

use scarlet::arch::mmio;

/// Device-mapped 32-bit register window.
#[derive(Clone, Copy)]
pub(crate) struct RegisterWindow {
    base: usize,
}

impl RegisterWindow {
    pub(crate) const fn new(base: usize) -> Self {
        Self { base }
    }

    pub(crate) fn read(self, offset: usize) -> u32 {
        // SAFETY: constructors only receive an ioremap'd device window, and
        // every caller supplies an SC7180 register offset inside that window.
        unsafe { mmio::read32(self.base + offset) }
    }

    pub(crate) fn write(self, offset: usize, value: u32) {
        // SAFETY: constructors only receive an ioremap'd device window, and
        // every caller supplies an SC7180 register offset inside that window.
        unsafe { mmio::write32(self.base + offset, value) }
    }

    pub(crate) fn update(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }
}
