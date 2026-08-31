// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Hardware entropy from the Google Cr50 TPM2 device on ChromeOS boards.
//!
//! Cr50 implements the TPM TIS FIFO interface over SPI. It requires an
//! in-band wait-state byte after every four-byte SPI header, a short delay
//! between transactions, and an explicit wake pulse after one second idle.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use scarlet::{
    device::{
        DeviceInfo,
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
        spi::{SpiBus, SpiError, SpiTransfer, SpiTransferFlags},
    },
    println,
    random::{EntropySource, RandomManager},
    sync::Mutex,
    time,
};

const DRIVER_NAME: &str = "google-cr50-tpm2-rng";

const TPM_ACCESS: u32 = 0x0000;
const TPM_STS: u32 = 0x0018;
const TPM_DATA_FIFO: u32 = 0x0024;

const TPM_ACCESS_VALID: u8 = 0x80;
const TPM_ACCESS_ACTIVE_LOCALITY: u8 = 0x20;
const TPM_ACCESS_REQUEST_USE: u8 = 0x02;

const TPM_STS_VALID: u8 = 0x80;
const TPM_STS_COMMAND_READY: u8 = 0x40;
const TPM_STS_GO: u8 = 0x20;
const TPM_STS_DATA_AVAIL: u8 = 0x10;
const TPM_STS_DATA_EXPECT: u8 = 0x08;

const TPM_ST_NO_SESSIONS: u16 = 0x8001;
const TPM_CC_GET_RANDOM: u32 = 0x0000_017b;
const TPM_RC_SUCCESS: u32 = 0;
const TPM_HEADER_SIZE: usize = 10;
const TPM_MAX_RANDOM_BYTES: usize = 64;
const TPM_MAX_RESPONSE_SIZE: usize = TPM_HEADER_SIZE + 2 + TPM_MAX_RANDOM_BYTES;

const TIS_SPI_ADDRESS_PREFIX: u8 = 0xd4;
const TIS_SPI_MAX_PAYLOAD: usize = 64;
const TIS_SPI_READY_MASK: u8 = 0x01;
const TIS_SPI_FLOW_MAX_BYTES: usize = 512;
const CR50_ACCESS_DELAY_US: u64 = 2_000;
const CR50_SLEEP_DELAY_NS: u64 = 1_000_000_000;
const CR50_WAKE_CS_US: u64 = 1_000;
const CR50_WAKE_START_US: u64 = 1_000;

const POLL_DELAY_US: u64 = 2_000;
const TIMEOUT_A_POLLS: usize = 375;
const TIMEOUT_B_POLLS: usize = 1_000;
const TIMEOUT_C_POLLS: usize = 1_000;

#[derive(Debug)]
enum Cr50Error {
    Spi(SpiError),
    Timeout,
    Protocol,
    Tpm(u32),
}

impl From<SpiError> for Cr50Error {
    fn from(error: SpiError) -> Self {
        Self::Spi(error)
    }
}

struct Cr50State {
    last_access_ns: u64,
    wake_after_ns: u64,
    locality_active: bool,
}

/// One Cr50 TPM used solely as a cryptographic entropy source.
struct Cr50TpmRng {
    bus: Arc<dyn SpiBus>,
    chip_select: u8,
    speed_hz: u32,
    state: Mutex<Cr50State>,
    failure_logged: AtomicBool,
}

impl Cr50TpmRng {
    fn new(bus: Arc<dyn SpiBus>, chip_select: u8, maximum_speed_hz: u32) -> Self {
        Self {
            speed_hz: bus.bus_speed().min(maximum_speed_hz),
            bus,
            chip_select,
            state: Mutex::new(Cr50State {
                last_access_ns: 0,
                wake_after_ns: 0,
                locality_active: false,
            }),
            failure_logged: AtomicBool::new(false),
        }
    }

    fn set_speed(&self, segments: &mut [SpiTransfer]) {
        for segment in segments {
            segment.speed_hz = self.speed_hz;
        }
    }

    fn wait_between_transactions(&self, state: &Cr50State) {
        if state.last_access_ns == 0 {
            return;
        }
        let elapsed_ns = time::current_time_ns().wrapping_sub(state.last_access_ns);
        let required_ns = CR50_ACCESS_DELAY_US * 1_000;
        if elapsed_ns < required_ns {
            time::udelay((required_ns - elapsed_ns).div_ceil(1_000));
        }
    }

