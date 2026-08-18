// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Native Qualcomm SC7180 display driver for CoachZ.
//!
//! The driver owns a low-memory XRGB8888 scanout buffer and reconstructs the
//! complete internal-panel path after Depthcharge's alternate-firmware cleanup:
//!
//! `DPU VIG0 → LM0 → INTF1 → DSI0/10nm PHY → SN65DSI86 → eDP panel`.
//!
//! # Provenance
//!
//! Register programming and PHY calculations are adapted from coreboot's
//! GPL-2.0-only SC7180 display implementation under
//! `src/soc/qualcomm/sc7180/display` and
//! `src/soc/qualcomm/common/display`.

extern crate alloc;

mod clock;
mod dpu;
mod dsi;
mod phy;
mod registers;

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::{any::Any, ptr};

use clock::DisplayClocks;
use dpu::Dpu;
use dsi::{DSI_BASE, DsiHost};
use phy::DsiPhy;
use registers::RegisterWindow;
use scarlet::{
    arch,
    device::{
        Device, DeviceType,
        graphics::{FramebufferConfig, GraphicsDevice, PixelFormat, output::DisplayRegion},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    environment::PAGE_SIZE,
    mem::page::ContiguousPages,
    object::capability::{
        ControlOps, MemoryMappingOps,
        selectable::{ReadyInterest, SelectWaitOutcome, Selectable},
    },
    println,
    sync::IrqSpinLock,
    time, vm,
};
use scarlet_driver_cros_ec_spi::get_cros_ec_spi_by_phandle;
use scarlet_driver_ti_sn65dsi86::{DisplayTiming, Sn65dsi86, get_sn65dsi86_by_phandle};

const MDSS_MAP_SIZE: usize = 0x0c_0000;
const DISPCC_MAP_SIZE: usize = 0x1_0000;
const DSI_LANES: u8 = 4;
const DSI_BITS_PER_PIXEL: u8 = 24;
const MAXIMUM_SCANOUT_ADDRESS: usize = u32::MAX as usize;

#[derive(Clone, Copy)]
struct GpioSpecifier {
    controller: u32,
    pin: u32,
    flags: u32,
}

struct Sc7180DisplayEngine {
    dpu: Dpu,
    present_lock: IrqSpinLock<()>,
}

impl Sc7180DisplayEngine {
    fn new(mdss: RegisterWindow) -> Self {
        Self {
            dpu: Dpu::new(mdss),
            present_lock: IrqSpinLock::new(()),
        }
    }

    fn initialize(
        &self,
        mdss: RegisterWindow,
        dispcc: RegisterWindow,
        bridge: &Sn65dsi86,
        timing: DisplayTiming,
        scanout_paddr: usize,
    ) -> Result<(), &'static str> {
        let dsi = DsiHost::new(mdss);
        dsi.reset();
        DsiPhy::new(mdss).initialize(
            timing,
            u32::from(DSI_LANES),
            u32::from(DSI_BITS_PER_PIXEL),
            DSI_BASE,
        )?;
        DisplayClocks::new(dispcc).enable_dsi0()?;
        dsi.configure_host(u32::from(DSI_LANES))?;

        bridge
            .configure_link(timing, DSI_LANES, DSI_BITS_PER_PIXEL)
            .map_err(|error| {
                println!(
                    "[qcom-sc7180-mdss] SN65DSI86 link setup failed: {:?}",
                    error
                );
                "qcom-sc7180-mdss: bridge link setup failed"
            })?;
        self.dpu.configure(timing, scanout_paddr)?;
        dsi.configure_video(timing);
        self.dpu.start();
        Ok(())
    }

    fn present(&self, scanout_paddr: usize) -> Result<(), &'static str> {
        let _guard = self.present_lock.lock();
        self.dpu.present(scanout_paddr)
    }
}

pub struct Sc7180GraphicsDevice {
    config: FramebufferConfig,
    timing: DisplayTiming,
    scanout: ContiguousPages,
    engine: Sc7180DisplayEngine,
}

impl Sc7180GraphicsDevice {
    fn clean_regions(&self, regions: &[DisplayRegion]) -> Result<(), &'static str> {
        if regions.is_empty() {
            arch::clean_dcache_to_poc_range(self.scanout.as_vaddr(), self.config.size());
            return Ok(());
        }

