// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 glue for the Synopsys DWC3 USB host controller.
//!
//! The SC7180 device tree separates the Qualcomm clock/reset wrapper from the
//! nested `snps,dwc3` core. This crate mirrors that split with two platform
//! drivers and deliberately fixes the core in host mode; connector role
//! switching is outside this controller's responsibility.

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use scarlet::{
    device::{
        DeviceInfo,
        clk::{ClkError, ClkHandle},
        fdt::FdtManager,
        iommu::{IommuDomainConfig, IommuDomainType},
        manager::{DeviceManager, DriverPriority, is_probe_defer, probe_defer},
        phy::{PhyError, PhyHandle, PhyMode},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        reset::ResetHandle,
    },
    drivers::usb::{
        dwc3::dwc3_core::{
            DWC3_GCTL, DWC3_GUCTL1, DWC3_GUSB2PHYCFG, DWC3_GUSB3PIPECTL, Dwc3Core,
            GCTL_PRTCAP_HOST, GCTL_PRTCAP_MASK, GCTL_SCALEDOWN_MASK,
        },
        xhci::bind_xhci_mmio,
    },
    early_println,
    interrupt::resolve_platform_irq,
    sync::IrqSpinLock,
    time, vm,
};

const WRAPPER_CLOCK_NAMES: [&str; 5] = ["cfg_noc", "core", "iface", "sleep", "mock_utmi"];
const WRAPPER_RESET_DELAY_US: u64 = 10;

// Qualcomm QSCRATCH registers.  On SC7180 the wrapper's sole 0x400-byte MMIO
// resource is the QSCRATCH window (usb@a6f8800 in the upstream device tree).
const QSCRATCH_REGISTER_WINDOW_SIZE: usize = 0x400;
const QSCRATCH_GENERAL_CFG: usize = 0x08;
const PIPE_UTMI_CLK_SEL: u32 = 1 << 0;
const PIPE3_PHYSTATUS_SW: u32 = 1 << 3;
const PIPE_UTMI_CLK_DIS: u32 = 1 << 8;
const PIPE_UTMI_SWITCH_DELAY_US: u64 = 100;

const DWC3_GEVNTADRLO: usize = 0xc400;
const DWC3_GEVNTADRHI: usize = 0xc404;
const DWC3_GEVNTSIZ: usize = 0xc408;
const DWC3_GEVNTCOUNT: usize = 0xc40c;

const GUSB2PHYCFG_SUSPHY: u32 = 1 << 6;
const GUSB2PHYCFG_ENBLSLPM: u32 = 1 << 8;
const GUSB2PHYCFG_PHYSOFTRST: u32 = 1 << 31;
const GUSB3PIPECTL_SUSPHY: u32 = 1 << 17;
const GUSB3PIPECTL_PHYSOFTRST: u32 = 1 << 31;
const GUCTL1_DEV_FORCE_20_CLK_FOR_30_CLK: u32 = 1 << 26;
const GUCTL1_PARKMODE_DISABLE_SS: u32 = 1 << 17;
const GEVNTSIZ_INTMASK: u32 = 1 << 31;

const SC7180_USB_SID: u32 = 0x540;
const SC7180_DWC3_SPI: usize = 133;
// SC7180 exposes a 32-bit DMA window to USB. Keep page zero outside the IOVA
// allocator so null DMA addresses remain invalid while the SMMU maps all xHCI
// objects through a translated stage-1 domain.
const XHCI_IOVA_BASE: u64 = 0x1000;
const XHCI_IOVA_SIZE: u64 = (1 << 32) - XHCI_IOVA_BASE;
const DWC3_REGISTER_WINDOW_SIZE: usize = 0xe000;

struct QcomDwc3Wrapper {
    clocks: Vec<ClkHandle>,
    reset: ResetHandle,
    phandle: u32,
    qscratch_base: usize,
    qscratch_original_general_cfg: Option<u32>,
}

