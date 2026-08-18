// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm GENI serial-engine I2C controller.
//!
//! The controller uses GENI's FIFO mode and implements Scarlet's generic
//! [`I2cBus`] interface. The initial target is the SC7180 QUPv3 wrapper used by
//! Google CoachZ, including the bus connected to its TI SN65DSI86 display
//! bridge.
//!
//! # Provenance
//!
//! Register definitions and FIFO sequencing follow Linux
//! `drivers/i2c/busses/i2c-qcom-geni.c` and U-Boot
//! `drivers/i2c/geni_i2c.c`. The integration, ownership, and error model are
//! adapted to Scarlet's platform-device, clock, and I2C abstractions.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};

use scarlet::{
    arch::mmio,
    device::{
        DeviceInfo,
        clk::ClkHandle,
        i2c::{I2cAddress, I2cBus, I2cError, I2cMessage, I2cMessageFlags},
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    println,
    sync::IrqSpinLock,
    time, vm,
};

const GENI_FORCE_DEFAULT: usize = 0x020;
const GENI_OUTPUT_CTRL: usize = 0x024;
const GENI_CGC_CTRL: usize = 0x028;
const GENI_SERIAL_MASTER_CLOCK: usize = 0x048;
const GENI_INTERFACE_DISABLE: usize = 0x064;
const GENI_FIRMWARE_REVISION: usize = 0x068;
const GENI_CLOCK_SELECT: usize = 0x07c;

const I2C_TX_LENGTH: usize = 0x26c;
const I2C_RX_LENGTH: usize = 0x270;
const I2C_SCL_COUNTERS: usize = 0x278;

const GENI_BYTE_GRANULARITY: usize = 0x254;
const GENI_DMA_MODE_ENABLE: usize = 0x258;
const GENI_TX_PACKING_0: usize = 0x260;
const GENI_TX_PACKING_1: usize = 0x264;
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
const GENI_HW_PARAM_0: usize = 0xe24;

const FIRMWARE_PROTOCOL_MASK: u32 = 0xff << 8;
const FIRMWARE_PROTOCOL_SHIFT: u32 = 8;
const FIRMWARE_PROTOCOL_I2C: u32 = 3;
const FIFO_INTERFACE_DISABLED: u32 = 1 << 0;

const DEFAULT_CLOCK_GATING: u32 = 0x7f;
const DEFAULT_OUTPUT_ENABLE: u32 = 0x7f;
const FORCE_DEFAULT: u32 = 1;
const SERIAL_CLOCK_ENABLE: u32 = 1;
const SERIAL_CLOCK_DIV_SHIFT: u32 = 4;

const TOP_MASTER_IRQ_ENABLE: u32 = 1 << 2;
const TOP_SECONDARY_IRQ_ENABLE: u32 = 1 << 3;
const FIFO_DMA_MODE_ENABLE: u32 = 1;

const MASTER_COMMAND_DONE: u32 = 1 << 0;
const MASTER_COMMAND_OVERRUN: u32 = 1 << 1;
const MASTER_ILLEGAL_COMMAND: u32 = 1 << 2;
const MASTER_COMMAND_FAILURE: u32 = 1 << 3;
const MASTER_COMMAND_ABORTED: u32 = 1 << 5;
const MASTER_NACK: u32 = 1 << 10;
const MASTER_BUS_PROTOCOL_ERROR: u32 = 1 << 12;
const MASTER_ARBITRATION_LOST: u32 = 1 << 13;
const MASTER_RX_FIFO_READ_ERROR: u32 = 1 << 24;
const MASTER_RX_FIFO_WRITE_ERROR: u32 = 1 << 25;
const MASTER_RX_FIFO_WATERMARK: u32 = 1 << 26;
const MASTER_RX_FIFO_LAST: u32 = 1 << 27;
const MASTER_TX_FIFO_READ_ERROR: u32 = 1 << 28;
const MASTER_TX_FIFO_WRITE_ERROR: u32 = 1 << 29;
const MASTER_TX_FIFO_WATERMARK: u32 = 1 << 30;

const MASTER_ERROR_MASK: u32 = MASTER_COMMAND_OVERRUN
    | MASTER_ILLEGAL_COMMAND
    | MASTER_COMMAND_FAILURE
    | MASTER_NACK
    | MASTER_BUS_PROTOCOL_ERROR
    | MASTER_ARBITRATION_LOST;
const MASTER_COMMON_IRQS: u32 = MASTER_COMMAND_OVERRUN
    | MASTER_ILLEGAL_COMMAND
    | MASTER_COMMAND_FAILURE
    | (1 << 4)
    | MASTER_COMMAND_ABORTED
    | (1 << 6)
    | (1 << 22)
    | (1 << 23)
    | MASTER_RX_FIFO_READ_ERROR
    | MASTER_RX_FIFO_WRITE_ERROR
    | MASTER_TX_FIFO_READ_ERROR
    | MASTER_TX_FIFO_WRITE_ERROR;
const SECONDARY_COMMON_IRQS: u32 = (0x1f << 1) | (0x1f << 9) | (1 << 24) | (1 << 25);

const MASTER_COMMAND_ABORT: u32 = 1 << 1;
const COMMAND_OPCODE_SHIFT: u32 = 27;
const COMMAND_PARAMETER_MASK: u32 = (1 << 27) - 1;
const I2C_WRITE: u32 = 1;
const I2C_READ: u32 = 2;
const I2C_ADDRESS_ONLY: u32 = 4;
const I2C_STOP_STRETCH: u32 = 1 << 2;
const I2C_BYPASS_ADDRESS: u32 = 1 << 8;
const I2C_ADDRESS_SHIFT: u32 = 9;
const I2C_ADDRESS_MASK: u32 = 0x7f << I2C_ADDRESS_SHIFT;

const SCL_HIGH_SHIFT: u32 = 20;
const SCL_LOW_SHIFT: u32 = 10;
const RX_FIFO_WORD_COUNT_MASK: u32 = 0x01ff_ffff;
const TX_FIFO_DEPTH_MASK: u32 = 0x003f_0000;
const TX_FIFO_DEPTH_SHIFT: u32 = 16;

// Four 8-bit protocol words are packed into each 32-bit FIFO entry. These are
// GENI packing vectors for bit ranges 7:0, 15:8, 23:16, and 31:24.
const PACKING_CONFIG_0: u32 = 0x0007_f8fe;
const PACKING_CONFIG_1: u32 = 0x000f_fefe;

const TRANSFER_TIMEOUT_US: u64 = 100_000;
const POLL_INTERVAL_US: u64 = 1;

#[derive(Clone, Copy)]
struct ClockProfile {
    frequency: u32,
    divider: u32,
    high: u32,
    low: u32,
    cycle: u32,
}

const CLOCK_PROFILES: [ClockProfile; 3] = [
    ClockProfile {
        frequency: 100_000,
        divider: 7,
        high: 10,
        low: 11,
        cycle: 26,
    },
    ClockProfile {
        frequency: 400_000,
        divider: 2,
        high: 5,
        low: 12,
        cycle: 24,
    },
    ClockProfile {
        frequency: 1_000_000,
        divider: 1,
        high: 3,
        low: 9,
        cycle: 18,
    },
];

fn profile_for(frequency: u32) -> Option<ClockProfile> {
    CLOCK_PROFILES
        .iter()
        .copied()
        .find(|profile| profile.frequency == frequency)
}

/// FIFO-mode Qualcomm GENI I2C controller.
pub struct QcomGeniI2c {
    base: usize,
    bus_number: u32,
    tx_fifo_words: usize,
    profile: IrqSpinLock<ClockProfile>,
    transfer_lock: IrqSpinLock<()>,
    _serial_clock: Option<ClkHandle>,
}

impl QcomGeniI2c {
    /// Create and initialize a GENI I2C controller.
    ///
    /// # Arguments
    ///
    /// * `base` - Device-mapped GENI serial-engine register base.
    /// * `bus_number` - Logical Scarlet bus number.
    /// * `frequency` - Initial SCL frequency in Hz.
    /// * `serial_clock` - Optional prepared GENI serial-engine clock.
    ///
    /// # Returns
    ///
    /// An initialized FIFO-mode controller or an error when the firmware did
    /// not configure this serial engine for I2C.
    pub fn new(
        base: usize,
        bus_number: u32,
        frequency: u32,
        serial_clock: Option<ClkHandle>,
    ) -> Result<Self, &'static str> {
        let profile = profile_for(frequency).ok_or("qcom-geni-i2c: unsupported bus frequency")?;
        let protocol = (Self::read_at(base, GENI_FIRMWARE_REVISION) & FIRMWARE_PROTOCOL_MASK)
            >> FIRMWARE_PROTOCOL_SHIFT;
        if protocol != FIRMWARE_PROTOCOL_I2C {
            return Err("qcom-geni-i2c: serial engine firmware is not I2C");
        }
        if Self::read_at(base, GENI_INTERFACE_DISABLE) & FIFO_INTERFACE_DISABLED != 0 {
            return Err("qcom-geni-i2c: FIFO mode is disabled");
        }

        let tx_fifo_words = ((Self::read_at(base, GENI_HW_PARAM_0) & TX_FIFO_DEPTH_MASK)
            >> TX_FIFO_DEPTH_SHIFT) as usize;
        if tx_fifo_words < 2 {
            return Err("qcom-geni-i2c: invalid TX FIFO depth");
        }

        let controller = Self {
            base,
            bus_number,
            tx_fifo_words,
            profile: IrqSpinLock::new(profile),
            transfer_lock: IrqSpinLock::new(()),
            _serial_clock: serial_clock,
        };
        controller.initialize_fifo_mode();
        Ok(controller)
    }

    fn read_at(base: usize, offset: usize) -> u32 {
        // SAFETY: `base` is an ioremap'd GENI register window and every caller
        // supplies a defined 32-bit register offset within that window.
        unsafe { mmio::read32(base + offset) }
    }

    fn write_at(base: usize, offset: usize, value: u32) {
        // SAFETY: `base` is an ioremap'd GENI register window and every caller
        // supplies a defined 32-bit register offset within that window.
        unsafe { mmio::write32(base + offset, value) }
    }

    fn read(&self, offset: usize) -> u32 {
        Self::read_at(self.base, offset)
    }

    fn write(&self, offset: usize, value: u32) {
        Self::write_at(self.base, offset, value)
    }

    fn initialize_fifo_mode(&self) {
        self.write(GENI_GSI_EVENT_ENABLE, 0);
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
        self.write(GENI_SECONDARY_IRQ_CLEAR, u32::MAX);

        self.write(
            GENI_CGC_CTRL,
            self.read(GENI_CGC_CTRL) | DEFAULT_CLOCK_GATING,
        );
        self.write(GENI_OUTPUT_CTRL, DEFAULT_OUTPUT_ENABLE);
        self.write(GENI_FORCE_DEFAULT, FORCE_DEFAULT);
        self.write(
            GENI_TOP_IRQ_ENABLE,
            self.read(GENI_TOP_IRQ_ENABLE) | TOP_MASTER_IRQ_ENABLE | TOP_SECONDARY_IRQ_ENABLE,
        );
        self.write(
            GENI_DMA_MODE_ENABLE,
            self.read(GENI_DMA_MODE_ENABLE) & !FIFO_DMA_MODE_ENABLE,
        );

        self.write(GENI_RX_WATERMARK, (self.tx_fifo_words - 1) as u32);
        self.write(GENI_RX_RFR_WATERMARK, self.tx_fifo_words as u32);
        self.write(
            GENI_MASTER_IRQ_ENABLE,
            self.read(GENI_MASTER_IRQ_ENABLE)
                | MASTER_COMMON_IRQS
                | MASTER_COMMAND_DONE
                | MASTER_TX_FIFO_WATERMARK
                | MASTER_RX_FIFO_WATERMARK
                | MASTER_RX_FIFO_LAST
                | MASTER_NACK
                | MASTER_BUS_PROTOCOL_ERROR
                | MASTER_ARBITRATION_LOST,
        );
        self.write(
            GENI_SECONDARY_IRQ_ENABLE,
            self.read(GENI_SECONDARY_IRQ_ENABLE) | SECONDARY_COMMON_IRQS,
        );

        self.write(GENI_TX_PACKING_0, PACKING_CONFIG_0);
        self.write(GENI_TX_PACKING_1, PACKING_CONFIG_1);
        self.write(GENI_RX_PACKING_0, PACKING_CONFIG_0);
        self.write(GENI_RX_PACKING_1, PACKING_CONFIG_1);
        self.write(GENI_BYTE_GRANULARITY, 0);
    }

    fn configure_timing(&self, profile: ClockProfile) {
        self.write(GENI_CLOCK_SELECT, 0);
        self.write(
            GENI_SERIAL_MASTER_CLOCK,
            (profile.divider << SERIAL_CLOCK_DIV_SHIFT) | SERIAL_CLOCK_ENABLE,
        );
        self.write(
            I2C_SCL_COUNTERS,
            (profile.high << SCL_HIGH_SHIFT) | (profile.low << SCL_LOW_SHIFT) | profile.cycle,
        );
        self.write(GENI_MASTER_IRQ_CLEAR, u32::MAX);
    }

    fn start_command(&self, opcode: u32, parameters: u32) {
        self.write(
            GENI_MASTER_COMMAND,
            (opcode << COMMAND_OPCODE_SHIFT) | (parameters & COMMAND_PARAMETER_MASK),
        );
    }

    fn status_error(status: u32) -> Option<I2cError> {
        if status & MASTER_NACK != 0 {
            Some(I2cError::Nack)
        } else if status & MASTER_ARBITRATION_LOST != 0 {
            Some(I2cError::ArbitrationLost)
        } else if status & MASTER_ERROR_MASK != 0 {
            Some(I2cError::BusError)
        } else {
            None
        }
    }

    fn timed_out(start: u64) -> bool {
        time::current_time().saturating_sub(start) >= TRANSFER_TIMEOUT_US
    }

    fn abort_command(&self) {
        self.write(GENI_MASTER_COMMAND_CONTROL, MASTER_COMMAND_ABORT);
        let start = time::current_time();
        while !Self::timed_out(start) {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if status & MASTER_COMMAND_ABORTED != 0 {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return;
            }
            time::udelay(POLL_INTERVAL_US);
        }
    }

    fn address_parameters(message: &I2cMessage, keep_bus: bool) -> Result<u32, I2cError> {
        let address = match message.addr {
            I2cAddress::SevenBit(address) if address <= 0x7f => address as u32,
            _ => return Err(I2cError::InvalidArg),
        };
        let mut parameters = (address << I2C_ADDRESS_SHIFT) & I2C_ADDRESS_MASK;
        if keep_bus {
            parameters |= I2C_STOP_STRETCH;
        }
        if message.flags.contains(I2cMessageFlags::NOSTART) {
            parameters |= I2C_BYPASS_ADDRESS;
        }
        Ok(parameters)
    }

    fn wait_address_only(&self, parameters: u32) -> Result<(), I2cError> {
        self.start_command(I2C_ADDRESS_ONLY, parameters);
        let start = time::current_time();
        while !Self::timed_out(start) {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if let Some(error) = Self::status_error(status) {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return Err(error);
            }
            if status & MASTER_COMMAND_DONE != 0 {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return Ok(());
            }
            time::udelay(POLL_INTERVAL_US);
        }
        self.abort_command();
        Err(I2cError::Timeout)
    }

    fn transfer_write(&self, message: &I2cMessage, parameters: u32) -> Result<(), I2cError> {
        let length = u32::try_from(message.data.len()).map_err(|_| I2cError::InvalidArg)?;
        if length == 0 {
            return self.wait_address_only(parameters);
        }

        self.write(I2C_TX_LENGTH, length);
        self.start_command(I2C_WRITE, parameters);
        self.write(GENI_TX_WATERMARK, 1);

        let start = time::current_time();
        let mut sent = 0usize;
        while !Self::timed_out(start) {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if let Some(error) = Self::status_error(status) {
                self.write(GENI_TX_WATERMARK, 0);
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return Err(error);
            }

            if status & MASTER_TX_FIFO_WATERMARK != 0 {
                for _ in 0..self.tx_fifo_words.saturating_sub(1) {
                    let mut word = 0u32;
                    for byte_index in 0..4 {
                        let Some(byte) = message.data.get(sent) else {
                            break;
                        };
                        word |= (*byte as u32) << (byte_index * 8);
                        sent += 1;
                    }
                    self.write(GENI_TX_FIFO, word);
                    if sent == message.data.len() {
                        self.write(GENI_TX_WATERMARK, 0);
                        break;
                    }
                }
            }

            self.write(GENI_MASTER_IRQ_CLEAR, status);
            if status & MASTER_COMMAND_DONE != 0 {
                return if sent == message.data.len() {
                    Ok(())
                } else {
                    Err(I2cError::BusError)
                };
            }
            time::udelay(POLL_INTERVAL_US);
        }

        self.write(GENI_TX_WATERMARK, 0);
        self.abort_command();
        Err(I2cError::Timeout)
    }

    fn transfer_read(&self, message: &mut I2cMessage, parameters: u32) -> Result<(), I2cError> {
        let length = u32::try_from(message.data.len()).map_err(|_| I2cError::InvalidArg)?;
        if length == 0 {
            return self.wait_address_only(parameters);
        }

        self.write(I2C_RX_LENGTH, length);
        self.start_command(I2C_READ, parameters);

        let start = time::current_time();
        let mut received = 0usize;
        while !Self::timed_out(start) {
            let status = self.read(GENI_MASTER_IRQ_STATUS);
            if let Some(error) = Self::status_error(status) {
                self.write(GENI_MASTER_IRQ_CLEAR, status);
                return Err(error);
            }

            if status & (MASTER_RX_FIFO_WATERMARK | MASTER_RX_FIFO_LAST) != 0 {
                let words = (self.read(GENI_RX_FIFO_STATUS) & RX_FIFO_WORD_COUNT_MASK) as usize;
                for _ in 0..words {
                    let mut word = self.read(GENI_RX_FIFO);
                    for _ in 0..4 {
                        let Some(destination) = message.data.get_mut(received) else {
                            break;
                        };
                        *destination = word as u8;
                        word >>= 8;
                        received += 1;
                    }
                }
            }

            self.write(GENI_MASTER_IRQ_CLEAR, status);
            if status & MASTER_COMMAND_DONE != 0 {
                return if received == message.data.len() {
                    Ok(())
                } else {
                    Err(I2cError::BusError)
                };
            }
            time::udelay(POLL_INTERVAL_US);
        }

        self.abort_command();
        Err(I2cError::Timeout)
    }
}

