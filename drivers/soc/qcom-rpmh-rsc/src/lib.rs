// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm RPMh Resource State Coordinator transport.
//!
//! The driver owns the application RSC MMIO window and implements synchronous
//! active-only TCS transfers.  Power-domain policy and Command DB interpretation
//! deliberately live in the separate SC7180 RPMhPD driver.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::{self, mmio},
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    early_println,
    sync::IrqSpinLock,
    time, vm,
};

const RSC_DRV_ID: usize = 0;
const VERSION_MAJOR_SHIFT: u32 = 16;
const VERSION_MINOR_SHIFT: u32 = 8;
const VERSION_MASK: u32 = 0xff;

const DRV_PRNT_CHLD_CONFIG: usize = 0x0c;
const DRV_NUM_TCS_MASK: u32 = 0x3f;
const DRV_NUM_TCS_SHIFT: u32 = 6;
const DRV_NCPT_MASK: u32 = 0x1f;
const DRV_NCPT_SHIFT: u32 = 27;

const SLEEP_TCS: u32 = 0;
const WAKE_TCS: u32 = 1;
const ACTIVE_TCS: u32 = 2;
const CONTROL_TCS: u32 = 3;
const TCS_TYPE_COUNT: usize = 4;
const MAX_COMMANDS_PER_TCS: u32 = 16;

const TCS_AMC_MODE_ENABLE: u32 = 1 << 16;
const TCS_AMC_MODE_TRIGGER: u32 = 1 << 24;
const CMD_MSGID_LEN: u32 = 8;
const CMD_MSGID_RESP_REQ: u32 = 1 << 8;
const CMD_MSGID_WRITE: u32 = 1 << 16;
const CMD_STATUS_ISSUED: u32 = 1 << 8;
const CMD_STATUS_COMPLETE: u32 = 1 << 16;
const REGISTER_TIMEOUT_US: u64 = 1_000_000;
const TRANSFER_TIMEOUT_US: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct RegisterLayout {
    tcs_stride: usize,
    command_stride: usize,
    irq_enable: usize,
    irq_status: usize,
    irq_clear: usize,
    command_wait_for_completion: usize,
    control: usize,
    status: usize,
    command_enable: usize,
    command_msgid: usize,
    command_address: usize,
    command_data: usize,
    command_status: usize,
}

const REGISTERS_V2_7: RegisterLayout = RegisterLayout {
    tcs_stride: 672,
    command_stride: 20,
    irq_enable: 0x00,
    irq_status: 0x04,
    irq_clear: 0x08,
    command_wait_for_completion: 0x10,
    control: 0x14,
    status: 0x18,
    command_enable: 0x1c,
    command_msgid: 0x30,
    command_address: 0x34,
    command_data: 0x38,
    command_status: 0x3c,
};

const REGISTERS_V3_0: RegisterLayout = RegisterLayout {
    tcs_stride: 672,
    command_stride: 24,
    irq_enable: 0x00,
    irq_status: 0x04,
    irq_clear: 0x08,
    command_wait_for_completion: 0x20,
    control: 0x24,
    status: 0x28,
    command_enable: 0x2c,
    command_msgid: 0x34,
    command_address: 0x38,
    command_data: 0x3c,
    command_status: 0x40,
};

struct MmioMapping {
    base: usize,
}

impl Drop for MmioMapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

/// A probed Qualcomm application Resource State Coordinator.
pub struct RpmhRsc {
    tcs_base: usize,
    registers: RegisterLayout,
    active_tcs_offset: u32,
    active_tcs_count: u32,
    commands_per_tcs: u32,
    _mapping: MmioMapping,
    lock: IrqSpinLock<()>,
}

/// One command in a synchronous active-only RPMh transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveCommand {
    /// Command DB resource address.
    pub address: u32,
    /// Resource-specific vote payload.
    pub data: u32,
    /// Request a response for this command before the TCS completes.
    pub wait_for_completion: bool,
}

impl RpmhRsc {
    fn read_tcs_register(&self, tcs_id: u32, offset: usize) -> u32 {
        // SAFETY: `tcs_id` is selected from the validated active TCS group.
        unsafe { mmio::read32(self.tcs_register_address(tcs_id, offset)) }
    }

    fn write_tcs_register(&self, tcs_id: u32, offset: usize, value: u32) {
        // SAFETY: see `read_tcs_register`; the register lies inside the mapping.
        unsafe { mmio::write32(self.tcs_register_address(tcs_id, offset), value) }
    }

    fn read_tcs_command(&self, tcs_id: u32, command_id: u32, offset: usize) -> u32 {
        // SAFETY: the transfer path uses command zero and probe validated NCPT.
        unsafe { mmio::read32(self.tcs_command_address(tcs_id, command_id, offset)) }
    }

