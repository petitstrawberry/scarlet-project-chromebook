// SPDX-License-Identifier: GPL-2.0-only
//! SC7180 native external DP path. AUX/link programming follows ChromeOS Linux
//! 6.6 cc1a33bf3fcee271d9f66ccd9fbbc7b06eacdabd, msm/dp/dp_{aux,catalog,ctrl}.c.
//! EC firmware owns Type-C negotiation; this driver only observes its result.
use super::*;
use scarlet::device::phy::{DisplayPortConfig, PhyHandle, PhyMode, PhyOrientation};
use scarlet_driver_cros_ec_spi::CrosEcTypecState;

const AUX: usize = 0x200;
const LINK: usize = 0x400;
const AUX_DONE: u32 = 1 << 3;
const AUX_EVENTS: u32 = (1 << 3)
    | (1 << 6)
    | (1 << 9)
    | (1 << 12)
    | (1 << 15)
    | (1 << 18)
    | (1 << 21)
    | (1 << 24)
    | (1 << 27);
static CONTROLLER: IrqSpinLock<Option<Arc<DpController>>> = IrqSpinLock::new(None);
static PRIMARY: IrqSpinLock<Option<Arc<Sc7180GraphicsDevice>>> = IrqSpinLock::new(None);
static WORKER_WAKER: Waker = Waker::new_uninterruptible("sc7180-dp-hotplug");

pub(super) struct DpController {
    registers: RegisterWindow,
    parent: Arc<Sc7180GraphicsDevice>,
    phy: PhyHandle,
    clocks: Arc<Sc7180DispCc>,
    connected: AtomicBool,
    aux_powered: AtomicBool,
    link_powered: AtomicBool,
    display: IrqSpinLock<Option<Arc<Sc7180GraphicsDevice>>>,
}

