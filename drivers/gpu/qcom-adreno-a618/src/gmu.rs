// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 legacy GMU bring-up for the Adreno A618.

use alloc::{sync::Arc, vec::Vec};

use scarlet::{
    arch,
    device::{
        clk::ClkHandle,
        iommu::{DmaContext, IommuDomainConfig, IommuDomainType},
        manager::{DeviceManager, probe_defer},
        platform::{PlatformDeviceInfo, resource::PlatformDeviceResourceType},
    },
    early_println,
    sync::{IrqSpinLock, Mutex},
    time, vm,
};

use crate::{
    firmware,
    hfi::{HfiPowerTable, LegacyHfi},
    hfi_abi::HfiPerfLevel,
    memory::{DmaAllocation, bidirectional_flags},
    opp::{OperatingPoint, read_gmu_operating_points},
    registers::*,
};

const GMU_IOVA_BASE: u64 = 0x6000_0000;
const GMU_IOVA_SIZE: u64 = 0x1fff_f000;
const DUMMY_SIZE: usize = 0x1000;
const DEBUG_SIZE: usize = 0x4000;
const LOG_SIZE: usize = 0x4000;
const GMU_FIRMWARE_MAX_SIZE: usize = 0x8000;
const REGISTER_TIMEOUT_US: u64 = 10_000;

const OOB_GPU_SET_REQUEST: u32 = 16;
const OOB_GPU_SET_ACK: u32 = 24;
const OOB_BOOT_REQUEST: u32 = 22;
const OOB_BOOT_ACK: u32 = 30;
const OOB_DCVS_REQUEST: u32 = 23;
const OOB_DCVS_ACK: u32 = 31;

const RPMH_ENABLES: u32 = 1 | (1 << 4) | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11);

struct MmioMapping {
    base: usize,
}

struct EnabledClocks(Vec<ClkHandle>);

impl Drop for EnabledClocks {
    fn drop(&mut self) {
        for clock in self.0.iter().rev() {
            clock.disable_unprepare();
        }
    }
}

impl MmioMapping {
    fn new(paddr: usize, size: usize, error: &'static str) -> Result<Self, &'static str> {
        Ok(Self {
            base: vm::ioremap(paddr, size).map_err(|_| error)?,
        })
    }
}

impl Drop for MmioMapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

pub(crate) struct A618Gmu {
    registers: GmuRegisters,
    rscc: DwordRegisters,
    pdc: DwordRegisters,
    pdc_sequence: DwordRegisters,
    dma_context: DmaContext,
    _clocks: EnabledClocks,
    _dummy: DmaAllocation,
    debug: DmaAllocation,
    log: DmaAllocation,
    hfi: LegacyHfi,
    gmu_operating_points: Vec<OperatingPoint>,
    power: Option<HfiPowerTable>,
    current_gpu_index: Option<usize>,
    phandle: u32,
    ready: bool,
    active: bool,
    rsc_asleep: bool,
    _gmu_mapping: MmioMapping,
    _pdc_mapping: MmioMapping,
    _pdc_sequence_mapping: MmioMapping,
}

impl A618Gmu {
    #[allow(clippy::too_many_arguments)]
    fn new(
        gmu_mapping: MmioMapping,
        pdc_mapping: MmioMapping,
        pdc_sequence_mapping: MmioMapping,
        dma_context: DmaContext,
        clocks: EnabledClocks,
        gmu_operating_points: Vec<OperatingPoint>,
        phandle: u32,
    ) -> Result<Self, &'static str> {
        let dummy = DmaAllocation::new(&dma_context, DUMMY_SIZE, bidirectional_flags())?;
        let debug = DmaAllocation::new(&dma_context, DEBUG_SIZE, bidirectional_flags())?;
        let log = DmaAllocation::new(&dma_context, LOG_SIZE, bidirectional_flags())?;
        let hfi = LegacyHfi::new(&dma_context)?;
        if dummy.dma_addr() != 0x6000_0000
            || debug.dma_addr() != 0x6000_1000
            || log.dma_addr() != 0x6000_5000
            || hfi.dma_addr() != 0x6000_9000
        {
            return Err("qcom-adreno-a618: legacy GMU IOVA layout is not contiguous");
        }
        let result = Self {
            registers: GmuRegisters::new(gmu_mapping.base),
            rscc: DwordRegisters::new(gmu_mapping.base + RSCC_BYTE_OFFSET),
            pdc: DwordRegisters::new(pdc_mapping.base),
            pdc_sequence: DwordRegisters::new(pdc_sequence_mapping.base),
            dma_context,
            _clocks: clocks,
            _dummy: dummy,
            debug,
            log,
            hfi,
            gmu_operating_points,
            power: None,
            current_gpu_index: None,
            phandle,
            ready: false,
            active: false,
            rsc_asleep: false,
            _gmu_mapping: gmu_mapping,
            _pdc_mapping: pdc_mapping,
            _pdc_sequence_mapping: pdc_sequence_mapping,
        };
        result.initialize_rpmh();
        Ok(result)
    }

