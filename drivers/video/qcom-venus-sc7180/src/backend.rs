// SPDX-License-Identifier: GPL-2.0-only

//! Stateful H.264 decode backend for the SC7180 Venus 5.4 firmware.

use alloc::{
    collections::VecDeque,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::{mem, ptr};

use crate::{
    EnabledVideoClocks, firmware,
    hfi::{HfiTransport, SHARED_REGION_SIZE},
    hfi_abi::{self, BufferRequirement, FillDone, SequenceInfo},
    memory::{DmaPagedAllocation, rw_flags},
    registers::VenusRegisters,
};
use qcom_sc7180_interconnect::VenusInterconnectPaths;
use scarlet::{
    arch,
    device::{
        iommu::{DmaContext, DmaMapping},
        video::{
            SCARLET_VIDEO_FORMAT_H264, SCARLET_VIDEO_FRAME_HEADER_LEN, SCARLET_VIDEO_FRAME_MAGIC,
            SCARLET_VIDEO_PIXEL_FORMAT_NV12, ScarletVideoDequeuedFrame, VideoBackendCapabilities,
            VideoBackendDecodeRequest, VideoBackendDecodedFrame, VideoCompletionNotifier,
            VideoDecodeBackend,
        },
    },
    interrupt::{
        InterruptClaim, InterruptId, InterruptResult, InterruptSource, MaskableInterruptSource,
    },
    println,
    sync::{IrqGuard, IrqSpinLock, Mutex, Waker},
    time,
};

const MAPPED_INPUT_BYTES: usize = 8 * 1024 * 1024;
// SC7180 tops out at 4096x2160 decode. Its padded linear NV12 output occupies
// about 12.8 MiB, so 16 MiB covers the complete supported frame plus the
// in-place compacted payload without retaining a wasteful 64 MiB aperture.
const MAPPED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const HARDWARE_OUTPUT_OFFSET: usize = 4096;
const INITIAL_WIDTH: u32 = 96;
const INITIAL_HEIGHT: u32 = 96;
const MAX_DYNAMIC_BUFFERS: u32 = 32;
const MAX_INTERNAL_BUFFERS_PER_TYPE: u32 = 8;
// Venus HFI supports independent firmware sessions over one shared command
// transport. Match the upstream Venus fallback limit.
const MAX_CONCURRENT_SESSIONS: usize = 16;
// Keep one access unit in flight per session, while allowing the firmware to
// schedule several sessions concurrently.  Four matches the practical
// full-HD limit of the current CoachZ memory/display pipeline without
// advertising the old, globally serialized one-frame path.
const MAX_INFLIGHT_DECODES: usize = 4;
const HFI_RESPONSE_TIMEOUT_US: u64 = 1_000_000;
const HFI_DECODE_TIMEOUT_US: u64 = 2_000_000;
// Match Linux's internal-DPB tag range: application-visible capture tags live
// below VB2_MAX_FRAME and firmware-owned DPBs start at that boundary.
const DPB_TAG_BASE: u32 = 32;
const HFI_BUFFERFLAG_READONLY: u32 = 0x200;

static VENUS_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static VENUS_WORKER_PENDING: AtomicBool = AtomicBool::new(false);
static VENUS_WORKER_WAKER: Waker = Waker::new_uninterruptible("venus-decode-worker");
static VENUS_HFI_WAKER: Waker = Waker::new_uninterruptible("venus-hfi-response");
static VENUS_WORKER_BACKENDS: IrqSpinLock<Vec<Weak<VenusBackend>>> = IrqSpinLock::new(Vec::new());

fn is_session_resource_error(error: &'static str) -> bool {
    matches!(
        error,
        "qcom-venus-sc7180: paged DMA allocation failed"
            | "qcom-venus-sc7180: paged DMA length overflows"
            | "qcom-venus-sc7180: paged DMA mapping failed"
            | "qcom-venus-sc7180: firmware requires a 32-bit DMA address"
            | "qcom-venus-sc7180: invalid frontend input-buffer size"
            | "qcom-venus-sc7180: invalid frontend output-buffer size"
            | "qcom-venus-sc7180: frontend buffer range overflows"
            | "qcom-venus-sc7180: frontend buffers are not contiguous"
            | "qcom-venus-sc7180: failed to map frontend input"
            | "qcom-venus-sc7180: failed to map frontend output"
            | "qcom-venus-sc7180: frontend DMA address exceeds HFI 32-bit range"
            | "qcom-venus-sc7180: frontend DMA buffers are not page aligned"
            | "qcom-venus-sc7180: invalid H.264 access-unit size"
            | "qcom-venus-sc7180: decoded frame exceeds frontend output buffer"
    )
}

fn log_sequence_packet(words: &[u32]) {
    println!(
        "[qcom-venus-sc7180] sequence event parse failed packet_words={}",
        words.len(),
    );
    for (chunk_index, chunk) in words.chunks(8).enumerate() {
        println!(
            "[qcom-venus-sc7180] sequence event word={} data={:x?}",
            chunk_index * 8,
            chunk,
        );
    }
}

/// Reserved firmware RAM and the dedicated firmware context-bank mapping.
pub(crate) struct FirmwareRegion {
    pub(crate) paddr: usize,
    pub(crate) vaddr: usize,
    pub(crate) size: usize,
    pub(crate) dma: DmaContext,
    /// Own the fixed IOVA-zero mapping for the firmware lifetime.
    pub(crate) _mapping: DmaMapping,
}

struct HfiCore {
    transport: HfiTransport,
    deferred: VecDeque<Vec<u32>>,
}

impl HfiCore {
    fn boot(
        registers: &VenusRegisters,
        dma: &DmaContext,
        firmware_region: &FirmwareRegion,
    ) -> Result<Self, &'static str> {
        registers.assert_arm9_reset();
        let image_span = firmware::load_into_reserved_region(
            firmware_region.vaddr,
            firmware_region.paddr,
            firmware_region.size,
        )?;
        arch::clean_dcache_to_poc_range(firmware_region.vaddr, firmware_region.size);
        firmware_region
            .dma
            .restore_iommu()
            .map_err(|_| "qcom-venus-sc7180: firmware IOMMU flush failed")?;

        let transport = HfiTransport::new(dma)?;
        registers.release_arm9(firmware_region.size)?;
        let control = match registers.initialize_hfi(
            transport.queue_dma(),
            SHARED_REGION_SIZE as u32,
            transport.sfr_dma()?,
        ) {
            Ok(control) => control,
            Err(error) => {
                if let Some(fault) = firmware_region.dma.primary_iommu_fault_snapshot() {
                    println!(
                        "[qcom-venus-sc7180] firmware-iommu-fault global={:#010x} context={:#010x} far={:#018x} syn={:#010x}/{:#010x}",
                        fault.global_status,
                        fault.context_status,
                        fault.fault_address,
                        fault.syndrome0,
                        fault.syndrome1,
                    );
                }
                return Err(error);
            }
        };
        registers.clear_pending_interrupts();
        registers.unmask_interrupts();

        let mut core = Self {
            transport,
            deferred: VecDeque::new(),
        };
        core.send(registers, &hfi_abi::sys_init(), true)?;
        let response = core.wait_matching(registers, HFI_RESPONSE_TIMEOUT_US, |packet| {
            hfi_abi::packet_type(packet) == Some(hfi_abi::HFI_MSG_SYS_INIT)
        })?;
        ensure_response_success(&response)?;
        core.send(registers, &hfi_abi::sys_debug_errors_only(), false)?;
        // Scarlet does not yet have Venus runtime PM. Keep firmware-managed
        // collapse disabled so clocks and context banks remain valid.
        core.send(registers, &hfi_abi::sys_disable_power_collapse(), false)?;
        println!(
            "[qcom-venus-sc7180] firmware ready image={} bytes control={:#x} hw={:#x}",
            image_span,
            control,
            registers.hardware_version()
        );
        Ok(core)
    }

    fn send(
        &mut self,
        registers: &VenusRegisters,
        packet: &[u32],
        synchronous: bool,
    ) -> Result<(), &'static str> {
        self.transport.send(registers, packet, synchronous)
    }

    fn command_response(
        &mut self,
        registers: &VenusRegisters,
        packet: &[u32],
        response_type: u32,
        session: u32,
    ) -> Result<Vec<u32>, &'static str> {
        self.command_response_with_wait(registers, packet, response_type, session, true)
    }

    fn command_response_polling(
        &mut self,
        registers: &VenusRegisters,
        packet: &[u32],
        response_type: u32,
        session: u32,
    ) -> Result<Vec<u32>, &'static str> {
        self.command_response_with_wait(registers, packet, response_type, session, false)
    }

    fn command_response_with_wait(
        &mut self,
        registers: &VenusRegisters,
        packet: &[u32],
        response_type: u32,
        session: u32,
        allow_sleep: bool,
    ) -> Result<Vec<u32>, &'static str> {
        self.send(registers, packet, true)?;
        let response = self.wait_matching_with_wait(
            registers,
            HFI_RESPONSE_TIMEOUT_US,
            |candidate| {
                hfi_abi::packet_type(candidate) == Some(response_type)
                    && hfi_abi::session_id(candidate) == Some(session)
            },
            allow_sleep,
        )?;
        ensure_response_success(&response)?;
        Ok(response)
    }

    fn buffer_requirements(
        &mut self,
        registers: &VenusRegisters,
        session: u32,
    ) -> Result<Vec<BufferRequirement>, &'static str> {
        self.send(registers, &hfi_abi::get_buffer_requirements(session), true)?;
        let response = self.wait_matching(registers, HFI_RESPONSE_TIMEOUT_US, |candidate| {
            hfi_abi::packet_type(candidate) == Some(hfi_abi::HFI_MSG_SESSION_PROPERTY_INFO)
                && hfi_abi::session_id(candidate) == Some(session)
        })?;
        hfi_abi::parse_buffer_requirements(&response)
    }

    fn wait_matching<F>(
        &mut self,
        registers: &VenusRegisters,
        timeout_us: u64,
        matches: F,
    ) -> Result<Vec<u32>, &'static str>
    where
        F: Fn(&[u32]) -> bool,
    {
        self.wait_matching_with_wait(registers, timeout_us, matches, true)
    }

    fn wait_matching_with_wait<F>(
        &mut self,
        registers: &VenusRegisters,
        timeout_us: u64,
        matches: F,
        allow_sleep: bool,
    ) -> Result<Vec<u32>, &'static str>
    where
        F: Fn(&[u32]) -> bool,
    {
        let start = time::current_time();
        loop {
            if let Some(index) = self
                .deferred
                .iter()
                .position(|packet| hfi_abi::is_fatal_event(packet, 0))
            {
                let _ = self.deferred.remove(index);
                return Err("qcom-venus-sc7180: firmware reported a fatal HFI event");
            }
            if let Some(index) = self.deferred.iter().position(|packet| matches(packet)) {
                return self
                    .deferred
                    .remove(index)
                    .ok_or("qcom-venus-sc7180: deferred HFI response disappeared");
            }

            match self.transport.read_message(registers)? {
                Some(packet) => {
                    if hfi_abi::is_fatal_event(&packet, 0) {
                        return Err("qcom-venus-sc7180: firmware reported a fatal HFI event");
                    }
                    if matches(&packet) {
                        return Ok(packet);
                    }
                    self.deferred.push_back(packet);
                }
                None => {
                    if registers.interrupt_status() != 0 {
                        let _ = registers.acknowledge_interrupt();
                    }
                    self.transport.drain_debug(registers);
                    if allow_sleep {
                        wait_without_burning_cpu(start);
                    } else {
                        // Video-open destruction may run from the task reaper.
                        // Keep firmware shutdown independent of scheduler
                        // wake progress so teardown remains bounded and cannot
                        // recursively strand the reaper. Linux likewise treats
                        // a missing stop response as an abort condition.
                        core::hint::spin_loop();
                    }
                }
            }
            if time::current_time().saturating_sub(start) >= timeout_us {
                println!(
                    "[qcom-venus-sc7180] HFI response timeout deferred={}",
                    self.deferred.len()
                );
                for packet in self.deferred.iter().take(8) {
                    println!(
                        "[qcom-venus-sc7180] deferred type={:#010x?} session={:?} words={:x?}",
                        hfi_abi::packet_type(packet),
                        hfi_abi::session_id(packet),
                        packet,
                    );
                }
                self.transport.log_diagnostics(registers);
                return Err("qcom-venus-sc7180: timed out waiting for HFI response");
            }
        }
    }

    /// Return the next already-available HFI packet without waiting.
    ///
    /// Command/response helpers retain unrelated session packets in
    /// `deferred`.  The asynchronous decode worker must drain that queue
    /// before the hardware message ring so completions remain ordered while
    /// still being demultiplexed by firmware session id.
    fn try_next_packet(
        &mut self,
        registers: &VenusRegisters,
    ) -> Result<Option<Vec<u32>>, &'static str> {
        if let Some(packet) = self.deferred.pop_front() {
            return Ok(Some(packet));
        }
        self.transport.read_message(registers)
    }
}

