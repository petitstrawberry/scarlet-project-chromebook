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
    memory::{DmaAllocation, bidirectional_flags},
    registers::*,
};

const GMU_IOVA_BASE: u64 = 0x6000_0000;
const GMU_IOVA_SIZE: u64 = 0x1fff_f000;
const DUMMY_SIZE: usize = 0x1000;
const DEBUG_SIZE: usize = 0x4000;
const LOG_SIZE: usize = 0x4000;
const GMU_FIRMWARE_MAX_SIZE: usize = 0x8000;
const GPU_LEVEL: u16 = 0x30;
const GMU_LEVEL: u16 = 0x30;
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
    phandle: u32,
    ready: bool,
    active: bool,
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
            phandle,
            ready: false,
            active: false,
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

    pub(crate) fn ensure_ready(&mut self) -> Result<(), &'static str> {
        if self.ready {
            return Ok(());
        }
        self.dma_context
            .restore_iommu()
            .map_err(|_| "qcom-adreno-a618: failed to restore GMU IOMMU")?;
        let power = build_power_table()?;
        let firmware = firmware::load(firmware::GMU_FIRMWARE_PATH, GMU_FIRMWARE_MAX_SIZE)?;

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
            self.enable_gfx_rail(power.gx_votes[1])?;
            self.enable_sptprac()?;
            self.start_hfi_transport()?;
            // Publish HFI CTRL status and queue-table contents before the
            // first host-to-GMU interrupt.
            arch::io_wmb();
            self.hfi.start_legacy_sequence(
                self.registers,
                self.debug.dma_addr() as u32,
                self.debug.allocation_size() as u32,
                power,
            )?;
            self.set_frequency(1)?;
            self.registers
                .update(GMU_POWER_COUNTER_SELECT_0, 0xff, 1 << 5);
            self.registers.write(GMU_POWER_COUNTER_ENABLE, 1);
            Ok::<(), &'static str>(())
        })();
        if let Err(error) = boot {
            let _ = self.force_shutdown();
            return Err(error);
        }
        self.ready = true;
        early_println!(
            "[qcom-adreno-a618] GMU ready hfi={:#x} debug={:#x} log={:#x}",
            self.hfi.dma_addr(),
            self.debug.dma_addr(),
            self.log.dma_addr(),
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
        quiesced &= poll_dword_register(self.rscc, RSCC_RSC_STATUS0_DRV0, |value| {
            value & (1 << 16) != 0
        })
        .is_ok();
        self.registers.write(GMU_RSCC_CONTROL_REQ, 0);
        arch::io_wmb();
        self.ready = false;
        if quiesced {
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

    fn set_frequency(&self, index: u32) -> Result<(), &'static str> {
        self.registers.write(GMU_DCVS_ACK_OPTION, 0);
        self.registers
            .write(GMU_DCVS_PERF_SETTING, (3 << 28) | index);
        self.registers.write(GMU_DCVS_BW_SETTING, 0xff);
        self.set_oob(OOB_DCVS_REQUEST, OOB_DCVS_ACK)?;
        self.clear_oob(OOB_DCVS_ACK);
        if self.registers.read(GMU_DCVS_RETURN) != 0 {
            return Err("qcom-adreno-a618: GMU rejected initial frequency vote");
        }
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

fn build_power_table() -> Result<HfiPowerTable, &'static str> {
    let gx_secondary = if scarlet_driver_qcom_cmd_db::read_aux_u16("gmxc.lvl").is_some() {
        "gmxc.lvl"
    } else {
        "mx.lvl"
    };
    Ok(HfiPowerTable {
        gx_votes: [
            build_vote(0, "gfx.lvl", gx_secondary)?,
            build_vote(GPU_LEVEL, "gfx.lvl", gx_secondary)?,
        ],
        cx_votes: [
            build_vote(0, "cx.lvl", "mx.lvl")?,
            build_vote(GMU_LEVEL, "cx.lvl", "mx.lvl")?,
        ],
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