impl Drop for QcomDwc3Wrapper {
    fn drop(&mut self) {
        // QSCRATCH selects a clock consumed by the live DWC3/xHCI block.  Do
        // not restore it until the whole wrapper is held in reset.
        let reset_asserted = self.reset.assert().is_ok();
        if reset_asserted {
            if let Some(original) = self.qscratch_original_general_cfg.take() {
                let restored = qscratch_write_general_cfg(self.qscratch_base, original);
                if restored != original {
                    early_println!(
                        "[qcom-dwc3] QSCRATCH cleanup failed under wrapper reset: GENERAL_CFG={:#010x} expected={:#010x}",
                        restored,
                        original
                    );
                }
            }
        } else if self.qscratch_original_general_cfg.is_some() {
            early_println!("[qcom-dwc3] QSCRATCH cleanup skipped: wrapper reset assertion failed");
        }
        for clock in self.clocks.iter().rev() {
            clock.disable_unprepare();
        }
        let _ =
            WRAPPER_PHANDLE.compare_exchange(self.phandle, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

struct QcomDwc3 {
    usb2_phy: PhyHandle,
    usb3_phy: PhyHandle,
}

impl Drop for QcomDwc3 {
    fn drop(&mut self) {
        self.usb3_phy.power_off();
        self.usb2_phy.power_off();
    }
}

static WRAPPERS: IrqSpinLock<Vec<QcomDwc3Wrapper>> = IrqSpinLock::new(Vec::new());
static CONTROLLERS: IrqSpinLock<Vec<QcomDwc3>> = IrqSpinLock::new(Vec::new());
static WRAPPER_PHANDLE: AtomicU32 = AtomicU32::new(0);
static CORE_DRIVER_REGISTERED: AtomicBool = AtomicBool::new(false);

fn enable_usb_power_domain() -> Result<(), &'static str> {
    // Kept as the sole coupling point to the SC7180 GCC bootstrap crate.
    scarlet_driver_qcom_sc7180_gcc_display::enable_usb30_prim_gdsc()
}

fn wrapper_probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    enable_usb_power_domain()?;

    let manager = DeviceManager::get_manager();
    let phandle = device_phandle(device)?;
    let reset = match manager.resolve_reset_by_index(device, 0) {
        Ok(reset) => reset,
        Err(error) if is_probe_defer(error) => return probe_defer(),
        Err(_) => return Err("qcom-dwc3: failed to resolve wrapper reset"),
    };

    reset
        .assert()
        .map_err(|_| "qcom-dwc3: failed to assert wrapper reset")?;
    time::udelay(WRAPPER_RESET_DELAY_US);
    reset
        .deassert()
        .map_err(|_| "qcom-dwc3: failed to deassert wrapper reset")?;

    let mut clocks = Vec::with_capacity(WRAPPER_CLOCK_NAMES.len());
    for name in WRAPPER_CLOCK_NAMES {
        let clock = match manager.resolve_clk(device, name) {
            Ok(clock) => clock,
            Err(error) if is_probe_defer(error) => {
                disable_clocks(&clocks);
                let _ = reset.assert();
                return probe_defer();
            }
            Err(_) => {
                disable_clocks(&clocks);
                let _ = reset.assert();
                return Err("qcom-dwc3: failed to resolve wrapper clock");
            }
        };
        if let Err(error) = clock.prepare_enable() {
            disable_clocks(&clocks);
            let _ = reset.assert();
            return Err(clk_error_to_str(error));
        }
        clocks.push(clock);
    }

    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or_else(|| {
            disable_clocks(&clocks);
            let _ = reset.assert();
            "qcom-dwc3: missing QSCRATCH memory resource"
        })?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or_else(|| {
            disable_clocks(&clocks);
            let _ = reset.assert();
            "qcom-dwc3: invalid QSCRATCH memory resource"
        })?;
    if size < QSCRATCH_REGISTER_WINDOW_SIZE {
        disable_clocks(&clocks);
        let _ = reset.assert();
        return Err("qcom-dwc3: QSCRATCH memory resource is smaller than 0x400 bytes");
    }
    let qscratch_base = match vm::ioremap(resource.start, size) {
        Ok(base) => base,
        Err(_) => {
            disable_clocks(&clocks);
            let _ = reset.assert();
            return Err("qcom-dwc3: failed to map QSCRATCH MMIO");
        }
    };

    WRAPPERS.lock().push(QcomDwc3Wrapper {
        clocks,
        reset,
        phandle,
        qscratch_base,
        qscratch_original_general_cfg: None,
    });
    WRAPPER_PHANDLE.store(phandle, Ordering::Release);
    register_core_driver_once();
    early_println!("[qcom-dwc3] SC7180 wrapper clocks, reset, and USB GDSC ready");
    log_core_dependency_preflight();
    Ok(())
}

fn disable_clocks(clocks: &[ClkHandle]) {
    for clock in clocks.iter().rev() {
        clock.disable_unprepare();
    }
}

fn core_probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    early_println!(
        "[qcom-dwc3] core stage 1/8: Standard pre-probe passed for {} (IOMMU and PHY providers resolved)",
        device.name()
    );
    let wrapper_phandle = WRAPPER_PHANDLE.load(Ordering::Acquire);
    if wrapper_phandle == 0 {
        early_println!("[qcom-dwc3] core deferred: SC7180 wrapper is not ready");
        return probe_defer();
    }
    if device.parent_phandle() != Some(wrapper_phandle) {
        early_println!(
            "[qcom-dwc3] core rejected: parent={:?}, expected wrapper phandle={:#x}",
            device.parent_phandle(),
            wrapper_phandle
        );
        return Err("qcom-dwc3: DWC3 core is not a child of the active SC7180 wrapper");
    }
    let qscratch_base = wrapper_qscratch_base(wrapper_phandle)?;