        let stride = self.config.stride as usize;
        let bytes_per_pixel = self.config.format.bytes_per_pixel();
        for region in regions {
            let x = region.x.min(self.config.width);
            let y = region.y.min(self.config.height);
            let width = region.width.min(self.config.width.saturating_sub(x));
            let height = region.height.min(self.config.height.saturating_sub(y));
            if width == 0 || height == 0 {
                continue;
            }
            let start = (y as usize)
                .checked_mul(stride)
                .and_then(|offset| offset.checked_add(x as usize * bytes_per_pixel))
                .ok_or("qcom-sc7180-mdss: damage range overflow")?;
            let end = (y as usize + height as usize - 1)
                .checked_mul(stride)
                .and_then(|offset| {
                    offset.checked_add((x as usize + width as usize) * bytes_per_pixel)
                })
                .ok_or("qcom-sc7180-mdss: damage range overflow")?;
            if end > self.config.size() {
                return Err("qcom-sc7180-mdss: damage range exceeds scanout");
            }
            arch::clean_dcache_to_poc_range(self.scanout.as_vaddr() + start, end - start);
        }
        Ok(())
    }

    fn validate_scanout(
        &self,
        config: &FramebufferConfig,
        physical_addr: usize,
    ) -> Result<(), &'static str> {
        if physical_addr != self.scanout.as_paddr()
            || config.width != self.config.width
            || config.height != self.config.height
            || config.stride != self.config.stride
            || config.format != self.config.format
        {
            return Err("qcom-sc7180-mdss: framebuffer does not match native scanout");
        }
        Ok(())
    }
}

impl Device for Sc7180GraphicsDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Graphics
    }

    fn name(&self) -> &'static str {
        "qcom-sc7180-mdss"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_graphics_device(&self) -> Option<&dyn GraphicsDevice> {
        Some(self)
    }
}

impl GraphicsDevice for Sc7180GraphicsDevice {
    fn get_display_name(&self) -> &'static str {
        "coachz-internal-panel"
    }

    fn get_framebuffer_config(&self) -> Result<FramebufferConfig, &'static str> {
        Ok(self.config.clone())
    }

    fn get_framebuffer_address(&self) -> Result<usize, &'static str> {
        Ok(self.scanout.as_paddr())
    }

    fn present_framebuffer_region(
        &self,
        config: &FramebufferConfig,
        physical_addr: usize,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        self.validate_scanout(config, physical_addr)?;
        self.clean_regions(&[region])?;
        self.engine.present(physical_addr)
    }

    fn scanout_buffer_count(&self) -> usize {
        1
    }

    fn front_scanout_buffer(&self) -> Option<usize> {
        Some(0)
    }

    fn get_scanout_buffer_info(
        &self,
        index: usize,
    ) -> Result<(FramebufferConfig, usize), &'static str> {
        if index != 0 {
            return Err("qcom-sc7180-mdss: invalid scanout index");
        }
        Ok((self.config.clone(), self.scanout.as_paddr()))
    }

    fn present_scanout_buffer(&self, index: usize) -> Result<(), &'static str> {
        self.present_scanout_buffer_regions(index, &[])
    }

    fn present_scanout_buffer_regions(
        &self,
        index: usize,
        regions: &[DisplayRegion],
    ) -> Result<(), &'static str> {
        if index != 0 {
            return Err("qcom-sc7180-mdss: invalid scanout index");
        }
        self.clean_regions(regions)?;
        self.engine.present(self.scanout.as_paddr())
    }

    fn init_graphics(&self) -> Result<(), &'static str> {
        Ok(())
    }
}

impl ControlOps for Sc7180GraphicsDevice {
    fn control(&self, _command: u32, _arg: usize) -> Result<i32, &'static str> {
        Err("qcom-sc7180-mdss: control command unsupported")
    }

    fn supported_control_commands(&self) -> Vec<(u32, &'static str)> {
        Vec::new()
    }
}

impl MemoryMappingOps for Sc7180GraphicsDevice {
    fn get_mapping_info(
        &self,
        _offset: usize,
        _length: usize,
    ) -> Result<scarlet::object::capability::MemoryMappingInfo, &'static str> {
        Err("qcom-sc7180-mdss: map /dev/display0 instead")
    }

    fn supports_mmap(&self) -> bool {
        false
    }
}

impl Selectable for Sc7180GraphicsDevice {
    fn current_ready(
        &self,
        _interest: ReadyInterest,
    ) -> scarlet::object::capability::selectable::ReadySet {
        scarlet::object::capability::selectable::ReadySet::none()
    }

    fn wait_until_ready(
        &self,
        _interest: ReadyInterest,
        _trapframe: &mut scarlet::arch::Trapframe,
        _timeout_ticks: Option<u64>,
        _min_wait_ticks: u64,
    ) -> SelectWaitOutcome {
        SelectWaitOutcome::Ready
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn compatible_phandle(wanted: &str) -> Option<u32> {
    let fdt = scarlet::device::fdt::FdtManager::get_manager().get_fdt()?;
    for node in fdt.all_nodes() {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|value| value == wanted))
        {
            continue;
        }
        let property = node
            .property("phandle")
            .or_else(|| node.property("linux,phandle"))?;
        return read_be_u32(property.value, 0);
    }
    None
}

