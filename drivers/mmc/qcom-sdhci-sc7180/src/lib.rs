// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 glue for the v5 SDHCI eMMC and SD controllers.
//!
//! The generic Scarlet SDHCI implementation supplies the polling/PIO command
//! engine. This crate handles the SC7180 platform binding, clocks, and the v5
//! vendor-register initialization required before the generic host reset.
//! SDCC1 is the soldered eMMC card; SDCC2 is the removable SD slot.

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
        gpio::GpioController,
        manager::{DeviceManager, DriverPriority, is_probe_defer, probe_defer},
        mmc::{MmcBusWidth, MmcCommand, MmcData, MmcError, MmcHost, MmcResponse, MmcResult},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo,
            resource::{PlatformDeviceResource, PlatformDeviceResourceType},
        },
    },
    drivers::mmc::{
        core::MmcBlockDevice,
        sdhci::{SdhciHost, SdhciHostConfig},
    },
    println, vm,
};

const DRIVER_NAME: &str = "qcom-sdhci-sc7180";
const EMMC_DEVICE_NAME: &str = "mmcblk0";
const SD_DEVICE_NAME: &str = "mmcblk1";
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

struct CardDetect {
    controller: Arc<dyn GpioController>,
    pin: u32,
    active_low: bool,
}

impl CardDetect {
    fn present(&self) -> bool {
        self.controller.get_value(self.pin) != self.active_low
    }
}

/// Host wrapper retaining platform resources for the registered card lifetime.
struct Sc7180SdhciHost {
    host: SdhciHost,
    kind: MediaKind,
    mmio_base: usize,
    cqhci_base: Option<usize>,
    clocks: Vec<ClkHandle>,
    pwr_irq: u32,
    handoff_power_control: u8,
    handoff_power_requests: u32,
    inherited_host_control: u8,
    inherited_host_control2: u16,
    card_detect: Option<CardDetect>,
}

