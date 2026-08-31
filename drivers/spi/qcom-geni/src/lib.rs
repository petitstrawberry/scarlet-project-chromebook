// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm GENI serial-engine SPI controller.
//!
//! This driver uses GENI FIFO mode and Scarlet's generic SPI interface.  It
//! initially targets the SC7180 QUPv3 serial engine carrying the Chrome EC on
//! Google Trogdor-family boards.
//!
//! # Provenance
//!
//! Register definitions and FIFO sequencing are adapted from U-Boot's
//! `drivers/spi/spi-geni-qcom.c` and coreboot's Qualcomm QUPv3 SPI driver.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::mmio,
    device::{
        DeviceInfo,
        clk::ClkHandle,
        gpio::GpioController,
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        spi::{SpiBus, SpiError, SpiTransfer, SpiTransferFlags},
    },
    println,
    sync::IrqSpinLock,
    time, vm,
};

const GENI_FORCE_DEFAULT: usize = 0x020;
const GENI_OUTPUT_CONTROL: usize = 0x024;
const GENI_CLOCK_GATING: usize = 0x028;
const GENI_SERIAL_MASTER_CLOCK: usize = 0x048;
const GENI_INTERFACE_DISABLE: usize = 0x064;
const GENI_FIRMWARE_REVISION: usize = 0x068;

const SPI_CLOCK_PHASE: usize = 0x224;
const SPI_LOOPBACK: usize = 0x22c;
const SPI_CLOCK_POLARITY: usize = 0x230;
const SPI_DEMUX_INVERT: usize = 0x24c;
const SPI_DEMUX_SELECT: usize = 0x250;
const GENI_BYTE_GRANULARITY: usize = 0x254;
const GENI_DMA_MODE_ENABLE: usize = 0x258;
const SPI_TRANSFER_CONFIG: usize = 0x25c;
const GENI_TX_PACKING_0: usize = 0x260;
const GENI_TX_PACKING_1: usize = 0x264;
const SPI_WORD_LENGTH: usize = 0x268;
const SPI_TX_LENGTH: usize = 0x26c;
const SPI_RX_LENGTH: usize = 0x270;
const GENI_RX_PACKING_0: usize = 0x284;
const GENI_RX_PACKING_1: usize = 0x288;

const GENI_MASTER_COMMAND: usize = 0x600;
const GENI_MASTER_COMMAND_CONTROL: usize = 0x604;
const GENI_MASTER_IRQ_STATUS: usize = 0x610;
const GENI_MASTER_IRQ_ENABLE: usize = 0x614;
const GENI_MASTER_IRQ_CLEAR: usize = 0x618;
const GENI_SECONDARY_IRQ_ENABLE: usize = 0x644;
const GENI_SECONDARY_IRQ_CLEAR: usize = 0x648;

const GENI_TX_FIFO: usize = 0x700;
const GENI_RX_FIFO: usize = 0x780;
const GENI_RX_FIFO_STATUS: usize = 0x804;
const GENI_TX_WATERMARK: usize = 0x80c;
const GENI_RX_WATERMARK: usize = 0x810;
const GENI_RX_RFR_WATERMARK: usize = 0x814;

const GENI_GSI_EVENT_ENABLE: usize = 0xe18;
const GENI_TOP_IRQ_ENABLE: usize = 0xe1c;
const GENI_HW_PARAMETER_0: usize = 0xe24;
const GENI_HW_PARAMETER_1: usize = 0xe28;

const FIRMWARE_PROTOCOL_MASK: u32 = 0xff << 8;
const FIRMWARE_PROTOCOL_SHIFT: u32 = 8;
const FIRMWARE_PROTOCOL_SPI: u32 = 1;
const FIFO_INTERFACE_DISABLED: u32 = 1;

const DEFAULT_CLOCK_GATING: u32 = 0x7f;
const DEFAULT_OUTPUT_ENABLE: u32 = 0x7f;
const FORCE_DEFAULT: u32 = 1;
const SERIAL_CLOCK_ENABLE: u32 = 1;
const SERIAL_CLOCK_DIVIDER_SHIFT: u32 = 4;
const SERIAL_CLOCK_DIVIDER_MAX: u32 = 0x0fff;
const SERIAL_ENGINE_SOURCE_HZ: u32 = 19_200_000;

