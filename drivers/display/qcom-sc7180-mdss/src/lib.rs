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

#[cfg(any(
    all(feature = "diagnostic-color-bar", feature = "diagnostic-border-fill"),
    all(feature = "diagnostic-color-bar", feature = "diagnostic-dsi-pattern"),
    all(feature = "diagnostic-border-fill", feature = "diagnostic-dsi-pattern"),
))]
compile_error!("display diagnostic modes are mutually exclusive");

mod dpu;
mod dsi;
mod phy;
mod registers;

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::{
    any::Any,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use dpu::Dpu;
use dsi::DsiHost;
use phy::DsiPhy;
use registers::RegisterWindow;
use scarlet::{
    arch,
    device::{
        Device, DeviceType,
        graphics::{
            FramebufferConfig, GpuDisplayResource, GpuLinearDisplayBacking, GraphicsDevice,
            PixelFormat, output::DisplayRegion,
        },
        iommu::{DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    environment::PAGE_SIZE,
    mem::page::ContiguousPages,
    object::capability::{
        ControlOps, MemoryMappingOps,
        selectable::{ReadyInterest, SelectWaitOutcome, Selectable},
    },
    println,
    sync::Mutex,
    time,
    vm::{self, vmem::MemoryAttribute},
};
use scarlet_driver_cros_ec_spi::get_primary_cros_ec_spi;
use scarlet_driver_qcom_sc7180_dispcc::Sc7180DispCc;
#[cfg(any(feature = "diagnostic-color-bar", feature = "diagnostic-dsi-pattern"))]
use scarlet_driver_ti_sn65dsi86::Sn65dsi86ColorBar;
use scarlet_driver_ti_sn65dsi86::{DisplayTiming, Sn65dsi86, get_sn65dsi86_by_phandle};

const MDSS_MAP_SIZE: usize = 0x0c_0000;
const DSI_LANES: u8 = 4;
const DSI_BITS_PER_PIXEL: u8 = 24;
const MAXIMUM_SCANOUT_ADDRESS: usize = u32::MAX as usize;
const SCANOUT_BUFFER_COUNT: usize = 2;
const PRESENT_TIMEOUT_US: u64 = 50_000;

#[derive(Clone, Copy)]
struct GpioSpecifier {
    controller: u32,
    pin: u32,
    flags: u32,
}

struct Sc7180DisplayEngine {
    dpu: Dpu,
}

impl Sc7180DisplayEngine {
    fn new(mdss: RegisterWindow) -> Self {
        Self {
            dpu: Dpu::new(mdss),
        }
    }

    fn initialize(
        &self,
        mdss: RegisterWindow,
        dispcc: &Sc7180DispCc,
        bridge: &Sn65dsi86,
        timing: DisplayTiming,
        scanout_dma_addr: usize,
    ) -> Result<(), &'static str> {
        let dsi = DsiHost::new(mdss);
        // Linux prepares the downstream bridge first and observes td7 before
        // allowing DSI data onto the link.
        bridge
            .initialize_reference_clock()
            .map_err(|_| "qcom-sc7180-mdss: failed to prepare bridge reference clock")?;
        time::udelay(100);

        dsi.reset_phy();
        let dsi_clock_timings = DsiPhy::new(mdss).initialize(
            timing,
            u32::from(DSI_LANES),
            u32::from(DSI_BITS_PER_PIXEL),
        )?;
        dispcc.enable_dsi0()?;
        dsi.configure_timing(timing, dsi_clock_timings);
        // The DSI controller can only be reset while the link clocks run.
        dsi.reset();
        dsi.configure_host(u32::from(DSI_LANES))?;

        #[cfg(feature = "diagnostic-border-fill")]
        println!("[qcom-sc7180-mdss] DPU border-only diagnostic enabled (white)");
        self.dpu.configure(timing, scanout_dma_addr)?;
        let pre_video_fifo = dsi.clear_fifo_status();
        println!("[mdss-diag] dsi pre-video fifo={:#010x}", pre_video_fifo,);
        // The upstream DSI bridge is enabled during pre-enable, before the
        // encoder starts feeding pixels.
        dsi.enable_video();
        #[cfg(feature = "diagnostic-dsi-pattern")]
        {
            dsi.enable_video_test_pattern();
            println!("[qcom-sc7180-mdss] DSI host checkerboard diagnostic enabled");
        }

        // Linux enables and trains the downstream bridge after DSI pre-enable,
        // but before the DPU commit kickoff. Only after VSTREAM is live does
        // the DPU issue CTL flush/start and enable its timing engine.
        bridge
            .configure_link(timing, DSI_LANES, DSI_BITS_PER_PIXEL)
            .map_err(|error| {
                println!(
                    "[qcom-sc7180-mdss] SN65DSI86 link setup failed: {:?}",
                    error
                );
                "qcom-sc7180-mdss: bridge link setup failed"
            })?;
        #[cfg(feature = "diagnostic-dsi-pattern")]
        {
            bridge.arm_dsi_clock_detector().map_err(|error| {
                println!(
                    "[qcom-sc7180-mdss] failed to arm SN65 DSI clock detector: {:?}",
                    error
                );
                "qcom-sc7180-mdss: bridge DSI clock detector setup failed"
            })?;
            println!("[qcom-sc7180-mdss] SN65 DSI input clock detector armed");
        }
        self.dpu.start();
        #[cfg(feature = "diagnostic-color-bar")]
        {
            bridge
                .set_color_bar(Some(Sn65dsi86ColorBar::VerticalEightColors))
                .map_err(|_| "qcom-sc7180-mdss: failed to enable bridge color bar")?;
            println!("[qcom-sc7180-mdss] SN65DSI86 diagnostic color bar enabled");
        }
        time::udelay(20_000);

        let dpu = self.dpu.diagnostic_snapshot();
        println!(
            "[mdss-diag] dpu layer={:#010x} flush={:#010x} start={:#010x}",
            dpu.control_layer0, dpu.control_flush, dpu.control_start,
        );
        println!(
            "[mdss-diag] dpu active={:#010x} address={:#010x}",
            dpu.interface_active, dpu.source_address,
        );
        println!(
            "[mdss-diag] dpu stride={:#010x} format={:#010x} op={:#010x}",
            dpu.source_stride, dpu.source_format, dpu.source_operation,
        );
        println!(
            "[mdss-diag] dpu mixer={:#010x} border={:#010x}/{:#010x}",
            dpu.mixer_output_size, dpu.mixer_border_color_0, dpu.mixer_border_color_1,
        );
        println!(
            "[mdss-diag] dpu enable={:#010x} config={:#010x}",
            dpu.timing_enable, dpu.interface_config,
        );
        println!(
            "[mdss-diag] dpu hsync={:#010x} vsync={:#010x}",
            dpu.hsync_control, dpu.vsync_period,
        );
        println!(
            "[mdss-diag] dpu hctl={:#010x} panel={:#010x}",
            dpu.display_hcontrol, dpu.panel_format,
        );
        println!(
            "[mdss-diag] dpu data-hctl={:#010x} config2={:#010x} polarity={:#010x}",
            dpu.display_data_hcontrol, dpu.interface_config2, dpu.polarity_control,
        );
        println!(
            "[mdss-diag] dpu fetch={:#010x} mux={:#010x}",
            dpu.fetch_start, dpu.interface_mux,
        );

        let dsi = dsi.diagnostic_snapshot();
        println!(
            "[mdss-diag] dsi hw={:#010x} ctrl={:#010x}",
            dsi.hardware_version, dsi.control,
        );
        println!(
            "[mdss-diag] dsi status={:#010x} fifo={:#010x}",
            dsi.status, dsi.fifo_status,
        );
        println!(
            "[mdss-diag] dsi video={:#010x} clk={:#010x} clk-pre-extend={:#010x}",
            dsi.video_mode_control, dsi.clock_control, dsi.clock_pre_extend,
        );
        println!("[mdss-diag] dsi clk-status={:#010x}", dsi.clock_status,);
        println!(
            "[mdss-diag] dsi lane={:#010x} lane-ctrl={:#010x}",
            dsi.lane_status, dsi.lane_control,
        );
        println!(
            "[mdss-diag] dsi ack={:#010x} phy0={:#010x}",
            dsi.ack_error_status, dsi.data_lane0_phy_error,
        );
        println!(
            "[mdss-diag] dsi timeout={:#010x} intr={:#010x}",
            dsi.timeout_status, dsi.interrupt_control,
        );
        println!("[mdss-diag] dsi tpg={:#010x}", dsi.test_pattern_control,);
        println!(
            "[mdss-diag] phy clk={:#010x}/{:#010x}",
            dsi.phy_clock_config0, dsi.phy_clock_config1,
        );
        println!(
            "[mdss-diag] phy global={:#010x} vreg={:#010x}",
            dsi.phy_global_control, dsi.phy_vreg_control,
        );
        println!(
            "[mdss-diag] phy ctrl={:#010x}/{:#010x}",
            dsi.phy_control0, dsi.phy_control2,
        );
        println!(
            "[mdss-diag] phy map={:#010x}/{:#010x}",
            dsi.phy_lane_config0, dsi.phy_lane_config1,
        );
        println!(
            "[mdss-diag] phy pll-ctrl={:#010x} lane={:#010x}",
            dsi.phy_pll_control, dsi.phy_lane_control0,
        );
        println!(
            "[mdss-diag] phy status={:#010x} pll={:#010x}",
            dsi.phy_status, dsi.pll_status,
        );

        let clocks = dispcc.dsi0_clock_snapshot();
        println!(
            "[mdss-diag] pclk cmd={:#010x} cfg={:#010x}",
            clocks.pclk0_command, clocks.pclk0_config,
        );
        println!("[mdss-diag] pclk branch={:#010x}", clocks.pclk0_branch,);
        println!(
            "[mdss-diag] byte cmd={:#010x} cfg={:#010x}",
            clocks.byte0_command, clocks.byte0_config,
        );
        println!("[mdss-diag] byte branch={:#010x}", clocks.byte0_branch,);
        println!(
            "[mdss-diag] byte-intf div={:#010x} branch={:#010x}",
            clocks.byte0_interface_divider, clocks.byte0_interface_branch,
        );
        println!(
            "[mdss-diag] esc cmd={:#010x} cfg={:#010x}",
            clocks.esc0_command, clocks.esc0_config,
        );
        println!("[mdss-diag] esc branch={:#010x}", clocks.esc0_branch,);

        let bridge = bridge.diagnostic_snapshot().map_err(|error| {
            println!(
                "[qcom-sc7180-mdss] SN65DSI86 post-start diagnostic failed: {:?}",
                error
            );
            "qcom-sc7180-mdss: bridge post-start diagnostic failed"
        })?;
        println!(
            "[mdss-diag] bridge pll={} lock={} stream={}",
            bridge.pll_enabled,
            bridge.dp_pll_locked(),
            bridge.video_stream_enabled(),
        );
        println!(
            "[mdss-diag] bridge hpd={} disabled={}",
            bridge.hpd_asserted(),
            bridge.hpd_disabled(),
        );
        println!(
            "[mdss-diag] bridge color={} reg={:#04x}",
            bridge.color_bar_enabled(),
            bridge.color_bar,
        );
        println!(
            "[mdss-diag] bridge dsi-lanes={:#04x} dsi-clk={:#04x}",
            bridge.dsi_lanes, bridge.dsi_clock,
        );
        #[cfg(feature = "diagnostic-dsi-pattern")]
        if bridge.dsi_clock == 0 {
            println!("[mdss-diag] bridge DSI clock detector: no input clock observed");
        } else {
            println!(
                "[mdss-diag] bridge DSI clock detector: measured range={:#04x}",
                bridge.dsi_clock,
            );
        }
        println!(
            "[mdss-diag] bridge lane-map={:#04x} polarity={:#04x} format={:#04x} training={:#04x}",
            bridge.dp_lane_assignment,
            bridge.enhanced_frame & 0xf0,
            bridge.data_format,
            bridge.training_settings,
        );
        println!(
            "[mdss-diag] bridge sync h-positive={} v-positive={} regs={:#04x}/{:#04x}",
            timing.hsync_positive,
            timing.vsync_positive,
            bridge.hsync_width_high,
            bridge.vsync_width_high,
        );
        println!(
            "[mdss-diag] bridge dp-lanes={:#04x} rate={:#04x} link={:#04x}",
            bridge.ssc_config, bridge.data_rate, bridge.main_link_mode,
        );
        println!("[mdss-diag] bridge f0..f8={:02x?}", bridge.error_status,);
        Ok(())
    }

    fn present(&self, scanout_dma_addr: usize) -> Result<(), &'static str> {
        self.dpu.present(scanout_dma_addr)?;

        // SC7180 video-mode commits clear CTL_FLUSH at the vblank where the
        // new source address becomes active. Do not release the previous
        // front buffer back to the compositor before that boundary.
        let start = time::current_time();
        while self.dpu.pending_flush() != 0 {
            let elapsed = time::current_time().saturating_sub(start);
            if elapsed >= PRESENT_TIMEOUT_US {
                return Err("qcom-sc7180-mdss: timeout waiting for page flip");
            }
            if elapsed < 50 {
                core::hint::spin_loop();
            } else if let Some(task) = scarlet::task::mytask() {
                // The flip is vblank-paced and can be almost one frame away.
                // Yield the submitting task instead of burning that interval
                // in the kernel with preemption otherwise enabled.
                scarlet::sched::scheduler::schedule(task.get_trapframe());
            } else {
                core::hint::spin_loop();
            }
        }
        Ok(())
    }
}

pub struct Sc7180GraphicsDevice {
    config: FramebufferConfig,
    timing: DisplayTiming,
    scanout: [ContiguousPages; SCANOUT_BUFFER_COUNT],
    scanout_dma: [DmaMapping; SCANOUT_BUFFER_COUNT],
    gpu_staging: Mutex<GpuStagingState>,
    gpu_present_count: AtomicUsize,
    front: AtomicUsize,
    present_lock: Mutex<()>,
    engine: Sc7180DisplayEngine,
}

struct GpuStagingState {
    source: Option<GpuLinearDisplayBacking>,
    initialized: [bool; SCANOUT_BUFFER_COUNT],
    previous_damage: Option<DisplayRegion>,
}

impl GpuStagingState {
    const fn new() -> Self {
        Self {
            source: None,
            initialized: [false; SCANOUT_BUFFER_COUNT],
            previous_damage: None,
        }
    }

    fn reset(&mut self) {
        self.source = None;
        self.initialized.fill(false);
        self.previous_damage = None;
    }
}

impl Sc7180GraphicsDevice {
    fn clean_regions(
        &self,
        scanout: &ContiguousPages,
        regions: &[DisplayRegion],
    ) -> Result<(), &'static str> {
        if scanout.memory_attribute() != MemoryAttribute::Normal {
            arch::io_wmb();
            return Ok(());
        }

        if regions.is_empty() {
            arch::clean_dcache_to_poc_range(scanout.as_vaddr(), self.config.size());
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
            arch::clean_dcache_to_poc_range(scanout.as_vaddr() + start, end - start);
        }
        Ok(())
    }

    fn validate_config(&self, config: &FramebufferConfig) -> Result<(), &'static str> {
        if config.width != self.config.width
            || config.height != self.config.height
            || config.stride != self.config.stride
            || config.format != self.config.format
        {
            return Err("qcom-sc7180-mdss: framebuffer does not match native scanout");
        }
        Ok(())
    }

    fn validate_gpu_backing(
        &self,
        resource: GpuDisplayResource,
    ) -> Result<GpuLinearDisplayBacking, &'static str> {
        let backing = resource
            .linear_backing()
            .ok_or("qcom-sc7180-mdss: GPU image is not a linear framebuffer")?;
        if resource.width() != self.config.width
            || resource.height() != self.config.height
            || backing.stride() != self.config.stride
            || backing.format() != self.config.format
        {
            return Err("qcom-sc7180-mdss: GPU image does not match native scanout");
        }
        let required = u64::from(backing.stride())
            .checked_mul(u64::from(resource.height()))
            .ok_or("qcom-sc7180-mdss: GPU scanout size overflow")?;
        if backing.allocation_size() < required {
            return Err("qcom-sc7180-mdss: GPU scanout backing is undersized");
        }
        Ok(backing)
    }

    fn clamp_region(&self, region: DisplayRegion) -> Option<DisplayRegion> {
        let x = region.x.min(self.config.width);
        let y = region.y.min(self.config.height);
        let width = region.width.min(self.config.width.saturating_sub(x));
        let height = region.height.min(self.config.height.saturating_sub(y));
        (width != 0 && height != 0).then_some(DisplayRegion::new(x, y, width, height))
    }

    fn region_contains(outer: DisplayRegion, inner: DisplayRegion) -> bool {
        outer.x <= inner.x
            && outer.y <= inner.y
            && outer.x.saturating_add(outer.width) >= inner.x.saturating_add(inner.width)
            && outer.y.saturating_add(outer.height) >= inner.y.saturating_add(inner.height)
    }

    fn copy_gpu_region_to_scanout(
        &self,
        backing: &GpuLinearDisplayBacking,
        destination: &ContiguousPages,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        let region = self
            .clamp_region(region)
            .ok_or("qcom-sc7180-mdss: GPU damage region is empty")?;
        let bytes_per_pixel = self.config.format.bytes_per_pixel();
        let source_stride = backing.stride() as usize;
        let destination_stride = self.config.stride as usize;
        let x_bytes = (region.x as usize)
            .checked_mul(bytes_per_pixel)
            .ok_or("qcom-sc7180-mdss: GPU damage x offset overflows")?;
        let row_bytes = (region.width as usize)
            .checked_mul(bytes_per_pixel)
            .ok_or("qcom-sc7180-mdss: GPU damage row size overflows")?;
        let first_source_offset = (region.y as usize)
            .checked_mul(source_stride)
            .and_then(|offset| offset.checked_add(x_bytes))
            .ok_or("qcom-sc7180-mdss: GPU source offset overflows")?;
        let last_source_end = (region.y as usize + region.height as usize - 1)
            .checked_mul(source_stride)
            .and_then(|offset| offset.checked_add(x_bytes))
            .and_then(|offset| offset.checked_add(row_bytes))
            .ok_or("qcom-sc7180-mdss: GPU source range overflows")?;
        let source_allocation_size = usize::try_from(backing.allocation_size())
            .map_err(|_| "qcom-sc7180-mdss: GPU source allocation exceeds usize")?;
        if last_source_end > source_allocation_size {
            return Err("qcom-sc7180-mdss: GPU source damage exceeds backing");
        }

        let source_base = vm::phys_to_virt(backing.physical_addr());
        arch::invalidate_dcache_to_poc_range(
            source_base + first_source_offset,
            last_source_end - first_source_offset,
        );
        for row in 0..region.height as usize {
            let source_offset = (region.y as usize + row)
                .checked_mul(source_stride)
                .and_then(|offset| offset.checked_add(x_bytes))
                .ok_or("qcom-sc7180-mdss: GPU source row overflows")?;
            let destination_offset = (region.y as usize + row)
                .checked_mul(destination_stride)
                .and_then(|offset| offset.checked_add(x_bytes))
                .ok_or("qcom-sc7180-mdss: GPU destination row overflows")?;
            if destination_offset.saturating_add(row_bytes) > self.config.size() {
                return Err("qcom-sc7180-mdss: GPU destination damage exceeds scanout");
            }
            // SAFETY: the source is a retained, validated linear GPU backing;
            // the destination is the non-visible owned scanout; both row
            // ranges were checked against their complete allocations.
            unsafe {
                ptr::copy_nonoverlapping(
                    (source_base + source_offset) as *const u8,
                    (destination.as_vaddr() + destination_offset) as *mut u8,
                    row_bytes,
                );
            }
        }
        self.clean_regions(destination, &[region])
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
        let back = self.front.load(Ordering::Acquire) ^ 1;
        Ok(self.scanout[back].as_paddr())
    }

    fn present_framebuffer_region(
        &self,
        config: &FramebufferConfig,
        physical_addr: usize,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        self.validate_config(config)?;
        let _present = self.present_lock.lock();
        let back = self.front.load(Ordering::Acquire) ^ 1;
        if physical_addr != self.scanout[back].as_paddr() {
            return Err("qcom-sc7180-mdss: framebuffer does not match back scanout");
        }
        self.clean_regions(&self.scanout[back], &[region])?;
        self.engine
            .present(dpu_dma_address(&self.scanout_dma[back])?)?;
        self.gpu_staging.lock().reset();
        self.front.store(back, Ordering::Release);
        Ok(())
    }

    fn scanout_buffer_count(&self) -> usize {
        self.scanout.len()
    }

    fn front_scanout_buffer(&self) -> Option<usize> {
        Some(self.front.load(Ordering::Acquire))
    }

    fn get_scanout_buffer_info(
        &self,
        index: usize,
    ) -> Result<(FramebufferConfig, usize), &'static str> {
        let scanout = self
            .scanout
            .get(index)
            .ok_or("qcom-sc7180-mdss: invalid scanout index")?;
        Ok((self.config.clone(), scanout.as_paddr()))
    }

    fn present_scanout_buffer(&self, index: usize) -> Result<(), &'static str> {
        self.present_scanout_buffer_regions(index, &[])
    }

    fn present_scanout_buffer_regions(
        &self,
        index: usize,
        regions: &[DisplayRegion],
    ) -> Result<(), &'static str> {
        let scanout = self
            .scanout
            .get(index)
            .ok_or("qcom-sc7180-mdss: invalid scanout index")?;
        let scanout_dma = self
            .scanout_dma
            .get(index)
            .ok_or("qcom-sc7180-mdss: invalid scanout DMA index")?;
        let _present = self.present_lock.lock();
        if index == self.front.load(Ordering::Acquire) {
            return Err("qcom-sc7180-mdss: scanout buffer is already front-most");
        }

        self.clean_regions(scanout, regions)?;
        self.engine.present(dpu_dma_address(scanout_dma)?)?;
        self.gpu_staging.lock().reset();
        self.front.store(index, Ordering::Release);
        Ok(())
    }

    fn present_gpu_resource_region(
        &self,
        resource: GpuDisplayResource,
        region: DisplayRegion,
    ) -> Result<(), &'static str> {
        let backing = self.validate_gpu_backing(resource)?;
        let damage = self
            .clamp_region(region)
            .ok_or("qcom-sc7180-mdss: GPU damage region is empty")?;
        let _present = self.present_lock.lock();
        let back = self.front.load(Ordering::Acquire) ^ 1;
        let mut staging = self.gpu_staging.lock();
        if staging
            .source
            .as_ref()
            .is_none_or(|source| source != &backing)
        {
            staging.reset();
            staging.source = Some(backing.clone());
        }

        // Each scanout is reused two presents later. If it already contains a
        // staged frame, bring it forward by copying both the damage from the
        // skipped frame and this frame. Keep disjoint regions separate: using
        // their bounding box turns an 80-pixel taskbar update followed by one
        // small window update into a multi-megabyte full-width CPU copy.
        let copy_regions = if staging.initialized[back] {
            match staging.previous_damage {
                Some(previous) if Self::region_contains(previous, damage) => [Some(previous), None],
                Some(previous) if Self::region_contains(damage, previous) => [Some(damage), None],
                Some(previous) => [Some(previous), Some(damage)],
                None => [Some(damage), None],
            }
        } else {
            [Some(DisplayRegion::full(&self.config)), None]
        };
        #[cfg(debug_assertions)]
        let copy_start = time::current_time();
        #[cfg(debug_assertions)]
        let mut copied_pixels = 0u64;
        #[cfg(debug_assertions)]
        let mut copied_regions = 0usize;
        for copy_region in copy_regions.into_iter().flatten() {
            self.copy_gpu_region_to_scanout(&backing, &self.scanout[back], copy_region)?;
            #[cfg(debug_assertions)]
            {
                copied_pixels = copied_pixels.saturating_add(
                    u64::from(copy_region.width).saturating_mul(u64::from(copy_region.height)),
                );
                copied_regions += 1;
            }
        }
        #[cfg(debug_assertions)]
        let copy_us = time::current_time().saturating_sub(copy_start);
        #[cfg(debug_assertions)]
        let flip_start = time::current_time();
        self.engine
            .present(dpu_dma_address(&self.scanout_dma[back])?)?;
        #[cfg(debug_assertions)]
        let flip_us = time::current_time().saturating_sub(flip_start);
        staging.source = Some(backing);
        staging.initialized[back] = true;
        staging.previous_damage = Some(damage);
        self.front.store(back, Ordering::Release);

        #[cfg(debug_assertions)]
        {
            let sequence = self.gpu_present_count.fetch_add(1, Ordering::Relaxed) + 1;
            if sequence <= 4 || sequence.is_power_of_two() {
                println!(
                    "[qcom-sc7180-mdss] GPU stage seq={} back={} regions={} pixels={} damage={}x{}+{},{} copy_us={} flip_us={}",
                    sequence,
                    back,
                    copied_regions,
                    copied_pixels,
                    damage.width,
                    damage.height,
                    damage.x,
                    damage.y,
                    copy_us,
                    flip_us,
                );
            }
        }
        Ok(())
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
    let mut scanout = ContiguousPages::new(pages)
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
    // `/dev/fb0` and `/dev/display0` expose scanout pages as DeviceBurstable.
    // Keep the kernel direct-map alias at the same attribute, matching the
    // Apple DCP driver and Scarlet's direct-map aliasing contract.
    scanout.retag_memory_attribute(MemoryAttribute::DeviceBurstable)?;
    Ok(scanout)
}

