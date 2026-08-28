// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 hardware CPU frequency driver.
//!
//! SC7180 exposes one autonomous frequency-control register window per CPU
//! cluster. The hardware LUT is the authoritative list of fuse-qualified
//! operating points; writing a LUT index requests a coupled voltage and clock
//! transition without software regulator sequencing.
//!
//! # Provenance
//!
//! Register offsets, LUT fields, and transition semantics follow Linux 6.6
//! `drivers/cpufreq/qcom-cpufreq-hw.c` and the
//! `qcom,cpufreq-hw` Device Tree binding.

extern crate alloc;

use alloc::{boxed::Box, vec};
use core::sync::atomic::{AtomicBool, Ordering};

use scarlet::{
    arch::{self, mmio},
    device::{
        cpufreq::{
            CpuFrequencyBackend, CpuFrequencyGovernor, CpuFrequencyInfo, CpuFrequencyOpp,
            CpuFrequencyPolicyRegistration, MAX_CPUFREQ_OPPS, compose_performance_domain_id,
            cpu_performance_domain, register_backend, register_policy, set_domain_target_frequency,
        },
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    early_println,
    environment::MAX_NUM_CPUS,
    sync::IrqSpinLock,
    vm,
};

const BACKEND_NAME: &str = "qcom-sc7180-cpufreq-hw";
const DOMAIN_COUNT: usize = 2;
const LUT_MAX_ENTRIES: usize = 40;
const LUT_ROW_SIZE: usize = 32;

const REG_ENABLE: usize = 0x000;
const REG_DCVS_CTRL: usize = 0x0bc;
const REG_FREQ_LUT: usize = 0x110;
const REG_VOLT_LUT: usize = 0x114;
const REG_PERF_STATE: usize = 0x920;

const ENABLE_BIT: u32 = 1;
const PER_CORE_DCVS_BIT: u32 = 1;
const LUT_SOURCE_SHIFT: u32 = 30;
const LUT_SOURCE_MASK: u32 = 0x3;
const LUT_L_VALUE_MASK: u32 = 0xff;
const LUT_CORE_COUNT_SHIFT: u32 = 16;
const LUT_CORE_COUNT_MASK: u32 = 0x7;
const LUT_VOLTAGE_MASK: u32 = 0x0fff;
const LUT_TURBO_CORE_COUNT: u32 = 1;
const ALTERNATE_CLOCK_DIVIDER: u64 = 2;
const TRANSITION_LATENCY_NS: u64 = 1_000;
const MIN_REGISTER_WINDOW_SIZE: usize = REG_PERF_STATE + MAX_NUM_CPUS * 4;

static PROBED: AtomicBool = AtomicBool::new(false);
static DOMAINS: IrqSpinLock<[QcomCpuFreqDomain; DOMAIN_COUNT]> =
    IrqSpinLock::new([QcomCpuFreqDomain::empty(); DOMAIN_COUNT]);

#[derive(Debug, Clone, Copy)]
struct RegisterWindow {
    base: usize,
    size: usize,
}

impl RegisterWindow {
    const fn empty() -> Self {
        Self { base: 0, size: 0 }
    }

    const fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }

    fn read(self, offset: usize) -> u32 {
        debug_assert!(offset.checked_add(4).is_some_and(|end| end <= self.size));
        // SAFETY: probe validates the complete SC7180 CPUFreq HW resource and
        // every caller supplies a register offset within that mapping.
        unsafe { mmio::read32(self.base + offset) }
    }

    fn write(self, offset: usize, value: u32) {
        debug_assert!(offset.checked_add(4).is_some_and(|end| end <= self.size));
        // SAFETY: see `read`; writes use the same validated device mapping.
        unsafe { mmio::write32(self.base + offset, value) }
    }
}

#[derive(Debug, Clone, Copy)]
struct QcomCpuFreqDomain {
    valid: bool,
    provider_index: u32,
    domain_id: u32,
    paddr: usize,
    registers: RegisterWindow,
    per_core_dcvs: bool,
    cpu_count: usize,
    cpus_mask: u64,
    opp_count: usize,
    opps: [CpuFrequencyOpp; MAX_CPUFREQ_OPPS],
    min_voltage_uv: u64,
    max_voltage_uv: u64,
}