    require_host_mode(device).map_err(|error| log_stage_error("host-mode validation", error))?;

    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-dwc3: missing DWC3 memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-dwc3: invalid DWC3 memory resource")?;
    if size < DWC3_REGISTER_WINDOW_SIZE {
        return Err("qcom-dwc3: DWC3 register resource is smaller than 0xe000 bytes");
    }
    let base = vm::ioremap(resource.start, size)
        .map_err(|_| log_stage_error("DWC3 MMIO map", "qcom-dwc3: ioremap failed"))?;
    early_println!(
        "[qcom-dwc3] core stage 2/8: MMIO mapped paddr={:#x} vaddr={:#x} size={:#x}",
        resource.start,
        base,
        size
    );

    let manager = DeviceManager::get_manager();
    early_println!("[qcom-dwc3] core stage 3/8: resolving usb2-phy in host mode");
    let usb2_phy = resolve_host_phy(manager, device, "usb2-phy")?;
    early_println!("[qcom-dwc3] core stage 3/8: usb2-phy host mode ready");
    early_println!("[qcom-dwc3] core stage 4/8: resolving usb3-phy in host mode");
    let usb3_phy = resolve_host_phy(manager, device, "usb3-phy")?;
    early_println!("[qcom-dwc3] core stage 4/8: usb3-phy host mode ready");

    let core = Dwc3Core::new(base);
    early_println!("[qcom-dwc3] core stage 5/8: resetting and initializing DWC3 core");
    initialize_core(
        &core,
        device,
        &usb2_phy,
        &usb3_phy,
        wrapper_phandle,
        qscratch_base,
    )
    .map_err(|error| log_stage_error("DWC3 core initialization", error))?;
    // From this point onward every fallible path owns a cleanup guard, so an
    // IRQ, IOMMU, or xHCI failure cannot leave either external PHY powered.
    let controller = QcomDwc3 { usb2_phy, usb3_phy };

    let irq_resource = device
        .get_resources()
        .iter()
        .find(|resource| {
            resource.res_type == PlatformDeviceResourceType::IRQ
                && resource
                    .irq_metadata
                    .map_or(resource.start, |metadata| metadata.irq_number as usize)
                    == SC7180_DWC3_SPI
        })
        .ok_or("qcom-dwc3: missing DWC3 SPI 133")?;
    let interrupt = resolve_platform_irq(irq_resource).map_err(|_| {
        log_stage_error(
            "SPI 133 resolution",
            "qcom-dwc3: failed to resolve DWC3 SPI 133",
        )
    })?;
    early_println!(
        "[qcom-dwc3] core stage 6/8: SPI {} resolved to IRQ {}",
        SC7180_DWC3_SPI,
        interrupt
    );
    early_println!(
        "[qcom-dwc3] core stage 7/8: resolving DMA context for SID {:#x}",
        SC7180_USB_SID
    );
    let dma_context = manager
        .resolve_platform_dma_context(
            device,
            IommuDomainConfig {
                domain_type: IommuDomainType::Dma,
                iova_base: XHCI_IOVA_BASE,
                iova_size: XHCI_IOVA_SIZE,
            },
        )
        .map_err(|error| log_stage_error("SID 0x540 DMA context", error))?;
    early_println!("[qcom-dwc3] core stage 7/8: DMA context attached");