    pub(crate) fn phandle(&self) -> u32 {
        self.phandle
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// Install the GPU OPPs before the first lazy hardware bring-up.
    ///
    /// The GMU platform node is probed separately from the GPU node, so the
    /// two Linux OPP tables can only be joined after both devices exist.
    pub(crate) fn configure_gpu_operating_points(
        &mut self,
        gpu_operating_points: Vec<OperatingPoint>,
    ) -> Result<(), &'static str> {
        if self.active || self.ready {
            return Err("qcom-adreno-a618: cannot replace OPPs while the GMU is active");
        }
        let power = build_power_table(&gpu_operating_points, &self.gmu_operating_points)?;
        let minimum = power
            .gx_levels
            .get(1)
            .ok_or("qcom-adreno-a618: GPU OPP table has no active level")?;
        let maximum = power
            .gx_levels
            .last()
            .ok_or("qcom-adreno-a618: GPU OPP table is empty")?;
        early_println!(
            "[qcom-adreno-a618] OPP table gpu-levels={} range={}..{} kHz gmu-levels={} initial-index={} dt-peak={} kB/s",
            power.gx_levels.len(),
            minimum.frequency_khz,
            maximum.frequency_khz,
            power.cx_levels.len(),
            power.initial_gpu_index,
            power.peak_kbps.unwrap_or(0),
        );
        self.power = Some(power);
        self.current_gpu_index = None;
        Ok(())
    }

    pub(crate) fn ensure_ready(&mut self) -> Result<(), &'static str> {
        if self.ready {
            return Ok(());
        }
        if self.active {
            return Err("qcom-adreno-a618: GMU is not quiesced after a prior failure");
        }
        early_println!("[qcom-adreno-a618] GMU bring-up begin");
        self.wake_rsc()?;
        self.dma_context
            .restore_iommu()
            .map_err(|_| "qcom-adreno-a618: failed to restore GMU IOMMU")?;
        let power = self
            .power
            .clone()
            .ok_or("qcom-adreno-a618: GPU OPP table was not configured")?;
        let firmware = firmware::load(firmware::GMU_FIRMWARE_PATH, GMU_FIRMWARE_MAX_SIZE)?;
        let initial = power
            .gx_levels
            .get(power.initial_gpu_index)
            .ok_or("qcom-adreno-a618: initial GPU OPP index is invalid")?;
        early_println!(
            "[qcom-adreno-a618] GMU inputs firmware={} gpu-levels={} gmu-levels={} initial={} kHz vote={:#x}",
            firmware.len(),
            power.gx_levels.len(),
            power.cx_levels.len(),
            initial.frequency_khz,
            initial.vote,
        );