impl QcomCpuFreqDomain {
    const fn empty() -> Self {
        Self {
            valid: false,
            provider_index: 0,
            domain_id: 0,
            paddr: 0,
            registers: RegisterWindow::empty(),
            per_core_dcvs: false,
            cpu_count: 0,
            cpus_mask: 0,
            opp_count: 0,
            opps: [CpuFrequencyOpp {
                pstate: 0,
                freq_khz: 0,
            }; MAX_CPUFREQ_OPPS],
            min_voltage_uv: 0,
            max_voltage_uv: 0,
        }
    }

    fn frequency_for_pstate(&self, pstate: u32) -> Option<u64> {
        self.opps[..self.opp_count]
            .iter()
            .find(|opp| opp.pstate == pstate)
            .map(|opp| opp.freq_khz)
    }

    fn min_frequency(&self) -> Option<u64> {
        self.opps[..self.opp_count]
            .iter()
            .map(|opp| opp.freq_khz)
            .min()
    }

    fn max_frequency(&self) -> Option<u64> {
        self.opps[..self.opp_count]
            .iter()
            .map(|opp| opp.freq_khz)
            .max()
    }

    fn requested_pstate(&self) -> u32 {
        self.registers
            .read(REG_PERF_STATE)
            .min((LUT_MAX_ENTRIES - 1) as u32)
    }
}

fn cpu_frequency_info(cpu_id: usize) -> Option<CpuFrequencyInfo> {
    let domain_id = cpu_performance_domain(cpu_id)?;
    let domains = DOMAINS.lock();
    let domain = domains
        .iter()
        .find(|domain| domain.valid && domain.domain_id == domain_id)?;
    let raw_status = domain.registers.read(REG_PERF_STATE);
    let pstate = raw_status.min((LUT_MAX_ENTRIES - 1) as u32);
    let frequency = domain.frequency_for_pstate(pstate);

    Some(CpuFrequencyInfo {
        performance_domain: domain.domain_id,
        raw_status,
        current_pstate: Some(pstate),
        target_pstate: Some(pstate),
        current_freq_khz: frequency,
        target_freq_khz: frequency,
        max_freq_khz: domain.max_frequency(),
    })
}

fn set_domain_pstate(domain_id: u32, pstate: u32) -> Result<(), &'static str> {
    let domains = DOMAINS.lock();
    let domain = domains
        .iter()
        .find(|domain| domain.valid && domain.domain_id == domain_id)
        .ok_or("qcom-sc7180-cpufreq-hw: domain not found")?;
    if domain.frequency_for_pstate(pstate).is_none() {
        return Err("qcom-sc7180-cpufreq-hw: invalid LUT pstate");
    }

    let writes = if domain.per_core_dcvs {
        domain.cpu_count
    } else {
        1
    };
    for cpu_index in 0..writes {
        domain
            .registers
            .write(REG_PERF_STATE + cpu_index * 4, pstate);
    }
    arch::io_wmb();
    Ok(())
}

fn decode_lut_entry(raw: u32, xo_rate_hz: u64, alternate_rate_hz: u64) -> (u64, u32) {
    let source = (raw >> LUT_SOURCE_SHIFT) & LUT_SOURCE_MASK;
    let l_value = u64::from(raw & LUT_L_VALUE_MASK);
    let core_count = (raw >> LUT_CORE_COUNT_SHIFT) & LUT_CORE_COUNT_MASK;
    let frequency_hz = if source == 0 {
        alternate_rate_hz / ALTERNATE_CLOCK_DIVIDER
    } else {
        xo_rate_hz.saturating_mul(l_value)
    };
    (frequency_hz / 1_000, core_count)
}

fn load_lut(
    domain: &mut QcomCpuFreqDomain,
    xo_rate_hz: u64,
    alternate_rate_hz: u64,
) -> Result<(), &'static str> {
    let mut previous_frequency = None;

    for index in 0..LUT_MAX_ENTRIES {
        let row_offset = index * LUT_ROW_SIZE;
        let raw_frequency = domain.registers.read(REG_FREQ_LUT + row_offset);
        let raw_voltage = domain.registers.read(REG_VOLT_LUT + row_offset);
        let (frequency_khz, core_count) =
            decode_lut_entry(raw_frequency, xo_rate_hz, alternate_rate_hz);

        if frequency_khz == 0 {
            return Err("qcom-sc7180-cpufreq-hw: zero-frequency LUT entry");
        }
        if index > 0 && previous_frequency == Some(frequency_khz) {
            break;
        }

        if core_count != LUT_TURBO_CORE_COUNT {
            if domain.opp_count >= MAX_CPUFREQ_OPPS {
                return Err("qcom-sc7180-cpufreq-hw: LUT exceeds cpufreq OPP capacity");
            }
            domain.opps[domain.opp_count] = CpuFrequencyOpp {
                pstate: index as u32,
                freq_khz: frequency_khz,
            };
            domain.opp_count += 1;

            let voltage_uv = u64::from(raw_voltage & LUT_VOLTAGE_MASK) * 1_000;
            domain.min_voltage_uv = if domain.min_voltage_uv == 0 {
                voltage_uv
            } else {
                domain.min_voltage_uv.min(voltage_uv)
            };
            domain.max_voltage_uv = domain.max_voltage_uv.max(voltage_uv);
        }

        previous_frequency = Some(frequency_khz);
    }

    if domain.opp_count == 0 {
        return Err("qcom-sc7180-cpufreq-hw: hardware LUT has no usable OPPs");
    }
    Ok(())
}