    fn wake_if_needed(&self, state: &mut Cr50State) -> Result<(), Cr50Error> {
        let now = time::current_time_ns();
        if state.wake_after_ns != 0 && now < state.wake_after_ns {
            return Ok(());
        }

        let mut wake = SpiTransfer::with_flags(
            self.chip_select,
            SpiTransferFlags::NONE,
            Vec::new(),
            self.speed_hz,
        );
        wake.delay_after_us = CR50_WAKE_CS_US;
        self.bus.transfer(core::slice::from_mut(&mut wake))?;
        time::udelay(CR50_WAKE_START_US);
        state.last_access_ns = time::current_time_ns();
        state.wake_after_ns = state.last_access_ns.wrapping_add(CR50_SLEEP_DELAY_NS);
        Ok(())
    }

    fn begin_transaction(&self, state: &mut Cr50State) -> Result<(), Cr50Error> {
        self.wait_between_transactions(state);
        self.wake_if_needed(state)
    }

    fn finish_transaction(&self, state: &mut Cr50State) {
        state.last_access_ns = time::current_time_ns();
        state.wake_after_ns = state.last_access_ns.wrapping_add(CR50_SLEEP_DELAY_NS);
    }

    fn spi_header(read: bool, address: u32, length: usize) -> Result<[u8; 4], Cr50Error> {
        if length == 0 || length > TIS_SPI_MAX_PAYLOAD || address > 0xffff {
            return Err(Cr50Error::Protocol);
        }
        Ok([
            if read { 0x80 } else { 0 } | (length as u8 - 1),
            TIS_SPI_ADDRESS_PREFIX,
            (address >> 8) as u8,
            address as u8,
        ])
    }

    fn read_frame(
        &self,
        state: &mut Cr50State,
        address: u32,
        output: &mut [u8],
    ) -> Result<(), Cr50Error> {
        let header = Self::spi_header(true, address, output.len())?;
        self.begin_transaction(state)?;
        let mut segments = vec![
            SpiTransfer::write(self.chip_select, &header),
            SpiTransfer::read_until(
                self.chip_select,
                TIS_SPI_READY_MASK,
                TIS_SPI_READY_MASK,
                TIS_SPI_FLOW_MAX_BYTES,
            ),
            SpiTransfer::read(self.chip_select, output.len()),
        ];
        self.set_speed(&mut segments);
        let result = self.bus.transfer(&mut segments).map_err(Cr50Error::from);
        self.finish_transaction(state);
        result?;
        output.copy_from_slice(&segments[2].data);
        Ok(())
    }

    fn write_frame(
        &self,
        state: &mut Cr50State,
        address: u32,
        input: &[u8],
    ) -> Result<(), Cr50Error> {
        let header = Self::spi_header(false, address, input.len())?;
        self.begin_transaction(state)?;
        let mut segments = vec![
            SpiTransfer::write(self.chip_select, &header),
            SpiTransfer::read_until(
                self.chip_select,
                TIS_SPI_READY_MASK,
                TIS_SPI_READY_MASK,
                TIS_SPI_FLOW_MAX_BYTES,
            ),
            SpiTransfer::write(self.chip_select, input),
        ];
        self.set_speed(&mut segments);
        let result = self.bus.transfer(&mut segments).map_err(Cr50Error::from);
        self.finish_transaction(state);
        result
    }

    fn read_bytes(
        &self,
        state: &mut Cr50State,
        address: u32,
        output: &mut [u8],
    ) -> Result<(), Cr50Error> {
        for chunk in output.chunks_mut(TIS_SPI_MAX_PAYLOAD) {
            self.read_frame(state, address, chunk)?;
        }
        Ok(())
    }

    fn write_bytes(
        &self,
        state: &mut Cr50State,
        address: u32,
        input: &[u8],
    ) -> Result<(), Cr50Error> {
        for chunk in input.chunks(TIS_SPI_MAX_PAYLOAD) {
            self.write_frame(state, address, chunk)?;
        }
        Ok(())
    }

    fn read_u8(&self, state: &mut Cr50State, address: u32) -> Result<u8, Cr50Error> {
        let mut value = [0u8; 1];
        self.read_frame(state, address, &mut value)?;
        Ok(value[0])
    }

    fn write_u8(&self, state: &mut Cr50State, address: u32, value: u8) -> Result<(), Cr50Error> {
        self.write_frame(state, address, &[value])
    }

    fn read_u32(&self, state: &mut Cr50State, address: u32) -> Result<u32, Cr50Error> {
        let mut value = [0u8; 4];
        self.read_frame(state, address, &mut value)?;
        Ok(u32::from_le_bytes(value))
    }