fn compatible_register(wanted: &str, index: usize) -> Option<(usize, usize)> {
    let fdt = scarlet::device::fdt::FdtManager::get_manager().get_fdt()?;
    for node in fdt.all_nodes() {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|value| value == wanted))
        {
            continue;
        }
        let region = node.reg()?.nth(index)?;
        return Some((region.starting_address as usize, region.size.unwrap_or(0)));
    }
    None
}

fn gpio_from_bytes(bytes: &[u8]) -> Option<GpioSpecifier> {
    Some(GpioSpecifier {
        controller: read_be_u32(bytes, 0)?,
        pin: read_be_u32(bytes, 4)?,
        flags: read_be_u32(bytes, 8)?,
    })
}

fn compatible_gpio(wanted: &str, property_name: &str) -> Option<GpioSpecifier> {
    let fdt = scarlet::device::fdt::FdtManager::get_manager().get_fdt()?;
    for node in fdt.all_nodes() {
        if node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|value| value == wanted))
        {
            return gpio_from_bytes(node.property(property_name)?.value);
        }
    }
    None
}

fn phandle_property(wanted: &str, property_name: &str) -> Option<u32> {
    let fdt = scarlet::device::fdt::FdtManager::get_manager().get_fdt()?;
    for node in fdt.all_nodes() {
        if node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|value| value == wanted))
        {
            return read_be_u32(node.property(property_name)?.value, 0);
        }
    }
    None
}

fn phandle_gpio(phandle: u32, property_name: &str) -> Option<GpioSpecifier> {
    let fdt = scarlet::device::fdt::FdtManager::get_manager().get_fdt()?;
    for node in fdt.all_nodes() {
        let node_phandle = node
            .property("phandle")
            .or_else(|| node.property("linux,phandle"))
            .and_then(|property| read_be_u32(property.value, 0));
        if node_phandle == Some(phandle) {
            return gpio_from_bytes(node.property(property_name)?.value);
        }
    }
    None
}

fn drive_gpio(specifier: GpioSpecifier, asserted: bool) -> Result<(), &'static str> {
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(specifier.controller)
        .ok_or_else(|| {
            println!(
                "[qcom-sc7180-mdss] GPIO controller {:#x} is not ready",
                specifier.controller
            );
            scarlet::device::manager::PROBE_DEFER
        })?;
    let active_low = specifier.flags & 1 != 0;
    controller.set_direction_output(specifier.pin, asserted ^ active_low);
    Ok(())
}

fn power_panel_path() -> Result<Option<GpioSpecifier>, &'static str> {
    if let Some(enable) = compatible_gpio("ti,sn65dsi86", "enable-gpios") {
        drive_gpio(enable, true)?;
    }
    if let Some(supply) = phandle_property("boe,nv110wtm-n61", "power-supply")
        && let Some(enable) = phandle_gpio(supply, "gpio")
    {
        drive_gpio(enable, true)?;
    }
    time::udelay(250_000);
    Ok(compatible_gpio("pwm-backlight", "enable-gpios"))
}

fn allocate_scanout(config: &FramebufferConfig) -> Result<ContiguousPages, &'static str> {
    let pages = config
        .size()
        .checked_add(PAGE_SIZE - 1)
        .map(|size| size / PAGE_SIZE)
        .ok_or("qcom-sc7180-mdss: scanout size overflow")?;
    let scanout = ContiguousPages::new(pages)
        .ok_or("qcom-sc7180-mdss: failed to allocate contiguous scanout")?;
    let end = scanout
        .as_paddr()
        .checked_add(config.size().saturating_sub(1))
        .ok_or("qcom-sc7180-mdss: scanout range overflow")?;
    if end > MAXIMUM_SCANOUT_ADDRESS {
        return Err("qcom-sc7180-mdss: PMM did not provide a DMA32 scanout");
    }

    // SAFETY: `scanout` owns `pages` contiguous, CPU-mapped pages and the
    // write is bounded by that exact allocation.
    unsafe { ptr::write_bytes(scanout.as_ptr().cast::<u8>(), 0, pages * PAGE_SIZE) };
    Ok(scanout)
}

