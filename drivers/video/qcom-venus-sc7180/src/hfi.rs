// SPDX-License-Identifier: GPL-2.0-only

//! Cache-coherent transport for the Venus host-firmware interface queues.

use alloc::{string::String, vec, vec::Vec};
use core::ptr;

use scarlet::{arch, device::iommu::DmaContext, println};

use crate::{memory::DmaAllocation, registers::VenusRegisters};

const QUEUE_COUNT: usize = 3;
const COMMAND_QUEUE: usize = 0;
const MESSAGE_QUEUE: usize = 1;
const DEBUG_QUEUE: usize = 2;
const QUEUE_TABLE_HEADER_SIZE: usize = 24;
const QUEUE_HEADER_SIZE: usize = 56;
const QUEUE_TABLE_SIZE: usize = QUEUE_TABLE_HEADER_SIZE + QUEUE_COUNT * QUEUE_HEADER_SIZE;
const QUEUE_DATA_SIZE: usize = 1024 * 50 * 16;
const QUEUE_DATA_WORDS: u32 = (QUEUE_DATA_SIZE / 4) as u32;
const RAW_QUEUE_SIZE: usize = QUEUE_TABLE_SIZE + QUEUE_COUNT * QUEUE_DATA_SIZE;
const ALIGNED_QUEUE_SIZE: usize = align_up(RAW_QUEUE_SIZE, 4096);
const SFR_SIZE: usize = 4096;
const QDSS_SIZE: usize = 4096;
pub(crate) const SHARED_REGION_SIZE: usize =
    align_up(ALIGNED_QUEUE_SIZE + SFR_SIZE + QDSS_SIZE, 1 << 20);
const SFR_OFFSET: usize = ALIGNED_QUEUE_SIZE;
const MAX_PACKET_BYTES: usize = 12 * 1024;
const DEFAULT_QUEUE_TYPE: u32 = 0x0101_0000;
const HFI_MSG_SYS_DEBUG: u32 = 0x0002_0004;

const Q_STATUS: usize = 0;
const Q_START_ADDR: usize = 1;
const Q_TYPE: usize = 2;
const Q_SIZE: usize = 3;
const Q_PKT_SIZE: usize = 4;
const Q_PKT_DROP_COUNT: usize = 5;
const Q_RX_WATERMARK: usize = 6;
const Q_TX_WATERMARK: usize = 7;
const Q_RX_REQUEST: usize = 8;
const Q_TX_REQUEST: usize = 9;
const Q_RX_IRQ_STATUS: usize = 10;
const Q_TX_IRQ_STATUS: usize = 11;
const Q_READ_INDEX: usize = 12;
const Q_WRITE_INDEX: usize = 13;

const fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn queue_header_offset(index: usize) -> usize {
    QUEUE_TABLE_HEADER_SIZE + index * QUEUE_HEADER_SIZE
}

fn queue_data_offset(index: usize) -> usize {
    QUEUE_TABLE_SIZE + index * QUEUE_DATA_SIZE
}

/// Venus HFI shared-memory transport.
pub(crate) struct HfiTransport {
    shared: DmaAllocation,
}