    fn write_tcs_command(&self, tcs_id: u32, command_id: u32, offset: usize, value: u32) {
        // SAFETY: see `read_tcs_command`; the register lies inside the mapping.
        unsafe { mmio::write32(self.tcs_command_address(tcs_id, command_id, offset), value) }
    }

    fn tcs_register_address(&self, tcs_id: u32, offset: usize) -> usize {
        self.tcs_base + self.registers.tcs_stride * tcs_id as usize + offset
    }

    fn tcs_command_address(&self, tcs_id: u32, command_id: u32, offset: usize) -> usize {
        self.tcs_register_address(tcs_id, offset)
            + self.registers.command_stride * command_id as usize
    }

    fn read_irq_register(&self, offset: usize) -> u32 {
        // SAFETY: global IRQ registers are at the validated TCS base.
        unsafe { mmio::read32(self.tcs_base + offset) }
    }

    fn write_irq_register(&self, offset: usize, value: u32) {
        // SAFETY: see `read_irq_register`.
        unsafe { mmio::write32(self.tcs_base + offset, value) }
    }

    fn write_tcs_register_sync(
        &self,
        tcs_id: u32,
        offset: usize,
        value: u32,
    ) -> Result<(), &'static str> {
        self.write_tcs_register(tcs_id, offset, value);
        arch::io_wmb();
        let start = time::current_time();
        loop {
            if self.read_tcs_register(tcs_id, offset) == value {
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= REGISTER_TIMEOUT_US {
                return Err("qcom-rpmh-rsc: register write did not become visible");
            }
            time::udelay(1);
        }
    }

