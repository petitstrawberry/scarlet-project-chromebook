// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 glue for the v5 SDHCI eMMC controller.
//!
//! The generic Scarlet SDHCI implementation supplies the polling/PIO command
//! engine. This crate handles the SC7180 platform binding, clocks, and the v5
//! vendor-register initialization required before the generic host reset.

extern crate alloc;

mod binding;

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::ptr::{read_volatile, write_volatile};

use binding::*;

use scarlet::interrupt::resolve_platform_irq;
use scarlet::{
    device::{
        Device,
        clk::ClkHandle,
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority, is_probe_defer, probe_defer},
        mmc::{MmcBusWidth, MmcCommand, MmcData, MmcError, MmcHost, MmcResponse, MmcResult},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo,
            resource::{PlatformDeviceResource, PlatformDeviceResourceType},
        },
    },
    drivers::mmc::{
        core::EmmcBlockDevice,
        sdhci::{SdhciHost, SdhciHostConfig},
    },
    early_println, println, vm,
};

const DRIVER_NAME: &str = "qcom-sdhci-sc7180";
const DEVICE_NAME: &str = "mmcblk0";
const PREFERRED_CORE_CLOCK_HZ: u64 = 100_000_000;
const CLOCK_NAMES: [&str; 3] = ["iface", "core", "xo"];

const CLOCK_PROVIDER_NOT_FOUND: &str = "clk: provider not found";

const SDHCI_PRESENT_STATE: usize = 0x24;
const SDHCI_HOST_CONTROL: usize = 0x28;
const SDHCI_BLOCK_SIZE_COUNT: usize = 0x04;
const SDHCI_ARGUMENT: usize = 0x08;
const SDHCI_TRANSFER_COMMAND: usize = 0x0c;
const SDHCI_CLOCK_CONTROL: usize = 0x2c;
const SDHCI_SOFTWARE_RESET: usize = 0x2f;
const SDHCI_INTERRUPT_STATUS: usize = 0x30;
const SDHCI_HOST_CONTROL2: usize = 0x3e;

/// Host wrapper retaining platform resources for the registered eMMC lifetime.
struct Sc7180SdhciHost {
    host: SdhciHost,
    mmio_base: usize,
    cqhci_base: usize,
    clocks: Vec<ClkHandle>,
    pwr_irq: u32,
    handoff_power_control: u8,
    handoff_power_requests: u32,
    inherited_host_control: u8,
    inherited_host_control2: u16,
}

impl Sc7180SdhciHost {
    fn log_cmd8_state(&self, phase: &str) {
        early_println!(
            "[qcom-sdhci-sc7180] CMD8 {}: fifo={:#010x} mci_status={:#010x} debug={:#010x} data_count={:#010x} host={:#04x}(inherited {:#04x}) host2={:#06x}(inherited {:#06x})",
            phase,
            read32(self.mmio_base, CORE_MCI_FIFO_CNT),
            read32(self.mmio_base, CORE_MCI_STATUS),
            read32(self.mmio_base, CORE_SDCC_DEBUG_REG),
            read32(self.mmio_base, CORE_MCI_DATA_CNT),
            read8(self.mmio_base, SDHCI_HOST_CONTROL),
            self.inherited_host_control,
            read16(self.mmio_base, SDHCI_HOST_CONTROL2),
            self.inherited_host_control2,
        );
    }