impl I2cBus for QcomGeniI2c {
    fn transfer(&self, messages: &mut [I2cMessage]) -> Result<(), I2cError> {
        if messages.is_empty() {
            return Err(I2cError::InvalidArg);
        }

        let _guard = self.transfer_lock.lock();
        self.configure_timing(*self.profile.lock());

        let count = messages.len();
        for (index, message) in messages.iter_mut().enumerate() {
            if message.addr.is_ten_bit() || message.flags.contains(I2cMessageFlags::TEN_BIT) {
                return Err(I2cError::InvalidArg);
            }
            let keep_bus = index + 1 < count && !message.flags.contains(I2cMessageFlags::STOP);
            let parameters = Self::address_parameters(message, keep_bus)?;
            if message.flags.contains(I2cMessageFlags::READ) {
                self.transfer_read(message, parameters)?;
            } else {
                self.transfer_write(message, parameters)?;
            }
        }
        Ok(())
    }

    fn set_bus_speed(&self, frequency: u32) -> Result<(), I2cError> {
        let profile = profile_for(frequency).ok_or(I2cError::InvalidArg)?;
        *self.profile.lock() = profile;
        Ok(())
    }

    fn bus_speed(&self) -> u32 {
        self.profile.lock().frequency
    }

    fn bus_number(&self) -> u32 {
        self.bus_number
    }
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-geni-i2c: no memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-geni-i2c: invalid memory resource")?;
    let base = vm::ioremap(resource.start, size).map_err(|_| "qcom-geni-i2c: ioremap failed")?;