fn probe_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if DeviceManager::get_manager()
        .get_device_by_name("qcom-sc7180-mdss")
        .is_some()
    {
        return Ok(());
    }

    let backlight_enable = power_panel_path()?;
    let ec_phandle = compatible_phandle("google,cros-ec-spi")
        .ok_or("qcom-sc7180-mdss: Chrome EC phandle not found")?;
    let ec = match get_cros_ec_spi_by_phandle(ec_phandle) {
        Some(ec) => ec,
        None => {
            println!("[qcom-sc7180-mdss] Chrome EC is not ready, deferring");
            return probe_defer();
        }
    };
    let bridge_phandle = compatible_phandle("ti,sn65dsi86")
        .ok_or("qcom-sc7180-mdss: SN65DSI86 phandle not found")?;
    let bridge = match get_sn65dsi86_by_phandle(bridge_phandle) {
        Some(bridge) => bridge,
        None => {
            println!("[qcom-sc7180-mdss] SN65DSI86 is not ready, deferring");
            return probe_defer();
        }
    };

    bridge
        .initialize_reference_clock()
        .map_err(|_| "qcom-sc7180-mdss: failed to select bridge reference clock")?;
    let timing = bridge
        .read_edid()
        .map_err(|error| {
            println!("[qcom-sc7180-mdss] EDID read failed: {:?}", error);
            "qcom-sc7180-mdss: failed to read panel EDID"
        })?
        .preferred_timing()
        .map_err(|_| "qcom-sc7180-mdss: invalid preferred EDID timing")?;

    let config = FramebufferConfig {
        width: u32::from(timing.hactive),
        height: u32::from(timing.vactive),
        format: PixelFormat::XRGB8888,
        stride: u32::from(timing.hactive) * 4,
    };
    let scanout = allocate_scanout(&config)?;

    // Bind this board-level pipeline to the panel node. The MDSS parent node
    // carries an SMMU stream contract used by Linux, while this early native
    // scanout deliberately uses a physical DMA32 buffer and does not require
    // an IOMMU domain. Resolve the SoC register block independently so
    // Scarlet's generic pre-probe dependency resolver does not wait for an
    // unrelated SMMU driver.
    let (mdss_paddr, _) = compatible_register("qcom,sc7180-mdss", 0)
        .ok_or("qcom-sc7180-mdss: MDSS resource not found")?;
    let mdss_vaddr = vm::ioremap(mdss_paddr, MDSS_MAP_SIZE)
        .map_err(|_| "qcom-sc7180-mdss: MDSS ioremap failed")?;
    let (dispcc_paddr, dispcc_size) = compatible_register("qcom,sc7180-dispcc", 0)
        .ok_or("qcom-sc7180-mdss: DISP_CC resource not found")?;
    if dispcc_size < DISPCC_MAP_SIZE {
        return Err("qcom-sc7180-mdss: DISP_CC resource is too small");
    }
    let dispcc_vaddr = vm::ioremap(dispcc_paddr, DISPCC_MAP_SIZE)
        .map_err(|_| "qcom-sc7180-mdss: DISP_CC ioremap failed")?;
    let mdss = RegisterWindow::new(mdss_vaddr);
    let dispcc = RegisterWindow::new(dispcc_vaddr);
    let engine = Sc7180DisplayEngine::new(mdss);

    arch::clean_dcache_to_poc_range(scanout.as_vaddr(), config.size());
    engine.initialize(mdss, dispcc, &bridge, timing, scanout.as_paddr())?;
    ec.set_display_backlight_percent(80).map_err(|error| {
        println!(
            "[qcom-sc7180-mdss] failed to restore EC display PWM: {:?}",
            error
        );
        "qcom-sc7180-mdss: Chrome EC backlight command failed"
    })?;
    if let Some(enable) = backlight_enable {
        drive_gpio(enable, true)?;
    }

    let graphics = Arc::new(Sc7180GraphicsDevice {
        config,
        timing,
        scanout,
        engine,
    });
    let device_id = DeviceManager::get_manager()
        .register_device_with_name(String::from("qcom-sc7180-mdss"), graphics.clone());
    if let Err(error) = scarlet::device::graphics::manager::GraphicsManager::get_manager()
        .register_native_framebuffer_from_device(device_id, graphics.clone())
    {
        DeviceManager::get_manager().unregister_device(device_id);
        return Err(error);
    }

    if scarlet::earlyfb::is_initialized() {
        scarlet::earlyfb::deactivate();
    }
    println!(
        "[qcom-sc7180-mdss] native panel {}x{} pixel-clock={} kHz scanout={:#x} bytes={} refresh={} mHz",
        graphics.timing.hactive,
        graphics.timing.vactive,
        graphics.timing.pixel_clock_khz,
        graphics.scanout.as_paddr(),
        graphics.config.size(),
        u64::from(graphics.timing.pixel_clock_khz) * 1_000_000
            / u64::from(graphics.timing.horizontal_total())
            / u64::from(graphics.timing.vertical_total()),
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-sc7180-mdss",
        probe_fn,
        remove_fn,
        vec!["boe,nv110wtm-n61"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Late);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_MDSS_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