    early_println!("[qcom-dwc3] core stage 8/8: binding xHCI host");
    bind_xhci_mmio(base, Some(interrupt), dma_context)
        .map_err(|error| log_stage_error("xHCI bind", error))?;
    CONTROLLERS.lock().push(controller);

    let (major, minor) = core.read_revision();
    early_println!(
        "[qcom-dwc3] SC7180 host ready: revision={}.{} SID={:#x} SPI={}",
        major,
        minor,
        SC7180_USB_SID,
        SC7180_DWC3_SPI
    );
    Ok(())
}

fn initialize_core(
    core: &Dwc3Core,
    device: &PlatformDeviceInfo,
    usb2_phy: &PhyHandle,
    usb3_phy: &PhyHandle,
    wrapper_phandle: u32,
    qscratch_base: usize,
) -> Result<(), &'static str> {
    let (major, minor) = core.read_revision();
    early_println!(
        "[qcom-dwc3] DWC3 revision {}.{} usb3-capable={}",
        major,
        minor,
        core.is_usb3()
    );
    core.global_soft_reset();
    core.write32(
        DWC3_GUSB2PHYCFG,
        core.read32(DWC3_GUSB2PHYCFG) | GUSB2PHYCFG_PHYSOFTRST,
    );
    core.write32(
        DWC3_GUSB3PIPECTL,
        core.read32(DWC3_GUSB3PIPECTL) | GUSB3PIPECTL_PHYSOFTRST,
    );
    time::udelay(100);

    // Linux applies these SC7180 properties before PHY power-on so the HS PHY
    // cannot enter suspend or LPM while its initialization is in progress.
    let mut usb2 = core.read32(DWC3_GUSB2PHYCFG);
    if device.property("snps,dis_u2_susphy_quirk").is_some() {
        usb2 &= !GUSB2PHYCFG_SUSPHY;
    }
    if device.property("snps,dis_enblslpm_quirk").is_some() {
        usb2 &= !GUSB2PHYCFG_ENBLSLPM;
    }
    core.write32(DWC3_GUSB2PHYCFG, usb2);

    early_println!("[qcom-dwc3] DWC3 init: powering usb2-phy");
    usb2_phy
        .power_on()
        .map_err(|error| log_stage_error("usb2-phy power-on", phy_error_to_str(error)))?;
    early_println!("[qcom-dwc3] DWC3 init: usb2-phy powered");
    early_println!("[qcom-dwc3] DWC3 init: powering usb3-phy");
    let high_speed_only = match usb3_phy.power_on() {
        Ok(()) => {
            early_println!("[qcom-dwc3] DWC3 init: usb3-phy powered");
            false
        }
        Err(PhyError::Timeout) => {
            early_println!(
                "[qcom-dwc3] DWC3 init: usb3-phy power-on timed out; falling back to USB2 high-speed host"
            );
            // A failed power_on does not acquire a PhyHandle power reference,
            // so this is only a best-effort release. A provider that timed out
            // after partial hardware setup must unwind that sequence itself.
            // Keep the provider rolled back and configure the DWC3 to run its
            // USB3 clock domain from the USB2 clock below.
            usb3_phy.power_off();
            early_println!(
                "[qcom-dwc3] DWC3 init: SuperSpeed PHY remains off; continuing with USB2 high-speed only"
            );
            true
        }
        Err(error) => {
            usb2_phy.power_off();
            return Err(log_stage_error(
                "usb3-phy power-on",
                phy_error_to_str(error),
            ));
        }
    };

    // Qualcomm requires the QSCRATCH PIPE-to-UTMI mux to be switched while
    // the DWC3 PHY interfaces and global core remain in reset.  The QMP
    // provider has completed its rollback before this sequence begins.
    if high_speed_only && let Err(error) = select_utmi_as_pipe_clock(wrapper_phandle, qscratch_base)
    {
        usb2_phy.power_off();
        return Err(error);
    }

    // Keep both DWC3 PHY interfaces in reset until their external providers
    // have completed power-on (or QMP has rolled back).  This follows the
    // established DWC3 reset ordering and leaves both interface resets clear
    // before releasing GCTL.CORESOFTRESET for the xHCI host.
    core.write32(
        DWC3_GUSB2PHYCFG,
        core.read32(DWC3_GUSB2PHYCFG) & !(GUSB2PHYCFG_PHYSOFTRST | GUSB2PHYCFG_SUSPHY),
    );
    core.write32(
        DWC3_GUSB3PIPECTL,
        core.read32(DWC3_GUSB3PIPECTL) & !(GUSB3PIPECTL_PHYSOFTRST | GUSB3PIPECTL_SUSPHY),
    );
    if high_speed_only && let Err(error) = verify_high_speed_phy_state(core) {
        usb3_phy.power_off();
        usb2_phy.power_off();
        return Err(error);
    }
    time::udelay(100);

    core.write32(
        DWC3_GCTL,
        core.read32(DWC3_GCTL) & !scarlet::drivers::usb::dwc3::dwc3_core::GCTL_CORESOFTRESET,
    );
    if let Err(error) = core.wait_for_reset() {
        usb3_phy.power_off();
        usb2_phy.power_off();
        return Err(log_stage_error("DWC3 global reset", error));
    }
    early_println!("[qcom-dwc3] DWC3 init: global reset released");

    // GUCTL1 is part of the live core configuration, not the PHY-reset
    // handshake. Linux programs it after its core-reset step. Program and
    // verify the HS-only state only after GCTL.CORESOFTRESET is released;
    // writes to GUCTL1 while that reset is asserted may be ignored.
    if high_speed_only && let Err(error) = configure_high_speed_clock(core) {
        usb3_phy.power_off();
        usb2_phy.power_off();
        return Err(error);
    }

    let mut gctl = core.read32(DWC3_GCTL);
    gctl &= !(GCTL_SCALEDOWN_MASK | GCTL_PRTCAP_MASK);
    gctl |= GCTL_PRTCAP_HOST;
    core.write32(DWC3_GCTL, gctl);
    let programmed_gctl = core.read32(DWC3_GCTL);
    if programmed_gctl & GCTL_PRTCAP_MASK != GCTL_PRTCAP_HOST {
        usb3_phy.power_off();
        usb2_phy.power_off();
        return Err(log_stage_error(
            "DWC3 host-mode programming",
            "qcom-dwc3: GCTL host mode did not latch",
        ));
    }
    early_println!(
        "[qcom-dwc3] DWC3 init: fixed host mode latched GCTL={:#010x}",
        programmed_gctl
    );

    // Keep SUSPHY clear through PHY and core initialization, as required by
    // DWC3.  Once host mode has latched, the software PHYSTATUS path lets us
    // quiesce the unavailable SuperSpeed interface safely.
    if high_speed_only {
        let pipe = core.read32(DWC3_GUSB3PIPECTL) | GUSB3PIPECTL_SUSPHY;
        core.write32(DWC3_GUSB3PIPECTL, pipe);
        let programmed_pipe = core.read32(DWC3_GUSB3PIPECTL);
        if programmed_pipe & GUSB3PIPECTL_SUSPHY == 0 {
            usb3_phy.power_off();
            usb2_phy.power_off();
            return Err(log_stage_error(
                "USB2 fallback PIPE suspend",
                "qcom-dwc3: SuperSpeed PIPE suspend did not latch",
            ));
        }
        early_println!(
            "[qcom-dwc3] USB2 fallback: SuperSpeed PIPE quiesced after core initialization GUSB3PIPECTL={:#010x}",
            programmed_pipe
        );
    }

    if device.property("snps,parkmode-disable-ss-quirk").is_some() {
        core.write32(
            DWC3_GUCTL1,
            core.read32(DWC3_GUCTL1) | GUCTL1_PARKMODE_DISABLE_SS,
        );
    }

    // The xHCI host owns its event rings. Mask the unused gadget event buffer.
    core.write32(DWC3_GEVNTADRLO, 0);
    core.write32(DWC3_GEVNTADRHI, 0);
    core.write32(DWC3_GEVNTSIZ, GEVNTSIZ_INTMASK);
    core.write32(DWC3_GEVNTCOUNT, 0);
    Ok(())
}

