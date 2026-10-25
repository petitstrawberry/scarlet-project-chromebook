//! SDCC5 voltage switching and CM DLL tuning (Linux sdhci-msm.c).
use super::*;

const DLL_STATUS: usize = 0x208;
const DLL_CONFIG2: usize = 0x254;
const CK_OUT: u32 = 1 << 18;
const DLL_EN: u32 = 1 << 16;
const CDR_EN: u32 = 1 << 17;
const CDR_EXT: u32 = 1 << 19;
const DAT_MASK: u32 = 0xf << 20;
const TUNING: [u8; 64] = [
    0xff, 0x0f, 0xff, 0x00, 0xff, 0xcc, 0xc3, 0xcc, 0xc3, 0x3c, 0xcc, 0xff, 0xfe, 0xff, 0xfe, 0xef,
    0xff, 0xdf, 0xff, 0xdd, 0xff, 0xfb, 0xff, 0xfb, 0xbf, 0xff, 0x7f, 0xff, 0x77, 0xf7, 0xbd, 0xef,
    0xff, 0xf0, 0xff, 0xf0, 0x0f, 0xfc, 0xcc, 0x3c, 0xcc, 0x33, 0xcc, 0xcf, 0xff, 0xef, 0xff, 0xee,
    0xff, 0xfd, 0xff, 0xfd, 0xdf, 0xff, 0xbf, 0xff, 0xbb, 0xff, 0xf7, 0xff, 0xf7, 0x7f, 0x7b, 0xde,
];

impl Sc7180SdhciHost {
    pub(super) fn configure_command_cdr(&self, command: u8, read: bool) {
        if self.timing == MmcTiming::Sdr104 {
            let config = read32(self.mmio_base, CORE_DLL_CONFIG);
            let next = command_cdr_config(config, command, read);
            if next != config {
                write32(self.mmio_base, CORE_DLL_CONFIG, next);
            }
        }
    }

    fn wait_bits(&self, offset: usize, mask: u32, expected: u32) -> MmcResult<()> {
        for _ in 0..1000 {
            if read32(self.mmio_base, offset) & mask == expected {
                return Ok(());
            }
            scarlet::time::udelay(1);
        }
        Err(MmcError::Timeout)
    }