fn ensure_response_success(packet: &[u32]) -> Result<(), &'static str> {
    match hfi_abi::response_error(packet) {
        Some(hfi_abi::HFI_ERR_NONE) => Ok(()),
        Some(hfi_abi::HFI_ERR_SESSION_EMPTY_BUFFER_DONE_OUTPUT_PENDING)
            if hfi_abi::packet_type(packet) == Some(hfi_abi::HFI_MSG_SESSION_EMPTY_BUFFER) =>
        {
            Ok(())
        }
        Some(_) => Err("qcom-venus-sc7180: HFI command failed"),
        None => Err("qcom-venus-sc7180: malformed HFI command response"),
    }
}

fn wait_without_burning_cpu(start: u64) {
    if time::current_time().saturating_sub(start) < 50 {
        core::hint::spin_loop();
    } else if let Some(task) = scarlet::task::mytask() {
        // Venus signals every response through its dedicated IRQ. Block on
        // that level transition instead of merely yielding a runnable kernel
        // worker: under sustained userspace load a plain yield can postpone
        // both response processing and teardown indefinitely. The short
        // timeout also covers firmware messages that race ahead of the IRQ or
        // a lost/deasserted interrupt.
        let _ =
            VENUS_HFI_WAKER.wait_with_timeout(task.get_id(), task.get_trapframe(), Some(100_000));
    } else {
        core::hint::spin_loop();
    }
}

struct InternalBuffer {
    buffer_type: u32,
    allocation: DmaPagedAllocation,
}

struct FrontendMappings {
    input_paddr: usize,
    output_paddr: usize,
    input_len: usize,
    output_len: usize,
    input: DmaMapping,
    output: DmaMapping,
}

impl FrontendMappings {
    fn new(dma: &DmaContext, request: &VideoBackendDecodeRequest) -> Result<Self, &'static str> {
        let input_len = usize::try_from(request.output_offset)
            .map_err(|_| "qcom-venus-sc7180: invalid frontend input-buffer size")?;
        let output_len = request.output_len as usize;
        if input_len == 0 || input_len > MAPPED_INPUT_BYTES {
            return Err("qcom-venus-sc7180: invalid frontend input-buffer size");
        }
        if output_len == 0 || output_len > MAPPED_OUTPUT_BYTES {
            return Err("qcom-venus-sc7180: invalid frontend output-buffer size");
        }
        let expected_output_paddr = request
            .input_paddr
            .checked_add(input_len)
            .ok_or("qcom-venus-sc7180: frontend buffer range overflows")?;
        if request.output_paddr != expected_output_paddr {
            return Err("qcom-venus-sc7180: frontend buffers are not contiguous");
        }
        let input = dma
            .map_phys_owned(request.input_paddr, input_len, rw_flags())
            .map_err(|_| "qcom-venus-sc7180: failed to map frontend input")?;
        let output = dma
            .map_phys_owned(request.output_paddr, output_len, rw_flags())
            .map_err(|_| "qcom-venus-sc7180: failed to map frontend output")?;
        if input.dma_addr() > u32::MAX as u64 || output.dma_addr() > u32::MAX as u64 {
            return Err("qcom-venus-sc7180: frontend DMA address exceeds HFI 32-bit range");
        }
        if input.dma_addr() & 0xfff != 0 || output.dma_addr() & 0xfff != 0 {
            return Err("qcom-venus-sc7180: frontend DMA buffers are not page aligned");
        }
        Ok(Self {
            input_paddr: request.input_paddr,
            output_paddr: request.output_paddr,
            input_len,
            output_len,
            input,
            output,
        })
    }

    fn matches(&self, request: &VideoBackendDecodeRequest) -> bool {
        self.input_paddr == request.input_paddr
            && self.output_paddr == request.output_paddr
            && usize::try_from(request.output_offset).ok() == Some(self.input_len)
            && self.output_len == request.output_len as usize
    }

    fn input_capacity(&self) -> u32 {
        self.input_len as u32
    }

    fn input_dma(&self) -> u32 {
        self.input.dma_addr() as u32
    }

    fn output_dma(&self) -> u32 {
        self.output.dma_addr() as u32
    }
}