const TOP_MASTER_IRQ_ENABLE: u32 = 1 << 2;
const TOP_SECONDARY_IRQ_ENABLE: u32 = 1 << 3;
const FIFO_DMA_MODE_ENABLE: u32 = 1;

const COMMAND_DONE: u32 = 1 << 0;
const COMMAND_OVERRUN: u32 = 1 << 1;
const ILLEGAL_COMMAND: u32 = 1 << 2;
const COMMAND_FAILURE: u32 = 1 << 3;
const COMMAND_CANCELLED: u32 = 1 << 4;
const COMMAND_ABORTED: u32 = 1 << 5;
const RX_FIFO_READ_ERROR: u32 = 1 << 24;
const RX_FIFO_WRITE_ERROR: u32 = 1 << 25;
const RX_FIFO_WATERMARK: u32 = 1 << 26;
const RX_FIFO_LAST: u32 = 1 << 27;
const TX_FIFO_READ_ERROR: u32 = 1 << 28;
const TX_FIFO_WRITE_ERROR: u32 = 1 << 29;
const TX_FIFO_WATERMARK: u32 = 1 << 30;

const COMMAND_ERROR_MASK: u32 = COMMAND_OVERRUN
    | ILLEGAL_COMMAND
    | COMMAND_FAILURE
    | RX_FIFO_READ_ERROR
    | RX_FIFO_WRITE_ERROR
    | TX_FIFO_READ_ERROR
    | TX_FIFO_WRITE_ERROR;
const MASTER_IRQS: u32 = COMMAND_DONE
    | COMMAND_OVERRUN
    | ILLEGAL_COMMAND
    | COMMAND_FAILURE
    | COMMAND_CANCELLED
    | COMMAND_ABORTED
    | (1 << 6)
    | (1 << 22)
    | (1 << 23)
    | RX_FIFO_READ_ERROR
    | RX_FIFO_WRITE_ERROR
    | RX_FIFO_WATERMARK
    | RX_FIFO_LAST
    | TX_FIFO_READ_ERROR
    | TX_FIFO_WRITE_ERROR
    | TX_FIFO_WATERMARK;
const SECONDARY_IRQS: u32 = (0x1f << 1) | (0x1f << 9) | (1 << 24) | (1 << 25);

const MASTER_CANCEL: u32 = 1 << 2;
const MASTER_ABORT: u32 = 1 << 1;
const COMMAND_OPCODE_SHIFT: u32 = 27;
const COMMAND_PARAMETER_MASK: u32 = (1 << 27) - 1;
const SPI_TX_ONLY: u32 = 1;
const SPI_RX_ONLY: u32 = 2;
const SPI_FULL_DUPLEX: u32 = 3;
const SPI_ASSERT_CS: u32 = 8;
const SPI_DEASSERT_CS: u32 = 9;
const KEEP_CS_ASSERTED: u32 = 1 << 2;

const RX_FIFO_WORD_COUNT_MASK: u32 = 0x01ff_ffff;
const RX_FIFO_LAST_WORD: u32 = 1 << 31;
const RX_FIFO_LAST_BYTES_MASK: u32 = 0x7 << 28;
const RX_FIFO_LAST_BYTES_SHIFT: u32 = 28;
const FIFO_DEPTH_MASK: u32 = 0xff << 16;
const FIFO_DEPTH_SHIFT: u32 = 16;
const TRANSFER_LENGTH_MAX: usize = 0x00ff_ffff;

// Four 8-bit protocol words are packed least-significant byte first in each
// 32-bit FIFO entry.
const PACKING_CONFIG_0: u32 = 0x0007_f8fe;
const PACKING_CONFIG_1: u32 = 0x000f_fefe;

const TRANSFER_TIMEOUT_US: u64 = 100_000;
const CONTROL_TIMEOUT_US: u64 = 100;

#[derive(Clone)]
struct ExternalChipSelect {
    controller: Arc<dyn GpioController>,
    pin: u32,
    active_low: bool,
}

impl ExternalChipSelect {
    fn drive(&self, asserted: bool) {
        self.controller
            .set_direction_output(self.pin, asserted ^ self.active_low);
    }
}

/// FIFO-mode Qualcomm GENI SPI controller.
pub struct QcomGeniSpi {
    base: usize,
    bus_number: u32,
    tx_fifo_words: usize,
    rx_fifo_words: usize,
    chip_selects: Vec<ExternalChipSelect>,
    speed_hz: IrqSpinLock<u32>,
    transfer_lock: IrqSpinLock<()>,
    _serial_clock: Option<ClkHandle>,
}