impl HfiTransport {
    pub(crate) fn new(dma: &DmaContext) -> Result<Self, &'static str> {
        // Linux obtains the HFI queue table through dma_alloc_attrs(), giving
        // firmware and the host a coherent view of queue headers. Use a Normal
        // NonCacheable CPU alias here: cache maintenance on individual fields
        // cannot safely arbitrate a cache line whose ownership is split
        // between the host and Venus firmware.
        let shared =
            DmaAllocation::new_noncacheable(dma, SHARED_REGION_SIZE, crate::memory::rw_flags())?;
        let mut transport = Self { shared };
        transport.reset()?;
        Ok(transport)
    }

    pub(crate) fn queue_dma(&self) -> u32 {
        self.shared.dma_addr()
    }

    pub(crate) fn sfr_dma(&self) -> Result<u32, &'static str> {
        self.queue_dma()
            .checked_add(SFR_OFFSET as u32)
            .ok_or("qcom-venus-sc7180: SFR DMA address overflows")
    }

    pub(crate) fn reset(&mut self) -> Result<(), &'static str> {
        self.shared.zero();

        self.write_word(0, 0);
        self.write_word(1, QUEUE_TABLE_SIZE as u32);
        self.write_word(2, QUEUE_TABLE_HEADER_SIZE as u32);
        self.write_word(3, QUEUE_HEADER_SIZE as u32);
        self.write_word(4, QUEUE_COUNT as u32);
        self.write_word(5, QUEUE_COUNT as u32);

        for index in 0..QUEUE_COUNT {
            let start_addr = self
                .queue_dma()
                .checked_add(queue_data_offset(index) as u32)
                .ok_or("qcom-venus-sc7180: HFI queue DMA address overflows")?;
            self.write_header_word(index, Q_STATUS, 1);
            self.write_header_word(index, Q_START_ADDR, start_addr);
            self.write_header_word(index, Q_TYPE, DEFAULT_QUEUE_TYPE | index as u32);
            self.write_header_word(index, Q_SIZE, QUEUE_DATA_WORDS);
            self.write_header_word(index, Q_PKT_SIZE, 0);
            self.write_header_word(index, Q_PKT_DROP_COUNT, 0);
            self.write_header_word(index, Q_RX_WATERMARK, 1);
            self.write_header_word(index, Q_TX_WATERMARK, 1);
            self.write_header_word(index, Q_RX_REQUEST, u32::from(index != DEBUG_QUEUE));
            self.write_header_word(index, Q_TX_REQUEST, 0);
            self.write_header_word(index, Q_RX_IRQ_STATUS, 0);
            self.write_header_word(index, Q_TX_IRQ_STATUS, 0);
            self.write_header_word(index, Q_READ_INDEX, 0);
            self.write_header_word(index, Q_WRITE_INDEX, 0);
        }

        self.write_word(SFR_OFFSET / 4, SFR_SIZE as u32);
        self.shared.clean(0, self.shared.allocation_size())?;
        arch::io_mb();
        Ok(())
    }

    pub(crate) fn send(
        &mut self,
        registers: &VenusRegisters,
        words: &[u32],
        synchronous: bool,
    ) -> Result<(), &'static str> {
        if words.len() < 2
            || words.len() * 4 > MAX_PACKET_BYTES
            || words[0] as usize != words.len() * 4
        {
            return Err("qcom-venus-sc7180: invalid outgoing HFI packet");
        }

        self.shared
            .invalidate(queue_header_offset(COMMAND_QUEUE), QUEUE_HEADER_SIZE)?;
        let read_index = self.read_header_word(COMMAND_QUEUE, Q_READ_INDEX);
        let write_index = self.read_header_word(COMMAND_QUEUE, Q_WRITE_INDEX);
        let queue_size = self.read_header_word(COMMAND_QUEUE, Q_SIZE);
        if queue_size == 0 || queue_size > QUEUE_DATA_WORDS {
            return Err("qcom-venus-sc7180: invalid HFI command queue size");
        }
        if read_index >= queue_size || write_index >= queue_size {
            return Err("qcom-venus-sc7180: invalid HFI command queue index");
        }
        let used = if write_index >= read_index {
            write_index - read_index
        } else {
            queue_size - (read_index - write_index)
        };
        let free = queue_size - used;
        if free <= words.len() as u32 {
            self.write_header_word(COMMAND_QUEUE, Q_TX_REQUEST, 1);
            self.shared
                .clean(queue_header_offset(COMMAND_QUEUE), QUEUE_HEADER_SIZE)?;
            return Err("qcom-venus-sc7180: HFI command queue is full");
        }

        self.write_header_word(COMMAND_QUEUE, Q_TX_REQUEST, 0);
        self.copy_words_to_queue(COMMAND_QUEUE, write_index, words, queue_size)?;
        let new_write = (write_index + words.len() as u32) % queue_size;
        self.write_header_word(COMMAND_QUEUE, Q_WRITE_INDEX, new_write);
        self.shared
            .clean(queue_header_offset(COMMAND_QUEUE), QUEUE_HEADER_SIZE)?;

        if synchronous {
            self.shared
                .invalidate(queue_header_offset(MESSAGE_QUEUE), QUEUE_HEADER_SIZE)?;
            self.write_header_word(MESSAGE_QUEUE, Q_RX_REQUEST, 1);
            self.shared
                .clean(queue_header_offset(MESSAGE_QUEUE), QUEUE_HEADER_SIZE)?;
        }
        arch::io_mb();
        // Raising an H2A interrupt unconditionally is safe and avoids relying
        // on a stale firmware-owned rx_req cache line after a reset.
        registers.raise_host_interrupt();
        Ok(())
    }

    pub(crate) fn read_message(
        &mut self,
        registers: &VenusRegisters,
    ) -> Result<Option<Vec<u32>>, &'static str> {
        let (packet, notify_firmware) = self.read_queue(MESSAGE_QUEUE, true)?;
        if notify_firmware {
            registers.raise_host_interrupt();
        }
        Ok(packet)
    }

    pub(crate) fn drain_debug(&mut self, registers: &VenusRegisters) {
        for _ in 0..32 {
            match self.read_queue(DEBUG_QUEUE, false) {
                Ok((Some(packet), notify_firmware)) => {
                    Self::log_debug_packet(&packet);
                    if notify_firmware {
                        registers.raise_host_interrupt();
                    }
                }
                Ok((None, _)) | Err(_) => break,
            }
        }
    }

    pub(crate) fn log_diagnostics(&mut self, registers: &VenusRegisters) {
        self.drain_debug(registers);
        for index in 0..QUEUE_COUNT {
            let _ = self
                .shared
                .invalidate(queue_header_offset(index), QUEUE_HEADER_SIZE);
            println!(
                "[qcom-venus-sc7180] HFI queue={} status={:#x} rx-req={:#x} rx-irq={:#x} read={:#x} write={:#x}",
                index,
                self.read_header_word(index, Q_STATUS),
                self.read_header_word(index, Q_RX_REQUEST),
                self.read_header_word(index, Q_RX_IRQ_STATUS),
                self.read_header_word(index, Q_READ_INDEX),
                self.read_header_word(index, Q_WRITE_INDEX),
            );
        }

        let _ = self.shared.invalidate(SFR_OFFSET, SFR_SIZE);
        let bytes = self.bytes_at(SFR_OFFSET + 4, SFR_SIZE - 4);
        let len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if len != 0 {
            println!(
                "[qcom-venus-sc7180] firmware SFR: {}",
                String::from_utf8_lossy(&bytes[..len]),
            );
        }
    }

    fn log_debug_packet(packet: &[u32]) {
        if packet.get(1).copied() != Some(HFI_MSG_SYS_DEBUG) || packet.len() < 6 {
            println!(
                "[qcom-venus-fw] unrecognized debug packet words={:x?}",
                &packet[..packet.len().min(16)]
            );
            return;
        }
        let available = (packet.len() - 6) * 4;
        let requested = packet[3] as usize;
        let bytes = Self::packet_bytes(packet, 6, available.min(requested));
        let len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if len != 0 {
            println!(
                "[qcom-venus-fw] type={:#x} {}",
                packet[2],
                String::from_utf8_lossy(&bytes[..len]),
            );
        }
    }

    fn packet_bytes(packet: &[u32], word: usize, len: usize) -> &[u8] {
        // SAFETY: `word` and `len` are derived from the packet slice bounds,
        // and `u32` storage may be viewed as bytes for firmware text parsing.
        unsafe { core::slice::from_raw_parts(packet.as_ptr().add(word).cast::<u8>(), len) }
    }

    fn bytes_at(&self, offset: usize, len: usize) -> &[u8] {
        // SAFETY: callers pass ranges fully contained in the owned shared DMA
        // allocation. The returned view is read-only and tied to `&self`.
        unsafe { core::slice::from_raw_parts((self.shared.vaddr() + offset) as *const u8, len) }
    }

    fn read_queue(
        &mut self,
        index: usize,
        request_interrupt: bool,
    ) -> Result<(Option<Vec<u32>>, bool), &'static str> {
        self.shared
            .invalidate(queue_header_offset(index), QUEUE_HEADER_SIZE)?;
        let read_index = self.read_header_word(index, Q_READ_INDEX);
        let write_index = self.read_header_word(index, Q_WRITE_INDEX);
        let queue_size = self.read_header_word(index, Q_SIZE);
        if queue_size == 0 || queue_size > QUEUE_DATA_WORDS {
            return Err("qcom-venus-sc7180: invalid HFI receive queue size");
        }
        if read_index >= queue_size || write_index >= queue_size {
            return Err("qcom-venus-sc7180: invalid HFI receive queue index");
        }
        if read_index == write_index {
            self.write_header_word(index, Q_RX_REQUEST, u32::from(request_interrupt));
            self.shared
                .clean(queue_header_offset(index), QUEUE_HEADER_SIZE)?;
            return Ok((None, false));
        }

        let first_offset = queue_data_offset(index) + read_index as usize * 4;
        self.shared.invalidate(first_offset, 4)?;
        let packet_bytes = self.read_word(first_offset / 4) as usize;
        if !(8..=MAX_PACKET_BYTES).contains(&packet_bytes) || packet_bytes % 4 != 0 {
            self.drop_receive_queue(index, write_index)?;
            return Err("qcom-venus-sc7180: firmware produced an invalid HFI packet size");
        }
        let packet_words = packet_bytes / 4;
        if packet_words as u32 >= queue_size {
            self.drop_receive_queue(index, write_index)?;
            return Err("qcom-venus-sc7180: firmware HFI packet exceeds queue capacity");
        }

        let words = self.copy_words_from_queue(index, read_index, packet_words, queue_size)?;
        if words.first().copied() != Some(packet_bytes as u32) {
            self.drop_receive_queue(index, write_index)?;
            return Err("qcom-venus-sc7180: inconsistent HFI packet header");
        }
        let new_read = (read_index + packet_words as u32) % queue_size;
        self.write_header_word(index, Q_READ_INDEX, new_read);
        self.write_header_word(
            index,
            Q_RX_REQUEST,
            u32::from(request_interrupt && new_read == write_index),
        );
        self.shared
            .clean(queue_header_offset(index), QUEUE_HEADER_SIZE)?;
        arch::io_mb();
        self.shared
            .invalidate(queue_header_offset(index), QUEUE_HEADER_SIZE)?;
        let notify_firmware = self.read_header_word(index, Q_TX_REQUEST) != 0;
        Ok((Some(words), notify_firmware))
    }

    fn drop_receive_queue(&mut self, index: usize, write_index: u32) -> Result<(), &'static str> {
        self.write_header_word(index, Q_READ_INDEX, write_index);
        self.write_header_word(index, Q_RX_REQUEST, u32::from(index == MESSAGE_QUEUE));
        self.shared
            .clean(queue_header_offset(index), QUEUE_HEADER_SIZE)
    }

    fn copy_words_to_queue(
        &mut self,
        index: usize,
        write_index: u32,
        words: &[u32],
        queue_size: u32,
    ) -> Result<(), &'static str> {
        let until_wrap = (queue_size - write_index) as usize;
        let first_words = words.len().min(until_wrap);
        let first_offset = queue_data_offset(index) + write_index as usize * 4;
        // SAFETY: queue bounds and free space were validated by `send`; the
        // source slice is live and cannot overlap the DMA allocation.
        unsafe {
            ptr::copy_nonoverlapping(
                words.as_ptr(),
                (self.shared.vaddr() + first_offset) as *mut u32,
                first_words,
            );
        }
        self.shared.clean(first_offset, first_words * 4)?;
        if first_words != words.len() {
            let remaining = words.len() - first_words;
            let second_offset = queue_data_offset(index);
            // SAFETY: the wrapped portion is bounded by the queue and is
            // disjoint from the source slice.
            unsafe {
                ptr::copy_nonoverlapping(
                    words.as_ptr().add(first_words),
                    (self.shared.vaddr() + second_offset) as *mut u32,
                    remaining,
                );
            }
            self.shared.clean(second_offset, remaining * 4)?;
        }
        Ok(())
    }

    fn copy_words_from_queue(
        &self,
        index: usize,
        read_index: u32,
        count: usize,
        queue_size: u32,
    ) -> Result<Vec<u32>, &'static str> {
        let until_wrap = (queue_size - read_index) as usize;
        let first_words = count.min(until_wrap);
        let first_offset = queue_data_offset(index) + read_index as usize * 4;
        self.shared.invalidate(first_offset, first_words * 4)?;
        let mut words = vec![0; count];
        // SAFETY: the receive packet size and queue bounds were validated;
        // `words` owns enough initialized storage for the copy.
        unsafe {
            ptr::copy_nonoverlapping(
                (self.shared.vaddr() + first_offset) as *const u32,
                words.as_mut_ptr(),
                first_words,
            );
        }
        if first_words != count {
            let remaining = count - first_words;
            let second_offset = queue_data_offset(index);
            self.shared.invalidate(second_offset, remaining * 4)?;
            // SAFETY: the wrapped receive range lies inside the same queue and
            // the destination tail has `remaining` words available.
            unsafe {
                ptr::copy_nonoverlapping(
                    (self.shared.vaddr() + second_offset) as *const u32,
                    words.as_mut_ptr().add(first_words),
                    remaining,
                );
            }
        }
        Ok(words)
    }

    fn write_word(&mut self, word_offset: usize, value: u32) {
        // SAFETY: all callers use word offsets within the owned shared region.
        unsafe { ptr::write_volatile((self.shared.vaddr() as *mut u32).add(word_offset), value) }
    }

    fn read_word(&self, word_offset: usize) -> u32 {
        // SAFETY: all callers use word offsets within the owned shared region.
        unsafe { ptr::read_volatile((self.shared.vaddr() as *const u32).add(word_offset)) }
    }

    fn write_header_word(&mut self, queue: usize, field: usize, value: u32) {
        self.write_word(queue_header_offset(queue) / 4 + field, value);
    }

    fn read_header_word(&self, queue: usize, field: usize) -> u32 {
        self.read_word(queue_header_offset(queue) / 4 + field)
    }
}