#[derive(Clone, Copy)]
struct Nv12Layout {
    width: u32,
    height: u32,
    display_x: u32,
    display_y: u32,
    display_width: u32,
    display_height: u32,
    y_stride: u32,
    y_scanlines: u32,
    uv_scanlines: u32,
    linear_size: u32,
    ubwc_size: u32,
}

impl Nv12Layout {
    fn new(sequence: SequenceInfo) -> Result<Self, &'static str> {
        if sequence.width == 0 || sequence.height == 0 {
            return Err("qcom-venus-sc7180: invalid decoded dimensions");
        }
        let y_stride = align_up_u32(sequence.width, 128)?;
        let y_scanlines = align_up_u32(sequence.height, 32)?;
        let uv_scanlines = align_up_u32((sequence.height + 1) / 2, 16)?;
        let linear_size = y_stride
            .checked_mul(y_scanlines)
            .and_then(|y| {
                y_stride
                    .checked_mul(uv_scanlines)
                    .and_then(|uv| y.checked_add(uv))
            })
            .and_then(|size| align_up_u32(size, 4096).ok())
            .ok_or("qcom-venus-sc7180: NV12 layout overflows")?;
        let ubwc_size = nv12_ubwc_size(sequence.width, sequence.height)?;
        let display_width = sequence.crop_width;
        let display_height = sequence.crop_height;
        if display_width == 0
            || display_height == 0
            || display_width & 1 != 0
            || display_height & 1 != 0
            || sequence.crop_left & 1 != 0
            || sequence.crop_top & 1 != 0
            || sequence.crop_left > sequence.width
            || sequence.crop_top > sequence.height
            || display_width > sequence.width - sequence.crop_left
            || display_height > sequence.height - sequence.crop_top
        {
            return Err("qcom-venus-sc7180: unsupported or invalid NV12 crop");
        }
        Ok(Self {
            width: sequence.width,
            height: sequence.height,
            display_x: sequence.crop_left,
            display_y: sequence.crop_top,
            display_width,
            display_height,
            y_stride,
            y_scanlines,
            uv_scanlines,
            linear_size,
            ubwc_size,
        })
    }

    fn initial(width: u32, height: u32) -> Result<Self, &'static str> {
        Self::new(SequenceInfo {
            width,
            height,
            crop_left: 0,
            crop_top: 0,
            crop_width: width,
            crop_height: height,
            minimum_dpb_count: 0,
        })
    }

    fn tight_payload_len(&self) -> Result<usize, &'static str> {
        (self.display_width as usize)
            .checked_mul(self.display_height as usize)
            .and_then(|y| y.checked_add(y / 2))
            .ok_or("qcom-venus-sc7180: tight NV12 payload overflows")
    }
}

struct VenusSession {
    id: u32,
    internal: Vec<InternalBuffer>,
    dpbs: Vec<DmaPagedAllocation>,
    dpb_queued: Vec<bool>,
    layout: Option<Nv12Layout>,
    mappings: Option<FrontendMappings>,
    next_input_tag: u32,
    pending: Option<PendingDecode>,
}

#[derive(Clone, Copy)]
struct PendingDecode {
    request: VideoBackendDecodeRequest,
    output_tag: u32,
    submitted_at: u64,
}