fn map_scanout(
    dma_context: &scarlet::device::iommu::DmaContext,
    scanout: &ContiguousPages,
) -> Result<DmaMapping, &'static str> {
    let mapping = dma_context
        .map_phys_owned(
            scanout.as_paddr(),
            scanout
                .len()
                .checked_mul(PAGE_SIZE)
                .ok_or("qcom-sc7180-mdss: scanout DMA length overflow")?,
            IommuMapFlags::READ | IommuMapFlags::COHERENT,
        )
        .map_err(|_| "qcom-sc7180-mdss: failed to map scanout for DPU DMA")?;
    dpu_dma_address(&mapping)?;
    Ok(mapping)
}

/// Return a DPU-programmable DMA address after validating the whole mapping.
///
/// The DPU address register is only 32 bits wide.  Checking the final mapped
/// byte prevents a page-rounded IOMMU mapping from silently crossing that
/// limit even when its first DMA address happens to fit.
fn dpu_dma_address(mapping: &DmaMapping) -> Result<usize, &'static str> {
    let mapped_length = mapping.len();
    let last_byte = u64::try_from(
        mapped_length
            .checked_sub(1)
            .ok_or("qcom-sc7180-mdss: DPU DMA mapping is empty")?,
    )
    .map_err(|_| "qcom-sc7180-mdss: DPU DMA mapping length exceeds u64")?;
    let dma_end = mapping
        .dma_addr()
        .checked_add(last_byte)
        .ok_or("qcom-sc7180-mdss: DPU DMA range overflows")?;
    if dma_end > u64::from(u32::MAX) {
        return Err("qcom-sc7180-mdss: DPU DMA range exceeds 32-bit address space");
    }
    usize::try_from(mapping.dma_addr())
        .map_err(|_| "qcom-sc7180-mdss: DPU DMA address exceeds usize")
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if DeviceManager::get_manager()
        .get_device_by_name("qcom-sc7180-mdss")
        .is_some()
    {
        return Ok(());
    }

    let dispcc_phandle = compatible_phandle("qcom,sc7180-dispcc")
        .ok_or("qcom-sc7180-mdss: DISP_CC phandle not found")?;
    let dispcc =
        match scarlet_driver_qcom_sc7180_dispcc::get_sc7180_dispcc_by_phandle(dispcc_phandle) {
            Some(controller) => controller,
            None => {
                println!("[qcom-sc7180-mdss] DISP_CC is not ready, deferring");
                return probe_defer();
            }
        };
    println!("[qcom-sc7180-mdss] phase 1/5: restoring MDSS power and foundational clocks");
    dispcc.prepare_for_scanout()?;

    let ec = match get_primary_cros_ec_spi() {
        Some(ec) => ec,
        None => {
            println!("[qcom-sc7180-mdss] primary Chrome EC is not ready, deferring");
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

    println!("[qcom-sc7180-mdss] phase 2/5: enabling bridge and panel rails");
    let backlight_enable = power_panel_path()?;
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
        // Scarlet's compositor produces byte-ordered BGRA scanout buffers.
        // The DPU source pipe consumes those bytes as DRM ARGB/XRGB8888
        // (B, G, R, A/X in little-endian memory).
        format: PixelFormat::BGRA8888,
        stride: u32::from(timing.hactive) * 4,
    };
    let scanout = [allocate_scanout(&config)?, allocate_scanout(&config)?];

    let dma_context = DeviceManager::get_manager().resolve_platform_dma_context(
        device,
        IommuDomainConfig {
            domain_type: IommuDomainType::Identity,
            iova_base: 0,
            iova_size: 0,
        },
    )?;
    let scanout_dma = [
        map_scanout(&dma_context, &scanout[0])?,
        map_scanout(&dma_context, &scanout[1])?,
    ];
    let scanout_dma_addr = dpu_dma_address(&scanout_dma[0])?;

    let mdss_resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-sc7180-mdss: missing MDSS memory resource")?;
    let mdss_vaddr = vm::ioremap(mdss_resource.start, MDSS_MAP_SIZE)
        .map_err(|_| "qcom-sc7180-mdss: MDSS ioremap failed")?;
    let mdss = RegisterWindow::new(mdss_vaddr);
    let engine = Sc7180DisplayEngine::new(mdss);

    println!("[qcom-sc7180-mdss] phase 3/5: programming DSI PHY, bridge link, and DPU");
    if scanout[0].memory_attribute() == MemoryAttribute::Normal {
        arch::clean_dcache_to_poc_range(scanout[0].as_vaddr(), config.size());
    } else {
        arch::io_wmb();
    }
    engine.initialize(mdss, &dispcc, &bridge, timing, scanout_dma_addr)?;
    println!("[qcom-sc7180-mdss] phase 4/5: enabling EC backlight PWM");
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
    #[cfg(feature = "diagnostic-dsi-pattern")]
    {
        bridge
            .set_color_bar(Some(Sn65dsi86ColorBar::VerticalEightColors))
            .map_err(|_| "qcom-sc7180-mdss: failed to enable bridge comparison pattern")?;
        println!("[qcom-sc7180-mdss] showing SN65 comparison color bar for 2 seconds");
        time::udelay(2_000_000);
        bridge
            .set_color_bar(None)
            .map_err(|_| "qcom-sc7180-mdss: failed to restore DSI input video")?;
        println!("[qcom-sc7180-mdss] switched from SN65 color bar to DSI checkerboard");
    }

    println!("[qcom-sc7180-mdss] phase 5/5: publishing native framebuffer");

    let graphics = Arc::new(Sc7180GraphicsDevice {
        config,
        timing,
        scanout,
        scanout_dma,
        gpu_staging: Mutex::new(GpuStagingState::new()),
        gpu_present_count: AtomicUsize::new(0),
        front: AtomicUsize::new(0),
        present_lock: Mutex::new(()),
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
        "[qcom-sc7180-mdss] native panel {}x{} pixel-clock={} kHz scanout-paddr=[{:#x}, {:#x}] scanout-dma=[{:#x}, {:#x}] bytes={} refresh={} mHz",
        graphics.timing.hactive,
        graphics.timing.vactive,
        graphics.timing.pixel_clock_khz,
        graphics.scanout[0].as_paddr(),
        graphics.scanout[1].as_paddr(),
        graphics.scanout_dma[0].dma_addr(),
        graphics.scanout_dma[1].dma_addr(),
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
        vec!["qcom,sc7180-mdss"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Late);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_MDSS_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