    fn clear_irq_status(&self, irq_mask: u32) -> Result<(), &'static str> {
        if self.read_irq_register(self.registers.irq_status) & irq_mask == 0 {
            return Ok(());
        }
        self.write_irq_register(self.registers.irq_clear, irq_mask);
        arch::io_wmb();
        let start = time::current_time();
        while self.read_irq_register(self.registers.irq_status) & irq_mask != 0 {
            if time::current_time().saturating_sub(start) >= REGISTER_TIMEOUT_US {
                return Err("qcom-rpmh-rsc: stale completion IRQ did not clear");
            }
            time::udelay(1);
        }
        Ok(())
    }

    fn clear_trigger(&self, tcs_id: u32) -> Result<(), &'static str> {
        let control =
            self.read_tcs_register(tcs_id, self.registers.control) & !TCS_AMC_MODE_TRIGGER;
        self.write_tcs_register_sync(tcs_id, self.registers.control, control)?;
        self.write_tcs_register_sync(
            tcs_id,
            self.registers.control,
            control & !TCS_AMC_MODE_ENABLE,
        )
    }

    fn trigger(&self, tcs_id: u32) -> Result<(), &'static str> {
        self.write_tcs_register_sync(tcs_id, self.registers.control, TCS_AMC_MODE_ENABLE)?;
        self.write_tcs_register(
            tcs_id,
            self.registers.control,
            TCS_AMC_MODE_ENABLE | TCS_AMC_MODE_TRIGGER,
        );
        arch::io_wmb();
        Ok(())
    }

    /// Send one synchronous active-only RPMh write.
    ///
    /// The call owns one active TCS until hardware reports completion.  It is
    /// intentionally polling: SC7180 needs its first CX vote while critical
    /// devices are still being populated, before Scarlet enables interrupts.
    ///
    /// # Arguments
    ///
    /// * `address` - Command DB resource address.
    /// * `data` - Resource vote (for ARC resources, a hardware corner index).
    ///
    /// # Returns
    ///
    /// Success after the RSC completion status is acknowledged.
    pub fn write_active(&self, address: u32, data: u32) -> Result<(), &'static str> {
        self.write_active_batch(&[ActiveCommand {
            address,
            data,
            wait_for_completion: true,
        }])
    }

    /// Send a synchronous active-only RPMh command batch in one TCS.
    ///
    /// BCM interconnect votes must commit every resource in a virtual clock
    /// domain atomically.  Keeping the commands in one TCS preserves that
    /// firmware contract and avoids transiently applying a partial bandwidth
    /// vote.  At least one command must request a response so completion is an
    /// observable boundary.
    pub fn write_active_batch(&self, commands: &[ActiveCommand]) -> Result<(), &'static str> {
        let _guard = self.lock.lock();
        if self.active_tcs_count == 0 || self.commands_per_tcs == 0 {
            return Err("qcom-rpmh-rsc: no active TCS available");
        }
        if commands.is_empty() {
            return Err("qcom-rpmh-rsc: active request has no commands");
        }
        if commands.len() > self.commands_per_tcs as usize {
            return Err("qcom-rpmh-rsc: active request exceeds TCS command capacity");
        }

        let tcs_id = self.active_tcs_offset;
        let irq_mask = 1u32
            .checked_shl(tcs_id)
            .ok_or("qcom-rpmh-rsc: active TCS id is out of range")?;
        if self.read_tcs_register(tcs_id, self.registers.status) == 0 {
            return Err("qcom-rpmh-rsc: active TCS is busy");
        }

        let command_mask = (1u32 << commands.len()) - 1;
        let mut wait_mask = 0u32;
        for (index, command) in commands.iter().enumerate() {
            if command.wait_for_completion {
                wait_mask |= 1u32 << index;
            }
        }
        if wait_mask == 0 {
            return Err("qcom-rpmh-rsc: synchronous request has no completion command");
        }

        #[cfg(debug_assertions)]
        early_println!(
            "[qcom-rpmh-rsc] active batch begin tcs={} commands={} wait={:#x}",
            tcs_id,
            commands.len(),
            wait_mask,
        );

        self.clear_trigger(tcs_id)?;
        self.write_tcs_register_sync(tcs_id, self.registers.command_enable, 0)?;
        self.clear_irq_status(irq_mask)?;

        for (index, command) in commands.iter().enumerate() {
            let command_id = index as u32;
            let mut message_id = CMD_MSGID_LEN | CMD_MSGID_WRITE;
            if command.wait_for_completion {
                message_id |= CMD_MSGID_RESP_REQ;
            }
            self.write_tcs_command(tcs_id, command_id, self.registers.command_msgid, message_id);
            self.write_tcs_command(
                tcs_id,
                command_id,
                self.registers.command_address,
                command.address,
            );
            self.write_tcs_command(
                tcs_id,
                command_id,
                self.registers.command_data,
                command.data,
            );
        }
        self.write_tcs_register_sync(tcs_id, self.registers.command_enable, command_mask)?;
        // A response-request bit in MSGID asks the accelerator to acknowledge
        // each VCD commit command, while CMD_WAIT_FOR_CMPL holds TCS completion
        // until those acknowledgements arrive.
        self.write_tcs_register_sync(
            tcs_id,
            self.registers.command_wait_for_completion,
            wait_mask,
        )?;
        self.trigger(tcs_id)?;
        #[cfg(debug_assertions)]
        early_println!("[qcom-rpmh-rsc] active batch triggered tcs={}", tcs_id);

        // CMD_STATUS is sticky across TCS reuse and may still contain the
        // bootloader's ISSUED/COMPL bits when Scarlet takes ownership.  Wait
        // for this transfer's IRQ status instead; it was cleared immediately
        // before the trigger and is therefore an unambiguous completion
        // boundary.  This mirrors Linux's active-TCS completion path.
        let start = time::current_time();
        loop {
            if self.read_irq_register(self.registers.irq_status) & irq_mask != 0 {
                break;
            }
            if time::current_time().saturating_sub(start) >= TRANSFER_TIMEOUT_US {
                let _ = self.clear_trigger(tcs_id);
                let _ = self.write_tcs_register_sync(tcs_id, self.registers.command_enable, 0);
                return Err("qcom-rpmh-rsc: active request timed out");
            }
            time::udelay(1);
        }
        let mut acknowledged = true;
        for index in 0..commands.len() {
            if wait_mask & (1u32 << index) == 0 {
                continue;
            }
            let command_status =
                self.read_tcs_command(tcs_id, index as u32, self.registers.command_status);
            acknowledged &= command_status & (CMD_STATUS_ISSUED | CMD_STATUS_COMPLETE)
                == (CMD_STATUS_ISSUED | CMD_STATUS_COMPLETE);
        }

        self.clear_trigger(tcs_id)?;
        self.write_tcs_register_sync(tcs_id, self.registers.command_enable, 0)?;
        self.write_tcs_register_sync(tcs_id, self.registers.command_wait_for_completion, 0)?;
        self.clear_irq_status(irq_mask)?;

        if !acknowledged {
            return Err("qcom-rpmh-rsc: request completed without command acknowledgement");
        }
        #[cfg(debug_assertions)]
        early_println!(
            "[qcom-rpmh-rsc] active batch complete tcs={} commands={}",
            tcs_id,
            commands.len(),
        );
        Ok(())
    }
}

static CONTROLLERS: IrqSpinLock<Vec<(u32, Arc<RpmhRsc>)>> = IrqSpinLock::new(Vec::new());