impl VenusSession {
    fn create(
        id: u32,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        dma: &DmaContext,
    ) -> Result<Self, &'static str> {
        core.command_response(
            registers,
            &hfi_abi::session_init(id),
            hfi_abi::HFI_MSG_SYS_SESSION_INIT,
            id,
        )?;
        core.send(
            registers,
            &hfi_abi::set_frame_size(id, hfi_abi::HFI_BUFFER_INPUT, INITIAL_WIDTH, INITIAL_HEIGHT),
            false,
        )?;
        // The Scarlet stateful frontend currently submits one access unit and
        // waits for one frame before submitting the next. Display-order H.264
        // decoding can require a future access unit for B-frame reordering, so
        // use the firmware's supported low-delay decode-order mode. Linux
        // sends this after the initial input resolution has been established.
        core.send(registers, &hfi_abi::set_decode_order(id), false)?;
        let initial = Nv12Layout::initial(INITIAL_WIDTH, INITIAL_HEIGHT)?;
        configure_output(registers, core, id, initial)?;
        let requirements = core.buffer_requirements(registers, id)?;
        let internal = allocate_internal_buffers(registers, core, dma, id, &requirements, false)?;
        core.command_response(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SESSION_LOAD_RESOURCES, id),
            hfi_abi::HFI_MSG_SESSION_LOAD_RESOURCES,
            id,
        )?;
        core.command_response(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SESSION_START, id),
            hfi_abi::HFI_MSG_SESSION_START,
            id,
        )?;
        Ok(Self {
            id,
            internal,
            dpbs: Vec::new(),
            dpb_queued: Vec::new(),
            layout: None,
            mappings: None,
            next_input_tag: 1,
            pending: None,
        })
    }

    fn destroy(
        &mut self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
    ) -> Result<(), &'static str> {
        core.command_response_polling(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SESSION_STOP, self.id),
            hfi_abi::HFI_MSG_SESSION_STOP,
            self.id,
        )?;
        // STOP is the firmware ownership barrier for any input/output buffer
        // that was still in flight when userspace closed the session.
        self.pending = None;
        core.command_response_polling(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SESSION_RELEASE_RESOURCES, self.id),
            hfi_abi::HFI_MSG_SESSION_RELEASE_RESOURCES,
            self.id,
        )?;
        let internal = mem::take(&mut self.internal);
        for buffer in internal {
            core.command_response_polling(
                registers,
                &hfi_abi::release_internal_buffer(
                    self.id,
                    buffer.buffer_type,
                    buffer.allocation.requested_size() as u32,
                    buffer.allocation.dma_addr(),
                ),
                hfi_abi::HFI_MSG_SESSION_RELEASE_BUFFERS,
                self.id,
            )?;
        }
        core.command_response_polling(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SYS_SESSION_END, self.id),
            hfi_abi::HFI_MSG_SYS_SESSION_END,
            self.id,
        )?;
        self.dpbs.clear();
        self.dpb_queued.clear();
        self.mappings = None;
        Ok(())
    }

    fn begin_submit(
        &mut self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        dma: &DmaContext,
        request: &VideoBackendDecodeRequest,
    ) -> Result<(), &'static str> {
        if request.stream_id != self.id || request.coded_format != SCARLET_VIDEO_FORMAT_H264 {
            return Err("qcom-venus-sc7180: invalid decode session or format");
        }
        if self.pending.is_some() {
            return Err("qcom-venus-sc7180: session decode is already in flight");
        }
        let input_capacity = usize::try_from(request.output_offset)
            .map_err(|_| "qcom-venus-sc7180: invalid frontend input-buffer size")?;
        if request.input_len == 0
            || request.input_len as usize > input_capacity
            || input_capacity > MAPPED_INPUT_BYTES
        {
            return Err("qcom-venus-sc7180: invalid H.264 access-unit size");
        }
        if request.output_len == 0 || request.output_len as usize > MAPPED_OUTPUT_BYTES {
            return Err("qcom-venus-sc7180: invalid frontend output-buffer size");
        }
        if self
            .mappings
            .as_ref()
            .is_none_or(|mappings| !mappings.matches(request))
        {
            self.mappings = Some(FrontendMappings::new(dma, request)?);
        }
        let mappings = self
            .mappings
            .as_ref()
            .ok_or("qcom-venus-sc7180: frontend mappings are unavailable")?;
        let input_dma = mappings.input_dma();
        let input_capacity = mappings.input_capacity();
        arch::clean_dcache_to_poc_range(request.input_vaddr, request.input_len as usize);

        let input_tag = self.next_input_tag;
        self.next_input_tag = self.next_input_tag.wrapping_add(1).max(1);
        let output_tag = input_tag;
        if let Some(layout) = self.layout {
            self.queue_output(registers, core, mappings, layout, output_tag, request)?;
        }
        // Publish the request before the input command makes it executable.
        // A queued output buffer cannot complete without an input access unit,
        // while the input doorbell may produce an IRQ immediately.
        self.pending = Some(PendingDecode {
            request: *request,
            output_tag,
            submitted_at: time::current_time(),
        });
        if let Err(error) = core.send(
            registers,
            &hfi_abi::empty_buffer(
                self.id,
                request.timestamp,
                input_tag,
                input_dma,
                input_capacity,
                request.input_len,
            ),
            false,
        ) {
            self.pending = None;
            return Err(error);
        }
        Ok(())
    }

    fn reconfigure(
        &mut self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        dma: &DmaContext,
        sequence: SequenceInfo,
    ) -> Result<(), &'static str> {
        if self.layout.is_some() {
            return Err("qcom-venus-sc7180: mid-stream resolution change requires a new session");
        }
        let layout = Nv12Layout::new(sequence)?;
        configure_output(registers, core, self.id, layout)?;
        let requirements = core.buffer_requirements(registers, self.id)?;

        let old_internal = mem::take(&mut self.internal);
        let mut retained = Vec::new();
        for buffer in old_internal {
            if matches!(
                buffer.buffer_type,
                hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_1
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_2
            ) {
                core.command_response(
                    registers,
                    &hfi_abi::release_internal_buffer(
                        self.id,
                        buffer.buffer_type,
                        buffer.allocation.requested_size() as u32,
                        buffer.allocation.dma_addr(),
                    ),
                    hfi_abi::HFI_MSG_SESSION_RELEASE_BUFFERS,
                    self.id,
                )?;
            } else {
                retained.push(buffer);
            }
        }
        // Keep every still-registered persistent buffer owned by the session
        // before attempting the optional scratch replacement. If physical
        // memory is tight, allocation may fail here; dropping `retained`
        // would otherwise leave firmware pointing at freed IOVAs and force a
        // global core quarantine that also kills unrelated streams.
        self.internal = retained;
        let mut scratch =
            allocate_internal_buffers(registers, core, dma, self.id, &requirements, true)?;
        self.internal.append(&mut scratch);

        let output_requirement = requirements
            .iter()
            .find(|requirement| requirement.buffer_type == hfi_abi::HFI_BUFFER_OUTPUT);
        let required_dpb_count = output_requirement
            .map(|requirement| requirement.count_min)
            .unwrap_or(sequence.minimum_dpb_count)
            .max(sequence.minimum_dpb_count)
            .max(1);
        if required_dpb_count > MAX_DYNAMIC_BUFFERS {
            return Err("qcom-venus-sc7180: firmware requested too many DPB buffers");
        }
        let dpb_count = required_dpb_count;
        let dpb_size = output_requirement
            .map(|requirement| requirement.size)
            .unwrap_or(0)
            .max(layout.ubwc_size);
        self.dpbs = Vec::with_capacity(dpb_count as usize);
        self.dpb_queued = Vec::with_capacity(dpb_count as usize);
        for _ in 0..dpb_count {
            self.dpbs
                .push(DmaPagedAllocation::new(dma, dpb_size as usize, rw_flags())?);
            self.dpb_queued.push(false);
        }

        core.send(
            registers,
            &hfi_abi::session_command(hfi_abi::HFI_CMD_SESSION_CONTINUE, self.id),
            false,
        )?;
        for index in 0..self.dpbs.len() {
            self.queue_dpb(registers, core, index)?;
        }
        self.layout = Some(layout);
        println!(
            "[qcom-venus-sc7180] stream={} configured {}x{} crop={}x{}+{},{} dpbs={} linear={} ubwc={}",
            self.id,
            layout.width,
            layout.height,
            layout.display_width,
            layout.display_height,
            layout.display_x,
            layout.display_y,
            self.dpbs.len(),
            layout.linear_size,
            dpb_size
        );
        Ok(())
    }

    fn queue_dpb(
        &mut self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        index: usize,
    ) -> Result<(), &'static str> {
        let allocation = self
            .dpbs
            .get(index)
            .ok_or("qcom-venus-sc7180: invalid DPB index")?;
        core.send(
            registers,
            &hfi_abi::fill_buffer(
                self.id,
                hfi_abi::HFI_BUFFER_OUTPUT,
                DPB_TAG_BASE + index as u32,
                allocation.dma_addr(),
                allocation.requested_size() as u32,
            ),
            false,
        )?;
        self.dpb_queued[index] = true;
        Ok(())
    }

    fn queue_output(
        &self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        mappings: &FrontendMappings,
        layout: Nv12Layout,
        output_tag: u32,
        request: &VideoBackendDecodeRequest,
    ) -> Result<(), &'static str> {
        let available = request
            .output_len
            .checked_sub(HARDWARE_OUTPUT_OFFSET as u32)
            .ok_or("qcom-venus-sc7180: output buffer lacks an aligned hardware region")?;
        if layout.linear_size > available {
            return Err("qcom-venus-sc7180: decoded frame exceeds frontend output buffer");
        }
        let output_dma = mappings
            .output_dma()
            .checked_add(HARDWARE_OUTPUT_OFFSET as u32)
            .ok_or("qcom-venus-sc7180: output payload DMA address overflows")?;
        let output_vaddr = request
            .output_vaddr
            .checked_add(HARDWARE_OUTPUT_OFFSET)
            .ok_or("qcom-venus-sc7180: output payload address overflows")?;
        arch::clean_invalidate_dcache_to_poc_range(output_vaddr, layout.linear_size as usize);
        core.send(
            registers,
            &hfi_abi::fill_buffer(
                self.id,
                hfi_abi::HFI_BUFFER_OUTPUT2,
                output_tag,
                output_dma,
                layout.linear_size,
            ),
            false,
        )
    }

    fn pending_expired(&self, now: u64) -> bool {
        self.pending.is_some_and(|pending| {
            now.saturating_sub(pending.submitted_at) >= HFI_DECODE_TIMEOUT_US
        })
    }

    fn fail_pending(
        &mut self,
        error: &'static str,
    ) -> Option<Result<VideoBackendDecodedFrame, &'static str>> {
        self.pending.take().map(|_| Err(error))
    }

    /// Consume one HFI packet already demultiplexed to this firmware session.
    fn handle_packet(
        &mut self,
        registers: &VenusRegisters,
        core: &mut HfiCore,
        dma: &DmaContext,
        packet: &[u32],
    ) -> Result<Option<VideoBackendDecodedFrame>, &'static str> {
        if hfi_abi::is_fatal_event(packet, self.id) {
            return Err("qcom-venus-sc7180: fatal firmware event during decode");
        }
        match hfi_abi::packet_type(packet) {
            Some(hfi_abi::HFI_MSG_SESSION_EMPTY_BUFFER) => {
                ensure_response_success(packet)?;
                Ok(None)
            }
            Some(hfi_abi::HFI_MSG_SESSION_FILL_BUFFER) => {
                let done = hfi_abi::parse_fill_done(packet)?;
                if done.session != self.id {
                    return Err("qcom-venus-sc7180: fill completion has the wrong session");
                }
                if done.error != hfi_abi::HFI_ERR_NONE {
                    return Err("qcom-venus-sc7180: firmware failed an output buffer");
                }
                if done.stream_id == 1 {
                    let pending = self
                        .pending
                        .ok_or("qcom-venus-sc7180: output completed without an in-flight frame")?;
                    if done.output_tag != pending.output_tag {
                        return Err("qcom-venus-sc7180: output completed with an unknown tag");
                    }
                    self.pending = None;
                    return self.finish_output(&pending.request, done).map(Some);
                }
                if done.stream_id == 0 && done.output_tag >= DPB_TAG_BASE {
                    let index = (done.output_tag - DPB_TAG_BASE) as usize;
                    if index < self.dpb_queued.len() {
                        self.dpb_queued[index] = false;
                        if done.flags & HFI_BUFFERFLAG_READONLY == 0 {
                            self.queue_dpb(registers, core, index)?;
                        }
                    }
                }
                Ok(None)
            }
            Some(hfi_abi::HFI_MSG_EVENT_NOTIFY)
                if packet.get(3) == Some(&hfi_abi::HFI_EVENT_SESSION_SEQUENCE_CHANGED) =>
            {
                if self.layout.is_some() {
                    return Err("qcom-venus-sc7180: unsupported mid-stream sequence change");
                }
                let sequence = match hfi_abi::parse_sequence_changed(packet) {
                    Ok(sequence) => sequence,
                    Err(error) => {
                        log_sequence_packet(packet);
                        return Err(error);
                    }
                };
                self.reconfigure(registers, core, dma, sequence)?;
                let pending = self
                    .pending
                    .ok_or("qcom-venus-sc7180: sequence event has no in-flight frame")?;
                let mappings = self
                    .mappings
                    .as_ref()
                    .ok_or("qcom-venus-sc7180: frontend mappings disappeared")?;
                self.queue_output(
                    registers,
                    core,
                    mappings,
                    self.layout
                        .ok_or("qcom-venus-sc7180: sequence configuration was not retained")?,
                    pending.output_tag,
                    &pending.request,
                )?;
                Ok(None)
            }
            Some(hfi_abi::HFI_MSG_EVENT_NOTIFY) if packet.get(3) == Some(&0x0100_0006) => {
                let tag = packet.get(8).copied().unwrap_or(0);
                if tag >= DPB_TAG_BASE {
                    let index = (tag - DPB_TAG_BASE) as usize;
                    if index < self.dpb_queued.len() && !self.dpb_queued[index] {
                        self.queue_dpb(registers, core, index)?;
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn finish_output(
        &self,
        request: &VideoBackendDecodeRequest,
        done: FillDone,
    ) -> Result<VideoBackendDecodedFrame, &'static str> {
        let layout = self
            .layout
            .ok_or("qcom-venus-sc7180: output completed without a decoded layout")?;
        let expected_packet_buffer = self
            .mappings
            .as_ref()
            .and_then(|mappings| {
                mappings
                    .output_dma()
                    .checked_add(HARDWARE_OUTPUT_OFFSET as u32)
            })
            .ok_or("qcom-venus-sc7180: output DMA mapping is unavailable")?;
        if done.packet_buffer != expected_packet_buffer {
            return Err("qcom-venus-sc7180: firmware completed an unknown output buffer");
        }
        if done.filled_len == 0 || done.filled_len > layout.linear_size {
            return Err("qcom-venus-sc7180: firmware returned an invalid output length");
        }
        let source_vaddr = request
            .output_vaddr
            .checked_add(HARDWARE_OUTPUT_OFFSET)
            .ok_or("qcom-venus-sc7180: hardware output address overflows")?;
        let payload_vaddr = request
            .output_vaddr
            .checked_add(SCARLET_VIDEO_FRAME_HEADER_LEN)
            .ok_or("qcom-venus-sc7180: frame payload address overflows")?;
        let source_capacity = request
            .output_len
            .checked_sub(HARDWARE_OUTPUT_OFFSET as u32)
            .ok_or("qcom-venus-sc7180: hardware output region is unavailable")?
            as usize;
        let payload_capacity = request
            .output_len
            .checked_sub(SCARLET_VIDEO_FRAME_HEADER_LEN as u32)
            .ok_or("qcom-venus-sc7180: frame payload region is unavailable")?
            as usize;
        arch::invalidate_dcache_to_poc_range(source_vaddr, layout.linear_size as usize);
        compact_nv12(
            source_vaddr,
            source_capacity,
            payload_vaddr,
            payload_capacity,
            layout,
            done,
        )?;
        let payload_len = layout.tight_payload_len()?;
        write_frame_header(
            request.output_vaddr,
            layout.display_width,
            layout.display_height,
            payload_len as u32,
        );
        arch::clean_dcache_to_poc_range(
            request.output_vaddr,
            SCARLET_VIDEO_FRAME_HEADER_LEN + payload_len,
        );
        Ok(VideoBackendDecodedFrame {
            stream_id: self.id,
            frame: ScarletVideoDequeuedFrame {
                width: layout.display_width,
                height: layout.display_height,
                pixel_format: SCARLET_VIDEO_PIXEL_FORMAT_NV12,
                payload_offset: request.output_offset + SCARLET_VIDEO_FRAME_HEADER_LEN as u64,
                payload_len: payload_len as u32,
                flags: done.flags,
                timestamp: request.timestamp,
            },
        })
    }
}

fn configure_output(
    registers: &VenusRegisters,
    core: &mut HfiCore,
    session: u32,
    layout: Nv12Layout,
) -> Result<(), &'static str> {
    let macroblocks = u64::from(layout.width.div_ceil(16)) * u64::from(layout.height.div_ceil(16));
    let mode = if macroblocks <= 3600 {
        hfi_abi::VIDC_WORK_MODE_1
    } else {
        hfi_abi::VIDC_WORK_MODE_2
    };
    let properties = [
        hfi_abi::set_work_mode(session, mode),
        hfi_abi::set_raw_format(
            session,
            hfi_abi::HFI_BUFFER_OUTPUT2,
            hfi_abi::HFI_COLOR_FORMAT_NV12,
        ),
        hfi_abi::set_multistream(session, hfi_abi::HFI_BUFFER_OUTPUT, false),
        hfi_abi::set_multistream(session, hfi_abi::HFI_BUFFER_OUTPUT2, true),
        hfi_abi::set_raw_format(
            session,
            hfi_abi::HFI_BUFFER_OUTPUT,
            hfi_abi::HFI_COLOR_FORMAT_NV12_UBWC,
        ),
        hfi_abi::set_frame_size(
            session,
            hfi_abi::HFI_BUFFER_OUTPUT2,
            layout.width,
            layout.height,
        ),
        hfi_abi::set_buffer_count(session, hfi_abi::HFI_BUFFER_INPUT, 4),
        hfi_abi::set_buffer_count(session, hfi_abi::HFI_BUFFER_OUTPUT, MAX_DYNAMIC_BUFFERS),
        hfi_abi::set_buffer_count(session, hfi_abi::HFI_BUFFER_OUTPUT2, MAX_DYNAMIC_BUFFERS),
        hfi_abi::set_buffer_size(session, hfi_abi::HFI_BUFFER_OUTPUT, layout.ubwc_size),
        hfi_abi::set_buffer_size(session, hfi_abi::HFI_BUFFER_OUTPUT2, layout.linear_size),
    ];
    for property in properties {
        core.send(registers, &property, false)?;
    }
    Ok(())
}

fn allocate_internal_buffers(
    registers: &VenusRegisters,
    core: &mut HfiCore,
    dma: &DmaContext,
    session: u32,
    requirements: &[BufferRequirement],
    scratch_only: bool,
) -> Result<Vec<InternalBuffer>, &'static str> {
    let mut buffers = Vec::new();
    for requirement in requirements {
        let selected = if scratch_only {
            matches!(
                requirement.buffer_type,
                hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_1
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_2
            )
        } else {
            matches!(
                requirement.buffer_type,
                hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_1
                    | hfi_abi::HFI_BUFFER_INTERNAL_SCRATCH_2
                    | hfi_abi::HFI_BUFFER_INTERNAL_PERSIST
                    | hfi_abi::HFI_BUFFER_INTERNAL_PERSIST_1
            )
        };
        if !selected || requirement.size == 0 {
            continue;
        }
        if requirement.alignment > 4096 || requirement.contiguous > 1 {
            return Err("qcom-venus-sc7180: unsupported internal-buffer requirement");
        }
        if requirement.count_actual > MAX_INTERNAL_BUFFERS_PER_TYPE {
            return Err("qcom-venus-sc7180: firmware requested too many internal buffers");
        }
        let count = requirement.count_actual;
        for _ in 0..count {
            let allocation = DmaPagedAllocation::new(dma, requirement.size as usize, rw_flags())?;
            buffers.push(InternalBuffer {
                buffer_type: requirement.buffer_type,
                allocation,
            });
        }
    }
    // Allocate the complete set before publishing any address to firmware.
    // Resource exhaustion is then a session-local failure with no stale HFI
    // buffer registrations, while transport failures below remain fatal.
    for buffer in &buffers {
        core.send(
            registers,
            &hfi_abi::set_internal_buffer(
                session,
                buffer.buffer_type,
                buffer.allocation.requested_size() as u32,
                buffer.allocation.dma_addr(),
            ),
            false,
        )?;
    }
    Ok(buffers)
}