        self.active = true;
        let boot = (|| {
            self.registers.write(GMU_AO_HOST_INTERRUPT_CLR, u32::MAX);
            self.registers.write(GMU_AO_HOST_INTERRUPT_MASK, u32::MAX);
            self.registers.write(GMU_GMU2HOST_INTR_CLR, u32::MAX);
            self.registers.write(GMU_GMU2HOST_INTR_MASK, u32::MAX);
            self.registers.write(GMU_GENERAL_7, 1);
            self.registers.write_bulk(GMU_CM3_ITCM_START, &firmware);
            arch::io_wmb();

            self.registers.write(GMU_CM3_FW_INIT_RESULT, 0);
            self.registers.write(GMU_CM3_BOOT_CONFIG, 2);
            self.registers.write(GMU_HFI_QTBL_ADDR, self.hfi.dma_addr());
            self.registers.write(GMU_HFI_QTBL_INFO, 1);
            self.registers
                .write(GMU_AHB_FENCE_RANGE_0, (1 << 31) | (0x0a << 18) | 0x0a0);
            self.registers.write(GMU_CM3_CFG, 0x4052);
            self.registers.write(GMU_HFI_SFR_ADDR, gmu_chip_id());
            self.registers.write(
                GMU_PWR_COL_CP_MSG,
                self.log.dma_addr() as u32 | (self.log.allocation_size() as u32 / 0x1000 - 1),
            );
            self.configure_power();
            self.start_cm3()?;
            early_println!(
                "[qcom-adreno-a618] GMU CM3 ready init={:#010x}",
                self.registers.read(GMU_CM3_FW_INIT_RESULT),
            );
            self.enable_gfx_rail(initial.vote)?;
            early_println!("[qcom-adreno-a618] GMU GX rail acknowledged");
            self.enable_sptprac()?;
            early_println!(
                "[qcom-adreno-a618] GMU SPTPRAC ready status={:#010x}",
                self.registers.read(GMU_SPTPRAC_PWR_CLK_STATUS),
            );
            self.start_hfi_transport()?;
            early_println!(
                "[qcom-adreno-a618] GMU HFI transport ready status={:#010x}",
                self.registers.read(GMU_HFI_CTRL_STATUS),
            );
            // Publish HFI CTRL status and queue-table contents before the
            // first host-to-GMU interrupt.
            arch::io_wmb();
            self.hfi.start_legacy_sequence(
                self.registers,
                self.debug.dma_addr() as u32,
                self.debug.allocation_size() as u32,
                &power,
            )?;
            self.set_performance_index(power.initial_gpu_index)?;
            self.registers
                .update(GMU_POWER_COUNTER_SELECT_0, 0xff, 1 << 5);
            self.registers.write(GMU_POWER_COUNTER_ENABLE, 1);
            Ok::<(), &'static str>(())
        })();
        if let Err(error) = boot {
            early_println!("[a618] {}", error);
            early_println!(
                "[a618] GMU state init={:#010x} hfi={:#010x}",
                self.registers.read(GMU_CM3_FW_INIT_RESULT),
                self.registers.read(GMU_HFI_CTRL_STATUS),
            );
            early_println!(
                "[a618] GMU intr={:#010x} sptprac={:#010x}",
                self.registers.read(GMU_GMU2HOST_INTR_INFO),
                self.registers.read(GMU_SPTPRAC_PWR_CLK_STATUS),
            );
            if self.force_shutdown().is_err() {
                return Err("qcom-adreno-a618: failed to quiesce GMU after bring-up error");
            }
            return Err(error);
        }
        self.ready = true;
        let frequency_khz = self
            .current_gpu_index
            .and_then(|index| self.power.as_ref()?.gx_levels.get(index))
            .map(|level| level.frequency_khz)
            .unwrap_or(0);
        early_println!(
            "[qcom-adreno-a618] GMU ready hfi={:#x} debug={:#x} log={:#x} gpu={} kHz",
            self.hfi.dma_addr(),
            self.debug.dma_addr(),
            self.log.dma_addr(),
            frequency_khz,
        );
        Ok(())
    }

    pub(crate) fn begin_gpu_boot(&self) -> Result<(), &'static str> {
        self.set_oob(OOB_GPU_SET_REQUEST, OOB_GPU_SET_ACK)
    }

    pub(crate) fn finish_gpu_boot(&self) {
        self.clear_oob(OOB_GPU_SET_ACK);
        self.clear_oob(OOB_BOOT_ACK);
    }

    pub(crate) fn finish_initial_boot_keep_gpu_on(&self) {
        self.clear_oob(OOB_BOOT_ACK);
    }

    pub(crate) fn release_gpu(&self) {
        self.clear_oob(OOB_GPU_SET_ACK);
    }

    /// Idempotently force the legacy GMU and its DMA engines off.
    pub(crate) fn force_shutdown(&mut self) -> Result<(), &'static str> {
        if !self.active {
            self.ready = false;
            return Ok(());
        }
        let mut quiesced = true;
        self.registers.write(GMU_PWR_COL_KEEPALIVE, 0);
        self.hfi.stop();
        self.registers.write(GMU_HFI_CTRL_INIT, 0);
        self.registers.write(GMU_AO_HOST_INTERRUPT_MASK, u32::MAX);
        self.registers.write(GMU_GMU2HOST_INTR_MASK, u32::MAX);
        self.registers.write(GMU_AO_HOST_INTERRUPT_CLR, u32::MAX);
        self.registers.write(GMU_GMU2HOST_INTR_CLR, u32::MAX);
        self.registers.write(GMU_POWER_COUNTER_ENABLE, 0);

        // Legacy A618 requires CPU-controlled SPTP collapse.
        self.registers.update(GPU_CC_GX_GDSCR, 0, 1 << 11);
        self.registers.write(GMU_GX_SPTPRAC_POWER_CONTROL, 0x778001);
        quiesced &= poll_register(
            self.registers,
            GMU_SPTPRAC_PWR_CLK_STATUS,
            |value| value & 0x04 != 0,
            "qcom-adreno-a618: SPTPRAC power-off timed out",
        )
        .is_ok();

        for status in [
            RSCC_TCS0_DRV0_STATUS,
            RSCC_TCS1_DRV0_STATUS,
            RSCC_TCS2_DRV0_STATUS,
            RSCC_TCS3_DRV0_STATUS,
        ] {
            quiesced &= poll_dword_register(self.rscc, status, |value| value & 1 != 0).is_ok();
        }
        self.registers.write(GMU_AHB_FENCE_STATUS_CLR, 0x7);
        self.registers.write(GMU_AO_AHB_FENCE_CTRL, 0);
        self.clear_oob(OOB_GPU_SET_ACK);
        self.clear_oob(OOB_BOOT_ACK);
        arch::io_wmb();
        self.registers.write(GMU_CM3_SYSRESET, 1);
        self.registers.write(GMU_RSCC_CONTROL_REQ, 1);
        let rsc_asleep = poll_dword_register(self.rscc, RSCC_RSC_STATUS0_DRV0, |value| {
            value & (1 << 16) != 0
        })
        .is_ok();
        quiesced &= rsc_asleep;
        self.registers.write(GMU_RSCC_CONTROL_REQ, 0);
        arch::io_wmb();
        self.ready = false;
        if quiesced {
            self.rsc_asleep = rsc_asleep;
            self.active = false;
            Ok(())
        } else {
            Err("qcom-adreno-a618: GMU force-off did not quiesce all power domains")
        }
    }

    fn initialize_rpmh(&self) {
        self.rscc.write(RSCC_RSC_STATUS0_DRV0, 1 << 24);
        self.rscc.write(RSCC_PDC_SLAVE_ID_DRV0, 1);
        for offset in [0, 2, 4] {
            self.rscc.write(RSCC_HIDDEN_TCS_CMD0_DATA + offset, 0);
            self.rscc.write(RSCC_HIDDEN_TCS_CMD0_ADDR + offset, 0);
        }
        self.rscc.write(RSCC_OVERRIDE_START_ADDR, 0);
        self.rscc.write(RSCC_PDC_SEQ_START_ADDR, 0x4520);
        self.rscc.write(RSCC_PDC_MATCH_VALUE_LO, 0x4510);
        self.rscc.write(RSCC_PDC_MATCH_VALUE_HI, 0x4514);
        for (index, word) in [
            0xa7a5_06a0,
            0xa1e6_a6e7,
            0xa2e0_81e1,
            0xe9a9_82e2,
            0x0020_e8a8,
        ]
        .into_iter()
        .enumerate()
        {
            self.rscc.write(RSCC_SEQ_MEM_0_DRV0 + index, word);
        }
        for (index, word) in [
            0xfebe_a1e1,
            0xa5a4_a3a2,
            0x8382_a6e0,
            0xbce3_e284,
            0x0020_81fc,
        ]
        .into_iter()
        .enumerate()
        {
            self.pdc_sequence.write(PDC_GPU_SEQ_MEM_0 + index, word);
        }

        self.pdc.write(PDC_GPU_TCS1_CMD_ENABLE_BANK, 7);
        self.pdc.write(PDC_GPU_TCS1_CMD_WAIT_FOR_CMPL_BANK, 0);
        self.pdc.write(PDC_GPU_TCS1_CONTROL, 0);
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS1_CMD0_MSGID,
            PDC_GPU_TCS1_CMD0_ADDR,
            PDC_GPU_TCS1_CMD0_DATA,
            0,
            0x10108,
            0x30010,
            1,
        );
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS1_CMD0_MSGID,
            PDC_GPU_TCS1_CMD0_ADDR,
            PDC_GPU_TCS1_CMD0_DATA,
            1,
            0x10108,
            0x30000,
            0,
        );
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS1_CMD0_MSGID,
            PDC_GPU_TCS1_CMD0_ADDR,
            PDC_GPU_TCS1_CMD0_DATA,
            2,
            0x10108,
            0x30090,
            0,
        );

        self.pdc.write(PDC_GPU_TCS3_CMD_ENABLE_BANK, 7);
        self.pdc.write(PDC_GPU_TCS3_CMD_WAIT_FOR_CMPL_BANK, 0);
        self.pdc.write(PDC_GPU_TCS3_CONTROL, 0);
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS3_CMD0_MSGID,
            PDC_GPU_TCS3_CMD0_ADDR,
            PDC_GPU_TCS3_CMD0_DATA,
            0,
            0x10108,
            0x30010,
            2,
        );
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS3_CMD0_MSGID,
            PDC_GPU_TCS3_CMD0_ADDR,
            PDC_GPU_TCS3_CMD0_DATA,
            1,
            0x10108,
            0x30000,
            2,
        );
        write_pdc_command(
            self.pdc,
            PDC_GPU_TCS3_CMD0_MSGID,
            PDC_GPU_TCS3_CMD0_ADDR,
            PDC_GPU_TCS3_CMD0_DATA,
            2,
            0x10108,
            0x30090,
            3,
        );
        self.pdc.write(PDC_GPU_SEQ_START_ADDR, 0);
        self.pdc.write(PDC_GPU_ENABLE_PDC, 0x8000_0001);
        arch::io_wmb();
    }

    fn wake_rsc(&mut self) -> Result<(), &'static str> {
        if !self.rsc_asleep {
            return Ok(());
        }
        self.registers.write(GMU_RSCC_CONTROL_REQ, 1 << 1);
        let result = (|| {
            poll_register(
                self.registers,
                GMU_RSCC_CONTROL_ACK,
                |value| value & (1 << 1) != 0,
                "qcom-adreno-a618: GMU RSC wake acknowledgement timed out",
            )?;
            poll_dword_register(self.rscc, RSCC_SEQ_BUSY_DRV0, |value| value == 0)
                .map_err(|_| "qcom-adreno-a618: GMU RSC wake sequence timed out")
        })();
        self.registers.write(GMU_RSCC_CONTROL_REQ, 0);
        arch::io_wmb();
        result?;
        self.rsc_asleep = false;
        early_println!("[qcom-adreno-a618] GMU RSC wake complete");
        Ok(())
    }

    fn configure_power(&self) {
        self.registers.write(GMU_SYS_BUS_CONFIG, 1);
        self.registers.write(GMU_ICACHE_CONFIG, 1);
        self.registers.write(GMU_DCACHE_CONFIG, 1);
        self.registers
            .write(GMU_PWR_COL_INTER_FRAME_CTRL, 0x09c4_0400);
        self.registers.update(GMU_RPMH_CTRL, 0, RPMH_ENABLES);
    }

    fn start_cm3(&self) -> Result<(), &'static str> {
        let version = self.registers.read(GMU_CM3_DTCM_START + 0xff8);
        let (mask, expected) = if version <= 0x2001_0004 {
            (u32::MAX, 0xbabe_face)
        } else {
            (0x1ff, 0x100)
        };
        self.registers.write(GMU_CM3_SYSRESET, 1);
        self.registers.write(GMU_PWR_COL_CP_RESP, 0);
        self.registers.write(GMU_CM3_SYSRESET, 0);
        poll_register(
            self.registers,
            GMU_CM3_FW_INIT_RESULT,
            |value| value & mask == expected,
            "qcom-adreno-a618: GMU firmware initialization timed out",
        )
    }

    fn enable_gfx_rail(&self, vote: u32) -> Result<(), &'static str> {
        self.registers.write(GMU_BOOT_SLUMBER_OPTION, 0);
        self.registers.write(GMU_GX_VOTE_IDX, vote & 0xff);
        self.registers.write(GMU_MX_VOTE_IDX, (vote >> 8) & 0xff);
        self.set_oob(OOB_BOOT_REQUEST, OOB_BOOT_ACK)
    }

    fn enable_sptprac(&self) -> Result<(), &'static str> {
        self.registers.write(GMU_GX_SPTPRAC_POWER_CONTROL, 0x778000);
        let start = time::current_time();
        loop {
            let status = self.registers.read(GMU_SPTPRAC_PWR_CLK_STATUS);
            if status & 0x38 == 0x28 {
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= 100 {
                return Err("qcom-adreno-a618: SPTPRAC power-on timed out");
            }
            time::udelay(1);
        }
    }

    fn start_hfi_transport(&self) -> Result<(), &'static str> {
        self.registers.write(GMU_HFI_CTRL_INIT, 1);
        poll_register(
            self.registers,
            GMU_HFI_CTRL_STATUS,
            |value| value & 1 != 0,
            "qcom-adreno-a618: HFI transport failed to start",
        )
    }

    fn set_performance_index(&mut self, index: usize) -> Result<(), &'static str> {
        let level_count = self
            .power
            .as_ref()
            .map(|power| power.gx_levels.len())
            .ok_or("qcom-adreno-a618: GPU OPP table was not configured")?;
        if index == 0 || index >= level_count {
            return Err("qcom-adreno-a618: GPU performance index is out of range");
        }
        if !self.active {
            return Err("qcom-adreno-a618: GMU is inactive during frequency change");
        }
        let index_u32 = u32::try_from(index)
            .map_err(|_| "qcom-adreno-a618: GPU performance index exceeds register width")?;
        self.registers.write(GMU_DCVS_ACK_OPTION, 0);
        self.registers
            .write(GMU_DCVS_PERF_SETTING, (3 << 28) | index_u32);
        self.registers.write(GMU_DCVS_BW_SETTING, 0xff);
        self.set_oob(OOB_DCVS_REQUEST, OOB_DCVS_ACK)?;
        self.clear_oob(OOB_DCVS_ACK);
        if self.registers.read(GMU_DCVS_RETURN) != 0 {
            return Err("qcom-adreno-a618: GMU rejected frequency vote");
        }
        self.current_gpu_index = Some(index);
        Ok(())
    }

    fn set_oob(&self, request: u32, acknowledgement: u32) -> Result<(), &'static str> {
        self.registers.write(GMU_HOST2GMU_INTR_SET, 1u32 << request);
        let start = time::current_time();
        loop {
            let info = self.registers.read(GMU_GMU2HOST_INTR_INFO);
            if info & (1u32 << acknowledgement) != 0 {
                self.registers
                    .write(GMU_GMU2HOST_INTR_CLR, 1u32 << acknowledgement);
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= REGISTER_TIMEOUT_US {
                return Err("qcom-adreno-a618: GMU OOB acknowledgement timed out");
            }
            time::udelay(10);
        }
    }

    fn clear_oob(&self, acknowledgement: u32) {
        self.registers
            .write(GMU_HOST2GMU_INTR_SET, 1u32 << acknowledgement);
    }
}