    fn log_failure(&self, operation: &str) {
        early_println!(
            "[qcom-sdhci-sc7180] {} failed: reset={:#04x} clock={:#06x} host={:#04x}/{:#04x} host2={:#06x}/{:#06x} block={:#010x} arg={:#010x} xfer_cmd={:#010x} present={:#010x} irq={:#010x} sdhci_pwr={:#04x} pwr_irq={} pwr={:#x}/{:#x}/{:#x} cqhci={:#x}/{:#x}/{:#x} func4={:#x}",
            operation,
            read8(self.mmio_base, SDHCI_SOFTWARE_RESET),
            read16(self.mmio_base, SDHCI_CLOCK_CONTROL),
            read8(self.mmio_base, SDHCI_HOST_CONTROL),
            self.inherited_host_control,
            read16(self.mmio_base, SDHCI_HOST_CONTROL2),
            self.inherited_host_control2,
            read32(self.mmio_base, SDHCI_BLOCK_SIZE_COUNT),
            read32(self.mmio_base, SDHCI_ARGUMENT),
            read32(self.mmio_base, SDHCI_TRANSFER_COMMAND),
            read32(self.mmio_base, SDHCI_PRESENT_STATE),
            read32(self.mmio_base, SDHCI_INTERRUPT_STATUS),
            read8(self.mmio_base, SDHCI_POWER_CONTROL),
            self.pwr_irq,
            read32(self.mmio_base, CORE_PWRCTL_STATUS),
            read32(self.mmio_base, CORE_PWRCTL_MASK),
            read32(self.mmio_base, CORE_PWRCTL_CTL),
            read32(self.cqhci_base, CQHCI_CFG),
            read32(self.cqhci_base, CQHCI_CTL),
            read32(self.cqhci_base, NONCQ_CRYPTO_PARM),
            read32(self.mmio_base, HC_VENDOR_SPECIFIC_FUNC4),
        );
    }
}

impl Drop for Sc7180SdhciHost {
    fn drop(&mut self) {
        for clock in self.clocks.iter().rev() {
            clock.disable_unprepare();
        }
        vm::iounmap(self.cqhci_base);
        vm::iounmap(self.mmio_base);
    }
}

impl MmcHost for Sc7180SdhciHost {
    fn reset(&mut self) -> MmcResult<()> {
        let result = self.host.reset();
        let power_control = read8(self.mmio_base, SDHCI_POWER_CONTROL);
        let power_requests = read32(self.mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK;
        if result.is_err() {
            self.log_failure("host reset");
        }
        result?;
        if power_control != self.handoff_power_control
            || power_requests != self.handoff_power_requests
        {
            self.log_failure("firmware power handoff verification");
            early_println!(
                "[qcom-sdhci-sc7180] power handoff changed: SDHCI {:#04x}->{:#04x}, request {:#x}->{:#x}",
                self.handoff_power_control,
                power_control,
                self.handoff_power_requests,
                power_requests
            );
            return Err(MmcError::Command);
        }
        Ok(())
    }

    fn set_clock(&mut self, frequency_hz: u32) -> MmcResult<()> {
        let result = self.host.set_clock(frequency_hz);
        if result.is_err() {
            self.log_failure("clock setup");
        }
        result
    }

    fn set_bus_width(&mut self, width: MmcBusWidth) -> MmcResult<()> {
        self.host.set_bus_width(width)
    }

    fn card_present(&self) -> bool {
        self.host.card_present()
    }

    fn is_removable(&self) -> bool {
        self.host.is_removable()
    }

    fn send_command(
        &mut self,
        command: MmcCommand,
        data: Option<MmcData<'_>>,
    ) -> MmcResult<MmcResponse> {
        let command_index = command.index();
        let command_argument = command.argument();
        if command_index == 8 {
            self.log_cmd8_state("pre");
        }
        let result = self.host.send_command(command, data);
        if command_index == 8 {
            self.log_cmd8_state("post");
            if let Ok(response) = &result {
                early_println!(
                    "[qcom-sdhci-sc7180] CMD8 response={:#010x}/{:#010x}/{:#010x}/{:#010x}",
                    response.word(0),
                    response.word(1),
                    response.word(2),
                    response.word(3),
                );
            }
        }
        if let Err(error) = &result {
            self.log_failure("command");
            early_println!(
                "[qcom-sdhci-sc7180] command index={} argument={:#010x} error={}",
                command_index,
                command_argument,
                error.as_str()
            );
        }
        result
    }
}

fn validate_emmc_node(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let non_removable = device.property("non-removable").is_some();
    let bus_width = device
        .property("bus-width")
        .and_then(|property| property.as_usize());
    if accepts_emmc_properties(non_removable, bus_width) {
        Ok(())
    } else {
        Err("qcom-sdhci-sc7180: only non-removable 8-bit eMMC is supported")
    }
}

fn read_be_u32(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.get(..4)?.try_into().ok()?))
}

