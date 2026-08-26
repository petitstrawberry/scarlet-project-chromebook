// SPDX-License-Identifier: GPL-2.0-only

//! A618 register offsets used by the project-local backend.
//!
//! GPU offsets are dword offsets from `gpu@5000000`. GMU offsets in Qualcomm's
//! XML are dword offsets in the complete GPU aperture; [`GmuRegisters`]
//! translates them to the `gmu@506a000` resource before touching MMIO.

use scarlet::arch::mmio;

pub(crate) const GPU_RESOURCE_SIZE: usize = 0x40000;
pub(crate) const GMU_RESOURCE_SIZE: usize = 0x31000;
pub(crate) const PDC_RESOURCE_SIZE: usize = 0x10000;
pub(crate) const PDC_SEQ_RESOURCE_SIZE: usize = 0x10000;

pub(crate) const GMU_GPU_DWORD_BASE: usize = 0x1a800;
pub(crate) const RSCC_BYTE_OFFSET: usize = 0x23000;

#[derive(Clone, Copy)]
pub(crate) struct DwordRegisters {
    base: usize,
}

impl DwordRegisters {
    pub(crate) const fn new(base: usize) -> Self {
        Self { base }
    }

    pub(crate) fn read(self, register: usize) -> u32 {
        // SAFETY: probe maps the complete firmware resource and every caller
        // uses a register constant validated to lie inside that resource.
        unsafe { mmio::read32(self.base + register * 4) }
    }

    pub(crate) fn write(self, register: usize, value: u32) {
        // SAFETY: see `read`; writes target the same mapped resource.
        unsafe { mmio::write32(self.base + register * 4, value) }
    }

    pub(crate) fn write64(self, register: usize, value: u64) {
        self.write(register, value as u32);
        self.write(register + 1, (value >> 32) as u32);
    }

    pub(crate) fn read64(self, register: usize) -> u64 {
        u64::from(self.read(register)) | (u64::from(self.read(register + 1)) << 32)
    }