pub(super) fn set_primary(display: Arc<Sc7180GraphicsDevice>) {
    *PRIMARY.lock() = Some(display);
}
pub(super) fn controller() -> Option<Arc<DpController>> {
    CONTROLLER.lock().clone()
}
impl DpController {
    pub(super) fn display(&self) -> Option<Arc<Sc7180GraphicsDevice>> {
        self.display.lock().clone()
    }
    pub(super) fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    fn reset_controller(&self) {
        // Once VIDEO/IDLE has run, the FSM reset requires live link clocks.
        self.registers.update(0x10, 0, 1);
        time::udelay(1000);
        self.registers.update(0x10, 1, 0);
    }
    fn reset_aux(&self) {
        self.registers.write(AUX + 0x30, 2);
        time::udelay(1000);
        self.registers.write(AUX + 0x30, 1);
        self.registers.write(AUX + 0x3c, 0xffff);
        self.registers.write(AUX + 0x40, 0xffff);
    }
    fn acknowledge_aux(&self) -> u32 {
        let status = self.registers.read(0x20) & AUX_EVENTS;
        self.registers.write(0x20, status << 1);
        status
    }
    fn transfer(
        &self,
        address: u32,
        native: bool,
        read: bool,
        bytes: &mut [u8],
        mot: bool,
    ) -> Result<(), &'static str> {
        if bytes.is_empty() || bytes.len() > 16 || address > 0xfffff {
            return Err("DP: invalid AUX transaction");
        }
        for _ in 0..7 {
            self.acknowledge_aux();
            self.registers.write(AUX + 0x38, 0);
            for clear in [0x1f, 0x9f, 0] {
                self.registers.write(AUX + 0x4c, clear);
            }
            let header = [
                ((address >> 16) as u8 & 15) | if read { 16 } else { 0 },
                (address >> 8) as u8,
                address as u8,
                (bytes.len() - 1) as u8,
            ];
            for (i, &byte) in header
                .iter()
                .chain(if read { &[][..] } else { &bytes[..] })
                .enumerate()
            {
                self.registers.write(
                    AUX + 0x34,
                    (u32::from(byte) << 8) | if i == 0 { 1 << 31 } else { 0 },
                );
            }
            self.registers.write(
                AUX + 0x38,
                0x200 | if native { 0 } else { 0x100 } | if mot { 0x800 } else { 0 },
            );
            let started = time::current_time();
            let status = loop {
                let status = self.acknowledge_aux();
                if status != 0 {
                    break status;
                }
                if time::current_time().saturating_sub(started) > 50_000 {
                    return Err("DP: AUX timeout");
                }
                time::udelay(50);
            };
            if status & AUX_DONE != 0 && status & (AUX_EVENTS & !AUX_DONE) == 0 {
                if read {
                    self.registers.update(AUX + 0x38, 0x200, 0);
                    self.registers.write(AUX + 0x34, (1 << 31) | 1);
                    let _ = self.registers.read(AUX + 0x34);
                    for (i, byte) in bytes.iter_mut().enumerate() {
                        let data = self.registers.read(AUX + 0x34);
                        if ((data >> 16) & 255) != i as u32 {
                            return Err("DP: AUX FIFO index mismatch");
                        }
                        *byte = (data >> 8) as u8;
                    }
                }
                return Ok(());
            }
            if status & ((1 << 12) | (1 << 21)) == 0 {
                return Err("DP: AUX transaction rejected");
            }
            time::udelay(500);
        }
        Err("DP: AUX defer retry limit")
    }
    fn read_dpcd(&self, address: u32, bytes: &mut [u8]) -> Result<(), &'static str> {
        for (i, part) in bytes.chunks_mut(16).enumerate() {
            self.transfer(address + (i * 16) as u32, true, true, part, false)?;
        }
        Ok(())
    }
    fn write_dpcd(&self, address: u32, bytes: &[u8]) -> Result<(), &'static str> {
        for (i, part) in bytes.chunks(16).enumerate() {
            let mut b = part.to_vec();
            self.transfer(address + (i * 16) as u32, true, false, &mut b, false)?;
        }
        Ok(())
    }
    fn edid(&self) -> Result<Vec<DisplayTiming>, &'static str> {
        let mut bytes = [0u8; 256];
        for block in 0..2 {
            if block == 1 && bytes[126] == 0 {
                break;
            }
            for offset in (block * 128..(block + 1) * 128).step_by(16) {
                self.transfer(0x50, false, false, &mut [offset as u8], true)?;
                self.transfer(0x50, false, true, &mut bytes[offset..offset + 16], false)?;
            }
            if bytes[block * 128..(block + 1) * 128]
                .iter()
                .fold(0u8, |sum, v| sum.wrapping_add(*v))
                != 0
            {
                return Err("DP: invalid EDID checksum");
            }
        }
        if bytes[..8] != [0, 255, 255, 255, 255, 255, 255, 0] {
            return Err("DP: invalid EDID header");
        }
        let mut modes = Vec::new();
        for descriptor in bytes[54..126].chunks_exact(18) {
            if let Ok(mode) = DisplayTiming::from_edid_descriptor(descriptor) {
                modes.push(mode);
            }
        }
        if bytes[126] != 0 && bytes[128] == 2 {
            let end = usize::from(bytes[130]);
            if (4..=127).contains(&end) {
                for descriptor in bytes[128 + end..255].chunks_exact(18) {
                    if let Ok(mode) = DisplayTiming::from_edid_descriptor(descriptor) {
                        modes.push(mode);
                    }
                }
                let mut offset = 132;
                while offset < 128 + end {
                    let header = bytes[offset];
                    let size = usize::from(header & 31);
                    offset += 1;
                    if offset + size > 128 + end {
                        return Err("DP: invalid CEA data block");
                    }
                    if header >> 5 == 2 {
                        for &vic in &bytes[offset..offset + size] {
                            let mode = match vic & 127 {
                                16 => Some(DisplayTiming {
                                    pixel_clock_khz: 148500,
                                    hactive: 1920,
                                    hblank: 280,
                                    hsync_offset: 88,
                                    hsync_width: 44,
                                    vactive: 1080,
                                    vblank: 45,
                                    vsync_offset: 4,
                                    vsync_width: 5,
                                    hsync_positive: true,
                                    vsync_positive: true,
                                }),
                                4 => Some(DisplayTiming {
                                    pixel_clock_khz: 74250,
                                    hactive: 1280,
                                    hblank: 370,
                                    hsync_offset: 110,
                                    hsync_width: 40,
                                    vactive: 720,
                                    vblank: 30,
                                    vsync_offset: 5,
                                    vsync_width: 5,
                                    hsync_positive: true,
                                    vsync_positive: true,
                                }),
                                _ => None,
                            };
                            if let Some(mode) = mode {
                                modes.push(mode);
                            }
                        }
                    }
                    offset += size;
                }
            }
        }
        Ok(modes)
    }
    fn lane_status(status: &[u8; 6], lane: usize) -> u8 {
        (status[lane / 2] >> ((lane % 2) * 4)) & 15
    }
    fn train(
        &self,
        rate: u32,
        lanes: u8,
        enhanced: bool,
        tps3: bool,
        interval: u8,
        pixel_clock_khz: u32,
    ) -> Result<(), &'static str> {
        let mut config = DisplayPortConfig {
            link_rate_mbps: rate,
            lanes,
            voltage: [0; 4],
            pre_emphasis: [0; 4],
        };
        self.clocks.disable_dp_link();
        self.link_powered.store(false, Ordering::Release);
        self.phy
            .configure_displayport(config)
            .map_err(|_| "DP: PHY link configuration failed")?;
        self.clocks.enable_dp_link(rate, pixel_clock_khz)?;
        self.link_powered.store(true, Ordering::Release);
        self.reset_controller();
        self.reset_aux();
        self.registers.write(LINK, 0);
        self.registers.write(LINK, 2);
        self.registers.write(LINK, 0);
        self.registers.write(LINK, 1 | (1 << 25));
        self.registers.write(LINK + 0x38, 0xe4);
        self.registers.write(
            LINK + 8,
            (u32::from(lanes - 1) << 4) | (1 << 8) | 0x4007 | if enhanced { 1 << 6 } else { 0 },
        );
        self.write_dpcd(
            0x100,
            &[(rate / 270) as u8, lanes | if enhanced { 0x80 } else { 0 }],
        )?;
        self.write_dpcd(0x107, &[0, 1])?;
        let wait = if interval & 127 == 0 {
            400
        } else {
            u64::from(interval & 127).min(4) * 4000
        };
        for pattern in [1u8, if tps3 { 3 } else { 2 }] {
            self.registers.write(LINK + 4, 0);
            let source_pattern = 1 << u32::from(pattern - 1);
            self.registers.write(LINK + 4, source_pattern);
            let started = time::current_time();
            while self.registers.read(LINK + 0x40) & (source_pattern << 3) == 0 {
                if time::current_time().saturating_sub(started) > 50_000 {
                    println!(
                        "[sc7180-dp] source pattern={} ready={:#x} state={:#x} mainlink={:#x}",
                        pattern,
                        self.registers.read(LINK + 0x40),
                        self.registers.read(LINK + 4),
                        self.registers.read(LINK)
                    );
                    return Err("DP: source training pattern is not ready");
                }
                time::udelay(100);
            }
            self.write_dpcd(0x102, &[pattern | 0x20])?;
            let mut passed = false;
            let mut last_status = [0; 6];
            for _ in 0..8 {
                let values: Vec<u8> = (0..usize::from(lanes))
                    .map(|i| {
                        config.voltage[i]
                            | (config.pre_emphasis[i] << 3)
                            | if config.voltage[i] == 3 { 4 } else { 0 }
                            | if config.pre_emphasis[i] + config.voltage[i] == 3 {
                                0x20
                            } else {
                                0
                            }
                    })
                    .collect();
                self.phy
                    .configure_displayport(config)
                    .map_err(|_| "DP: PHY electrical training failed")?;
                self.write_dpcd(0x103, &values)?;
                time::udelay(wait);
                let mut status = [0; 6];
                self.read_dpcd(0x202, &mut status)?;
                last_status = status;
                if (0..usize::from(lanes)).all(|i| {
                    Self::lane_status(&status, i) & if pattern == 1 { 1 } else { 7 }
                        == if pattern == 1 { 1 } else { 7 }
                }) && (pattern == 1 || status[2] & 1 != 0)
                {
                    passed = true;
                    break;
                }
                if pattern != 1
                    && (0..usize::from(lanes)).any(|i| Self::lane_status(&status, i) & 1 == 0)
                {
                    break;
                }
                for i in 0..usize::from(lanes) {
                    let request = (status[4 + i / 2] >> ((i % 2) * 4)) & 15;
                    config.voltage[i] = request & 3;
                    config.pre_emphasis[i] = (request >> 2).min(3 - config.voltage[i]);
                }
            }
            if !passed {
                println!(
                    "[sc7180-dp] training pattern={} status={:02x?} voltage={:?} emphasis={:?}",
                    pattern, last_status, config.voltage, config.pre_emphasis
                );
                let _ = self.write_dpcd(0x102, &[0]);
                return Err("DP: link training did not converge");
            }
        }
        self.write_dpcd(0x102, &[0])?;
        self.registers.write(LINK + 4, 0);
        Ok(())
    }
    fn enable_aux(&self, port: CrosEcTypecState) -> Result<(), &'static str> {
        self.clocks.enable_dp_aux()?;
        self.aux_powered.store(true, Ordering::Release);
        // PHY reset precedes PLL setup; the controller FSM is reset later,
        // after its link clocks have started.
        self.registers.write(0x14, 5);
        time::udelay(1000);
        self.registers.write(0x14, 0);
        self.phy
            .set_mode(PhyMode::DisplayPort)
            .map_err(|_| "DP: invalid PHY mode")?;
        self.phy
            .set_orientation(if port.reverse {
                PhyOrientation::Reverse
            } else {
                PhyOrientation::Normal
            })
            .map_err(|_| "DP: PHY orientation is busy")?;
        self.phy
            .configure_displayport(DisplayPortConfig {
                link_rate_mbps: 1620,
                lanes: 2,
                voltage: [0; 4],
                pre_emphasis: [0; 4],
            })
            .map_err(|_| "DP: initial PHY configuration failed")?;
        self.phy.power_on().map_err(|_| "DP: PHY power-on failed")?;
        self.registers.write(0x20, 0);
        self.registers.write(0x24, 0);
        self.registers.update(AUX + 0x18, 0, 1 << 16);
        self.registers.write(AUX + 0x1c, 0x03e800fa);
        self.registers.write(AUX + 0x20, 0x01f407d0);
        self.registers.write(AUX, 1);
        self.registers.write(AUX + 0x0c, 0);
        self.registers.write(AUX + 0x30, 2);
        time::udelay(1000);
        self.registers.write(AUX + 0x30, 1);
        self.registers.write(AUX + 0x3c, 0xffff);
        self.registers.write(AUX + 0x40, 0xffff);
        Ok(())
    }
    fn connect(&self, port: CrosEcTypecState) -> Result<(), &'static str> {
        self.enable_aux(port)?;
        let start = time::current_time();
        while self.registers.read(AUX + 4) & 0x40000000 == 0 {
            if time::current_time().saturating_sub(start) > 2_000_000 {
                println!(
                    "[sc7180-dp] HPD EC={} controller={:#x}",
                    port.hpd_high,
                    self.registers.read(AUX + 4)
                );
                return Err("DP: waiting for monitor HPD");
            }
            if let Some(task) = scarlet::task::mytask() {
                WORKER_WAKER.wait_with_timeout(
                    task.get_id(),
                    task.get_trapframe(),
                    Some(20_000_000),
                );
            } else {
                time::udelay(1000);
            }
        }
        let mut caps = [0; 16];
        self.read_dpcd(0, &mut caps)?;
        if caps[0] < 0x10 || caps[1] < 6 || caps[2] & 31 == 0 {
            return Err("DP: invalid sink capabilities");
        }
        if caps[0] >= 0x11 {
            let mut power = [0];
            self.read_dpcd(0x600, &mut power)?;
            self.write_dpcd(0x600, &[(power[0] & !3) | 1])?;
            time::udelay(1000);
        }
        let modes = self.edid()?;
        let lanes = (caps[2] & 31).min(2);
        let max_rate = u32::from(caps[1]) * 270;
        let retained = self.display();
        let timing = if let Some(display) = &retained {
            modes
                .iter()
                .copied()
                .find(|t| {
                    t.hactive == display.timing.hactive
                        && t.vactive == display.timing.vactive
                        && t.pixel_clock_khz == display.timing.pixel_clock_khz
                })
                .ok_or("DP: replacement monitor does not support the active scanout mode")?
        } else {
            modes
                .iter()
                .copied()
                .find(|t| {
                    u64::from(t.pixel_clock_khz) * 24
                        <= u64::from(max_rate.min(5400)) * u64::from(lanes) * 800
                        && t.hactive <= 2560
                        && t.vactive <= 2160
                })
                .ok_or("DP: no supported progressive EDID mode within link bandwidth")?
        };
        let mut chosen = None;
        for rate in [5400, 2700, 1620] {
            if rate > max_rate
                || u64::from(timing.pixel_clock_khz) * 24 > u64::from(rate) * u64::from(lanes) * 800
            {
                continue;
            }
            match self.train(
                rate,
                lanes,
                caps[2] & 0x80 != 0,
                caps[2] & 0x40 != 0,
                caps[14],
                timing.pixel_clock_khz,
            ) {
                Ok(()) => {
                    chosen = Some(rate);
                    break;
                }
                Err(error) => println!("[sc7180-dp] rate={} lanes={} {}", rate, lanes, error),
            }
        }
        let rate = chosen.ok_or("DP: no link rate trained successfully")?;
        let display = match retained {
            Some(display) => display,
            None => self.make_display(timing)?,
        };
        self.configure_stream(timing, rate, lanes)?;
        {
            let _present = display.present_lock.lock();
            display.engine.dpu.configure(
                timing,
                dpu_dma_address(&display.scanout_dma[display.front.load(Ordering::Acquire)])?,
            )?;
            display.engine.dpu.start();
            display.gpu_direct.lock().mark_staged();
            display.gpu_staging.lock().reset();
            display.active.store(true, Ordering::Release);
        }
        self.registers.write(LINK + 4, 0x80);
        let start = time::current_time();
        while self.registers.read(LINK + 0x40) & 1 == 0 {
            if time::current_time().saturating_sub(start) > 50_000 {
                display.active.store(false, Ordering::Release);
                display.engine.dpu.stop();
                time::udelay(20_000);
                return Err("DP: video stream not ready");
            }
            time::udelay(100);
        }
        println!(
            "[sc7180-dp] port={} reverse={} sink={:02x} link={}x{} mode={}x{} clock={}kHz",
            port.port,
            port.reverse,
            caps[0],
            rate,
            lanes,
            timing.hactive,
            timing.vactive,
            timing.pixel_clock_khz
        );
        time::udelay(20_000);
        let snapshot = display.engine.dpu.diagnostic_snapshot();
        let (frames, line) = display.engine.dpu.frame_counts();
        let mut link_status = [0; 6];
        let status_result = self.read_dpcd(0x202, &mut link_status);
        println!(
            "[sc7180-dp] stream ready={:#x} DPU frames={} line={} flush={:#x} timing={} intf={:#x} mux={:#x} source={:#x} stride={} status={:02x?} aux={:?}",
            self.registers.read(LINK + 0x40),
            frames,
            line,
            snapshot.control_flush,
            snapshot.timing_enable,
            snapshot.interface_config,
            snapshot.interface_mux,
            snapshot.source_address,
            snapshot.source_stride,
            link_status,
            status_result
        );
        *self.display.lock() = Some(display);
        self.connected.store(true, Ordering::Release);
        Ok(())
    }
    fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        // A failed AUX-only probe has no running pixel/link clock. Touching
        // the retained DPU interface in that state can stall the MMIO bus.
        if self.link_powered.swap(false, Ordering::AcqRel) {
            if let Some(display) = self.display() {
                let _present = display.present_lock.lock();
                if display.active.swap(false, Ordering::AcqRel) {
                    {
                        let _interrupt = MDSS_INTERRUPT_LOCK.lock();
                        display.engine.dpu.mask_vsync_interrupt();
                        display.engine.dpu.clear_vsync_interrupt();
                        display.engine.dpu.stop();
                    }
                    time::udelay(20_000);
                }
            }
            self.registers.write(LINK + 4, 0);
            self.registers.update(LINK, 1, 0);
            self.reset_controller();
        }
        if self.aux_powered.swap(false, Ordering::AcqRel) {
            self.registers.write(AUX + 0x30, 0);
            self.clocks.disable_dp();
        }
        self.phy.power_off();
    }
    fn make_display(
        &self,
        timing: DisplayTiming,
    ) -> Result<Arc<Sc7180GraphicsDevice>, &'static str> {
        let config = FramebufferConfig {
            width: u32::from(timing.hactive),
            height: u32::from(timing.vactive),
            format: PixelFormat::BGRA8888,
            stride: u32::from(timing.hactive) * 4,
        };
        let scanout = [allocate_scanout(&config)?, allocate_scanout(&config)?];
        let dma_context = self.parent.dma_context.clone();
        let scanout_dma = [
            map_scanout(&dma_context, &scanout[0])?,
            map_scanout(&dma_context, &scanout[1])?,
        ];
        Ok(Arc::new(Sc7180GraphicsDevice {
            config,
            timing,
            ec: self.parent.ec.clone(),
            scanout,
            scanout_dma,
            dma_context,
            gpu_direct: Mutex::new(GpuDirectScanoutState::new()),
            gpu_staging: Mutex::new(GpuStagingState::new()),
            gpu_present_count: AtomicUsize::new(0),
            front: AtomicUsize::new(0),
            present_lock: Mutex::new(()),
            interrupt_id: self.parent.interrupt_id,
            present_pending: AtomicBool::new(false),
            vblank_count: AtomicUsize::new(0),
            vblank_waker: Waker::new_uninterruptible("sc7180-dp-vblank"),
            engine: Sc7180DisplayEngine {
                dpu: self.parent.engine.dpu.external(),
                interrupt_lock: &MDSS_INTERRUPT_LOCK,
            },
            external: true,
            active: AtomicBool::new(false),
        }))
    }
    fn configure_stream(&self, t: DisplayTiming, rate: u32, lanes: u8) -> Result<(), &'static str> {
        let r = self.registers;
        r.write(
            LINK + 0x1c,
            t.horizontal_total() | (t.vertical_total() << 16),
        );
        r.write(
            LINK + 0x20,
            u32::from(t.hblank - t.hsync_offset) | (u32::from(t.vblank - t.vsync_offset) << 16),
        );
        r.write(
            LINK + 0x24,
            u32::from(t.hsync_width)
                | (u32::from(t.vsync_width) << 16)
                | (u32::from(!t.hsync_positive) << 15)
                | (u32::from(!t.vsync_positive) << 31),
        );
        r.write(
            LINK + 0x28,
            u32::from(t.hactive) | (u32::from(t.vactive) << 16),
        );
        r.write(LINK + 0x2c, (1 << 5) | 1);
        let link_khz = rate * 100;
        let n = 32768u64;
        let m = (u64::from(t.pixel_clock_khz) * n + u64::from(link_khz) / 2) / u64::from(link_khz);
        r.write(LINK + 0x10, m as u32);
        r.write(LINK + 0x18, n as u32);
        r.write(0x1000 + 0x7c, 0);
        r.update(0x1000 + 0x14, 1 << 4, 0);
        let (tu, valid, valid2) = transfer_unit(t, link_khz, lanes)?;
        r.write(LINK + 0x30, valid);
        r.write(LINK + 0x34, valid2);
        r.write(LINK + 0x4c, tu);
        Ok(())
    }
}