fn fixed_supply_voltage_uv(
    device: &PlatformDeviceInfo,
    property_name: &str,
) -> Result<(u32, u32), &'static str> {
    let phandle = device
        .property(property_name)
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-sdhci-sc7180: vqmmc-supply phandle is missing")?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("qcom-sdhci-sc7180: FDT is unavailable")?;
    let supply = fdt
        .all_nodes()
        .find(|node| {
            ["phandle", "linux,phandle"].iter().any(|name| {
                node.property(name)
                    .and_then(|property| read_be_u32(property.value))
                    == Some(phandle)
            })
        })
        .ok_or("qcom-sdhci-sc7180: vqmmc-supply node is missing")?;
    let minimum_uv = supply
        .property("regulator-min-microvolt")
        .and_then(|property| read_be_u32(property.value))
        .ok_or("qcom-sdhci-sc7180: vqmmc minimum voltage is missing")?;
    let maximum_uv = supply
        .property("regulator-max-microvolt")
        .and_then(|property| read_be_u32(property.value))
        .ok_or("qcom-sdhci-sc7180: vqmmc maximum voltage is missing")?;
    Ok((minimum_uv, maximum_uv))
}

fn resolve_named_irq(device: &PlatformDeviceInfo, name: &str) -> Result<u32, &'static str> {
    let names = device
        .property("interrupt-names")
        .ok_or("qcom-sdhci-sc7180: interrupt-names is missing")?;
    let index =
        string_list_index(names.value(), name).ok_or("qcom-sdhci-sc7180: pwr_irq is not named")?;
    let resource = device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .nth(index)
        .ok_or("qcom-sdhci-sc7180: named pwr_irq resource is missing")?;
    resolve_platform_irq(resource).map_err(|_| "qcom-sdhci-sc7180: failed to resolve pwr_irq")
}

fn resolve_named_memory<'a>(
    device: &'a PlatformDeviceInfo,
    name: &str,
) -> Result<&'a PlatformDeviceResource, &'static str> {
    let names = device
        .property("reg-names")
        .ok_or("qcom-sdhci-sc7180: reg-names is missing")?;
    let index = string_list_index(names.value(), name)
        .ok_or("qcom-sdhci-sc7180: named memory resource is missing")?;
    device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .nth(index)
        .ok_or("qcom-sdhci-sc7180: named memory resource index is missing")
}

fn resource_size(resource: &PlatformDeviceResource) -> Option<usize> {
    resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
}

fn resolve_and_enable_clocks(
    device: &PlatformDeviceInfo,
) -> Result<(Vec<ClkHandle>, u32), &'static str> {
    let manager = DeviceManager::get_manager();
    let mut clocks = Vec::with_capacity(CLOCK_NAMES.len());

    for name in CLOCK_NAMES {
        let clock = match manager.resolve_clk(device, name) {
            Ok(clock) => clock,
            Err(error) if is_probe_defer(error) || error == CLOCK_PROVIDER_NOT_FOUND => {
                return probe_defer();
            }
            Err(_) => return Err("qcom-sdhci-sc7180: failed to resolve required clock"),
        };
        clocks.push(clock);
    }

    let actual_rate = clocks[1]
        .set_rate(PREFERRED_CORE_CLOCK_HZ)
        .map_err(|_| "qcom-sdhci-sc7180: failed to set core clock rate")?;
    let actual_rate =
        u32::try_from(actual_rate).map_err(|_| "qcom-sdhci-sc7180: core clock rate exceeds u32")?;

    for (index, clock) in clocks.iter().enumerate() {
        if clock.prepare_enable().is_err() {
            disable_clocks(&clocks[..index]);
            return Err("qcom-sdhci-sc7180: failed to enable required clock");
        }
    }

    Ok((clocks, actual_rate))
}

fn disable_clocks(clocks: &[ClkHandle]) {
    for clock in clocks.iter().rev() {
        clock.disable_unprepare();
    }
}