    pub(crate) fn update(self, register: usize, clear: u32, set: u32) {
        self.write(register, (self.read(register) & !clear) | set);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GmuRegisters {
    resource: DwordRegisters,
}

impl GmuRegisters {
    pub(crate) const fn new(base: usize) -> Self {
        Self {
            resource: DwordRegisters::new(base),
        }
    }

    pub(crate) fn read(self, absolute_register: usize) -> u32 {
        self.resource.read(gmu_resource_register(absolute_register))
    }

    pub(crate) fn write(self, absolute_register: usize, value: u32) {
        self.resource
            .write(gmu_resource_register(absolute_register), value);
    }

    pub(crate) fn update(self, absolute_register: usize, clear: u32, set: u32) {
        self.write(
            absolute_register,
            (self.read(absolute_register) & !clear) | set,
        );
    }

    pub(crate) fn write_bulk(self, absolute_register: usize, bytes: &[u8]) {
        for (index, chunk) in bytes.chunks(4).enumerate() {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write(absolute_register + index, u32::from_le_bytes(word));
        }
    }
}

fn gmu_resource_register(absolute_register: usize) -> usize {
    let register = absolute_register
        .checked_sub(GMU_GPU_DWORD_BASE)
        .expect("GMU register precedes the mapped GMU resource");
    assert!(
        register < GMU_RESOURCE_SIZE / core::mem::size_of::<u32>(),
        "GMU register exceeds the mapped GMU resource"
    );
    register
}

// GMU register dword offsets, from current Mesa/Linux A6xx XML.
pub(crate) const GMU_GX_SPTPRAC_CLOCK_CONTROL: usize = 0x1a880;
pub(crate) const GMU_GX_SPTPRAC_POWER_CONTROL: usize = 0x1a881;
pub(crate) const GMU_CM3_ITCM_START: usize = 0x1b400;
pub(crate) const GMU_CM3_DTCM_START: usize = 0x1c400;
pub(crate) const GMU_BOOT_SLUMBER_OPTION: usize = 0x1cbf8;
pub(crate) const GMU_GX_VOTE_IDX: usize = 0x1cbf9;
pub(crate) const GMU_MX_VOTE_IDX: usize = 0x1cbfa;
pub(crate) const GMU_DCVS_ACK_OPTION: usize = 0x1cbfc;
pub(crate) const GMU_DCVS_PERF_SETTING: usize = 0x1cbfd;
pub(crate) const GMU_DCVS_BW_SETTING: usize = 0x1cbfe;
pub(crate) const GMU_DCVS_RETURN: usize = 0x1cbff;
pub(crate) const GMU_ICACHE_CONFIG: usize = 0x1f400;
pub(crate) const GMU_DCACHE_CONFIG: usize = 0x1f401;
pub(crate) const GMU_SYS_BUS_CONFIG: usize = 0x1f40f;
pub(crate) const GMU_CM3_SYSRESET: usize = 0x1f800;
pub(crate) const GMU_CM3_BOOT_CONFIG: usize = 0x1f801;
pub(crate) const GMU_CM3_FW_INIT_RESULT: usize = 0x1f81c;
pub(crate) const GMU_CM3_CFG: usize = 0x1f82d;
pub(crate) const GMU_POWER_COUNTER_SELECT_0: usize = 0x1f840;
pub(crate) const GMU_POWER_COUNTER_ENABLE: usize = 0x1f841;
pub(crate) const GMU_PWR_COL_INTER_FRAME_CTRL: usize = 0x1f8c0;
pub(crate) const GMU_SPTPRAC_PWR_CLK_STATUS: usize = 0x1f8d0;
pub(crate) const GMU_RPMH_CTRL: usize = 0x1f8e8;
pub(crate) const GMU_PWR_COL_CP_MSG: usize = 0x1f900;
pub(crate) const GMU_PWR_COL_CP_RESP: usize = 0x1f901;
pub(crate) const GMU_PWR_COL_KEEPALIVE: usize = 0x1f8c3;
pub(crate) const GMU_HFI_CTRL_STATUS: usize = 0x1f980;
pub(crate) const GMU_HFI_SFR_ADDR: usize = 0x1f982;
pub(crate) const GMU_HFI_QTBL_INFO: usize = 0x1f984;
pub(crate) const GMU_HFI_QTBL_ADDR: usize = 0x1f985;
pub(crate) const GMU_HFI_CTRL_INIT: usize = 0x1f986;
pub(crate) const GMU_GMU2HOST_INTR_CLR: usize = 0x1f991;
pub(crate) const GMU_GMU2HOST_INTR_INFO: usize = 0x1f992;
pub(crate) const GMU_GMU2HOST_INTR_MASK: usize = 0x1f993;
pub(crate) const GMU_HOST2GMU_INTR_SET: usize = 0x1f994;
pub(crate) const GMU_GENERAL_7: usize = 0x1f9cc;
pub(crate) const GMU_AO_HOST_INTERRUPT_CLR: usize = 0x23b04;
pub(crate) const GMU_AO_HOST_INTERRUPT_MASK: usize = 0x23b06;
pub(crate) const GMU_RSCC_CONTROL_REQ: usize = 0x23b07;
pub(crate) const GMU_RSCC_CONTROL_ACK: usize = 0x23b08;
pub(crate) const GMU_AO_GPU_CX_BUSY_MASK: usize = 0x23b0e;
pub(crate) const GMU_AHB_FENCE_RANGE_0: usize = 0x23b11;
pub(crate) const GMU_AO_AHB_FENCE_CTRL: usize = 0x23b10;
pub(crate) const GMU_AHB_FENCE_STATUS_CLR: usize = 0x23b14;
pub(crate) const GPU_CC_GX_GDSCR: usize = 0x24403;

// RSCC offsets relative to the RSCC sub-window, in dwords.
pub(crate) const RSCC_RSC_STATUS0_DRV0: usize = 0x004;
pub(crate) const RSCC_PDC_SEQ_START_ADDR: usize = 0x008;
pub(crate) const RSCC_PDC_MATCH_VALUE_LO: usize = 0x009;
pub(crate) const RSCC_PDC_MATCH_VALUE_HI: usize = 0x00a;
pub(crate) const RSCC_PDC_SLAVE_ID_DRV0: usize = 0x00b;
pub(crate) const RSCC_HIDDEN_TCS_CMD0_ADDR: usize = 0x00d;
pub(crate) const RSCC_HIDDEN_TCS_CMD0_DATA: usize = 0x00e;
pub(crate) const RSCC_OVERRIDE_START_ADDR: usize = 0x100;
pub(crate) const RSCC_SEQ_BUSY_DRV0: usize = 0x101;
pub(crate) const RSCC_TCS0_DRV0_STATUS: usize = 0x346;
pub(crate) const RSCC_TCS1_DRV0_STATUS: usize = 0x3ee;
pub(crate) const RSCC_TCS2_DRV0_STATUS: usize = 0x496;
pub(crate) const RSCC_TCS3_DRV0_STATUS: usize = 0x53e;
pub(crate) const RSCC_SEQ_MEM_0_DRV0: usize = 0x180;

// PDC offsets are dword offsets within their dedicated resources.
pub(crate) const PDC_GPU_SEQ_MEM_0: usize = 0x0000;
pub(crate) const PDC_GPU_ENABLE_PDC: usize = 0x1140;
pub(crate) const PDC_GPU_SEQ_START_ADDR: usize = 0x1148;
pub(crate) const PDC_GPU_TCS1_CONTROL: usize = 0x1572;
pub(crate) const PDC_GPU_TCS1_CMD_ENABLE_BANK: usize = 0x1573;
pub(crate) const PDC_GPU_TCS1_CMD_WAIT_FOR_CMPL_BANK: usize = 0x1574;
pub(crate) const PDC_GPU_TCS1_CMD0_MSGID: usize = 0x1575;
pub(crate) const PDC_GPU_TCS1_CMD0_ADDR: usize = 0x1576;
pub(crate) const PDC_GPU_TCS1_CMD0_DATA: usize = 0x1577;
pub(crate) const PDC_GPU_TCS3_CONTROL: usize = 0x15d6;
pub(crate) const PDC_GPU_TCS3_CMD_ENABLE_BANK: usize = 0x15d7;
pub(crate) const PDC_GPU_TCS3_CMD_WAIT_FOR_CMPL_BANK: usize = 0x15d8;
pub(crate) const PDC_GPU_TCS3_CMD0_MSGID: usize = 0x15d9;
pub(crate) const PDC_GPU_TCS3_CMD0_ADDR: usize = 0x15da;
pub(crate) const PDC_GPU_TCS3_CMD0_DATA: usize = 0x15db;

// GPU dword offsets.
pub(crate) const RBBM_INT_0_STATUS: usize = 0x0201;
pub(crate) const RBBM_INT_CLEAR_CMD: usize = 0x0037;
pub(crate) const RBBM_INT_0_MASK: usize = 0x0038;
// A6xx RBBM interrupt bits.  Timestamp/cache events share this status
// register with genuine execution faults, so callers must never treat every
// non-zero bit as a device loss.
pub(crate) const RBBM_INT_CP_AHB_ERROR: u32 = 1 << 1;
pub(crate) const RBBM_INT_CP_HW_ERROR: u32 = 1 << 9;
pub(crate) const RBBM_INT_CP_CCU_FLUSH_DEPTH_TS: u32 = 1 << 10;
pub(crate) const RBBM_INT_CP_CCU_FLUSH_COLOR_TS: u32 = 1 << 11;
pub(crate) const RBBM_INT_CP_CCU_RESOLVE_TS: u32 = 1 << 12;
pub(crate) const RBBM_INT_CP_RB_DONE_TS: u32 = 1 << 17;
pub(crate) const RBBM_INT_CP_WT_DONE_TS: u32 = 1 << 18;
pub(crate) const RBBM_INT_CP_CACHE_FLUSH_TS: u32 = 1 << 20;
pub(crate) const RBBM_INT_RBBM_HANG_DETECT: u32 = 1 << 23;
pub(crate) const RBBM_INT_UCHE_OOB_ACCESS: u32 = 1 << 24;
pub(crate) const RBBM_INT_UCHE_TRAP_INTR: u32 = 1 << 25;
pub(crate) const RBBM_INT_COMPLETION_MASK: u32 = RBBM_INT_CP_CCU_FLUSH_DEPTH_TS
    | RBBM_INT_CP_CCU_FLUSH_COLOR_TS
    | RBBM_INT_CP_CCU_RESOLVE_TS
    | RBBM_INT_CP_RB_DONE_TS
    | RBBM_INT_CP_WT_DONE_TS
    | RBBM_INT_CP_CACHE_FLUSH_TS;
pub(crate) const RBBM_INT_FATAL_MASK: u32 = RBBM_INT_CP_AHB_ERROR
    | RBBM_INT_CP_HW_ERROR
    | RBBM_INT_RBBM_HANG_DETECT
    | RBBM_INT_UCHE_OOB_ACCESS
    | RBBM_INT_UCHE_TRAP_INTR;
pub(crate) const RBBM_STATUS: usize = 0x0210;
pub(crate) const RBBM_STATUS1: usize = 0x0211;
pub(crate) const RBBM_STATUS2: usize = 0x0212;
pub(crate) const RBBM_STATUS3: usize = 0x0213;
pub(crate) const RBBM_STATUS3_SMMU_STALLED_ON_FAULT: u32 = 1 << 24;
/// Bit 0 in the upstream A6XX XML; it may remain set while CP queues are idle.
pub(crate) const RBBM_STATUS_CP_AHB_BUSY_CX_MASTER: u32 = 0x0000_0001;
pub(crate) const RBBM_SW_RESET_CMD: usize = 0x0043;
pub(crate) const RBBM_GBIF_HALT: usize = 0x0016;
pub(crate) const RBBM_GBIF_HALT_ACK: usize = 0x0017;
pub(crate) const GBIF_HALT: usize = 0x3c45;
pub(crate) const GBIF_HALT_ACK: usize = 0x3c46;
pub(crate) const RBBM_PERFCTR_CNTL: usize = 0x0500;
pub(crate) const RBBM_PERFCTR_GPU_BUSY_MASKED: usize = 0x050b;
pub(crate) const RBBM_VBIF_CLIENT_QOS_CNTL: usize = 0x0010;
pub(crate) const RBBM_INTERFACE_HANG_INT_CNTL: usize = 0x001f;
pub(crate) const CP_RB_BASE: usize = 0x0800;
pub(crate) const CP_RB_CNTL: usize = 0x0802;
pub(crate) const CP_RB_RPTR: usize = 0x0806;
pub(crate) const CP_RB_WPTR: usize = 0x0807;
pub(crate) const CP_SQE_CNTL: usize = 0x0808;
pub(crate) const CP_HW_FAULT: usize = 0x0821;
pub(crate) const CP_INTERRUPT_STATUS: usize = 0x0823;
pub(crate) const CP_INT_OPCODE_ERROR: u32 = 1 << 0;
pub(crate) const CP_INT_UCODE_ERROR: u32 = 1 << 1;
pub(crate) const CP_INT_HW_FAULT_ERROR: u32 = 1 << 2;
pub(crate) const CP_INT_REGISTER_PROTECTION_ERROR: u32 = 1 << 4;
pub(crate) const CP_INT_AHB_ERROR: u32 = 1 << 5;
pub(crate) const CP_INT_VSD_PARITY_ERROR: u32 = 1 << 6;
pub(crate) const CP_INT_ILLEGAL_INSTR_ERROR: u32 = 1 << 7;
pub(crate) const CP_INT_FATAL_MASK: u32 = CP_INT_OPCODE_ERROR
    | CP_INT_UCODE_ERROR
    | CP_INT_HW_FAULT_ERROR
    | CP_INT_REGISTER_PROTECTION_ERROR
    | CP_INT_AHB_ERROR
    | CP_INT_VSD_PARITY_ERROR
    | CP_INT_ILLEGAL_INSTR_ERROR;
pub(crate) const CP_PROTECT_STATUS: usize = 0x0824;
pub(crate) const CP_SQE_INSTR_BASE: usize = 0x0830;
pub(crate) const CP_ADDR_MODE_CNTL: usize = 0x0842;
pub(crate) const CP_SCRATCH_2: usize = 0x0885;
pub(crate) const CP_ROQ_THRESHOLDS_1: usize = 0x08c1;
pub(crate) const CP_ROQ_THRESHOLDS_2: usize = 0x08c2;
pub(crate) const CP_MEM_POOL_SIZE: usize = 0x08c3;
pub(crate) const CP_PERFCTR_CP_SEL_0: usize = 0x08d0;
pub(crate) const CP_AHB_CNTL: usize = 0x098d;
pub(crate) const CP_PROTECT_CNTL: usize = 0x084f;
pub(crate) const CP_PROTECT_BASE: usize = 0x0850;
pub(crate) const CP_IB1_BASE: usize = 0x0928;
pub(crate) const CP_IB1_REM_SIZE: usize = 0x092a;
pub(crate) const CP_IB2_BASE: usize = 0x092b;
pub(crate) const CP_IB2_REM_SIZE: usize = 0x092d;
pub(crate) const CP_ROQ_RB_STATUS: usize = 0x0939;
pub(crate) const CP_ROQ_IB1_STATUS: usize = 0x093a;
pub(crate) const CP_ROQ_IB2_STATUS: usize = 0x093b;
pub(crate) const CP_ROQ_SDS_STATUS: usize = 0x093c;
pub(crate) const CP_ROQ_MRB_STATUS: usize = 0x093d;
pub(crate) const CP_ROQ_VSD_STATUS: usize = 0x093e;

pub(crate) const UCHE_ADDR_MODE_CNTL: usize = 0x0e00;
pub(crate) const UCHE_WRITE_RANGE_MAX: usize = 0x0e05;
pub(crate) const UCHE_WRITE_THRU_BASE: usize = 0x0e07;
pub(crate) const UCHE_TRAP_BASE: usize = 0x0e09;
pub(crate) const UCHE_GMEM_RANGE_MIN: usize = 0x0e0b;
pub(crate) const UCHE_GMEM_RANGE_MAX: usize = 0x0e0d;
pub(crate) const UCHE_CACHE_WAYS: usize = 0x0e17;
pub(crate) const UCHE_FILTER_CNTL: usize = 0x0e18;
pub(crate) const UCHE_CLIENT_PF: usize = 0x0e19;
pub(crate) const UCHE_UNKNOWN_0E12: usize = 0x0e12;

pub(crate) const RBBM_SECVID_TRUST_CNTL: usize = 0xf400;
pub(crate) const RBBM_SECVID_TSB_TRUSTED_BASE: usize = 0xf800;
pub(crate) const RBBM_SECVID_TSB_TRUSTED_SIZE: usize = 0xf802;
pub(crate) const RBBM_SECVID_TSB_CNTL: usize = 0xf803;
pub(crate) const RBBM_SECVID_TSB_ADDR_MODE_CNTL: usize = 0xf810;

pub(crate) const VSC_ADDR_MODE_CNTL: usize = 0x0c01;
pub(crate) const GRAS_ADDR_MODE_CNTL: usize = 0x8601;
pub(crate) const GRAS_SC_CNTL: usize = 0x80a0;
pub(crate) const GRAS_DBG_ECO_CNTL: usize = 0x8600;
pub(crate) const PC_ADDR_MODE_CNTL: usize = 0x9e01;
pub(crate) const PC_MODE_CNTL: usize = 0x9804;
pub(crate) const PC_POWER_CNTL: usize = 0x9805;
pub(crate) const VFD_ADDR_MODE_CNTL: usize = 0xa601;
pub(crate) const VFD_MODE_CNTL: usize = 0xa009;
pub(crate) const VFD_POWER_CNTL: usize = 0xa0f8;
pub(crate) const VPC_ADDR_MODE_CNTL: usize = 0x9601;
pub(crate) const VPC_DBG_ECO_CNTL: usize = 0x9600;
pub(crate) const HLSQ_ADDR_MODE_CNTL: usize = 0xbe05;
pub(crate) const HLSQ_SHARED_CONSTS: usize = 0xbb11;
pub(crate) const HLSQ_UNKNOWN_BE00: usize = 0xbe00;
pub(crate) const HLSQ_UNKNOWN_BE01: usize = 0xbe01;
pub(crate) const HLSQ_DBG_ECO_CNTL: usize = 0xbe04;
pub(crate) const SP_ADDR_MODE_CNTL: usize = 0xae01;
pub(crate) const SP_DBG_ECO_CNTL: usize = 0xae00;
pub(crate) const SP_CHICKEN_BITS: usize = 0xae03;
pub(crate) const SP_GFX_USIZE: usize = 0xab20;
pub(crate) const SP_PERFCTR_SHADER_MASK: usize = 0xae0f;
pub(crate) const TPL1_ADDR_MODE_CNTL: usize = 0xb601;
pub(crate) const TPL1_DBG_ECO_CNTL: usize = 0xb600;
pub(crate) const TPL1_UNKNOWN_B605: usize = 0xb605;
pub(crate) const RB_ADDR_MODE_CNTL: usize = 0x8e05;
pub(crate) const RB_MODE_CNTL: usize = 0x8811;
pub(crate) const RB_RBP_CNTL: usize = 0x8e01;
pub(crate) const PC_DBG_ECO_CNTL: usize = 0x9e00;

pub(crate) const RB_A2D_BLT_CNTL: usize = 0x8c00;
pub(crate) const RB_A2D_PIXEL_CNTL: usize = 0x8c01;
pub(crate) const RB_A2D_DEST_BUFFER_INFO: usize = 0x8c17;
pub(crate) const RB_A2D_DEST_BUFFER_BASE: usize = 0x8c18;
pub(crate) const RB_A2D_DEST_BUFFER_PITCH: usize = 0x8c1a;
pub(crate) const RB_A2D_CLEAR_COLOR_DW0: usize = 0x8c2c;
pub(crate) const GRAS_A2D_BLT_CNTL: usize = 0x8400;
pub(crate) const GRAS_A2D_SRC_XMIN: usize = 0x8401;
pub(crate) const GRAS_A2D_SRC_XMAX: usize = 0x8402;
pub(crate) const GRAS_A2D_SRC_YMIN: usize = 0x8403;
pub(crate) const GRAS_A2D_SRC_YMAX: usize = 0x8404;
pub(crate) const GRAS_A2D_DEST_TL: usize = 0x8405;
pub(crate) const GRAS_A2D_DEST_BR: usize = 0x8406;
pub(crate) const GRAS_A2D_SCISSOR_TL: usize = 0x840a;
pub(crate) const GRAS_A2D_SCISSOR_BR: usize = 0x840b;
pub(crate) const RB_DBG_ECO_CNTL: usize = 0x8e04;
pub(crate) const RB_CCU_CNTL: usize = 0x8e07;
pub(crate) const SP_A2D_OUTPUT_INFO: usize = 0xacc0;
pub(crate) const TPL1_A2D_SRC_TEXTURE_INFO: usize = 0xb4c0;
pub(crate) const TPL1_A2D_SRC_TEXTURE_SIZE: usize = 0xb4c1;
pub(crate) const TPL1_A2D_SRC_TEXTURE_BASE: usize = 0xb4c2;
pub(crate) const TPL1_A2D_SRC_TEXTURE_PITCH: usize = 0xb4c4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_registers_resolve_inside_the_gmu_resource() {
        for register in [
            GMU_PWR_COL_KEEPALIVE,
            GMU_AO_AHB_FENCE_CTRL,
            GMU_AHB_FENCE_STATUS_CLR,
            GPU_CC_GX_GDSCR,
        ] {
            assert!(gmu_resource_register(register) < GMU_RESOURCE_SIZE / 4);
        }
        assert_eq!(gmu_resource_register(GPU_CC_GX_GDSCR) * 4, 0x2700c);
    }

    #[test]
    #[should_panic(expected = "GMU register precedes the mapped GMU resource")]
    fn rejects_gpu_relative_offsets_as_gmu_absolute_offsets() {
        let _ = gmu_resource_register(0x09c03);
    }

    #[test]
    fn timestamp_completion_interrupts_are_not_device_loss() {
        assert_ne!(RBBM_INT_COMPLETION_MASK & RBBM_INT_CP_CACHE_FLUSH_TS, 0);
        assert_ne!(RBBM_INT_COMPLETION_MASK & RBBM_INT_CP_CCU_FLUSH_COLOR_TS, 0);
        assert_eq!(RBBM_INT_COMPLETION_MASK & RBBM_INT_FATAL_MASK, 0);
        assert_ne!(RBBM_INT_FATAL_MASK & RBBM_INT_CP_HW_ERROR, 0);
        assert_ne!(RBBM_INT_FATAL_MASK & RBBM_INT_RBBM_HANG_DETECT, 0);
    }

    #[test]
    fn every_a6xx_cp_error_bit_is_fatal() {
        assert_eq!(CP_INT_FATAL_MASK, 0x0000_00f7);
        assert_ne!(CP_INT_FATAL_MASK & CP_INT_ILLEGAL_INSTR_ERROR, 0);
        assert_eq!(CP_INT_FATAL_MASK & (1 << 3), 0);
    }
}