/// Verify that both DWC3 PHY interfaces are ready for global-reset release.
fn verify_high_speed_phy_state(core: &Dwc3Core) -> Result<(), &'static str> {
    let programmed_usb2 = core.read32(DWC3_GUSB2PHYCFG);
    let programmed_pipe = core.read32(DWC3_GUSB3PIPECTL);
    early_println!(
        "[qcom-dwc3] USB2 fallback PHY readback before core-reset release: GUSB2PHYCFG={:#010x} GUSB3PIPECTL={:#010x}",
        programmed_usb2,
        programmed_pipe
    );
    if programmed_usb2 & (GUSB2PHYCFG_PHYSOFTRST | GUSB2PHYCFG_SUSPHY) != 0 {
        return Err(log_stage_error(
            "USB2 fallback USB2 PHY state",
            "qcom-dwc3: USB2 PHY reset or suspend remained asserted",
        ));
    }
    if programmed_pipe & (GUSB3PIPECTL_PHYSOFTRST | GUSB3PIPECTL_SUSPHY) != 0 {
        return Err(log_stage_error(
            "USB2 fallback USB3 PIPE state",
            "qcom-dwc3: USB3 PIPE reset or suspend remained asserted",
        ));
    }
    early_println!(
        "[qcom-dwc3] USB2 fallback: GUSB2PHYCFG={:#010x} reset/SUSPHY clear GUSB3PIPECTL={:#010x} reset/SUSPHY clear",
        programmed_usb2,
        programmed_pipe
    );
    Ok(())
}

