// SPDX-License-Identifier: GPL-2.0-only

//! Legacy A618 Host Firmware Interface queues and boot messages.

use alloc::{vec, vec::Vec};

use scarlet::{arch, device::iommu::DmaContext, time};

use crate::{
    hfi_abi::{
        COMMAND_HEADER_WORD, HfiPerfLevel, QUEUE_WORDS, RESPONSE_HEADER_WORD, ack_matches,
        bandwidth_table, initialize_legacy_table, performance_table,
    },
    memory::{DmaAllocation, bidirectional_flags},
    registers::{
        GMU_GMU2HOST_INTR_CLR, GMU_GMU2HOST_INTR_INFO, GMU_HOST2GMU_INTR_SET, GmuRegisters,
    },
};

const HFI_SIZE: usize = 0x4000;
const COMMAND_DATA_WORD: usize = 0x1000 / 4;
const RESPONSE_DATA_WORD: usize = 0x2000 / 4;

const Q_DROPPED: usize = 5;
const Q_RX_REQUEST: usize = 8;
const Q_READ_INDEX: usize = 10;
const Q_WRITE_INDEX: usize = 11;

const HFI_H2F_MSG_INIT: u8 = 0;
const HFI_H2F_MSG_FW_VERSION: u8 = 1;
const HFI_H2F_MSG_BW_TABLE: u8 = 3;
const HFI_H2F_MSG_PERF_TABLE: u8 = 4;
const HFI_H2F_MSG_TEST: u8 = 5;
const HFI_MSG_CMD: u32 = 0;

const MESSAGE_INTERRUPT: u32 = 1;
const HFI_TIMEOUT_US: u64 = 1_000_000;

/// Linux-compatible HFI v1 performance table built from device-tree OPPs.
#[derive(Debug, Clone)]
pub(crate) struct HfiPowerTable {
    pub(crate) gx_levels: Vec<HfiPerfLevel>,
    pub(crate) cx_levels: Vec<HfiPerfLevel>,
    pub(crate) initial_gpu_index: usize,
    // A618's HFI bandwidth table contains only an off entry. Linux applies
    // this OPP value through the separate SC7180 RPMh interconnect provider.
    pub(crate) peak_kbps: Option<u32>,
}

pub(crate) struct LegacyHfi {
    allocation: DmaAllocation,
    sequence: u32,
}