impl Sc7180SdhciHost {
    fn log_cmd8_state(&self, phase: &str) {
        println!(
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
        let (cqhci_cfg, cqhci_ctl, cqhci_crypto) = self.cqhci_base.map_or((0, 0, 0), |base| {
            (
                read32(base, CQHCI_CFG),
                read32(base, CQHCI_CTL),
                read32(base, NONCQ_CRYPTO_PARM),
            )
        });
        println!(
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
            cqhci_cfg,
            cqhci_ctl,
            cqhci_crypto,
            read32(self.mmio_base, HC_VENDOR_SPECIFIC_FUNC4),
        );
    }
}

impl Drop for Sc7180SdhciHost {
    fn drop(&mut self) {
        for clock in self.clocks.iter().rev() {
            clock.disable_unprepare();
        }
        if let Some(cqhci_base) = self.cqhci_base {
            vm::iounmap(cqhci_base);
        }
        vm::iounmap(self.mmio_base);
    }
}

impl MmcHost for Sc7180SdhciHost {
    fn reset(&mut self) -> MmcResult<()> {
        let result = self.host.reset();
        if result.is_err() {
            self.log_failure("host reset");
        }
        result?;
        if self.kind == MediaKind::Sd {
            // Match U-Boot's SDCC DLL handling: leave the DLL untouched until
            // the controller reset completes, then disable it for the legacy
            // (<= 100 MHz) SD clock. Programming DLL reset/power-down before
            // the host reset can keep SDCC5 stuck in software reset.
            let dll_config = read32(self.mmio_base, CORE_DLL_CONFIG);
            write32(self.mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_RST);
            let dll_config = read32(self.mmio_base, CORE_DLL_CONFIG);
            write32(self.mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_PDN);
        }
        let power_control = read8(self.mmio_base, SDHCI_POWER_CONTROL);
        if self.kind == MediaKind::Sd {
            let power_requests =
                read32(self.mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK;
            if power_requests == CORE_PWRCTL_BUS_ON && power_control & 1 != 0 {
                if let Err(error) = service_satisfied_bus_on(self.mmio_base) {
                    println!(
                        "[qcom-sdhci-sc7180] SD BUS_ON service failed: {} pwr={:#x}/{:#x}/{:#x}",
                        error,
                        read32(self.mmio_base, CORE_PWRCTL_STATUS),
                        read32(self.mmio_base, CORE_PWRCTL_MASK),
                        read32(self.mmio_base, CORE_PWRCTL_CTL),
                    );
                }
            }
            println!(
                "[qcom-sdhci-sc7180] SD power after reset: SDHCI={:#04x} request={:#x}",
                power_control, power_requests
            );
            return Ok(());
        }
        let power_requests = read32(self.mmio_base, CORE_PWRCTL_STATUS) & CORE_PWRCTL_REQUEST_MASK;
        if power_control != self.handoff_power_control
            || power_requests != self.handoff_power_requests
        {
            self.log_failure("firmware power handoff verification");
            println!(
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
        self.card_detect
            .as_ref()
            .map_or_else(|| self.host.card_present(), CardDetect::present)
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
                println!(
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
            println!(
                "[qcom-sdhci-sc7180] command index={} argument={:#010x} error={}",
                command_index,
                command_argument,
                error.as_str()
            );
        }
        result
    }
}

fn classify_node(device: &PlatformDeviceInfo) -> Result<MediaKind, &'static str> {
    let non_removable = device.property("non-removable").is_some();
    let bus_width = device
        .property("bus-width")
        .and_then(|property| property.as_usize());
    let has_card_detect = device.property("cd-gpios").is_some();
    classify_media(non_removable, bus_width, has_card_detect).ok_or(
        "qcom-sdhci-sc7180: expected non-removable 8-bit eMMC or removable 4-bit SD with cd-gpios",
    )
}

fn resolve_card_detect(device: &PlatformDeviceInfo) -> Result<CardDetect, &'static str> {
    let property = device
        .property("cd-gpios")
        .ok_or("qcom-sdhci-sc7180: cd-gpios is missing")?;
    let (controller_phandle, pin, active_low) =
        gpio_spec(property.value()).ok_or("qcom-sdhci-sc7180: malformed cd-gpios specifier")?;
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(controller_phandle)
        .ok_or_else(|| {
            println!(
                "[qcom-sdhci-sc7180] card-detect GPIO controller {:#x} is not ready, deferring",
                controller_phandle
            );
            scarlet::device::manager::PROBE_DEFER
        })?;
    controller.set_direction_input(pin);
    Ok(CardDetect {
        controller,
        pin,
        active_low,
    })
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

fn resolve_optional_named_memory<'a>(
    device: &'a PlatformDeviceInfo,
    name: &str,
) -> Result<Option<&'a PlatformDeviceResource>, &'static str> {
    let Some(names) = device.property("reg-names") else {
        return Ok(None);
    };
    let Some(index) = string_list_index(names.value(), name) else {
        return Ok(None);
    };
    Ok(device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .nth(index))
}

/// Resolve the controller aperture.
///
/// SDCC1 names its two windows (`hc`, `cqhci`), while SDCC2 has a single
/// unnamed `reg`. Select by name when available, otherwise use that sole
/// memory resource.
fn resolve_primary_memory<'a>(
    device: &'a PlatformDeviceInfo,
) -> Result<&'a PlatformDeviceResource, &'static str> {
    let memory: Vec<_> = device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .collect();
    let has_reg_names = device.property("reg-names").is_some();
    let named_hc = device
        .property("reg-names")
        .and_then(|names| string_list_index(names.value(), "hc"));
    let index = primary_memory_index(has_reg_names, named_hc, memory.len())
        .ok_or("qcom-sdhci-sc7180: host memory window is missing or ambiguous")?;
    Ok(memory[index])
}

fn resource_size(resource: &PlatformDeviceResource) -> Option<usize> {
    resource.size().ok()
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
    let kind = classify_node(device)?;
    let card_detect = if kind == MediaKind::Sd {
        let detect = resolve_card_detect(device)?;
        println!(
            "[qcom-sdhci-sc7180] SD card detect GPIO{} active_low={} present={}",
            detect.pin,
            detect.active_low,
            detect.present()
        );
        if !detect.present() {
            println!(
                "[qcom-sdhci-sc7180] no SD card present; skipping {}",
                SD_DEVICE_NAME
            );
            return Ok(());
        }
        Some(detect)
    } else {
        None
    };

    let (vmmc_minimum_uv, vmmc_maximum_uv) = fixed_supply_voltage_uv(device, "vmmc-supply")?;
    let (vqmmc_minimum_uv, vqmmc_maximum_uv) = fixed_supply_voltage_uv(device, "vqmmc-supply")?;
    let (supply_minimum_uv, supply_maximum_uv) = match kind {
        MediaKind::Emmc => (vqmmc_minimum_uv, vqmmc_maximum_uv),
        MediaKind::Sd => (vmmc_minimum_uv, vmmc_maximum_uv),
    };
    let vendor_spec = vendor_spec_for_media(
        kind,
        CORE_VENDOR_SPEC_POR_VAL,
        supply_minimum_uv,
        supply_maximum_uv,
    )
    .ok_or(match kind {
        MediaKind::Emmc => "qcom-sdhci-sc7180: only fixed 1.8 V vqmmc is currently supported",
        MediaKind::Sd => "qcom-sdhci-sc7180: SD vmmc must cover 2.7 V through 3.6 V",
    })?;
    // The active CoachZ DT orders `hc_irq` before `pwr_irq`; select by the
    // firmware name rather than accidentally taking the command IRQ.
    let pwr_irq = resolve_named_irq(device, "pwr_irq")?;
    let hc_resource = resolve_primary_memory(device)?;
    let cqhci_resource = resolve_optional_named_memory(device, "cqhci")?;
    let hc_size = resource_size(hc_resource)
        .filter(|size| *size >= REQUIRED_MMIO_SIZE)
        .ok_or("qcom-sdhci-sc7180: hc memory resource is too small")?;

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
    let handoff_power_requests = 0;
    if kind == MediaKind::Emmc {
        match handoff_power_action(pending_power_requests, handoff_power_control) {
            HandoffPowerAction::None => {}
            HandoffPowerAction::AcknowledgeBusOn => {
                if let Err(error) = service_satisfied_bus_on(mmio_base) {
                    println!(
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
                    "[qcom-sdhci-sc7180] serviced already-satisfied eMMC BUS_ON handoff with mask held at zero"
                );
            }
            HandoffPowerAction::Reject => {
                println!(
                    "[qcom-sdhci-sc7180] unsafe eMMC power handoff rejected: SDHCI={:#04x} request={:#x} mask={:#x} ctl={:#x}",
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
    }

    if kind == MediaKind::Sd {
        println!(
            "[qcom-sdhci-sc7180] SD pwr_irq masked; inherited SDHCI={:#04x} request={:#x} mask={:#x} ctl={:#x}",
            handoff_power_control,
            pending_power_requests,
            initial_power_mask,
            initial_power_control
        );
    }

    // Shut down any high-speed DLL state inherited from firmware before the
    // generic eMMC host starts identification. The SD path defers this until
    // after the SDHCI reset, matching U-Boot's SDCC5 sequence.
    if kind == MediaKind::Emmc {
        let dll_config = read32(mmio_base, CORE_DLL_CONFIG);
        write32(mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_RST);
        let dll_config = read32(mmio_base, CORE_DLL_CONFIG);
        write32(mmio_base, CORE_DLL_CONFIG, dll_config | CORE_DLL_PDN);
    }

    // Restore the v5 vendor POR image, then select the pad voltage that
    // matches eMMC's fixed 1.8 V VQMMC or the SD slot's 3.0 V VMMC before the
    // generic software reset. Do not enable CQE, DMA, or interrupt delivery.
    write32(mmio_base, CORE_VENDOR_SPEC, vendor_spec);
    let configured_vendor_spec = read32(mmio_base, CORE_VENDOR_SPEC);
    let pads_selected = match kind {
        MediaKind::Emmc => {
            configured_vendor_spec & CORE_IO_PAD_PWR_SWITCH_MASK == CORE_IO_PAD_PWR_SWITCH_MASK
        }
        MediaKind::Sd => configured_vendor_spec & CORE_IO_PAD_PWR_SWITCH_MASK == 0,
    };
    if !pads_selected {
        vm::iounmap(mmio_base);
        disable_clocks(&clocks);
        return Err("qcom-sdhci-sc7180: failed to select the media I/O pad voltage");
    }
    let cqhci_base = if kind == MediaKind::Emmc {
        let resource =
            cqhci_resource.ok_or("qcom-sdhci-sc7180: cqhci memory resource is missing")?;
        let size = resource_size(resource)
            .filter(|size| *size >= REQUIRED_CQHCI_MMIO_SIZE)
            .ok_or("qcom-sdhci-sc7180: cqhci memory resource is too small")?;
        Some(match vm::ioremap(resource.start, size) {
            Ok(base) => base,
            Err(_) => {
                vm::iounmap(mmio_base);
                disable_clocks(&clocks);
                return Err("qcom-sdhci-sc7180: failed to map cqhci memory resource");
            }
        })
    } else {
        None
    };
    if let Some(cqhci_base) = cqhci_base {
        if let Err(error) = isolate_legacy_pio(mmio_base, cqhci_base) {
            vm::iounmap(cqhci_base);
            vm::iounmap(mmio_base);
            disable_clocks(&clocks);
            return Err(error);
        }
    }
    let inherited_host_control = read8(mmio_base, SDHCI_HOST_CONTROL);
    let inherited_host_control2 = read16(mmio_base, SDHCI_HOST_CONTROL2);
    let version = read32(mmio_base, CORE_MCI_VERSION);
    let media = match kind {
        MediaKind::Emmc => "eMMC",
        MediaKind::Sd => "SD",
    };
    println!(
        "[qcom-sdhci-sc7180] {} SDCC v5 version={:#010x}, core={} Hz, vmmc={}..{} uV vqmmc={}..{} uV vendor={:#010x}, inherited_host={:#04x} inherited_host2={:#06x}, pwr_irq={} masked, handoff SDHCI={:#04x} pwr={:#x}/{:#x}/{:#x}",
        media,
        version,
        base_clock_hz,
        vmmc_minimum_uv,
        vmmc_maximum_uv,
        vqmmc_minimum_uv,
        vqmmc_maximum_uv,
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
            kind == MediaKind::Emmc,
            base_clock_hz,
            SdhciHostConfig {
                single_power_write: true,
                // Firmware leaves both SDCC rails powered (SDHCI=0x0d). The
                // SDCC5 reset-all path does not clear on this controller, so
                // preserve the validated power state and reset only the
                // command/data state machines, as the eMMC path already does.
                preserve_power_control: true,
                external_card_detect: kind == MediaKind::Sd,
                ..SdhciHostConfig::default()
            },
        ),
        kind,
        mmio_base,
        cqhci_base,
        clocks,
        pwr_irq,
        handoff_power_control,
        handoff_power_requests,
        inherited_host_control,
        inherited_host_control2,
        card_detect,
    };
    let name = match kind {
        MediaKind::Emmc => EMMC_DEVICE_NAME,
        MediaKind::Sd => SD_DEVICE_NAME,
    };
    let block_device = match kind {
        MediaKind::Emmc => {
            MmcBlockDevice::probe_with_bus_width(name, Box::new(host), MmcBusWidth::Eight)
        }
        MediaKind::Sd => MmcBlockDevice::probe_sd(name, Box::new(host), MmcBusWidth::Four),
    }
    .map_err(|error| {
        println!(
            "[qcom-sdhci-sc7180] failed to identify {}: {}",
            name,
            error.as_str()
        );
        error.as_str()
    })?;

    let card = block_device.card_info();
    println!(
        "[qcom-sdhci-sc7180] {}: {} sectors ({} MiB), kind {:?}, media generation {}",
        name,
        card.sector_count(),
        card.sector_count().saturating_mul(512) / (1024 * 1024),
        card.kind(),
        block_device.media_generation()
    );

    let registered: Arc<dyn Device> = Arc::new(block_device);
    DeviceManager::get_manager().register_device_with_name(String::from(name), registered);
    println!("[qcom-sdhci-sc7180] registered {}", name);
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        DRIVER_NAME,
        probe,
        remove,
        vec!["qcom,sc7180-sdhci", "qcom,sdhci-msm-v5"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SDHCI_SC7180_ANCHOR: fn() = force_link;

/// Keep the external SC7180 SDHCI driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