/// Select the USB2 clock for the DWC3 USB3 clock domain after core reset.
///
/// Linux applies this DWC3 2.90a+ high-speed-only bit after its core-reset
/// step. The xHCI block needs the substituted clock to complete HCRST while
/// the external QMP PHY remains powered off.
fn configure_high_speed_clock(core: &Dwc3Core) -> Result<(), &'static str> {
    let requested = core.read32(DWC3_GUCTL1) | GUCTL1_DEV_FORCE_20_CLK_FOR_30_CLK;
    core.write32(DWC3_GUCTL1, requested);
    let programmed = core.read32(DWC3_GUCTL1);
    early_println!(
        "[qcom-dwc3] USB2 fallback core-clock readback after reset release: GUCTL1={:#010x} requested={:#010x}",
        programmed,
        requested
    );
    if programmed & GUCTL1_DEV_FORCE_20_CLK_FOR_30_CLK == 0 {
        return Err(log_stage_error(
            "USB2 fallback core clock selection",
            "qcom-dwc3: GUCTL1 USB2 clock selection did not latch",
        ));
    }
    Ok(())
}

/// Select the wrapper's UTMI clock for the DWC3 PIPE clock domain.
///
/// This is the sequence used by Linux's `dwc3_qcom_select_utmi_clk()`:
/// gate PIPE, select UTMI and software-drive PIPE3 PHYSTATUS, then ungate.
/// Each read after a write also flushes the posted MMIO transaction.
fn select_utmi_as_pipe_clock(
    wrapper_phandle: u32,
    qscratch_base: usize,
) -> Result<(), &'static str> {
    preserve_qscratch_general_cfg(wrapper_phandle, qscratch_base)?;
    let disabled = qscratch_update_bits(qscratch_base, PIPE_UTMI_CLK_DIS, 0);
    if disabled & PIPE_UTMI_CLK_DIS == 0 {
        return Err(log_stage_error(
            "QSCRATCH PIPE clock disable",
            "qcom-dwc3: QSCRATCH PIPE clock disable did not latch",
        ));
    }
    time::udelay(PIPE_UTMI_SWITCH_DELAY_US);

    let selected = qscratch_update_bits(qscratch_base, PIPE_UTMI_CLK_SEL | PIPE3_PHYSTATUS_SW, 0);
    if selected & (PIPE_UTMI_CLK_SEL | PIPE3_PHYSTATUS_SW) != PIPE_UTMI_CLK_SEL | PIPE3_PHYSTATUS_SW
    {
        return Err(log_stage_error(
            "QSCRATCH UTMI PIPE selection",
            "qcom-dwc3: QSCRATCH UTMI PIPE selection did not latch",
        ));
    }
    time::udelay(PIPE_UTMI_SWITCH_DELAY_US);

    let enabled = qscratch_update_bits(qscratch_base, 0, PIPE_UTMI_CLK_DIS);
    if enabled & PIPE_UTMI_CLK_DIS != 0
        || enabled & (PIPE_UTMI_CLK_SEL | PIPE3_PHYSTATUS_SW)
            != PIPE_UTMI_CLK_SEL | PIPE3_PHYSTATUS_SW
    {
        return Err(log_stage_error(
            "QSCRATCH UTMI PIPE enable",
            "qcom-dwc3: QSCRATCH UTMI PIPE clock did not enable",
        ));
    }
    early_println!(
        "[qcom-dwc3] USB2 fallback: QSCRATCH_GENERAL_CFG={:#010x} PIPE clock=UTMI PHYSTATUS=software",
        enabled
    );
    Ok(())
}