impl QcomGeniSpi {
    fn read_at(base: usize, offset: usize) -> u32 {
        // SAFETY: `base` is an ioremap'd GENI register window and all offsets
        // used by this module are 32-bit registers inside that window.
        unsafe { mmio::read32(base + offset) }
    }

    fn write_at(base: usize, offset: usize, value: u32) {
        // SAFETY: see `read_at`; writes target the same bounded register map.
        unsafe { mmio::write32(base + offset, value) };
    }

    fn read(&self, offset: usize) -> u32 {
        Self::read_at(self.base, offset)
    }

    fn write(&self, offset: usize, value: u32) {
        Self::write_at(self.base, offset, value)
    }

    fn update(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }

    fn new(
        base: usize,
        bus_number: u32,
        requested_speed_hz: u32,
        chip_selects: Vec<ExternalChipSelect>,
        serial_clock: Option<ClkHandle>,
    ) -> Result<Self, &'static str> {
        let protocol = (Self::read_at(base, GENI_FIRMWARE_REVISION) & FIRMWARE_PROTOCOL_MASK)
            >> FIRMWARE_PROTOCOL_SHIFT;
        if protocol != FIRMWARE_PROTOCOL_SPI {
            return Err("qcom-geni-spi: serial-engine firmware is not SPI");
        }
        if Self::read_at(base, GENI_INTERFACE_DISABLE) & FIFO_INTERFACE_DISABLED != 0 {
            return Err("qcom-geni-spi: FIFO interface is disabled");
        }

        let tx_fifo_words = ((Self::read_at(base, GENI_HW_PARAMETER_0) & FIFO_DEPTH_MASK)
            >> FIFO_DEPTH_SHIFT) as usize;
        let rx_fifo_words = ((Self::read_at(base, GENI_HW_PARAMETER_1) & FIFO_DEPTH_MASK)
            >> FIFO_DEPTH_SHIFT) as usize;
        if tx_fifo_words < 4 || rx_fifo_words < 4 {
            return Err("qcom-geni-spi: invalid FIFO depth");
        }