fn read32(base: usize, offset: usize) -> u32 {
    // SAFETY: The caller supplies a mapped controller aperture, and every
    // offset used here is aligned and validated to lie within that aperture.
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn read16(base: usize, offset: usize) -> u16 {
    // SAFETY: The caller supplies the mapped first SDHCI resource, and every
    // offset used here is aligned and validated to lie within that resource.
    unsafe { core::ptr::read_volatile((base + offset) as *const u16) }
}

fn read8(base: usize, offset: usize) -> u8 {
    // SAFETY: The caller supplies the mapped first SDHCI resource, and every
    // offset used here is validated to lie within that resource.
    unsafe { core::ptr::read_volatile((base + offset) as *const u8) }
}

fn write32(base: usize, offset: usize, value: u32) {
    // SAFETY: The caller supplies a mapped controller aperture, and every
    // offset used here is aligned and validated to lie within that aperture.
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}

fn isolate_legacy_pio(hc_base: usize, cqhci_base: usize) -> Result<(), &'static str> {
    let inherited_cfg = read32(cqhci_base, CQHCI_CFG);
    let inherited_ctl = read32(cqhci_base, CQHCI_CTL);
    let inherited_noncq_crypto = read32(cqhci_base, NONCQ_CRYPTO_PARM);
    let inherited_func4 = read32(hc_base, HC_VENDOR_SPECIFIC_FUNC4);
    println!(
        "[qcom-sdhci-sc7180] inherited CQHCI cfg={:#010x} ctl={:#010x} noncq_crypto={:#010x} func4={:#010x}",
        inherited_cfg, inherited_ctl, inherited_noncq_crypto, inherited_func4
    );

    write32(
        cqhci_base,
        CQHCI_CFG,
        legacy_pio_cqhci_config(inherited_cfg),
    );
    write32(cqhci_base, NONCQ_CRYPTO_PARM, 0);
    write32(
        hc_base,
        HC_VENDOR_SPECIFIC_FUNC4,
        legacy_pio_func4(inherited_func4),
    );

    let configured_cfg = read32(cqhci_base, CQHCI_CFG);
    let configured_noncq_crypto = read32(cqhci_base, NONCQ_CRYPTO_PARM);
    let configured_func4 = read32(hc_base, HC_VENDOR_SPECIFIC_FUNC4);
    if configured_cfg & (CQHCI_ENABLE | CQHCI_CRYPTO_GENERAL_ENABLE) != 0
        || configured_noncq_crypto != 0
        || configured_func4 & HC_DISABLE_CRYPTO == 0
    {
        return Err("qcom-sdhci-sc7180: failed to isolate legacy PIO from CQHCI/crypto");
    }
    println!(
        "[qcom-sdhci-sc7180] legacy PIO isolated: CQHCI cfg={:#010x} ctl={:#010x} noncq_crypto={:#010x} func4={:#010x}",
        configured_cfg,
        read32(cqhci_base, CQHCI_CTL),
        configured_noncq_crypto,
        configured_func4
    );
    Ok(())
}