fn compact_nv12(
    source_vaddr: usize,
    source_capacity: usize,
    payload_vaddr: usize,
    payload_capacity: usize,
    layout: Nv12Layout,
    done: FillDone,
) -> Result<(), &'static str> {
    if done.stream_id != 1 {
        return Err("qcom-venus-sc7180: completed buffer is not linear output2");
    }
    let source_offset = done.offset as usize;
    let source_y_len = (layout.y_stride as usize)
        .checked_mul(layout.y_scanlines as usize)
        .ok_or("qcom-venus-sc7180: decoded Y plane overflows")?;
    let source_uv_len = (layout.y_stride as usize)
        .checked_mul(layout.uv_scanlines as usize)
        .ok_or("qcom-venus-sc7180: decoded UV plane overflows")?;
    if source_offset
        .checked_add(source_y_len)
        .and_then(|end| end.checked_add(source_uv_len))
        .is_none_or(|end| end > source_capacity)
    {
        return Err("qcom-venus-sc7180: decoded frame exceeds output allocation");
    }
    let width = layout.display_width as usize;
    let height = layout.display_height as usize;
    let x = layout.display_x as usize;
    let y = layout.display_y as usize;
    let stride = layout.y_stride as usize;
    if layout.tight_payload_len()? > payload_capacity {
        return Err("qcom-venus-sc7180: compacted frame exceeds output payload");
    }
    let source = source_vaddr + source_offset;
    for row in 0..height {
        let src = source + (y + row) * stride + x;
        let dst = payload_vaddr + row * width;
        // SAFETY: source and destination ranges were bounded by the decoded
        // layout and output capacity. They may overlap during compaction.
        unsafe { ptr::copy(src as *const u8, dst as *mut u8, width) };
    }
    let source_uv = source + source_y_len;
    let destination_uv = payload_vaddr + width * height;
    for row in 0..height / 2 {
        let src = source_uv + (y / 2 + row) * stride + x;
        let dst = destination_uv + row * width;
        // SAFETY: same validated in-place compaction bounds as the Y plane.
        unsafe { ptr::copy(src as *const u8, dst as *mut u8, width) };
    }
    Ok(())
}