        let controller = Self {
            base,
            bus_number,
            tx_fifo_words,
            rx_fifo_words,
            chip_selects,
            speed_hz: IrqSpinLock::new(0),
            transfer_lock: IrqSpinLock::new(()),
            _serial_clock: serial_clock,
        };
        controller.initialize_fifo_mode();
        controller
            .program_speed(requested_speed_hz)
            .map_err(|_| "qcom-geni-spi: invalid bus frequency")?;
        for chip_select in &controller.chip_selects {
            chip_select.drive(false);
        }
        Ok(controller)
    }

    fn initialize_fifo_mode(&self) {
        self.write(GENI_GSI_EVENT_ENABLE, 0);
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
        self.write(GENI_SECONDARY_IRQ_CLEAR, u32::MAX);
        self.update(GENI_CLOCK_GATING, 0, DEFAULT_CLOCK_GATING);
        self.write(GENI_OUTPUT_CONTROL, DEFAULT_OUTPUT_ENABLE);
        self.write(GENI_FORCE_DEFAULT, FORCE_DEFAULT);
        self.update(
            GENI_TOP_IRQ_ENABLE,
            0,
            TOP_MASTER_IRQ_ENABLE | TOP_SECONDARY_IRQ_ENABLE,
        );
        self.update(GENI_DMA_MODE_ENABLE, FIFO_DMA_MODE_ENABLE, 0);
        self.write(
            GENI_RX_WATERMARK,
            self.rx_fifo_words.saturating_sub(3) as u32,
        );
        self.write(
            GENI_RX_RFR_WATERMARK,
            self.rx_fifo_words.saturating_sub(2) as u32,
        );
        self.write(GENI_MASTER_IRQ_ENABLE, MASTER_IRQS);
        self.write(GENI_SECONDARY_IRQ_ENABLE, SECONDARY_IRQS);

        self.write(SPI_LOOPBACK, 0);
        self.write(SPI_CLOCK_PHASE, 0);
        self.write(SPI_CLOCK_POLARITY, 0);
        self.write(SPI_TRANSFER_CONFIG, 0);
        self.write(SPI_DEMUX_INVERT, 0);
        self.write(SPI_WORD_LENGTH, 8 - 4);
        self.write(GENI_TX_PACKING_0, PACKING_CONFIG_0);
        self.write(GENI_TX_PACKING_1, PACKING_CONFIG_1);
        self.write(GENI_RX_PACKING_0, PACKING_CONFIG_0);
        self.write(GENI_RX_PACKING_1, PACKING_CONFIG_1);
        self.write(GENI_BYTE_GRANULARITY, 0);
        self.drain_stale_rx();
    }

    fn program_speed(&self, requested_hz: u32) -> Result<u32, SpiError> {
        if requested_hz == 0 {
            return Err(SpiError::InvalidArg);
        }
        let divider = SERIAL_ENGINE_SOURCE_HZ
            .saturating_add(requested_hz - 1)
            .checked_div(requested_hz)
            .unwrap_or(0)
            .clamp(1, SERIAL_CLOCK_DIVIDER_MAX);
        let effective_hz = SERIAL_ENGINE_SOURCE_HZ / divider;
        self.write(
            GENI_SERIAL_MASTER_CLOCK,
            (divider << SERIAL_CLOCK_DIVIDER_SHIFT) | SERIAL_CLOCK_ENABLE,
        );
        *self.speed_hz.lock() = effective_hz;
        Ok(effective_hz)
    }

    fn command_word(opcode: u32, parameters: u32) -> u32 {
        (opcode << COMMAND_OPCODE_SHIFT) | (parameters & COMMAND_PARAMETER_MASK)
    }

    fn wait_for_control(&self, completion: u32, timeout_us: u64) -> Result<(), SpiError> {
        for _ in 0..timeout_us {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if status & completion != 0 {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return Ok(());
            }
            time::udelay(1);
        }
        Err(SpiError::Timeout)
    }

    fn recover_command(&self) {
        self.write(GENI_TX_WATERMARK, 0);
        self.write(GENI_MASTER_COMMAND_CONTROL, MASTER_CANCEL);
        if self
            .wait_for_control(COMMAND_CANCELLED, CONTROL_TIMEOUT_US)
            .is_err()
        {
            self.write(GENI_MASTER_COMMAND_CONTROL, MASTER_ABORT);
            let _ = self.wait_for_control(COMMAND_ABORTED, CONTROL_TIMEOUT_US);
        }
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
        self.drain_stale_rx();
    }

    fn drain_stale_rx(&self) {
        let status = self.read(GENI_RX_FIFO_STATUS);
        let words = (status & RX_FIFO_WORD_COUNT_MASK) as usize;
        for _ in 0..words {
            let _ = self.read(GENI_RX_FIFO);
        }
    }

    fn issue_chip_select_command(&self, opcode: u32) -> Result<(), SpiError> {
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
        self.write(GENI_MASTER_COMMAND, Self::command_word(opcode, 0));
        let result = self.wait_for_control(COMMAND_DONE, CONTROL_TIMEOUT_US);
        if result.is_err() {
            self.recover_command();
        }
        result
    }

    fn set_chip_select(&self, index: u8, asserted: bool) -> Result<(), SpiError> {
        let index = usize::from(index);
        self.write(SPI_DEMUX_SELECT, index as u32);
        if let Some(chip_select) = self.chip_selects.get(index) {
            chip_select.drive(asserted);
            return Ok(());
        }
        if self.chip_selects.is_empty() && index < 4 {
            return self.issue_chip_select_command(if asserted {
                SPI_ASSERT_CS
            } else {
                SPI_DEASSERT_CS
            });
        }
        Err(SpiError::InvalidArg)
    }

    fn fill_tx_fifo(&self, bytes: &[u8]) -> usize {
        let count = bytes
            .len()
            .min(self.tx_fifo_words.saturating_sub(1).saturating_mul(4));
        for chunk in bytes[..count].chunks(4) {
            let mut packed = [0u8; 4];
            packed[..chunk.len()].copy_from_slice(chunk);
            self.write(GENI_TX_FIFO, u32::from_le_bytes(packed));
        }
        count
    }

    fn drain_rx_fifo(&self, destination: &mut [u8]) -> usize {
        let status = self.read(GENI_RX_FIFO_STATUS);
        let words = (status & RX_FIFO_WORD_COUNT_MASK) as usize;
        let mut available = words.saturating_mul(4);
        if status & RX_FIFO_LAST_WORD != 0 {
            let valid = ((status & RX_FIFO_LAST_BYTES_MASK) >> RX_FIFO_LAST_BYTES_SHIFT) as usize;
            if (1..4).contains(&valid) {
                available = available.saturating_sub(4 - valid);
            }
        }

        let mut copied = 0usize;
        for word_index in 0..words {
            let bytes = self.read(GENI_RX_FIFO).to_le_bytes();
            let word_bytes = (available.saturating_sub(word_index * 4)).min(4);
            let writable = word_bytes.min(destination.len().saturating_sub(copied));
            destination[copied..copied + writable].copy_from_slice(&bytes[..writable]);
            copied += writable;
        }
        copied
    }

    fn run_transfer(&self, tx: Option<&[u8]>, mut rx: Option<&mut [u8]>) -> Result<(), SpiError> {
        let tx_len = tx.map_or(0, <[u8]>::len);
        let rx_len = rx.as_deref().map_or(0, <[u8]>::len);
        let length = tx_len.max(rx_len);
        if length == 0 || length > TRANSFER_LENGTH_MAX {
            return Err(SpiError::InvalidArg);
        }
        if tx_len != 0 && rx_len != 0 && tx_len != rx_len {
            return Err(SpiError::InvalidArg);
        }

        let opcode = match (tx_len != 0, rx_len != 0) {
            (true, false) => SPI_TX_ONLY,
            (false, true) => SPI_RX_ONLY,
            (true, true) => SPI_FULL_DUPLEX,
            (false, false) => return Err(SpiError::InvalidArg),
        };
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
        if tx_len != 0 {
            self.write(SPI_TX_LENGTH, tx_len as u32);
            self.write(GENI_TX_WATERMARK, 1);
        }
        if rx_len != 0 {
            self.write(SPI_RX_LENGTH, rx_len as u32);
        }
        self.write(
            GENI_MASTER_COMMAND,
            Self::command_word(opcode, KEEP_CS_ASSERTED),
        );

        let mut tx_offset = 0usize;
        let mut rx_offset = 0usize;
        let mut completed = false;
        for _ in 0..TRANSFER_TIMEOUT_US {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if status == 0 {
                time::udelay(1);
                continue;
            }
            if status & COMMAND_ERROR_MASK != 0 {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                self.recover_command();
                return Err(SpiError::BusError);
            }
            if status & TX_FIFO_WATERMARK != 0
                && let Some(source) = tx
            {
                tx_offset += self.fill_tx_fifo(&source[tx_offset..]);
                if tx_offset == source.len() {
                    self.write(GENI_TX_WATERMARK, 0);
                }
            }
            if status & (RX_FIFO_WATERMARK | RX_FIFO_LAST) != 0
                && let Some(destination) = rx.as_deref_mut()
            {
                rx_offset += self.drain_rx_fifo(&mut destination[rx_offset..]);
            }
            completed |= status & COMMAND_DONE != 0;
            self.write(GENI_MASTER_IRQ_CLEAR, status);
            if completed {
                break;
            }
        }

        if !completed || tx_offset != tx_len || rx_offset != rx_len {
            self.recover_command();
            return Err(SpiError::Timeout);
        }
        Ok(())
    }
}