fn node_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-sc7180-cpufreq-hw: missing provider phandle")
}

fn required_clock_rate(device: &PlatformDeviceInfo, name: &str) -> Result<u64, &'static str> {
    let clock = match DeviceManager::get_manager().resolve_clk(device, name) {
        Ok(clock) => clock,
        Err("clk: provider not found" | "clk: clock not found") => return probe_defer(),
        Err(error) => return Err(error),
    };
    let rate = clock.rate();
    if rate == 0 {
        return Err("qcom-sc7180-cpufreq-hw: source clock has zero rate");
    }
    Ok(rate)
}

fn memory_resources(
    device: &PlatformDeviceInfo,
) -> Result<[(usize, usize); DOMAIN_COUNT], &'static str> {
    let mut resources = [(0usize, 0usize); DOMAIN_COUNT];
    let mut count = 0usize;

    for resource in device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
    {
        if count >= DOMAIN_COUNT {
            return Err("qcom-sc7180-cpufreq-hw: unexpected extra MMIO domain");
        }
        let size = resource
            .end
            .checked_sub(resource.start)
            .and_then(|value| value.checked_add(1))
            .ok_or("qcom-sc7180-cpufreq-hw: invalid MMIO resource")?;
        if size < MIN_REGISTER_WINDOW_SIZE {
            return Err("qcom-sc7180-cpufreq-hw: MMIO resource is too small");
        }
        resources[count] = (resource.start, size);
        count += 1;
    }

    if count != DOMAIN_COUNT {
        return Err("qcom-sc7180-cpufreq-hw: expected two frequency domains");
    }
    Ok(resources)
}

fn cpu_mask_for_domain(domain_id: u32) -> (u64, usize) {
    let mut mask = 0u64;
    let mut count = 0usize;
    for cpu_id in 0..MAX_NUM_CPUS {
        if cpu_performance_domain(cpu_id) != Some(domain_id) {
            continue;
        }
        if cpu_id < u64::BITS as usize {
            mask |= 1u64 << cpu_id;
        }
        count += 1;
    }
    (mask, count)
}