impl Drop for A618Gmu {
    fn drop(&mut self) {
        // This runs before the owned HFI/debug/log allocations, MMIO mappings,
        // and clocks are dropped.
        if self.force_shutdown().is_err() {
            early_println!(
                "[qcom-adreno-a618] refusing unsafe GMU DMA/MMIO teardown after timeout"
            );
            loop {
                time::udelay(1_000_000);
            }
        }
    }
}

fn write_pdc_command(
    pdc: DwordRegisters,
    message_base: usize,
    address_base: usize,
    data_base: usize,
    command: usize,
    message: u32,
    address: u32,
    data: u32,
) {
    let offset = command * 4;
    pdc.write(message_base + offset, message);
    pdc.write(address_base + offset, address);
    pdc.write(data_base + offset, data);
}

fn poll_register(
    registers: GmuRegisters,
    register: usize,
    predicate: impl Fn(u32) -> bool,
    error: &'static str,
) -> Result<(), &'static str> {
    let start = time::current_time();
    loop {
        if predicate(registers.read(register)) {
            return Ok(());
        }
        if time::current_time().saturating_sub(start) >= REGISTER_TIMEOUT_US {
            return Err(error);
        }
        time::udelay(10);
    }
}

fn poll_dword_register(
    registers: DwordRegisters,
    register: usize,
    predicate: impl Fn(u32) -> bool,
) -> Result<(), &'static str> {
    let start = time::current_time();
    loop {
        if predicate(registers.read(register)) {
            return Ok(());
        }
        if time::current_time().saturating_sub(start) >= REGISTER_TIMEOUT_US {
            return Err("qcom-adreno-a618: GMU power sequence timed out");
        }
        time::udelay(10);
    }
}