impl SpiBus for QcomGeniSpi {
    fn transfer(&self, segments: &mut [SpiTransfer]) -> Result<(), SpiError> {
        if segments.is_empty() {
            return Err(SpiError::InvalidArg);
        }
        let chip_select = segments[0].cs;
        if segments.iter().any(|segment| segment.cs != chip_select) {
            return Err(SpiError::InvalidArg);
        }
        let requested_speed = segments
            .iter()
            .find_map(|segment| (segment.speed_hz != 0).then_some(segment.speed_hz))
            .unwrap_or_else(|| *self.speed_hz.lock());
        if segments
            .iter()
            .any(|segment| segment.speed_hz != 0 && segment.speed_hz != requested_speed)
        {
            return Err(SpiError::InvalidArg);
        }

        let _guard = self.transfer_lock.lock();
        self.program_speed(requested_speed)?;
        self.initialize_fifo_mode();
        self.set_chip_select(chip_select, true)?;
        let transfer_result = (|| {
            for segment in segments {
                time::udelay(segment.delay_before_us);
                let read = segment.flags.contains(SpiTransferFlags::READ);
                let write = segment.flags.contains(SpiTransferFlags::WRITE);
                if let Some(condition) = segment.read_until {
                    if !read
                        || write
                        || condition.max_bytes == 0
                        || condition.value & !condition.mask != 0
                    {
                        return Err(SpiError::InvalidArg);
                    }
                    segment.data.clear();
                    let mut matched = false;
                    for _ in 0..condition.max_bytes {
                        let mut byte = [0u8; 1];
                        self.run_transfer(None, Some(&mut byte))?;
                        segment.data.push(byte[0]);
                        if byte[0] & condition.mask == condition.value {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        return Err(SpiError::Timeout);
                    }
                } else {
                    match (read, write) {
                        (false, false) if segment.data.is_empty() => {}
                        (false, false) => return Err(SpiError::InvalidArg),
                        (false, true) => self.run_transfer(Some(&segment.data), None)?,
                        (true, false) => self.run_transfer(None, Some(&mut segment.data))?,
                        (true, true) => {
                            let tx = segment.data.clone();
                            self.run_transfer(Some(&tx), Some(&mut segment.data))?;
                        }
                    }
                }
                time::udelay(segment.delay_after_us);
            }
            Ok(())
        })();
        let deselect_result = self.set_chip_select(chip_select, false);
        transfer_result.and(deselect_result)
    }

    fn set_bus_speed(&self, hz: u32) -> Result<(), SpiError> {
        let _guard = self.transfer_lock.lock();
        self.program_speed(hz).map(|_| ())
    }

    fn bus_speed(&self) -> u32 {
        *self.speed_hz.lock()
    }

    fn bus_number(&self) -> u32 {
        self.bus_number
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn resolve_chip_selects(
    device: &PlatformDeviceInfo,
) -> Result<Vec<ExternalChipSelect>, &'static str> {
    let Some(property) = device.property("cs-gpios") else {
        return Ok(Vec::new());
    };
    if property.value().len() % 12 != 0 {
        return Err("qcom-geni-spi: malformed cs-gpios");
    }

    let mut lines = Vec::new();
    for specifier in property.value().chunks_exact(12) {
        let phandle = read_be_u32(specifier, 0).ok_or("qcom-geni-spi: malformed CS phandle")?;
        let pin = read_be_u32(specifier, 4).ok_or("qcom-geni-spi: malformed CS pin")?;
        let flags = read_be_u32(specifier, 8).ok_or("qcom-geni-spi: malformed CS flags")?;
        let controller = match DeviceManager::get_manager().get_gpio_controller(phandle) {
            Some(controller) => controller,
            None => {
                println!(
                    "[qcom-geni-spi] CS GPIO controller {:#x} is not ready, deferring",
                    phandle
                );
                return probe_defer();
            }
        };
        lines.push(ExternalChipSelect {
            controller,
            pin,
            active_low: flags & 1 != 0,
        });
    }
    Ok(lines)
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-geni-spi: no memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|value| value.checked_add(1))
        .ok_or("qcom-geni-spi: invalid memory resource")?;
    let base = vm::ioremap(resource.start, size).map_err(|_| "qcom-geni-spi: ioremap failed")?;

    let serial_clock = match DeviceManager::get_manager().resolve_clk(device, "se") {
        Ok(clock) => {
            clock
                .prepare_enable()
                .map_err(|_| "qcom-geni-spi: failed to enable serial clock")?;
            Some(clock)
        }
        Err(error) => {
            println!(
                "[qcom-geni-spi] serial clock unavailable ({}), using firmware handoff",
                error
            );
            None
        }
    };
    let phandle = device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-geni-spi: missing phandle")?;
    let maximum_speed = device
        .property("spi-max-frequency")
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1_010_000);
    let chip_selects = resolve_chip_selects(device)?;

    let controller = QcomGeniSpi::new(
        base,
        device.id() as u32,
        maximum_speed,
        chip_selects,
        serial_clock,
    )?;
    let speed = controller.bus_speed();
    let tx_words = controller.tx_fifo_words;
    let rx_words = controller.rx_fifo_words;
    DeviceManager::get_manager().register_spi_bus(phandle, Arc::new(controller));
    println!(
        "[qcom-geni-spi] registered {} Hz bus (phandle={:#x}, TX FIFO={} words, RX FIFO={} words)",
        speed, phandle, tx_words, rx_words
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver =
        PlatformDeviceDriver::new("qcom-geni-spi", probe_fn, remove_fn, vec!["qcom,geni-spi"]);
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_GENI_SPI_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