fn write_frame_header(output_vaddr: usize, width: u32, height: u32, payload_len: u32) {
    let header = output_vaddr as *mut u8;
    // SAFETY: the caller validated the header and payload against the live
    // frontend output buffer and retains its mapping through this write.
    unsafe {
        ptr::copy_nonoverlapping(SCARLET_VIDEO_FRAME_MAGIC.as_ptr(), header, 4);
        ptr::copy_nonoverlapping(width.to_le_bytes().as_ptr(), header.add(4), 4);
        ptr::copy_nonoverlapping(height.to_le_bytes().as_ptr(), header.add(8), 4);
        ptr::copy_nonoverlapping(
            SCARLET_VIDEO_PIXEL_FORMAT_NV12.to_le_bytes().as_ptr(),
            header.add(12),
            4,
        );
        ptr::copy_nonoverlapping(payload_len.to_le_bytes().as_ptr(), header.add(16), 4);
    }
}

fn nv12_ubwc_size(width: u32, height: u32) -> Result<u32, &'static str> {
    let y_meta_stride = align_up_u32(width.div_ceil(32), 64)?;
    let y_meta = align_up_u32(
        y_meta_stride
            .checked_mul(align_up_u32(height.div_ceil(8), 16)?)
            .ok_or("qcom-venus-sc7180: UBWC Y metadata overflows")?,
        4096,
    )?;
    let y_stride = align_up_u32(width, 128)?;
    let y_plane = align_up_u32(
        y_stride
            .checked_mul(align_up_u32(height, 32)?)
            .ok_or("qcom-venus-sc7180: UBWC Y plane overflows")?,
        4096,
    )?;
    let uv_meta_stride = align_up_u32((width / 2).div_ceil(16), 64)?;
    let uv_meta = align_up_u32(
        uv_meta_stride
            .checked_mul(align_up_u32((height / 2).div_ceil(8), 16)?)
            .ok_or("qcom-venus-sc7180: UBWC UV metadata overflows")?,
        4096,
    )?;
    let uv_plane = align_up_u32(
        y_stride
            .checked_mul(align_up_u32(height / 2, 32)?)
            .ok_or("qcom-venus-sc7180: UBWC UV plane overflows")?,
        4096,
    )?;
    let extradata = 16_384u32.max(
        y_stride
            .checked_mul(48)
            .ok_or("qcom-venus-sc7180: UBWC extradata overflows")?,
    );
    y_meta
        .checked_add(y_plane)
        .and_then(|size| size.checked_add(uv_meta))
        .and_then(|size| size.checked_add(uv_plane))
        .and_then(|size| size.checked_add(extradata))
        .and_then(|size| align_up_u32(size, 4096).ok())
        .ok_or("qcom-venus-sc7180: UBWC buffer size overflows")
}

fn align_up_u32(value: u32, alignment: u32) -> Result<u32, &'static str> {
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
        .ok_or("qcom-venus-sc7180: alignment overflows")
}

fn allocate_session_id<F>(next_session_id: &mut u32, mut in_use: F) -> Result<u32, &'static str>
where
    F: FnMut(u32) -> bool,
{
    // At most MAX_CONCURRENT_SESSIONS identifiers can be live. Therefore one
    // more consecutive non-zero candidate is sufficient even across wrap.
    for _ in 0..=MAX_CONCURRENT_SESSIONS {
        let candidate = (*next_session_id).max(1);
        *next_session_id = candidate.wrapping_add(1).max(1);
        if !in_use(candidate) {
            return Ok(candidate);
        }
    }
    Err("qcom-venus-sc7180: no free video session identifier")
}

struct BackendState {
    core: Option<HfiCore>,
    sessions: Vec<VenusSession>,
    next_session_id: u32,
    failed: bool,
    last_error: Option<&'static str>,
}

struct BackendCompletion {
    stream_id: u32,
    result: Result<VideoBackendDecodedFrame, &'static str>,
}

/// Registered SC7180 Venus decode backend.
pub(crate) struct VenusBackend {
    registers: VenusRegisters,
    dma: DmaContext,
    firmware: FirmwareRegion,
    _interconnect_paths: VenusInterconnectPaths,
    _video_clocks: EnabledVideoClocks,
    interrupt_id: InterruptId,
    irq_count: AtomicUsize,
    notifier: IrqSpinLock<Option<Weak<dyn VideoCompletionNotifier>>>,
    active_session_ids: IrqSpinLock<Vec<u32>>,
    inflight_decodes: AtomicUsize,
    queued_decodes: IrqSpinLock<VecDeque<VideoBackendDecodeRequest>>,
    completions: IrqSpinLock<VecDeque<BackendCompletion>>,
    process: Mutex<BackendState>,
}

impl VenusBackend {
    pub(crate) fn new(
        registers: VenusRegisters,
        dma: DmaContext,
        firmware: FirmwareRegion,
        interconnect_paths: VenusInterconnectPaths,
        video_clocks: EnabledVideoClocks,
        interrupt_id: InterruptId,
    ) -> Self {
        Self {
            registers,
            dma,
            firmware,
            _interconnect_paths: interconnect_paths,
            _video_clocks: video_clocks,
            interrupt_id,
            irq_count: AtomicUsize::new(0),
            notifier: IrqSpinLock::new(None),
            active_session_ids: IrqSpinLock::new(Vec::with_capacity(MAX_CONCURRENT_SESSIONS)),
            inflight_decodes: AtomicUsize::new(0),
            queued_decodes: IrqSpinLock::new(VecDeque::with_capacity(MAX_CONCURRENT_SESSIONS)),
            completions: IrqSpinLock::new(VecDeque::with_capacity(MAX_CONCURRENT_SESSIONS)),
            process: Mutex::new(BackendState {
                core: None,
                sessions: Vec::with_capacity(MAX_CONCURRENT_SESSIONS),
                next_session_id: 1,
                failed: false,
                last_error: None,
            }),
        }
    }

    /// Register this backend with the dedicated kernel decode worker.
    ///
    /// Venus HFI waits must not run while a userspace task owns `process`: a
    /// thread-group exit can retire that task in the middle of a syscall and
    /// strand every later open behind its task-owned mutex. The kernel worker
    /// is not part of the client process and therefore always reaches the
    /// normal guard-release path, including when the client exits mid-frame.
    pub(crate) fn start_worker(self: &Arc<Self>) {
        VENUS_WORKER_BACKENDS.lock().push(Arc::downgrade(self));
        if VENUS_WORKER_STARTED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // A later backend can be registered while the singleton worker is
            // blocked indefinitely because every existing backend is idle.
            VENUS_WORKER_WAKER.wake_one();
            return;
        }