fn transfer_unit(
    t: DisplayTiming,
    link_khz: u32,
    lanes: u8,
) -> Result<(u32, u32, u32), &'static str> {
    // Enumerate the hardware's 32..64 symbol TU sizes and 4-bit boundary
    // moderation counts, using exact rational arithmetic rather than floats.
    let p = u128::from(t.pixel_clock_khz);
    let l = u128::from(link_khz);
    let lanes = u128::from(lanes);
    let den = l * lanes;
    let num = p * 3;
    if num >= den || t.hblank <= 4 {
        return Err("DP: pixel bandwidth exceeds link");
    }
    let mut best: Option<(u128, u128, u128, u32, u32, u32)> = None;
    for tu in 32u128..=64 {
        let upper = (tu * num).div_ceil(den);
        for hi in 1u128..=15 {
            for lo in 0u128..=15 {
                let count = hi + lo;
                let average = upper * count - lo;
                let target = tu * num * count;
                if average * den < target {
                    continue;
                }
                let error = average * den - target;
                let tus = (u128::from(t.hactive) * 3 * count).div_ceil(average);
                let extra = (error * tus).div_ceil(den * count)
                    + (hi * lanes * (upper * den - tu * num)).div_ceil(den);
                let delay =
                    (extra.div_ceil(3) * l).div_ceil(p) + (tu - upper) + 1 + (4 * l).div_ceil(p);
                if delay > 1023 || delay * p >= u128::from(t.hblank - 4) * l {
                    continue;
                }
                let score_den = den * count * tu;
                if best.as_ref().is_none_or(|&(e, d, old_delay, _, _, _)| {
                    error * d < e * score_den || (error * d == e * score_den && delay < old_delay)
                }) {
                    best = Some((
                        error,
                        score_den,
                        delay,
                        (tu - 1) as u32,
                        upper as u32 | ((delay as u32) << 16),
                        if lo == 0 {
                            0
                        } else {
                            1 | (((upper - 1) as u32) << 1)
                                | ((hi as u32) << 16)
                                | ((lo as u32) << 20)
                        },
                    ));
                }
            }
        }
    }
    best.map(|(_, _, _, tu, a, b)| (tu, a, b))
        .ok_or("DP: no valid transfer-unit schedule")
}