fn gmu_chip_id() -> u32 {
    let chip_id = 0x0601_0800u32;
    (chip_id & 0xffff_0000) | ((chip_id << 4) & 0xf000) | ((chip_id << 8) & 0x0f00)
}

fn build_hfi_levels(
    operating_points: &[OperatingPoint],
    primary_id: &str,
) -> Result<Vec<HfiPerfLevel>, &'static str> {
    let mut levels = Vec::with_capacity(operating_points.len() + 1);
    levels.push(HfiPerfLevel {
        vote: build_vote(0, primary_id, "mx.lvl")?,
        frequency_khz: 0,
    });
    for point in operating_points {
        levels.push(HfiPerfLevel {
            vote: build_vote(point.level, primary_id, "mx.lvl")?,
            frequency_khz: point.frequency_khz,
        });
    }
    Ok(levels)
}

fn build_power_table(
    gpu_operating_points: &[OperatingPoint],
    gmu_operating_points: &[OperatingPoint],
) -> Result<HfiPowerTable, &'static str> {
    let gx_levels = build_hfi_levels(gpu_operating_points, "gfx.lvl")?;
    let initial_gpu_index = gx_levels
        .len()
        .checked_sub(1)
        .filter(|index| *index != 0)
        .ok_or("qcom-adreno-a618: GPU OPP table has no active level")?;
    Ok(HfiPowerTable {
        gx_levels,
        cx_levels: build_hfi_levels(gmu_operating_points, "cx.lvl")?,
        initial_gpu_index,
        peak_kbps: gpu_operating_points
            .iter()
            .filter_map(|point| point.peak_kbps)
            .max(),
    })
}