        let task = scarlet::task::new_kernel_task(
            String::from("venus-decode-worker"),
            1,
            venus_worker_entry,
        );
        task.init();
        scarlet::sched::scheduler::add_task(task, 0);
    }

    fn queue_worker(&self) {
        if !VENUS_WORKER_PENDING.swap(true, Ordering::AcqRel) {
            VENUS_WORKER_WAKER.wake_one();
        }
    }

    fn process_pending_work(&self) -> bool {
        let mut requests = Vec::with_capacity(MAX_INFLIGHT_DECODES);
        {
            let _irq_guard = IrqGuard::new();
            let mut queued = self.queued_decodes.lock();
            while let Some(request) = queued.pop_front() {
                requests.push(request);
            }
        }

        let mut completions = Vec::with_capacity(MAX_CONCURRENT_SESSIONS);
        let mut made_progress = !requests.is_empty();
        let mut state = self.process.lock();

        let mut request_iter = requests.into_iter();
        while let Some(request) = request_iter.next() {
            if !self.session_is_active(request.stream_id) {
                continue;
            }
            if state.failed {
                completions.push(BackendCompletion {
                    stream_id: request.stream_id,
                    result: Err("qcom-venus-sc7180: firmware is quarantined; recreate the session"),
                });
                continue;
            }
            let Some(session_index) = state
                .sessions
                .iter()
                .position(|session| session.id == request.stream_id)
            else {
                completions.push(BackendCompletion {
                    stream_id: request.stream_id,
                    result: Err("qcom-venus-sc7180: video session is not active"),
                });
                continue;
            };
            let begin_result = {
                let BackendState { core, sessions, .. } = &mut *state;
                let core = core
                    .as_mut()
                    .ok_or("qcom-venus-sc7180: HFI core is not initialized");
                match core {
                    Ok(core) => sessions[session_index].begin_submit(
                        &self.registers,
                        core,
                        &self.dma,
                        &request,
                    ),
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = begin_result {
                completions.push(BackendCompletion {
                    stream_id: request.stream_id,
                    result: Err(error),
                });
                if is_session_resource_error(error) {
                    state.last_error = Some(error);
                } else {
                    Self::quarantine(&mut state, &self.registers, error);
                    Self::fail_all_pending(&mut state, error, &mut completions);
                    for queued in request_iter {
                        completions.push(BackendCompletion {
                            stream_id: queued.stream_id,
                            result: Err(error),
                        });
                    }
                    break;
                }
            } else {
                state.last_error = None;
            }
        }

        while !state.failed {
            let packet = match state.core.as_mut() {
                Some(core) => match core.try_next_packet(&self.registers) {
                    Ok(packet) => packet,
                    Err(error) => {
                        Self::quarantine(&mut state, &self.registers, error);
                        Self::fail_all_pending(&mut state, error, &mut completions);
                        break;
                    }
                },
                None => None,
            };
            let Some(packet) = packet else {
                break;
            };
            made_progress = true;

            // A system event is the only decode-packet failure that poisons
            // the shared HFI core. Session events are completed only against
            // their owning stream, matching Linux Venus' instance dispatch.
            if hfi_abi::is_fatal_event(&packet, 0) {
                let error = "qcom-venus-sc7180: firmware reported a fatal HFI event";
                Self::quarantine(&mut state, &self.registers, error);
                Self::fail_all_pending(&mut state, error, &mut completions);
                break;
            }

            let Some(stream_id) = hfi_abi::session_id(&packet) else {
                continue;
            };
            let Some(session_index) = state
                .sessions
                .iter()
                .position(|session| session.id == stream_id)
            else {
                // A STOP/END command can race a final buffer notification.
                // The destroyed frontend mapping is no longer schedulable, so
                // safely discard that stale session packet.
                continue;
            };
            let packet_result = {
                let BackendState { core, sessions, .. } = &mut *state;
                let core = core
                    .as_mut()
                    .ok_or("qcom-venus-sc7180: HFI core is not initialized");
                match core {
                    Ok(core) => sessions[session_index].handle_packet(
                        &self.registers,
                        core,
                        &self.dma,
                        &packet,
                    ),
                    Err(error) => Err(error),
                }
            };
            match packet_result {
                Ok(Some(frame)) => {
                    state.last_error = None;
                    completions.push(BackendCompletion {
                        stream_id,
                        result: Ok(frame),
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    state.last_error = Some(error);
                    state.sessions[session_index].pending = None;
                    completions.push(BackendCompletion {
                        stream_id,
                        result: Err(error),
                    });
                }
            }
        }

        if !state.failed {
            let now = time::current_time();
            if state
                .sessions
                .iter()
                .any(|session| session.pending_expired(now))
            {
                let error = "qcom-venus-sc7180: asynchronous decode timed out";
                if let Some(core) = state.core.as_mut() {
                    core.transport.log_diagnostics(&self.registers);
                }
                Self::quarantine(&mut state, &self.registers, error);
                Self::fail_all_pending(&mut state, error, &mut completions);
            }
        }

        let inflight = state
            .sessions
            .iter()
            .filter(|session| session.pending.is_some())
            .count();
        self.inflight_decodes.store(inflight, Ordering::Release);
        drop(state);

        let mut published = false;
        for completion in completions {
            if !self.session_is_active(completion.stream_id) {
                continue;
            }
            let _irq_guard = IrqGuard::new();
            self.completions.lock().push_back(completion);
            published = true;
        }
        if published {
            self.notify_completion();
        }
        made_progress || published
    }

    fn fail_all_pending(
        state: &mut BackendState,
        error: &'static str,
        completions: &mut Vec<BackendCompletion>,
    ) {
        for session in &mut state.sessions {
            if session.fail_pending(error).is_some() {
                completions.push(BackendCompletion {
                    stream_id: session.id,
                    result: Err(error),
                });
            }
        }
    }

    fn session_is_active(&self, stream_id: u32) -> bool {
        self.active_session_ids.lock().contains(&stream_id)
    }

    fn activate_session(&self, stream_id: u32) {
        let mut active = self.active_session_ids.lock();
        debug_assert!(!active.contains(&stream_id));
        debug_assert!(active.len() < MAX_CONCURRENT_SESSIONS);
        active.push(stream_id);
    }

    fn deactivate_session(&self, stream_id: u32) -> bool {
        let mut active = self.active_session_ids.lock();
        let Some(index) = active.iter().position(|active| *active == stream_id) else {
            return false;
        };
        active.swap_remove(index);
        true
    }

    fn ensure_core<'a>(
        &self,
        state: &'a mut BackendState,
    ) -> Result<&'a mut HfiCore, &'static str> {
        if state.core.is_none() {
            self.dma
                .restore_iommu()
                .map_err(|_| "qcom-venus-sc7180: failed to restore Venus IOMMU")?;
            self.firmware
                .dma
                .restore_iommu()
                .map_err(|_| "qcom-venus-sc7180: failed to restore firmware context bank")?;
            state.core = Some(HfiCore::boot(&self.registers, &self.dma, &self.firmware)?);
        }
        state
            .core
            .as_mut()
            .ok_or("qcom-venus-sc7180: HFI core is unavailable")
    }

    fn notify_completion(&self) {
        let notifier = {
            let _irq_guard = IrqGuard::new();
            self.notifier.lock().as_ref().and_then(Weak::upgrade)
        };
        if let Some(notifier) = notifier {
            notifier.notify_video_completion();
        }
    }

    fn quarantine(state: &mut BackendState, registers: &VenusRegisters, error: &'static str) {
        registers.mask_interrupts();
        registers.assert_arm9_reset();
        state.failed = true;
        state.last_error = Some(error);
    }
}

fn process_pending_venus_work() -> bool {
    let was_woken = VENUS_WORKER_PENDING.swap(false, Ordering::AcqRel);

    let backends = {
        let mut registered = VENUS_WORKER_BACKENDS.lock();
        let mut live = Vec::with_capacity(registered.len());
        registered.retain(|weak| {
            if let Some(backend) = weak.upgrade() {
                live.push(backend);
                true
            } else {
                false
            }
        });
        live
    };
    let mut made_progress = false;
    for backend in backends {
        made_progress |= backend.process_pending_work();
    }
    was_woken || made_progress
}

fn venus_worker_has_inflight_decodes() -> bool {
    VENUS_WORKER_BACKENDS
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .any(|backend| backend.inflight_decodes.load(Ordering::Acquire) != 0)
}

fn venus_worker_entry() {
    loop {
        while process_pending_venus_work() {}

        let Some(task) = scarlet::task::mytask() else {
            scarlet::arch::instruction::idle();
        };
        // Poll only while a decode is actually in flight: that bounded timeout
        // is the lost-IRQ safety net and drives the per-session deadline.
        // With no in-flight work, an unconditional 100 us timeout would wake
        // this singleton roughly 10,000 times per second forever after the
        // final session closed. Waker latches an early wake, so the indefinite
        // idle wait cannot lose a queue or IRQ notification.
        let timeout_ns = venus_worker_has_inflight_decodes().then_some(100_000);
        let _ =
            VENUS_WORKER_WAKER.wait_with_timeout(task.get_id(), task.get_trapframe(), timeout_ns);
    }
}

impl VideoDecodeBackend for VenusBackend {
    fn name(&self) -> &'static str {
        "qcom-venus-sc7180"
    }

    fn debug_status(&self) -> Option<String> {
        let active = self.active_session_ids.lock().len();
        let queued = self.queued_decodes.lock().len();
        let inflight = self.inflight_decodes.load(Ordering::Acquire);
        let completed = self.completions.lock().len();
        match self.process.try_lock() {
            Some(state) => Some(format!(
                " core={} sessions={} active={} queued={} inflight={} completed={} failed={} irq={} control={:#x} last_error={}",
                state.core.is_some(),
                state.sessions.len(),
                active,
                queued,
                inflight,
                completed,
                state.failed,
                self.irq_count.load(Ordering::Relaxed),
                self.registers.control_status(),
                state.last_error.unwrap_or("none")
            )),
            None => Some(format!(
                " core=busy active={} queued={} inflight={} completed={} irq={} control={:#x}",
                active,
                queued,
                inflight,
                completed,
                self.irq_count.load(Ordering::Relaxed),
                self.registers.control_status(),
            )),
        }
    }

    fn capabilities(&self) -> VideoBackendCapabilities {
        VideoBackendCapabilities {
            max_sessions: MAX_CONCURRENT_SESSIONS as u32,
            max_inflight_decodes: MAX_INFLIGHT_DECODES as u32,
            mapped_input_len: MAPPED_INPUT_BYTES as u32,
            mapped_output_len: MAPPED_OUTPUT_BYTES as u32,
            output_pixel_format: SCARLET_VIDEO_PIXEL_FORMAT_NV12,
            supports_h264: true,
            supports_av1: false,
            supports_hevc: false,
            supports_stateless_h264: false,
        }
    }

    fn supports_variable_mapped_buffers(&self) -> bool {
        true
    }

    fn set_completion_notifier(&self, notifier: Option<Weak<dyn VideoCompletionNotifier>>) {
        let _irq_guard = IrqGuard::new();
        *self.notifier.lock() = notifier;
    }

    fn create_session(&self, coded_format: u32) -> Result<u32, &'static str> {
        if coded_format != SCARLET_VIDEO_FORMAT_H264 {
            return Err("qcom-venus-sc7180: only stateful H.264 is supported");
        }
        let mut state = self.process.lock();
        if state.sessions.len() >= MAX_CONCURRENT_SESSIONS {
            return Err("qcom-venus-sc7180: maximum video session count reached");
        }
        if state.failed {
            if !state.sessions.is_empty() {
                return Err(
                    "qcom-venus-sc7180: firmware is quarantined; close existing sessions first",
                );
            }
            state.core = None;
            state.failed = false;
            state.last_error = None;
        }
        let mut next_session_id = state.next_session_id;
        let session_id = allocate_session_id(&mut next_session_id, |candidate| {
            state.sessions.iter().any(|session| session.id == candidate)
        })?;
        state.next_session_id = next_session_id;
        let dma = self.dma.clone();
        let core = self.ensure_core(&mut state)?;
        match VenusSession::create(session_id, &self.registers, core, &dma) {
            Ok(session) => {
                state.sessions.push(session);
                state.last_error = None;
                self.activate_session(session_id);
                // Synchronous session setup may have deferred completion
                // packets belonging to existing streams.
                self.queue_worker();
                Ok(session_id)
            }
            Err(error) => {
                Self::quarantine(&mut state, &self.registers, error);
                Err(error)
            }
        }
    }

    fn destroy_session(&self, stream_id: u32) -> Result<(), &'static str> {
        if !self.deactivate_session(stream_id) {
            return Err("qcom-venus-sc7180: unknown video session");
        }
        {
            let _irq_guard = IrqGuard::new();
            self.queued_decodes
                .lock()
                .retain(|request| request.stream_id != stream_id);
            self.completions
                .lock()
                .retain(|completion| completion.stream_id != stream_id);
        }

        let mut reported_wait = false;
        let mut state = loop {
            if let Some(state) = self.process.try_lock() {
                break state;
            }
            if !reported_wait {
                println!(
                    "[qcom-venus-sc7180] stream={} teardown waiting for decode worker inflight={}",
                    stream_id,
                    self.inflight_decodes.load(Ordering::Acquire)
                );
                reported_wait = true;
            }

            // File teardown can run after the calling userspace task has
            // already entered Terminated state. Such a task cannot join a
            // sleepable Mutex wait queue: it will never be scheduled again,
            // and Mutex correctly rejects it. The process mutex owner is the
            // dedicated Venus kernel worker, whose HFI waits are bounded, so
            // poll without enqueueing the dying task until the worker drops
            // the guard. This also preserves destroy_session's contract that
            // submitted buffers are no longer touched when teardown returns.
            core::hint::spin_loop();
        };
        let Some(session_index) = state
            .sessions
            .iter()
            .position(|session| session.id == stream_id)
        else {
            return Err("qcom-venus-sc7180: video session is not active");
        };
        let mut session = state.sessions.swap_remove(session_index);
        self.inflight_decodes.store(
            state
                .sessions
                .iter()
                .filter(|session| session.pending.is_some())
                .count(),
            Ordering::Release,
        );
        if state.failed {
            self.registers.assert_arm9_reset();
            if state.sessions.is_empty() {
                state.core = None;
                state.failed = false;
                state.last_error = None;
            }
            self.queue_worker();
            return Ok(());
        }
        let result = match state.core.as_mut() {
            Some(core) => session.destroy(&self.registers, core),
            None => Err("qcom-venus-sc7180: HFI core disappeared during session destroy"),
        };
        if let Err(error) = result {
            Self::quarantine(&mut state, &self.registers, error);
            if state.sessions.is_empty() {
                state.core = None;
                state.failed = false;
                state.last_error = None;
            }
            self.queue_worker();
            return Err(error);
        }
        state.last_error = None;
        self.queue_worker();
        Ok(())
    }

    fn submit_decode(&self, request: &VideoBackendDecodeRequest) -> Result<(), &'static str> {
        if !self.session_is_active(request.stream_id) {
            return Err("qcom-venus-sc7180: unknown video session");
        }
        if self
            .completions
            .lock()
            .iter()
            .any(|completion| completion.stream_id == request.stream_id)
        {
            return Err("qcom-venus-sc7180: previous decoded frame was not dequeued");
        }
        {
            let _irq_guard = IrqGuard::new();
            let mut queued = self.queued_decodes.lock();
            if queued
                .iter()
                .any(|queued| queued.stream_id == request.stream_id)
            {
                return Err("qcom-venus-sc7180: session decode is already queued");
            }
            if queued.len() >= MAX_CONCURRENT_SESSIONS {
                return Err("qcom-venus-sc7180: decode queue is full");
            }
            queued.push_back(*request);
        }
        self.queue_worker();
        Ok(())
    }

    fn dequeue_frame(
        &self,
        stream_id: u32,
    ) -> Result<Option<VideoBackendDecodedFrame>, &'static str> {
        if !self.session_is_active(stream_id) {
            return Err("qcom-venus-sc7180: unknown video session");
        }
        let _irq_guard = IrqGuard::new();
        let mut completions = self.completions.lock();
        let Some(index) = completions
            .iter()
            .position(|completion| completion.stream_id == stream_id)
        else {
            return Ok(None);
        };
        let completion = completions
            .remove(index)
            .ok_or("qcom-venus-sc7180: completion queue changed unexpectedly")?;
        completion.result.map(Some)
    }
}

impl InterruptSource for VenusBackend {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.interrupt_id)
    }

    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        // IRQ 206 is dedicated to Venus. Linux clears the A2H soft interrupt
        // on every invocation even when WRAPPER_INTR_STATUS has already
        // deasserted or reads as zero. Returning NotMine in that window leaves
        // the level-triggered line asserted and creates an interrupt storm.
        let _ = self.registers.acknowledge_interrupt();
        self.irq_count.fetch_add(1, Ordering::Relaxed);
        VENUS_HFI_WAKER.wake_one();
        self.queue_worker();
        Ok(InterruptClaim::Handled)
    }
}

impl MaskableInterruptSource for VenusBackend {
    fn mask_source(&self) -> InterruptResult<()> {
        self.registers.mask_interrupts();
        Ok(())
    }

    fn unmask_source(&self) -> InterruptResult<()> {
        self.registers.unmask_interrupts();
        Ok(())
    }

    fn clear_pending_source(&self) -> InterruptResult<()> {
        self.registers.clear_pending_interrupts();
        Ok(())
    }
}