fn qscratch_update_bits(base: usize, set: u32, clear: u32) -> u32 {
    let value = (qscratch_read_general_cfg(base) | set) & !clear;
    qscratch_write_general_cfg(base, value)
}

fn qscratch_read_general_cfg(base: usize) -> u32 {
    let register = (base + QSCRATCH_GENERAL_CFG) as *const u32;
    // SAFETY: `base` is the live ioremap of the parent wrapper's validated
    // 0x400-byte QSCRATCH resource.  GENERAL_CFG is a naturally aligned u32.
    unsafe { read_volatile(register) }
}

fn qscratch_write_general_cfg(base: usize, value: u32) -> u32 {
    let register = (base + QSCRATCH_GENERAL_CFG) as *mut u32;
    // SAFETY: see `qscratch_read_general_cfg`.  The read flushes the posted
    // write, matching Linux's dwc3_qcom_setbits()/clrbits() helpers.
    unsafe {
        write_volatile(register, value);
        read_volatile(register)
    }
}

fn preserve_qscratch_general_cfg(phandle: u32, qscratch_base: usize) -> Result<(), &'static str> {
    let mut wrappers = WRAPPERS.lock();
    let wrapper = wrappers
        .iter_mut()
        .find(|wrapper| wrapper.phandle == phandle && wrapper.qscratch_base == qscratch_base)
        .ok_or("qcom-dwc3: active wrapper QSCRATCH state is unavailable")?;
    if wrapper.qscratch_original_general_cfg.is_none() {
        wrapper.qscratch_original_general_cfg = Some(qscratch_read_general_cfg(qscratch_base));
    }
    Ok(())
}

fn wrapper_qscratch_base(phandle: u32) -> Result<usize, &'static str> {
    WRAPPERS
        .lock()
        .iter()
        .find(|wrapper| wrapper.phandle == phandle)
        .map(|wrapper| wrapper.qscratch_base)
        .ok_or("qcom-dwc3: active wrapper QSCRATCH mapping is unavailable")
}

fn resolve_host_phy(
    manager: &DeviceManager,
    device: &PlatformDeviceInfo,
    name: &'static str,
) -> Result<PhyHandle, &'static str> {
    let phy = match manager.resolve_phy(device, name) {
        Ok(phy) => phy,
        Err(error) if is_probe_defer(error) => {
            early_println!(
                "[qcom-dwc3] {} provider disappeared after pre-probe; deferring",
                name
            );
            return probe_defer();
        }
        Err(error) => return Err(log_stage_error(name, error)),
    };
    phy.set_mode(PhyMode::UsbHost)
        .map_err(|error| log_stage_error(name, phy_error_to_str(error)))?;
    Ok(phy)
}