    let serial_clock = match DeviceManager::get_manager().resolve_clk(device, "se") {
        Ok(clock) => {
            clock
                .prepare_enable()
                .map_err(|_| "qcom-geni-i2c: failed to enable serial clock")?;
            Some(clock)
        }
        Err(error) => {
            // Depthcharge leaves the display I2C serial engine configured and
            // clocked. Keep that handoff usable until the SC7180 clock provider
            // is available as a separate module.
            println!(
                "[qcom-geni-i2c] serial clock unavailable ({}), using firmware handoff",
                error
            );
            None
        }
    };

    let frequency = device
        .property("clock-frequency")
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(100_000);
    let phandle = device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-geni-i2c: missing phandle")?;

    let controller = QcomGeniI2c::new(base, device.id() as u32, frequency, serial_clock)?;
    let bus_speed = controller.bus_speed();
    let fifo_words = controller.tx_fifo_words;
    DeviceManager::get_manager().register_i2c_bus(phandle, Arc::new(controller));
    println!(
        "[qcom-geni-i2c] registered {} Hz bus (phandle={:#x}, TX FIFO={} words)",
        bus_speed, phandle, fifo_words
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-geni-i2c",
        probe_fn,
        remove_fn,
        vec!["qcom,geni-i2c", "qcom,geni-i2c-master-hub"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_GENI_I2C_ANCHOR: fn() = force_link;

#[inline(never)]
pub fn force_link() {}