    fn poll_u8(
        &self,
        state: &mut Cr50State,
        address: u32,
        mask: u8,
        expected: u8,
        attempts: usize,
    ) -> Result<u8, Cr50Error> {
        for _ in 0..attempts {
            let value = self.read_u8(state, address)?;
            if value & mask == expected {
                return Ok(value);
            }
            time::udelay(POLL_DELAY_US);
        }
        Err(Cr50Error::Timeout)
    }

    fn request_locality(&self, state: &mut Cr50State) -> Result<(), Cr50Error> {
        let active = TPM_ACCESS_VALID | TPM_ACCESS_ACTIVE_LOCALITY;
        if state.locality_active && self.read_u8(state, TPM_ACCESS)? & active == active {
            return Ok(());
        }
        self.poll_u8(
            state,
            TPM_ACCESS,
            TPM_ACCESS_VALID,
            TPM_ACCESS_VALID,
            TIMEOUT_A_POLLS,
        )?;
        self.write_u8(state, TPM_ACCESS, TPM_ACCESS_REQUEST_USE)?;
        self.poll_u8(state, TPM_ACCESS, active, active, TIMEOUT_A_POLLS)?;
        state.locality_active = true;
        Ok(())
    }

    fn poll_status(
        &self,
        state: &mut Cr50State,
        mask: u8,
        expected: u8,
        attempts: usize,
    ) -> Result<u8, Cr50Error> {
        self.poll_u8(state, TPM_STS, mask, expected, attempts)
    }

    fn burst_count(&self, state: &mut Cr50State) -> Result<usize, Cr50Error> {
        for _ in 0..TIMEOUT_A_POLLS {
            let status = self.read_u32(state, TPM_STS)?;
            let burst = ((status >> 8) & 0xffff) as usize;
            if burst != 0 {
                return Ok(burst.min(TIS_SPI_MAX_PAYLOAD));
            }
            time::udelay(POLL_DELAY_US);
        }
        Err(Cr50Error::Timeout)
    }

    fn send_fifo(&self, state: &mut Cr50State, command: &[u8]) -> Result<(), Cr50Error> {
        self.write_u8(state, TPM_STS, TPM_STS_COMMAND_READY)?;
        self.poll_status(
            state,
            TPM_STS_COMMAND_READY,
            TPM_STS_COMMAND_READY,
            TIMEOUT_B_POLLS,
        )?;

        let mut offset = 0usize;
        while offset + 1 < command.len() {
            let count = self
                .burst_count(state)?
                .min(command.len().saturating_sub(offset + 1));
            self.write_bytes(state, TPM_DATA_FIFO, &command[offset..offset + count])?;
            offset += count;
            let status = self.poll_status(state, TPM_STS_VALID, TPM_STS_VALID, TIMEOUT_C_POLLS)?;
            if status & TPM_STS_DATA_EXPECT == 0 {
                return Err(Cr50Error::Protocol);
            }
        }

        self.write_u8(state, TPM_DATA_FIFO, command[offset])?;
        let status = self.poll_status(state, TPM_STS_VALID, TPM_STS_VALID, TIMEOUT_C_POLLS)?;
        if status & TPM_STS_DATA_EXPECT != 0 {
            return Err(Cr50Error::Protocol);
        }
        self.write_u8(state, TPM_STS, TPM_STS_GO)
    }

    fn receive_fifo(&self, state: &mut Cr50State, output: &mut [u8]) -> Result<(), Cr50Error> {
        let mut offset = 0usize;
        while offset < output.len() {
            self.poll_status(
                state,
                TPM_STS_VALID | TPM_STS_DATA_AVAIL,
                TPM_STS_VALID | TPM_STS_DATA_AVAIL,
                TIMEOUT_C_POLLS,
            )?;
            let count = self.burst_count(state)?.min(output.len() - offset);
            self.read_bytes(state, TPM_DATA_FIFO, &mut output[offset..offset + count])?;
            offset += count;
        }
        Ok(())
    }