fn log_core_dependency_preflight() {
    let Some(fdt) = FdtManager::get_manager().get_fdt() else {
        early_println!("[qcom-dwc3] core preflight unavailable: FDT is not initialized");
        return;
    };
    let Some(core_node) = fdt.all_nodes().find(|node| {
        node.compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "snps,dwc3"))
    }) else {
        early_println!("[qcom-dwc3] core preflight failed: no snps,dwc3 child in FDT");
        return;
    };

    let manager = DeviceManager::get_manager();
    let iommu_phandle = first_be_cell(core_node.property("iommus").map(|property| property.value));
    let phy_cells = core_node
        .property("phys")
        .and_then(|property| be_cells(property.value));
    // SC7180 has #phy-cells=0 for QUSB2 and #phy-cells=1 for QMP, hence
    // `<usb2-phandle usb3-phandle usb3-index>`.
    let usb2_phandle = phy_cells.as_ref().and_then(|cells| cells.first()).copied();
    let usb3_phandle = phy_cells.as_ref().and_then(|cells| cells.get(1)).copied();

    early_println!(
        "[qcom-dwc3] core preflight: driver=Standard iommu={:?}/ready={} usb2={:?}/ready={} usb3={:?}/ready={}",
        iommu_phandle,
        iommu_phandle
            .is_some_and(|phandle| manager.get_iommu_controller_by_phandle(phandle).is_some()),
        usb2_phandle,
        usb2_phandle.is_some_and(|phandle| manager.get_phy_provider_by_phandle(phandle).is_some()),
        usb3_phandle,
        usb3_phandle.is_some_and(|phandle| manager.get_phy_provider_by_phandle(phandle).is_some()),
    );
    if iommu_phandle
        .is_some_and(|phandle| manager.get_iommu_controller_by_phandle(phandle).is_none())
    {
        early_println!(
            "[qcom-dwc3] core will defer before driver probe: apps SMMU provider for SID {:#x} is not registered",
            SC7180_USB_SID
        );
    }
}

fn first_be_cell(bytes: Option<&[u8]>) -> Option<u32> {
    let bytes = bytes?.get(..4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn be_cells(bytes: &[u8]) -> Option<Vec<u32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| u32::from_be_bytes(chunk.try_into().unwrap_or([0; 4])))
            .collect(),
    )
}

fn log_stage_error(stage: &str, error: &'static str) -> &'static str {
    early_println!("[qcom-dwc3] {} failed: {}", stage, error);
    error
}

fn require_host_mode(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    match device
        .property("dr_mode")
        .and_then(|property| property.as_str())
    {
        None | Some("host") => Ok(()),
        Some(_) => Err("qcom-dwc3: controller is fixed in host mode"),
    }
}

fn device_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-dwc3: wrapper has no phandle")
}

fn clk_error_to_str(_error: ClkError) -> &'static str {
    "qcom-dwc3: failed to enable wrapper clock"
}

fn phy_error_to_str(error: PhyError) -> &'static str {
    match error {
        PhyError::NotFound => "qcom-dwc3: PHY not found",
        PhyError::NotSupported => "qcom-dwc3: PHY operation not supported",
        PhyError::InvalidMode => "qcom-dwc3: invalid PHY mode",
        PhyError::PowerOnFailed => "qcom-dwc3: PHY power on failed",
        PhyError::PowerOffFailed => "qcom-dwc3: PHY power off failed",
        PhyError::ResetFailed => "qcom-dwc3: PHY reset failed",
        PhyError::Busy => "qcom-dwc3: PHY busy",
        PhyError::Timeout => "qcom-dwc3: PHY timeout",
        PhyError::HardwareError => "qcom-dwc3: PHY hardware error",
    }
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_drivers() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-dwc3-wrapper",
            wrapper_probe,
            remove_fn,
            alloc::vec!["qcom,sc7180-dwc3"],
        )),
        DriverPriority::Core,
    );
}

fn register_core_driver_once() {
    if CORE_DRIVER_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-dwc3-core",
            core_probe,
            remove_fn,
            alloc::vec!["snps,dwc3"],
        )),
        // The SC7180 QUSB2 and QMP PHY providers probe at Core priority.
        // Run the consumer in the following phase so DeviceManager's generic
        // pre-probe PHY resolution sees both providers without depending on
        // DT traversal order or a same-phase deferred retry.
        DriverPriority::Standard,
    );
    early_println!(
        "[qcom-dwc3] registered nested snps,dwc3 driver at Standard priority; generic pre-probe checks IOMMU before PHYs"
    );
}

scarlet::driver_initcall!(register_drivers);

#[used]
static SCARLET_DRIVER_QCOM_DWC3_ANCHOR: fn() = force_link;

/// Keep this external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