fn worker() {
    let Some(dp) = controller() else {
        return;
    };
    let mut previous = None;
    let mut retry_at = 0;
    loop {
        let mut active = None;
        let mut complete = true;
        match dp.parent.ec.typec_port_count() {
            Ok(ports) => {
                for port in 0..ports {
                    match dp.parent.ec.typec_state(port) {
                        Ok(state) => {
                            if state.displayport_enabled && !state.safe_mode {
                                active = Some(state);
                            }
                        }
                        Err(_) => complete = false,
                    }
                }
            }
            Err(_) => complete = false,
        }
        if complete {
            let identity = active.map(|state| (state.port, state.reverse));
            if identity != previous {
                println!("[sc7180-dp] Type-C {:?}", active);
                if previous.is_some() || dp.connected() {
                    dp.disconnect();
                }
                previous = identity;
                retry_at = 0;
            }
            if dp.connected() && dp.registers.read(AUX + 4) & 0xe0000000 != 0x40000000 {
                println!("[sc7180-dp] monitor HPD disconnected");
                dp.disconnect();
                retry_at = 0;
            }
            if let Some(port) = active {
                if !dp.connected() && time::current_time_ns() >= retry_at {
                    if let Err(error) = dp.connect(port) {
                        println!("[sc7180-dp] {}", error);
                        dp.disconnect();
                        retry_at = time::current_time_ns() + 5_000_000_000;
                    }
                }
            }
        }
        let Some(task) = scarlet::task::mytask() else {
            scarlet::arch::instruction::idle();
        };
        WORKER_WAKER.wait_with_timeout(task.get_id(), task.get_trapframe(), Some(500_000_000));
    }
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if controller().is_some() {
        return Ok(());
    }
    let Some(parent) = PRIMARY.lock().clone() else {
        return probe_defer();
    };
    let phy = match DeviceManager::get_manager().resolve_phy(device, "dp") {
        Ok(phy) => phy,
        Err(_) => return probe_defer(),
    };
    let phandle = compatible_phandle("qcom,sc7180-dispcc").ok_or("DP: no DISP_CC")?;
    let Some(clocks) = scarlet_driver_qcom_sc7180_dispcc::get_sc7180_dispcc_by_phandle(phandle)
    else {
        return probe_defer();
    };
    let memory = device
        .get_resources()
        .iter()
        .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("DP: missing MMIO")?;
    let base = vm::ioremap(memory.start, 0x1800).map_err(|_| "DP: MMIO mapping failed")?;
    *CONTROLLER.lock() = Some(Arc::new(DpController {
        registers: RegisterWindow::new(base),
        parent,
        phy,
        clocks,
        connected: AtomicBool::new(false),
        aux_powered: AtomicBool::new(false),
        link_powered: AtomicBool::new(false),
        display: IrqSpinLock::new(None),
    }));
    let task = scarlet::task::new_kernel_task("sc7180-dp-hotplug".into(), 1, worker);
    task.init();
    scarlet::sched::scheduler::add_task(task, arch::get_cpu().get_cpuid());
    Ok(())
}
pub(super) fn register() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-dp",
            probe,
            |_| Ok(()),
            vec!["qcom,sc7180-dp"],
        )),
        DriverPriority::Late,
    );
}