/// Look up an RSC controller by its firmware phandle.
///
/// # Arguments
///
/// * `phandle` - Parent RSC node phandle recorded in `PlatformDeviceInfo`.
///
/// # Returns
///
/// The controller after probe, or `None` while the parent is unavailable.
pub fn controller(phandle: u32) -> Option<Arc<RpmhRsc>> {
    CONTROLLERS
        .lock()
        .iter()
        .find(|(registered, _)| *registered == phandle)
        .map(|(_, controller)| Arc::clone(controller))
}

fn read_be_cells(property: &[u8]) -> Result<Vec<u32>, &'static str> {
    if property.len() % 4 != 0 {
        return Err("qcom-rpmh-rsc: malformed cell property");
    }
    Ok(property
        .chunks_exact(4)
        .map(|chunk| u32::from_be_bytes(chunk.try_into().unwrap_or([0; 4])))
        .collect())
}

fn read_u32_property(device: &PlatformDeviceInfo, name: &str) -> Result<u32, &'static str> {
    let property = device
        .property(name)
        .ok_or("qcom-rpmh-rsc: required property is missing")?;
    let cells = read_be_cells(property.value())?;
    let [value] = cells.as_slice() else {
        return Err("qcom-rpmh-rsc: property must contain one cell");
    };
    Ok(*value)
}

fn resource_for_driver<'a>(
    device: &'a PlatformDeviceInfo,
    driver_id: u32,
) -> Result<&'a scarlet::device::platform::resource::PlatformDeviceResource, &'static str> {
    let target_name = match driver_id {
        0 => "drv-0",
        1 => "drv-1",
        2 => "drv-2",
        _ => return Err("qcom-rpmh-rsc: unsupported driver id"),
    };
    let names = device
        .property("reg-names")
        .and_then(|property| property.as_string_list())
        .ok_or("qcom-rpmh-rsc: missing reg-names")?;
    let index = names
        .iter()
        .position(|name| *name == target_name)
        .ok_or("qcom-rpmh-rsc: driver register window is missing")?;
    device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .nth(index)
        .ok_or("qcom-rpmh-rsc: driver MMIO resource is missing")
}