    pub(super) fn switch_1v8(&mut self) -> MmcResult<()> {
        // CMD11 drives all DAT lines low before the host stops SDCLK.
        self.wait_bits(SDHCI_PRESENT_STATE, DAT_MASK, 0)?;
        self.host.set_clock(0)?;
        let ctrl = read16(self.mmio_base, SDHCI_HOST_CONTROL2) | 8;
        write16(self.mmio_base, SDHCI_HOST_CONTROL2, ctrl);
        let supplies = self.supplies.as_ref().ok_or(MmcError::Unsupported)?;
        supplies.set_io(true).map_err(|_| MmcError::Command)?;
        let vendor = read32(self.mmio_base, CORE_VENDOR_SPEC) | CORE_IO_PAD_PWR_SWITCH_MASK;
        write32(self.mmio_base, CORE_VENDOR_SPEC, vendor);
        // Acknowledge only after the requested rail and pad transition.
        let pending = read32(self.mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK;
        if pending & !4 != 0 {
            return Err(MmcError::Command);
        }
        write32(self.mmio_base, CORE_PWRCTL_CLEAR, pending);
        self.wait_bits(CORE_PWRCTL_STATUS, pending, 0)?;
        write32(self.mmio_base, CORE_PWRCTL_CTL, 1 << 2);
        scarlet::time::udelay(5_000);
        if read16(self.mmio_base, SDHCI_HOST_CONTROL2) & 8 == 0 {
            return Err(MmcError::Command);
        }
        self.host.set_clock(400_000)?;
        scarlet::time::udelay(1_000);
        self.wait_bits(SDHCI_PRESENT_STATE, DAT_MASK, DAT_MASK)?;
        println!("[qcom-sdhci-sc7180] SD signaling switched to 1.8 V");
        Ok(())
    }

    fn init_dll(&self) -> MmcResult<()> {
        let base = self.mmio_base;
        write32(
            base,
            CORE_VENDOR_SPEC,
            read32(base, CORE_VENDOR_SPEC) & !(1 << 1),
        );
        let version = read32(base, CORE_MCI_VERSION);
        let modern = version >> 28 == 1 && version & 0xff >= 0x42;
        if modern {
            write32(
                base,
                CORE_DLL_CONFIG,
                read32(base, CORE_DLL_CONFIG) & !CK_OUT,
            );
            write32(base, DLL_CONFIG2, read32(base, DLL_CONFIG2) | (1 << 21));
        }
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) | CORE_DLL_RST,
        );
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) | CORE_DLL_PDN,
        );
        // Linux's CM DLL frequency selector wraps to 0 above 200 MHz.
        let rate = self.host.base_clock_hz();
        let selector = match rate {
            0..=112_000_000 => 0,
            112_000_001..=125_000_000 => 1,
            125_000_001..=137_000_000 => 2,
            137_000_001..=150_000_000 => 3,
            150_000_001..=162_000_000 => 4,
            162_000_001..=175_000_000 => 5,
            175_000_001..=187_000_000 => 6,
            187_000_001..=200_000_000 => 7,
            _ => 0,
        };
        write32(
            base,
            CORE_DLL_CONFIG,
            (read32(base, CORE_DLL_CONFIG) & !(7 << 24)) | (selector << 24),
        );
        if modern {
            let config = read32(base, DLL_CONFIG2);
            let multiplier = if config & (1 << 18) != 0 { 8 } else { 4 };
            let xo = self.clocks[2].rate();
            if xo == 0 {
                return Err(MmcError::Unsupported);
            }
            let cycles = (u64::from(rate) * multiplier + xo / 2) / xo;
            if cycles > 255 {
                return Err(MmcError::Unsupported);
            }
            write32(
                base,
                DLL_CONFIG2,
                (config & !(255 << 10)) | ((cycles as u32) << 10),
            );
            scarlet::time::udelay(5);
        }
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) & !CORE_DLL_RST,
        );
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) & !CORE_DLL_PDN,
        );
        if modern {
            write32(base, DLL_CONFIG2, read32(base, DLL_CONFIG2) & !(1 << 21));
        }
        if version >> 28 == 1 && version & 0xff >= 0x71 {
            write32(base, 0x388, 0x10800 | (1 << 27) | (1 << 26) | (1 << 29));
            write32(
                base,
                0x258,
                (read32(base, 0x258) & !255) | if rate < 150_000_000 { 8 } else { 16 },
            );
        }
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) | DLL_EN | CK_OUT,
        );
        self.wait_bits(DLL_STATUS, 1 << 7, 1 << 7)
    }

    fn set_phase(&self, phase: u8) -> MmcResult<()> {
        let base = self.mmio_base;
        let config = (read32(base, CORE_DLL_CONFIG) & !(CDR_EN | CK_OUT)) | CDR_EXT | DLL_EN;
        write32(base, CORE_DLL_CONFIG, config);
        self.wait_bits(CORE_DLL_CONFIG, CK_OUT, 0)?;
        let gray = phase ^ (phase >> 1);
        write32(
            base,
            CORE_DLL_CONFIG,
            (config & !(15 << 20)) | (u32::from(gray) << 20),
        );
        write32(
            base,
            CORE_DLL_CONFIG,
            read32(base, CORE_DLL_CONFIG) | CK_OUT,
        );
        self.wait_bits(CORE_DLL_CONFIG, CK_OUT, CK_OUT)?;
        write32(
            base,
            CORE_DLL_CONFIG,
            (read32(base, CORE_DLL_CONFIG) | CDR_EN) & !CDR_EXT,
        );
        Ok(())
    }

    fn tuning_read(&mut self) -> MmcResult<()> {
        let mut data = [0u8; 64];
        self.configure_command_cdr(19, true);
        let response = self.host.send_command(
            MmcCommand::new(19, 0, scarlet::device::mmc::MmcResponseType::R1),
            Some(MmcData::Read(&mut data)),
        )?;
        if response.word(0) & 0xfff9_a080 != 0 || data != TUNING {
            return Err(MmcError::Data);
        }
        Ok(())
    }

    pub(super) fn tune_sd(&mut self) -> MmcResult<()> {
        // SDCC uses fixed feedback and no DLL at <=100 MHz.
        if self.timing != MmcTiming::Sdr104 {
            return Ok(());
        }
        for attempt in 0..10 {
            self.init_dll()?;
            let mut valid = 0u16;
            for phase in 0..16 {
                self.set_phase(phase)?;
                if self.tuning_read().is_ok() {
                    valid |= 1 << phase;
                }
            }
            println!(
                "[qcom-sdhci-sc7180] tuning pass={} valid={:#06x}",
                attempt + 1,
                valid
            );
            if valid == u16::MAX && attempt < 9 {
                continue;
            }
            let Some(phase) = select_tuning_phase(valid) else {
                continue;
            };
            self.set_phase(phase)?;
            if !(0..3).all(|_| self.tuning_read().is_ok()) {
                println!(
                    "[qcom-sdhci-sc7180] SDR104 phase={} failed validation; retrying tuning",
                    phase
                );
                continue;
            }
            self.last_tuning_ms = scarlet::time::current_time_ms();
            self.retune_needed = false;
            println!(
                "[qcom-sdhci-sc7180] SDR104 tuned: valid={:#06x} phase={}",
                valid, phase
            );
            return Ok(());
        }
        Err(MmcError::Data)
    }
}