fn unmap_domains(domains: &[QcomCpuFreqDomain; DOMAIN_COUNT]) {
    for domain in domains.iter().filter(|domain| domain.registers.base != 0) {
        vm::iounmap(domain.registers.base);
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if PROBED.load(Ordering::Acquire) {
        return Ok(());
    }

    let provider = node_phandle(device)?;
    let xo_rate_hz = required_clock_rate(device, "xo")?;
    let alternate_rate_hz = required_clock_rate(device, "alternate")?;
    let resources = memory_resources(device)?;
    let mut discovered = [QcomCpuFreqDomain::empty(); DOMAIN_COUNT];

    for (index, (paddr, size)) in resources.into_iter().enumerate() {
        let base = match vm::ioremap(paddr, size) {
            Ok(base) => base,
            Err(_) => {
                unmap_domains(&discovered);
                return Err("qcom-sc7180-cpufreq-hw: ioremap failed");
            }
        };
        let domain_id = match compose_performance_domain_id(provider, index as u32) {
            Some(domain_id) => domain_id,
            None => {
                vm::iounmap(base);
                unmap_domains(&discovered);
                return Err("qcom-sc7180-cpufreq-hw: provider domain cannot be encoded");
            }
        };
        let registers = RegisterWindow::new(base, size);
        if registers.read(REG_ENABLE) & ENABLE_BIT == 0 {
            vm::iounmap(base);
            unmap_domains(&discovered);
            return Err("qcom-sc7180-cpufreq-hw: frequency domain is disabled");
        }
        let (cpus_mask, cpu_count) = cpu_mask_for_domain(domain_id);
        if cpu_count == 0 {
            vm::iounmap(base);
            unmap_domains(&discovered);
            return Err("qcom-sc7180-cpufreq-hw: frequency domain has no CPUs");
        }

        let mut domain = QcomCpuFreqDomain {
            valid: true,
            provider_index: index as u32,
            domain_id,
            paddr,
            registers,
            per_core_dcvs: registers.read(REG_DCVS_CTRL) & PER_CORE_DCVS_BIT != 0,
            cpu_count,
            cpus_mask,
            opp_count: 0,
            opps: [CpuFrequencyOpp {
                pstate: 0,
                freq_khz: 0,
            }; MAX_CPUFREQ_OPPS],
            min_voltage_uv: 0,
            max_voltage_uv: 0,
        };
        if let Err(error) = load_lut(&mut domain, xo_rate_hz, alternate_rate_hz) {
            vm::iounmap(base);
            unmap_domains(&discovered);
            return Err(error);
        }
        discovered[index] = domain;
    }

    *DOMAINS.lock() = discovered;

    for domain in discovered {
        register_policy(CpuFrequencyPolicyRegistration {
            backend_name: BACKEND_NAME,
            domain: domain.domain_id,
            opps: &domain.opps[..domain.opp_count],
            governor: CpuFrequencyGovernor::Schedutil,
            transition_latency_ns: TRANSITION_LATENCY_NS,
        })?;

        let boot_pstate = domain.requested_pstate();
        let boot_frequency = domain
            .frequency_for_pstate(boot_pstate)
            .or_else(|| domain.max_frequency())
            .ok_or("qcom-sc7180-cpufreq-hw: no boot frequency")?;
        set_domain_target_frequency(domain.domain_id, boot_frequency)?;
        early_println!(
            "[qcom-sc7180-cpufreq-hw] domain={} id={:#x} cpus={:#x} paddr={:#x} opps={} current={} kHz range={}..={} kHz voltage={}..={} uV per-core={}",
            domain.provider_index,
            domain.domain_id,
            domain.cpus_mask,
            domain.paddr,
            domain.opp_count,
            boot_frequency,
            domain.min_frequency().unwrap_or(0),
            domain.max_frequency().unwrap_or(0),
            domain.min_voltage_uv,
            domain.max_voltage_uv,
            domain.per_core_dcvs,
        );
    }

    PROBED.store(true, Ordering::Release);
    early_println!(
        "[qcom-sc7180-cpufreq-hw] registered xo={} Hz alternate={} Hz",
        xo_rate_hz,
        alternate_rate_hz,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    if let Err(error) = register_backend(CpuFrequencyBackend {
        name: BACKEND_NAME,
        snapshot: cpu_frequency_info,
        set_pstate: Some(set_domain_pstate),
    }) {
        early_println!(
            "[qcom-sc7180-cpufreq-hw] failed to register backend: {}",
            error,
        );
    }

    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            BACKEND_NAME,
            probe,
            remove,
            vec!["qcom,sc7180-cpufreq-hw"],
        )),
        DriverPriority::Standard,
    );
}

scarlet::driver_initcall!(register_driver);

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn lut_decodes_xo_and_alternate_sources() {
        let xo_entry = (1 << LUT_SOURCE_SHIFT) | 30 | (6 << LUT_CORE_COUNT_SHIFT);
        assert_eq!(
            decode_lut_entry(xo_entry, 19_200_000, 600_000_000),
            (576_000, 6),
        );

        let alternate_entry = 6 << LUT_CORE_COUNT_SHIFT;
        assert_eq!(
            decode_lut_entry(alternate_entry, 19_200_000, 600_000_000),
            (300_000, 6),
        );
    }

    #[test_case]
    fn turbo_core_count_is_distinct_from_cluster_opps() {
        let turbo_entry = (1 << LUT_SOURCE_SHIFT) | 133 | (1 << LUT_CORE_COUNT_SHIFT);
        let (_, core_count) = decode_lut_entry(turbo_entry, 19_200_000, 600_000_000);
        assert_eq!(core_count, LUT_TURBO_CORE_COUNT);
    }
}

#[used]
static SCARLET_DRIVER_QCOM_SC7180_CPUFREQ_HW_ANCHOR: fn() = force_link;

#[inline(never)]
/// Keep the external SC7180 CPUFreq HW driver linked into module builds.
pub fn force_link() {}