fn parse_active_tcs(device: &PlatformDeviceInfo, max_tcs: u32) -> Result<(u32, u32), &'static str> {
    let property = device
        .property("qcom,tcs-config")
        .ok_or("qcom-rpmh-rsc: missing TCS configuration")?;
    let config = read_be_cells(property.value())?;
    if config.len() != TCS_TYPE_COUNT * 2 {
        return Err("qcom-rpmh-rsc: invalid TCS configuration length");
    }

    let mut seen = [false; TCS_TYPE_COUNT];
    let mut offset = 0u32;
    let mut active = None;
    for entry in config.chunks_exact(2) {
        let kind = entry[0];
        let count = entry[1];
        let kind_index = usize::try_from(kind).map_err(|_| "qcom-rpmh-rsc: invalid TCS type")?;
        if kind_index >= TCS_TYPE_COUNT || seen[kind_index] {
            return Err("qcom-rpmh-rsc: invalid or duplicate TCS type");
        }
        seen[kind_index] = true;
        if count > 3 {
            return Err("qcom-rpmh-rsc: TCS group is too large");
        }
        if kind == CONTROL_TCS || count == 0 {
            continue;
        }
        if kind == ACTIVE_TCS {
            active = Some((offset, count));
        } else if kind != SLEEP_TCS && kind != WAKE_TCS {
            return Err("qcom-rpmh-rsc: unsupported TCS type");
        }
        offset = offset
            .checked_add(count)
            .ok_or("qcom-rpmh-rsc: TCS count overflow")?;
    }
    if offset > max_tcs || offset >= 32 {
        return Err("qcom-rpmh-rsc: TCS configuration exceeds hardware");
    }
    active.ok_or("qcom-rpmh-rsc: active TCS group is missing")
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    read_u32_property(device, "phandle").or_else(|_| read_u32_property(device, "linux,phandle"))
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let driver_id = read_u32_property(device, "qcom,drv-id")?;
    let tcs_offset = usize::try_from(read_u32_property(device, "qcom,tcs-offset")?)
        .map_err(|_| "qcom-rpmh-rsc: TCS offset does not fit usize")?;
    let phandle = read_phandle(device)?;
    let resource = resource_for_driver(device, driver_id)?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|span| span.checked_add(1))
        .ok_or("qcom-rpmh-rsc: invalid MMIO resource")?;
    let base = vm::ioremap(resource.start, size).map_err(|_| "qcom-rpmh-rsc: ioremap failed")?;
    let mapping = MmioMapping { base };

    early_println!(
        "[qcom-rpmh-rsc] mapped drv-{} paddr={:#x} vaddr={:#x} size={:#x}; reading RSC id",
        driver_id,
        resource.start,
        base,
        size,
    );
    // SAFETY: `base` covers the selected `drv-N` register resource.
    let rsc_id = unsafe { mmio::read32(base + RSC_DRV_ID) };
    let major = (rsc_id >> VERSION_MAJOR_SHIFT) & VERSION_MASK;
    let minor = (rsc_id >> VERSION_MINOR_SHIFT) & VERSION_MASK;
    let registers = if major >= 3 {
        REGISTERS_V3_0
    } else {
        REGISTERS_V2_7
    };

    // SAFETY: DRV_PRNT_CHLD_CONFIG is part of every supported RSC layout.
    let hardware_config = unsafe { mmio::read32(base + DRV_PRNT_CHLD_CONFIG) };
    let shift = DRV_NUM_TCS_SHIFT
        .checked_mul(driver_id)
        .ok_or("qcom-rpmh-rsc: driver id shift overflow")?;
    let max_tcs = hardware_config.checked_shr(shift).unwrap_or(0) & DRV_NUM_TCS_MASK;
    let commands_per_tcs = (hardware_config >> DRV_NCPT_SHIFT) & DRV_NCPT_MASK;
    early_println!(
        "[qcom-rpmh-rsc] RSC id={:#010x} parent/child={:#010x}",
        rsc_id,
        hardware_config,
    );
    if max_tcs == 0 {
        return Err("qcom-rpmh-rsc: hardware reports no TCS blocks");
    }
    if commands_per_tcs == 0 || commands_per_tcs > MAX_COMMANDS_PER_TCS {
        return Err("qcom-rpmh-rsc: invalid commands-per-TCS value");
    }
    let (active_tcs_offset, active_tcs_count) = parse_active_tcs(device, max_tcs)?;
    let last_tcs =
        usize::try_from(max_tcs - 1).map_err(|_| "qcom-rpmh-rsc: TCS count does not fit usize")?;
    let last_command = usize::try_from(commands_per_tcs - 1)
        .map_err(|_| "qcom-rpmh-rsc: command count does not fit usize")?;
    let required_size = tcs_offset
        .checked_add(
            registers
                .tcs_stride
                .checked_mul(last_tcs)
                .ok_or("qcom-rpmh-rsc: TCS window overflow")?,
        )
        .and_then(|offset| offset.checked_add(registers.command_stride.checked_mul(last_command)?))
        .and_then(|offset| offset.checked_add(registers.command_status))
        .and_then(|offset| offset.checked_add(core::mem::size_of::<u32>()))
        .ok_or("qcom-rpmh-rsc: register window overflow")?;
    if required_size > size {
        return Err("qcom-rpmh-rsc: MMIO resource is smaller than TCS geometry");
    }
    let tcs_base = base
        .checked_add(tcs_offset)
        .ok_or("qcom-rpmh-rsc: TCS base overflow")?;

    let controller = Arc::new(RpmhRsc {
        tcs_base,
        registers,
        active_tcs_offset,
        active_tcs_count,
        commands_per_tcs,
        _mapping: mapping,
        lock: IrqSpinLock::new(()),
    });
    let active_mask = ((1u32 << active_tcs_count) - 1) << active_tcs_offset;
    controller.write_irq_register(registers.irq_enable, active_mask);
    arch::io_wmb();

    let previous = {
        let mut controllers = CONTROLLERS.lock();
        if let Some(index) = controllers
            .iter()
            .position(|(registered, _)| *registered == phandle)
        {
            Some(core::mem::replace(
                &mut controllers[index].1,
                Arc::clone(&controller),
            ))
        } else {
            controllers.push((phandle, Arc::clone(&controller)));
            None
        }
    };
    drop(previous);

    early_println!(
        "[qcom-rpmh-rsc] registered phandle={:#x} drv={} version={}.{} active-tcs={}..{} ncpt={}",
        phandle,
        driver_id,
        major,
        minor,
        active_tcs_offset,
        active_tcs_offset + active_tcs_count - 1,
        commands_per_tcs,
    );
    Ok(())
}

fn remove(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = read_phandle(device)?;
    let removed = {
        let mut controllers = CONTROLLERS.lock();
        controllers
            .iter()
            .position(|(registered, _)| *registered == phandle)
            .map(|index| controllers.remove(index).1)
    };
    drop(removed);
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-rpmh-rsc",
            probe,
            remove,
            vec!["qcom,rpmh-rsc"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_RPMH_RSC_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