fn service_satisfied_bus_on(mmio_base: usize) -> Result<(), &'static str> {
    write32(mmio_base, CORE_PWRCTL_CLEAR, CORE_PWRCTL_BUS_ON);
    for _ in 0..10 {
        if read32(mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_BUS_ON == 0 {
            break;
        }
        write32(mmio_base, CORE_PWRCTL_CLEAR, CORE_PWRCTL_BUS_ON);
        scarlet::time::udelay(10);
    }

    let after_clear = read32(mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK;
    if after_clear != 0 {
        return Err("qcom-sdhci-sc7180: BUS_ON request did not clear");
    }

    // The firmware-owned rails are already on and DeviceManager applied the
    // default pinctrl state before probe, so BUS_ON describes the current
    // physical state. No other request is acknowledged by this driver.
    write32(mmio_base, CORE_PWRCTL_CTL, CORE_PWRCTL_BUS_SUCCESS);
    if read32(mmio_base, CORE_PWRCTL_MASK) != CORE_PWRCTL_INTERRUPTS_DISABLED {
        return Err("qcom-sdhci-sc7180: pwr_irq mask changed during BUS_ON service");
    }
    if read32(mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK != 0 {
        return Err("qcom-sdhci-sc7180: new request appeared after BUS_ON acknowledgement");
    }
    Ok(())
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    validate_emmc_node(device)?;
    let (vqmmc_minimum_uv, vqmmc_maximum_uv) = fixed_supply_voltage_uv(device, "vqmmc-supply")?;
    let vendor_spec = vendor_spec_for_fixed_1v8_supply(
        CORE_VENDOR_SPEC_POR_VAL,
        vqmmc_minimum_uv,
        vqmmc_maximum_uv,
    )
    .ok_or("qcom-sdhci-sc7180: only fixed 1.8 V vqmmc is currently supported")?;
    // The active CoachZ DT orders `hc_irq` before `pwr_irq`; select by the
    // firmware name rather than accidentally taking the command IRQ.
    let pwr_irq = resolve_named_irq(device, "pwr_irq")?;
    let hc_resource = resolve_named_memory(device, "hc")?;
    let cqhci_resource = resolve_named_memory(device, "cqhci")?;
    let hc_size = resource_size(hc_resource)
        .filter(|size| *size >= REQUIRED_MMIO_SIZE)
        .ok_or("qcom-sdhci-sc7180: hc memory resource is too small")?;
    let cqhci_size = resource_size(cqhci_resource)
        .filter(|size| *size >= REQUIRED_CQHCI_MMIO_SIZE)
        .ok_or("qcom-sdhci-sc7180: cqhci memory resource is too small")?;

    let (clocks, base_clock_hz) = resolve_and_enable_clocks(device)?;
    let mmio_base = match vm::ioremap(hc_resource.start, hc_size) {
        Ok(base) => base,
        Err(_) => {
            disable_clocks(&clocks);
            return Err("qcom-sdhci-sc7180: failed to map hc memory resource");
        }
    };

    let initial_power_mask = read32(mmio_base, CORE_PWRCTL_MASK);
    let initial_power_control = read32(mmio_base, CORE_PWRCTL_CTL);
    // Linux services pwr_irq only after switching VMMC/VQMMC and pinctrl to
    // the requested physical state. Scarlet applies the default pinctrl state
    // before probe but has no regulator-consumer API, so it cannot perform or
    // verify BUS_OFF or IO_LOW/HIGH transitions. Mask the dedicated IRQ before
    // further programming. The synchronous handoff below may acknowledge only
    // BUS_ON when the inherited SDHCI register confirms power is already on;
    // every request requiring a physical transition is rejected. This keeps
    // CoachZ firmware's always-on 2.9 V/1.8 V eMMC rail state aligned with the
    // controller.
    write32(mmio_base, CORE_PWRCTL_MASK, CORE_PWRCTL_INTERRUPTS_DISABLED);
    if read32(mmio_base, CORE_PWRCTL_MASK) != CORE_PWRCTL_INTERRUPTS_DISABLED {
        vm::iounmap(mmio_base);
        disable_clocks(&clocks);
        return Err("qcom-sdhci-sc7180: failed to mask unserviced pwr_irq requests");
    }
    // Sample status only after the request source is masked. A request that
    // raced with masking is therefore rejected rather than missed by a stale
    // pre-mask snapshot.
    let post_mask_power_status = read32(mmio_base, CORE_PWRCTL_STATUS);
    let pending_power_requests = post_mask_power_status & CORE_PWRCTL_REQUEST_MASK;
    let handoff_power_control = read8(mmio_base, SDHCI_POWER_CONTROL);
    match handoff_power_action(pending_power_requests, handoff_power_control) {
        HandoffPowerAction::None => {}
        HandoffPowerAction::AcknowledgeBusOn => {
            if let Err(error) = service_satisfied_bus_on(mmio_base) {
                early_println!(
                    "[qcom-sdhci-sc7180] BUS_ON handoff service failed: {} pwr={:#x}/{:#x}/{:#x}",
                    error,
                    read32(mmio_base, CORE_PWRCTL_STATUS),
                    read32(mmio_base, CORE_PWRCTL_MASK),
                    read32(mmio_base, CORE_PWRCTL_CTL)
                );
                vm::iounmap(mmio_base);
                disable_clocks(&clocks);
                return Err(error);
            }
            println!(
                "[qcom-sdhci-sc7180] serviced already-satisfied BUS_ON handoff with mask held at zero"
            );
        }
        HandoffPowerAction::Reject => {
            early_println!(
                "[qcom-sdhci-sc7180] unsafe power handoff rejected: SDHCI={:#04x} request={:#x} mask={:#x} ctl={:#x}",
                handoff_power_control,
                pending_power_requests,
                initial_power_mask,
                initial_power_control
            );
            vm::iounmap(mmio_base);
            disable_clocks(&clocks);
            return Err("qcom-sdhci-sc7180: firmware power handoff is not safely reusable");
        }
    }
    let handoff_power_requests = 0;

    // Shut down any high-speed DLL state inherited from firmware before the
    // generic host starts identification at 400 kHz and legacy transfer at
    // 26 MHz. Qualcomm requires reset and power-down as separate RMW writes.
    let dll_config = read32(mmio_base, CORE_DLL_CONFIG);
    write32(mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_RST);
    let dll_config = read32(mmio_base, CORE_DLL_CONFIG);
    write32(mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_PDN);

    // Restore the v5 vendor POR image, then select the pad voltage that
    // matches the fixed 1.8 V VQMMC supply before the generic software reset.
    // Do not enable CQE, DMA, or interrupt delivery here.
    write32(mmio_base, CORE_VENDOR_SPEC, vendor_spec);
    let configured_vendor_spec = read32(mmio_base, CORE_VENDOR_SPEC);
    if configured_vendor_spec & CORE_IO_PAD_PWR_SWITCH_MASK != CORE_IO_PAD_PWR_SWITCH_MASK {
        vm::iounmap(mmio_base);
        disable_clocks(&clocks);
        return Err("qcom-sdhci-sc7180: failed to select 1.8 V I/O pads");
    }
    let cqhci_base = match vm::ioremap(cqhci_resource.start, cqhci_size) {
        Ok(base) => base,
        Err(_) => {
            vm::iounmap(mmio_base);
            disable_clocks(&clocks);
            return Err("qcom-sdhci-sc7180: failed to map cqhci memory resource");
        }
    };
    if let Err(error) = isolate_legacy_pio(mmio_base, cqhci_base) {
        vm::iounmap(cqhci_base);
        vm::iounmap(mmio_base);
        disable_clocks(&clocks);
        return Err(error);
    }
    let inherited_host_control = read8(mmio_base, SDHCI_HOST_CONTROL);
    let inherited_host_control2 = read16(mmio_base, SDHCI_HOST_CONTROL2);
    let version = read32(mmio_base, CORE_MCI_VERSION);
    println!(
        "[qcom-sdhci-sc7180] SDCC v5 version={:#010x}, core={} Hz, vqmmc={} uV vendor={:#010x}, inherited_host={:#04x} inherited_host2={:#06x}, pwr_irq={} masked, handoff SDHCI={:#04x} pwr={:#x}/{:#x}/{:#x}",
        version,
        base_clock_hz,
        vqmmc_minimum_uv,
        configured_vendor_spec,
        inherited_host_control,
        inherited_host_control2,
        pwr_irq,
        handoff_power_control,
        pending_power_requests,
        initial_power_mask,
        initial_power_control
    );

    let host = Sc7180SdhciHost {
        host: SdhciHost::new_with_base_clock_and_config(
            mmio_base,
            true,
            base_clock_hz,
            SdhciHostConfig {
                single_power_write: true,
                preserve_power_control: true,
            },
        ),
        mmio_base,
        cqhci_base,
        clocks,
        pwr_irq,
        handoff_power_control,
        handoff_power_requests,
        inherited_host_control,
        inherited_host_control2,
    };
    let block_device =
        EmmcBlockDevice::probe_with_bus_width(DEVICE_NAME, Box::new(host), MmcBusWidth::Eight)
            .map_err(|error| {
                early_println!(
                    "[qcom-sdhci-sc7180] failed to identify {}: {}",
                    DEVICE_NAME,
                    error.as_str()
                );
                error.as_str()
            })?;

    let registered: Arc<dyn Device> = Arc::new(block_device);
    DeviceManager::get_manager().register_device_with_name(String::from(DEVICE_NAME), registered);
    println!("[qcom-sdhci-sc7180] registered {}", DEVICE_NAME);
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(DRIVER_NAME, probe, remove, vec!["qcom,sc7180-sdhci"]);
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SDHCI_SC7180_ANCHOR: fn() = force_link;

/// Keep the external SC7180 SDHCI driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