    fn execute_command(&self, state: &mut Cr50State, command: &[u8]) -> Result<Vec<u8>, Cr50Error> {
        self.request_locality(state)?;
        let result = (|| {
            self.send_fifo(state, command)?;
            self.poll_status(
                state,
                TPM_STS_VALID | TPM_STS_DATA_AVAIL,
                TPM_STS_VALID | TPM_STS_DATA_AVAIL,
                TIMEOUT_C_POLLS,
            )?;

            let mut header = [0u8; TPM_HEADER_SIZE];
            self.receive_fifo(state, &mut header)?;
            let response_size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
            if !(TPM_HEADER_SIZE..=TPM_MAX_RESPONSE_SIZE).contains(&response_size) {
                return Err(Cr50Error::Protocol);
            }
            let mut response = Vec::with_capacity(response_size);
            response.extend_from_slice(&header);
            response.resize(response_size, 0);
            self.receive_fifo(state, &mut response[TPM_HEADER_SIZE..])?;
            let final_status =
                self.poll_status(state, TPM_STS_VALID, TPM_STS_VALID, TIMEOUT_C_POLLS)?;
            if final_status & TPM_STS_DATA_AVAIL != 0 {
                return Err(Cr50Error::Protocol);
            }
            Ok(response)
        })();
        let _ = self.write_u8(state, TPM_STS, TPM_STS_COMMAND_READY);
        result
    }

    fn get_random_once(
        &self,
        state: &mut Cr50State,
        output: &mut [u8],
    ) -> Result<usize, Cr50Error> {
        if output.is_empty() || output.len() > TPM_MAX_RANDOM_BYTES {
            return Err(Cr50Error::Protocol);
        }
        let mut command = [0u8; 12];
        command[0..2].copy_from_slice(&TPM_ST_NO_SESSIONS.to_be_bytes());
        command[2..6].copy_from_slice(&(12u32).to_be_bytes());
        command[6..10].copy_from_slice(&TPM_CC_GET_RANDOM.to_be_bytes());
        command[10..12].copy_from_slice(&(output.len() as u16).to_be_bytes());

        let response = self.execute_command(state, &command)?;
        if response[0..2] != TPM_ST_NO_SESSIONS.to_be_bytes()
            || u32::from_be_bytes(response[6..10].try_into().unwrap()) != TPM_RC_SUCCESS
        {
            let code = u32::from_be_bytes(response[6..10].try_into().unwrap());
            return Err(Cr50Error::Tpm(code));
        }
        let count = usize::from(u16::from_be_bytes(
            response
                .get(10..12)
                .ok_or(Cr50Error::Protocol)?
                .try_into()
                .unwrap(),
        ));
        let bytes = response.get(12..12 + count).ok_or(Cr50Error::Protocol)?;
        if bytes.is_empty() || bytes.len() > output.len() {
            return Err(Cr50Error::Protocol);
        }
        output[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<usize, Cr50Error> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock();
        let mut offset = 0usize;
        while offset < buffer.len() {
            let requested = (buffer.len() - offset).min(TPM_MAX_RANDOM_BYTES);
            let count =
                self.get_random_once(&mut state, &mut buffer[offset..offset + requested])?;
            offset += count;
        }
        Ok(offset)
    }
}

impl EntropySource for Cr50TpmRng {
    fn name(&self) -> &'static str {
        "google-cr50-tpm2"
    }

    fn read_entropy(&self, buffer: &mut [u8]) -> usize {
        match self.fill_entropy(buffer) {
            Ok(count) => {
                self.failure_logged.store(false, Ordering::Relaxed);
                count
            }
            Err(error) => {
                if !self.failure_logged.swap(true, Ordering::Relaxed) {
                    println!("[google-cr50-rng] TPM2_GetRandom failed: {:?}", error);
                }
                0
            }
        }
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn read_u32_property(device: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    device
        .property(name)
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let bus_phandle = device
        .parent_phandle()
        .ok_or("google-cr50-rng: missing parent SPI bus")?;
    let bus = DeviceManager::get_manager()
        .get_spi_bus(bus_phandle)
        .ok_or(())
        .or_else(|_| probe_defer())?;
    let chip_select = read_u32_property(device, "reg")
        .and_then(|value| u8::try_from(value).ok())
        .ok_or("google-cr50-rng: invalid chip select")?;
    let maximum_speed = read_u32_property(device, "spi-max-frequency").unwrap_or(800_000);
    let source = Arc::new(Cr50TpmRng::new(bus, chip_select, maximum_speed));
    let mut probe_random = [0u8; 16];
    if let Err(error) = source.fill_entropy(&mut probe_random) {
        println!("[google-cr50-rng] probe failed: {:?}", error);
        return Err("google-cr50-rng: TPM2_GetRandom probe failed");
    }
    RandomManager::register_entropy_source(source.clone());
    println!(
        "[google-cr50-rng] registered {} on bus={:#x} cs={} speed={} Hz",
        device.name(),
        bus_phandle,
        chip_select,
        source.speed_hz,
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(DRIVER_NAME, probe_fn, remove_fn, vec!["google,cr50"]);
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_GOOGLE_CR50_TPM2_RNG_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