impl LegacyHfi {
    pub(crate) fn new(context: &DmaContext) -> Result<Self, &'static str> {
        let allocation = DmaAllocation::new(context, HFI_SIZE, bidirectional_flags())?;
        if allocation.dma_addr() > u32::MAX as u64 {
            return Err("qcom-adreno-a618: HFI IOVA exceeds legacy address width");
        }
        let mut result = Self {
            allocation,
            sequence: 0,
        };
        result.initialize_table()?;
        Ok(result)
    }

    pub(crate) fn dma_addr(&self) -> u32 {
        self.allocation.dma_addr() as u32
    }

    fn initialize_table(&mut self) -> Result<(), &'static str> {
        let hfi_iova = self.dma_addr();
        let words = self.allocation.as_words_mut();
        initialize_legacy_table(words, hfi_iova)?;
        self.allocation.clean_for_device();
        Ok(())
    }

    fn next_sequence(&mut self) -> u32 {
        self.sequence = (self.sequence + 1) & 0xfff;
        if self.sequence == 0 {
            self.sequence = 1;
        }
        self.sequence
    }

    /// Stop firmware access to both legacy queues before their DMA is freed.
    pub(crate) fn stop(&mut self) {
        self.allocation.invalidate_from_device();
        let words = self.allocation.as_words_mut();
        for header in [COMMAND_HEADER_WORD, RESPONSE_HEADER_WORD] {
            words[header + Q_READ_INDEX] = 0;
            words[header + Q_WRITE_INDEX] = 0;
        }
        self.allocation.clean_for_device();
        self.sequence = 0;
    }

    fn write_command(&mut self, message: &[u32]) -> Result<(), &'static str> {
        if message.is_empty() || message.len() >= QUEUE_WORDS {
            return Err("qcom-adreno-a618: invalid HFI message length");
        }
        self.allocation.invalidate_from_device();
        let words = self.allocation.as_words_mut();
        let read_index = words[COMMAND_HEADER_WORD + Q_READ_INDEX] as usize;
        let write_index = words[COMMAND_HEADER_WORD + Q_WRITE_INDEX] as usize;
        if read_index >= QUEUE_WORDS || write_index >= QUEUE_WORDS {
            return Err("qcom-adreno-a618: corrupt HFI command queue indices");
        }
        let space = if write_index >= read_index {
            QUEUE_WORDS - (write_index - read_index) - 1
        } else {
            read_index - write_index - 1
        };
        if space < message.len() {
            words[COMMAND_HEADER_WORD + Q_DROPPED] =
                words[COMMAND_HEADER_WORD + Q_DROPPED].wrapping_add(1);
            return Err("qcom-adreno-a618: HFI command queue is full");
        }
        let mut index = write_index;
        for value in message {
            words[COMMAND_DATA_WORD + index] = *value;
            index = (index + 1) % QUEUE_WORDS;
        }
        arch::io_wmb();
        words[COMMAND_HEADER_WORD + Q_WRITE_INDEX] = index as u32;
        self.allocation.clean_for_device();
        Ok(())
    }

    fn read_response(&mut self) -> Result<Option<Vec<u32>>, &'static str> {
        self.allocation.invalidate_from_device();
        let words = self.allocation.as_words_mut();
        let read_index = words[RESPONSE_HEADER_WORD + Q_READ_INDEX] as usize;
        let write_index = words[RESPONSE_HEADER_WORD + Q_WRITE_INDEX] as usize;
        if read_index >= QUEUE_WORDS || write_index >= QUEUE_WORDS {
            return Err("qcom-adreno-a618: corrupt HFI response queue indices");
        }
        if read_index == write_index {
            words[RESPONSE_HEADER_WORD + Q_RX_REQUEST] = 1;
            self.allocation.clean_for_device();
            return Ok(None);
        }
        let header = words[RESPONSE_DATA_WORD + read_index];
        let dwords = ((header >> 8) & 0xff) as usize;
        if dwords == 0 || dwords > 19 || dwords >= QUEUE_WORDS {
            return Err("qcom-adreno-a618: invalid HFI response length");
        }
        let mut response = Vec::with_capacity(dwords);
        let mut index = read_index;
        for _ in 0..dwords {
            response.push(words[RESPONSE_DATA_WORD + index]);
            index = (index + 1) % QUEUE_WORDS;
        }
        arch::io_wmb();
        words[RESPONSE_HEADER_WORD + Q_READ_INDEX] = index as u32;
        self.allocation.clean_for_device();
        Ok(Some(response))
    }

    fn wait_for_interrupt(registers: GmuRegisters) -> Result<(), &'static str> {
        let start = time::current_time();
        loop {
            let info = registers.read(GMU_GMU2HOST_INTR_INFO);
            if info & MESSAGE_INTERRUPT != 0 {
                registers.write(GMU_GMU2HOST_INTR_CLR, MESSAGE_INTERRUPT);
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= HFI_TIMEOUT_US {
                return Err("qcom-adreno-a618: HFI response timed out");
            }
            time::udelay(10);
        }
    }

    fn send(
        &mut self,
        registers: GmuRegisters,
        id: u8,
        mut message: Vec<u32>,
    ) -> Result<Vec<u32>, &'static str> {
        let sequence = self.next_sequence();
        if message.is_empty() || message.len() > u8::MAX as usize {
            return Err("qcom-adreno-a618: HFI message is invalid");
        }
        message[0] =
            (sequence << 20) | (HFI_MSG_CMD << 16) | ((message.len() as u32) << 8) | u32::from(id);
        self.write_command(&message)?;
        registers.write(GMU_HOST2GMU_INTR_SET, 1);

        loop {
            Self::wait_for_interrupt(registers)?;
            while let Some(response) = self.read_response()? {
                if ack_matches(&response, id, sequence)? {
                    return Ok(response);
                }
            }
        }
    }

    pub(crate) fn start_legacy_sequence(
        &mut self,
        registers: GmuRegisters,
        debug_iova: u32,
        debug_size: u32,
        power: &HfiPowerTable,
    ) -> Result<(), &'static str> {
        self.send(
            registers,
            HFI_H2F_MSG_INIT,
            vec![0, 0, debug_iova, debug_size, 0],
        )?;
        self.send(
            registers,
            HFI_H2F_MSG_FW_VERSION,
            vec![0, (1 << 28) | (1 << 19) | (1 << 17)],
        )?;
        self.send(
            registers,
            HFI_H2F_MSG_PERF_TABLE,
            performance_table(&power.gx_levels, &power.cx_levels)?,
        )?;
        self.send(registers, HFI_H2F_MSG_BW_TABLE, bandwidth_table())?;
        self.send(registers, HFI_H2F_MSG_TEST, vec![0])?;
        Ok(())
    }
}