fn build_vote(level: u16, primary_id: &str, secondary_id: &str) -> Result<u32, &'static str> {
    let primary = scarlet_driver_qcom_cmd_db::read_aux_u16(primary_id)
        .ok_or("qcom-adreno-a618: Command DB primary ARC table is unavailable")?;
    let secondary = scarlet_driver_qcom_cmd_db::read_aux_u16(secondary_id)
        .ok_or("qcom-adreno-a618: Command DB secondary ARC table is unavailable")?;
    let primary_index = primary
        .iter()
        .position(|value| *value >= level)
        .ok_or("qcom-adreno-a618: requested ARC level is unavailable")?;
    let mut secondary_index = 0usize;
    for (index, value) in secondary.iter().enumerate() {
        if *value >= level {
            secondary_index = index;
            break;
        }
        if *value != 0 {
            secondary_index = index;
        }
    }
    let primary_index = u8::try_from(primary_index)
        .map_err(|_| "qcom-adreno-a618: primary ARC table is too large")?;
    let secondary_index = u8::try_from(secondary_index)
        .map_err(|_| "qcom-adreno-a618: secondary ARC table is too large")?;
    Ok((u32::from(primary[usize::from(primary_index)]) << 16)
        | (u32::from(secondary_index) << 8)
        | u32::from(primary_index))
}

static GMUS: IrqSpinLock<Vec<Arc<Mutex<A618Gmu>>>> = IrqSpinLock::new(Vec::new());

pub(crate) fn get_by_phandle(phandle: u32) -> Option<Arc<Mutex<A618Gmu>>> {
    GMUS.lock()
        .iter()
        .find(|gmu| {
            gmu.try_lock()
                .is_some_and(|guard| guard.phandle() == phandle)
        })
        .cloned()
}

pub(crate) fn remove(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = read_phandle(device)?;
    let mut gmus = GMUS.lock();
    let index = gmus
        .iter()
        .position(|gmu| {
            gmu.try_lock()
                .is_some_and(|guard| guard.phandle() == phandle)
        })
        .ok_or("qcom-adreno-a618: GMU was not registered")?;
    gmus.swap_remove(index);
    Ok(())
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-adreno-a618: GMU is missing its phandle")
}

fn gpucc_phandle(device: &PlatformDeviceInfo) -> Option<u32> {
    let bytes = device.property("power-domains")?.value();
    let first: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(first))
}

fn enable_clocks(device: &PlatformDeviceInfo) -> Result<EnabledClocks, &'static str> {
    let manager = DeviceManager::get_manager();
    let mut clocks = EnabledClocks(Vec::new());
    for name in ["gmu", "cxo", "axi", "memnoc"] {
        let clock = manager.resolve_clk(device, name)?;
        if clock.prepare_enable().is_err() {
            return Err("qcom-adreno-a618: failed to enable a GMU clock");
        }
        clocks.0.push(clock);
    }
    Ok(clocks)
}

pub(crate) fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let gmu_operating_points = read_gmu_operating_points(device)?;
    let gpucc = match gpucc_phandle(device)
        .and_then(scarlet_driver_qcom_sc7180_gpucc::get_sc7180_gpucc_by_phandle)
    {
        Some(gpucc) => gpucc,
        None => return probe_defer(),
    };
    gpucc.prepare_for_gmu()?;
    let clocks = enable_clocks(device)?;
    let memory: Vec<_> = device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .collect();
    let [gmu_resource, pdc_resource, pdc_sequence_resource, ..] = memory.as_slice() else {
        return Err("qcom-adreno-a618: GMU register resources are incomplete");
    };
    let resource_size =
        |resource: &&scarlet::device::platform::resource::PlatformDeviceResource| {
            resource
                .end
                .checked_sub(resource.start)
                .and_then(|size| size.checked_add(1))
        };
    if resource_size(gmu_resource).is_none_or(|size| size < GMU_RESOURCE_SIZE)
        || resource_size(pdc_resource).is_none_or(|size| size < PDC_RESOURCE_SIZE)
        || resource_size(pdc_sequence_resource).is_none_or(|size| size < PDC_SEQ_RESOURCE_SIZE)
    {
        return Err("qcom-adreno-a618: GMU register resource is too small");
    }
    let gmu_mapping = MmioMapping::new(
        gmu_resource.start,
        GMU_RESOURCE_SIZE,
        "qcom-adreno-a618: failed to map GMU registers",
    )?;
    let pdc_mapping = MmioMapping::new(
        pdc_resource.start,
        PDC_RESOURCE_SIZE,
        "qcom-adreno-a618: failed to map PDC registers",
    )?;
    let pdc_sequence_mapping = MmioMapping::new(
        pdc_sequence_resource.start,
        PDC_SEQ_RESOURCE_SIZE,
        "qcom-adreno-a618: failed to map PDC sequence memory",
    )?;
    let dma_context = DeviceManager::get_manager().resolve_platform_dma_context(
        device,
        IommuDomainConfig {
            domain_type: IommuDomainType::Dma,
            iova_base: GMU_IOVA_BASE,
            iova_size: GMU_IOVA_SIZE,
        },
    )?;
    let phandle = read_phandle(device)?;
    let gmu = A618Gmu::new(
        gmu_mapping,
        pdc_mapping,
        pdc_sequence_mapping,
        dma_context,
        clocks,
        gmu_operating_points,
        phandle,
    )?;
    GMUS.lock().push(Arc::new(Mutex::new(gmu)));
    early_println!(
        "[qcom-adreno-a618] registered legacy GMU phandle={:#x} paddr={:#x}",
        phandle,
        gmu_resource.start,
    );
    Ok(())
}
