// SPDX-License-Identifier: GPL-2.0-only

//! Scarlet generic GPU backend for the SC7180 Adreno 618.

use alloc::{
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use scarlet::{
    arch,
    device::{
        events::InterruptCapableDevice,
        gpu::{
            GPU_EXECUTION_SUPPORT_ADDRESS_SPACE, GPU_EXECUTION_SUPPORT_IMAGE_MIPS,
            GPU_EXECUTION_SUPPORT_IMAGE_READBACK, GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD,
            GPU_EXECUTION_SUPPORT_MEMORY, GPU_EXECUTION_SUPPORT_PRESENTATION,
            GPU_EXECUTION_SUPPORT_QUEUE, GPU_EXECUTION_SUPPORT_TEXTURE_ARRAYS,
            GPU_EXECUTION_SUPPORT_TIMELINE, GPU_IMAGE_FORMAT_BGRA8_UNORM,
            GPU_IMAGE_FORMAT_DEPTH32_FLOAT, GPU_IMAGE_FORMAT_RGBA16_FLOAT,
            GPU_IMAGE_FORMAT_RGBA32_FLOAT, GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT,
            GPU_IMAGE_USAGE_PRESENTABLE, GPU_IMAGE_USAGE_RENDER_TARGET, GPU_IMAGE_USAGE_SAMPLED,
            GPU_IMAGE_USAGE_TRANSFER_DST, GPU_IMAGE_USAGE_TRANSFER_SRC, GpuBackend,
            GpuBackendBuffer, GpuBackendBufferInfo, GpuBackendContext, GpuBackendContextInfo,
            GpuBackendCpuAccessGuard, GpuBackendDialectDescriptor, GpuBackendDialectInfo,
            GpuBackendEnqueueError, GpuBackendImage, GpuBackendImageInfo, GpuBackendImageLayout,
            GpuBackendImagePlaneLayout, GpuBackendLinearDisplayInfo, GpuBackendQueue,
            GpuBackendQueueInfo, GpuBackendSubmitError, GpuBackingPin, GpuBufferCreateInfo,
            GpuCompletionFailure, GpuDeviceInfo, GpuDeviceState, GpuImageBackingInfo,
            GpuImageCreateInfo, GpuImageUploadInfo, GpuSubmission, register_gpu_control_device,
        },
        graphics::{GpuDisplayResource, PixelFormat},
        iommu::{
            DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags, Iova,
            PhysAddr,
        },
        manager::{DeviceManager, probe_defer},
        platform::{PlatformDeviceInfo, resource::PlatformDeviceResourceType},
    },
    environment::PAGE_SIZE,
    println,
    sync::{IrqSpinLock, Mutex, Waker},
    time, vm,
};

use adreno_a6xx_layout::{
    DEPTH32_LAYER_ALIGNMENT, IMAGE_MODIFIER_TILE6_3_COLOR, IMAGE_MODIFIER_TILE6_3_DEPTH,
    depth32_tile6_3_layout,
};
use adreno_a6xx_pm4::{opcode, type4, type7};
use adreno_a6xx_shader_pack::{PACK_SIZE, SHADER_ALIGNMENT, ShaderVariant, copy_pack};
use qcom_sc7180_interconnect::GpuMemoryPath;

use crate::{
    firmware,
    gmu::{self, A618Gmu},
    memory::{DmaAllocation, bidirectional_flags},
    opp::read_gpu_operating_points,
    pending::{PendingQueue, fence_retired},
    registers::*,
    submit::{
        ImageModifier, LinearImage, LinearImageFormat, ResolvedResource, diagnose_rejected_packet,
        validate_and_relocate_decoded,
    },
};

#[path = "isolation.rs"]
mod isolation;

#[path = "completion_wait.rs"]
mod completion_wait;
#[path = "readback.rs"]
mod readback;
#[path = "timestamp.rs"]
mod timestamp;
#[path = "ring_cache.rs"]
mod ring_cache;

const GPU_IOVA_BASE: u64 = 0x1_0000_0000;
const GPU_IOVA_SIZE: u64 = 0x1_0000_0000;
const DIALECT_TOKEN: u64 = adreno_a6xx_submit_wire::DIALECT_TOKEN;
const BACKEND_ID: &[u8] = b"qcom-adreno";
const DIALECT_ID: &[u8] = b"adreno-a6xx-pm4-reloc-v1";
const SQE_FIRMWARE_MAX_SIZE: usize = 256 * 1024;
const RING_SIZE: usize = 32 * 1024;
const A618_MAX_OPAQUE_COMMAND_SIZE: u32 = adreno_a6xx_submit_wire::MAX_SUBMIT_SIZE as u32;
const GMEM_BASE: u64 = 0x10_0000;
const GMEM_SIZE: u64 = 512 * 1024;
const GPU_TIMEOUT_US: u64 = 1_000_000;
const GPU_QUIESCE_TIMEOUT_US: u64 = 10_000;
const GPU_INTERRUPT_CLEAR_TIMEOUT_US: u64 = 1_000;
const ASYNC_CAPACITY: usize = 8;
const PROGRESS_INTERVAL_NS: u64 = 1_000_000;
const CP_INDIRECT_BUFFER: u8 = 0x3f;
const EVENT_CACHE_FLUSH_TS: u32 = 0x04;
const EVENT_CCU_INVALIDATE_DEPTH: u32 = 0x18;
const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
const EVENT_CACHE_INVALIDATE: u32 = 0x31;
const CP_EVENT_WRITE_IRQ: u32 = 1 << 31;
const RBBM_INT_POLL_MASK: u32 = RBBM_INT_FATAL_MASK | RBBM_INT_CP_CACHE_FLUSH_TS;
// Linux's A6xx hardware initialization uses the complete 49-bit UCHE address
// range.  Limiting this to a mapped trap page silently puts every later
// shader, vertex, and texture allocation into L2-bypass mode.
const UCHE_CACHED_WRITE_RANGE_MAX: u64 = 0x0001_ffff_ffff_ffc0;
const UCHE_UNMAPPED_TRAP_BASE: u64 = 0x0001_ffff_ffff_f000;

fn completion_commands(fence_address: u64, sequence: u32) -> Result<[u32; 7], &'static str> {
    let scratch_header = type4(CP_SCRATCH_2 as u32, 1)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel scratch write")?;
    let event_header = type7(opcode::EVENT_WRITE, 4)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel fence")?;
    Ok([
        scratch_header,
        sequence,
        event_header,
        EVENT_CACHE_FLUSH_TS | CP_EVENT_WRITE_IRQ,
        fence_address as u32,
        (fence_address >> 32) as u32,
        sequence,
    ])
}

fn submission_commands(command_address: u64, word_count: u32) -> Result<[u32; 10], &'static str> {
    let event_header = type7(opcode::EVENT_WRITE, 1)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel CCU invalidate")?;
    let indirect_header = type7(CP_INDIRECT_BUFFER, 3)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel indirect buffer")?;

    // Linux emits CACHE_INVALIDATE after a page-table switch and before the
    // next userspace IB, then unconditionally invalidates both CCUs.  Scarlet's
    // command mappings can reuse an IOVA after the previous process exits, so
    // the UCHE invalidate is required even though the SMMU TLB was invalidated
    // on the CPU side.  Do not append front-end waits after the IB: Linux's
    // A6xx submit path proceeds directly to the scratch fence and
    // CACHE_FLUSH_TS, whose event semantics retire the preceding work.  Extra
    // WAIT_MEM_WRITES/WAIT_FOR_ME packets fault after an addressed depth-CCU
    // flush on A618 and add a full-pipeline stall to every submission.
    Ok([
        event_header,
        EVENT_CACHE_INVALIDATE,
        event_header,
        EVENT_CCU_INVALIDATE_DEPTH,
        event_header,
        EVENT_CCU_INVALIDATE_COLOR,
        indirect_header,
        command_address as u32,
        (command_address >> 32) as u32,
        word_count,
    ])
}

const CP_PROTECT: [u32; 32] = [
    protect_readonly(0x00000, 0x04ff),
    protect_readonly(0x00501, 0x0005),
    protect_readonly(0x0050b, 0x02f4),
    protect_no_access(0x0050e, 0x0000),
    protect_no_access(0x00510, 0x0000),
    protect_no_access(0x00534, 0x0000),
    protect_no_access(0x00800, 0x0082),
    protect_no_access(0x008a0, 0x0008),
    protect_no_access(0x008ab, 0x0024),
    protect_readonly(0x008de, 0x00ae),
    protect_no_access(0x00900, 0x004d),
    protect_no_access(0x0098d, 0x0272),
    protect_no_access(0x00e00, 0x0001),
    protect_no_access(0x00e03, 0x000c),
    protect_no_access(0x03c00, 0x00c3),
    protect_readonly(0x03cc4, 0x1fff),
    protect_no_access(0x08630, 0x01cf),
    protect_no_access(0x08e00, 0x0000),
    protect_no_access(0x08e08, 0x0000),
    protect_no_access(0x08e50, 0x001f),
    protect_no_access(0x09624, 0x01db),
    protect_no_access(0x09e70, 0x0001),
    protect_no_access(0x09e78, 0x0187),
    protect_no_access(0x0a630, 0x01cf),
    protect_no_access(0x0ae02, 0x0000),
    protect_no_access(0x0ae50, 0x032f),
    protect_no_access(0x0b604, 0x0000),
    protect_no_access(0x0be02, 0x0001),
    protect_no_access(0x0be20, 0x17df),
    protect_no_access(0x0f000, 0x0bff),
    protect_readonly(0x0fc00, 0x1fff),
    protect_no_access(0x11c00, 0x0000),
];

const fn protect_no_access(register: u32, length: u32) -> u32 {
    (1 << 31) | ((length & 0x3fff) << 18) | (register & 0x3ffff)
}

const fn protect_readonly(register: u32, length: u32) -> u32 {
    ((length & 0x3fff) << 18) | (register & 0x3ffff)
}

static NEXT_BACKEND_COOKIE: AtomicU64 = AtomicU64::new(1);
static NEXT_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_monotonic(counter: &AtomicU64, exhausted: &'static str) -> Result<u64, &'static str> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0 && current != u64::MAX).then_some(current + 1)
        })
        .map_err(|_| exhausted)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootState {
    Cold,
    Ready,
    Lost,
}

#[derive(Clone, Copy, Default)]
struct RingInterruptSnapshot {
    rbbm: u32,
    cp: u32,
    cp_hw_fault: u32,
    cp_protect_status: u32,
    cp_opcode: u32,
}

impl RingInterruptSnapshot {
    fn merge(&mut self, newer: Self) {
        self.rbbm |= newer.rbbm;
        self.cp |= newer.cp;
        if newer.cp_hw_fault != 0 {
            self.cp_hw_fault = newer.cp_hw_fault;
        }
        if newer.cp_protect_status != 0 {
            self.cp_protect_status = newer.cp_protect_status;
        }
        if newer.cp_opcode != 0 {
            self.cp_opcode = newer.cp_opcode;
        }
    }

    fn is_fatal(self) -> bool {
        let completion = self.rbbm & RBBM_INT_COMPLETION_MASK;
        (self.rbbm & !completion) & RBBM_INT_FATAL_MASK != 0 || self.cp & CP_INT_FATAL_MASK != 0
    }

    fn is_empty(self) -> bool {
        self.rbbm == 0 && self.cp == 0
    }
}

#[derive(Clone, Copy)]
struct RingFailureSnapshot {
    reason: &'static str,
    sequence: u32,
    command_words: usize,
    interrupt: u32,
    rptr: u32,
    wptr: u32,
    target_wptr: u32,
    status: u32,
    status1: u32,
    status2: u32,
    status3: u32,
    cp_interrupt: u32,
    cp_hw_fault: u32,
    cp_protect_status: u32,
    cp_opcode: u32,
    ib1_base: u64,
    ib1_translation: Option<u64>,
    ib1_remaining: u32,
    ib2_base: u64,
    ib2_remaining: u32,
    roq_rb: u32,
    roq_ib1: u32,
    roq_ib2: u32,
    roq_sds: u32,
    roq_mrb: u32,
    roq_vsd: u32,
    scratch2: u32,
    fence_dma: u64,
    fence_paddr: u64,
    fence_translation: Option<u64>,
    fence: Option<(u32, u32)>,
    private_domain: bool,
    active_ib1_translation: Option<u64>,
    active_fence_translation: Option<u64>,
    active_iommu_fault: Option<isolation::DomainFault>,
    fence_head: [u32; 4],
    completion_tail: Option<[u32; 7]>,
}

struct HardwareState {
    boot: BootState,
    ring: DmaAllocation,
    command: DmaAllocation,
    sqe: Option<DmaAllocation>,
    shader_pack: Option<DmaAllocation>,
    fence: DmaAllocation,
    wptr: u32,
    fence_sequence: u32,
    gpu_oob_held: bool,
    quiesce_failed: bool,
    lost_reason: Option<&'static str>,
    last_ring_failure: Option<RingFailureSnapshot>,
    execution_domain: Option<isolation::ExecutionDomain>,
    idle_leases: isolation::IdleLeases<IdleLease>,
}

struct IdleLease {
    owner: isolation::ExecutionDomain,
    client: Arc<A618ClientVm>,
    stopped: isolation::DmaStopped,
}

/// Stack-only timestamps, compiled out without the existing profiling feature.
/// Bound output to the same sequences as the completion-path sampler.
#[cfg(feature = "submit-profile")]
#[derive(Default)]
struct SubmissionProfile {
    // start, locks, copy, authority, lookup, alloc/evict, map, halt, scrub,
    // bind/resume, ring, restore/resume
    stamps: [u64; 12],
    pages: usize,
    cache_hit: bool,
    retargeted: bool,
    private_ordinal: u64,
}

#[cfg(feature = "submit-profile")]
static PRIVATE_PHASE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "submit-profile")]
impl SubmissionProfile {
    fn stamp(&mut self, index: usize) {
        self.stamps[index] = time::current_time();
    }

    fn report(&self, sequence: u32, words: usize, private: bool) {
        let sampled = if private {
            self.private_ordinal <= 4 || self.private_ordinal % 16 == 0
        } else {
            sequence <= 4 || sequence % 64 == 0
        };
        if !sampled {
            return;
        }
        let us = |index: usize| self.stamps[index + 1].saturating_sub(self.stamps[index]);
        println!(
            "[a618-phases] seq={} words={} private={} private_n={} hit={} retarget={} pages={} total_us={} lock_us={} copy_us={} authority_us={} lookup_us={} alloc_evict_us={} map_us={} halt_us={} scrub_us={} bind_us={} ring_us={} restore_us={}",
            sequence, words, private, self.private_ordinal, self.cache_hit, self.retargeted, self.pages,
            self.stamps[11].saturating_sub(self.stamps[0]),
            us(0), us(1), us(2), us(3), us(4), us(5), us(6), us(7), us(8), us(9), us(10),
        );
    }
}

struct MappingGate {
    execution: Mutex<()>,
    quarantine: AtomicBool,
}

struct ResourceMapping {
    gate: Arc<MappingGate>,
    mapping: Option<DmaMapping>,
}

impl ResourceMapping {
    fn new(gate: Arc<MappingGate>, mapping: DmaMapping) -> Self {
        Self {
            gate,
            mapping: Some(mapping),
        }
    }
}

impl Drop for ResourceMapping {
    fn drop(&mut self) {
        let _gate = self.gate.execution.lock();
        if self.gate.quarantine.load(Ordering::Acquire) {
            // A failed bus halt cannot authorize SMMU invalidation or unmap.
            // The active request separately retains the physical backing.
            core::mem::forget(self.mapping.take());
        } else {
            drop(self.mapping.take());
        }
    }
}

struct ResourceEntry {
    token: u64,
    gpu_va: u64,
    allocation_size: u64,
    allowed_access: u32,
    linear_image: Option<LinearImage>,
    // Exact inputs of this immutable owned base mapping, never keyed by IOVA
    // alone. PreparedSubmission pins this entry until GPU retirement.
    physical_spans: Vec<isolation::PhysicalSpan>,
    _mapping: ResourceMapping,
}

/// Each adapter keeps one immutable SID list; additional controllers and
/// substreams are rejected until an equivalent all-controller switch exists.
struct IsolationDomainAdapter {
    domain: Arc<dyn scarlet::device::iommu::IommuDomain>,
    streams: Vec<scarlet::device::iommu::IommuStreamId>,
}

impl isolation::Domain for IsolationDomainAdapter {
    fn translate(&self, iova: u64) -> Option<u64> {
        self.domain
            .iova_to_phys(Iova::new(iova))
            .map(|p| p.as_u64())
    }
    fn fault_snapshot(&self) -> Option<isolation::DomainFault> {
        self.domain.fault_snapshot().map(|fault| isolation::DomainFault {
            context_fault_pending: fault.context_fault_pending,
            global_status: fault.global_status,
            context_status: fault.context_status,
            fault_address: fault.fault_address,
            syndrome0: fault.syndrome0,
            syndrome1: fault.syndrome1,
        })
    }
    fn pending_context_fault(&self) -> bool {
        self.domain.pending_context_fault().unwrap_or(false)
    }
    fn map(
        &self,
        iova: u64,
        physical: u64,
        len: usize,
        access: u32,
        coherent: bool,
    ) -> Result<(), &'static str> {
        let mut flags = if access & adreno_a6xx_submit_wire::ACCESS_READ != 0 {
            IommuMapFlags::READ
        } else {
            IommuMapFlags::WRITE
        };
        if access & adreno_a6xx_submit_wire::ACCESS_WRITE != 0 {
            flags = flags | IommuMapFlags::WRITE;
        }
        if coherent {
            flags = flags | IommuMapFlags::COHERENT;
        }
        self.domain
            .map(Iova::new(iova), PhysAddr::new(physical), len, flags)
            .map_err(|_| "a618 isolation: private page mapping failed")
    }
    fn unmap(&self, iova: u64, len: usize) -> Result<(), &'static str> {
        self.domain.unmap(Iova::new(iova), len)
            .map_err(|_| "a618 isolation: private page unmapping failed")
    }
    fn attach(&self, stream: usize) -> Result<(), &'static str> {
        self.domain
            .attach_stream(self.streams[stream])
            .map_err(|_| "a618 isolation: stream attach failed")
    }
    fn detach(&self, stream: usize) -> Result<(), &'static str> {
        self.domain
            .detach_stream(self.streams[stream])
            .map_err(|_| "a618 isolation: stream detach failed")
    }
    fn suspend(&self) -> Result<(), &'static str> {
        self.domain
            .suspend()
            .map_err(|_| "a618 isolation: VM context suspend failed")
    }
    fn resume(&self) -> Result<(), &'static str> {
        self.domain
            .resume()
            .map_err(|_| "a618 isolation: VM context resume failed")
    }
    fn try_resume(&self) -> Result<bool, &'static str> {
        self.domain.try_resume()
            .map_err(|_| "a618 isolation: VM context acquisition failed")
    }
}

struct PreparedSubmission {
    words: Vec<u32>,
    isolated_ranges: Option<Vec<isolation::Range>>,
    // Detach removes authority for future requests, but must not unmap an
    // accepted request. client_vm owns real backing pins in addition to the
    // generic GpuSubmission's independently retained full attachment snapshot.
    _resources: Arc<Vec<Arc<ResourceEntry>>>,
    client_vm: Option<Arc<A618ClientVm>>,
}

struct SyncResult {
    result: IrqSpinLock<Option<Result<(), GpuBackendSubmitError>>>,
    waker: Waker,
}

impl SyncResult {
    fn new() -> Self {
        Self {
            result: IrqSpinLock::new(None),
            waker: Waker::new_uninterruptible("a618-sync-result"),
        }
    }

    fn complete(&self, result: Result<(), GpuBackendSubmitError>) {
        *self.result.lock() = Some(result);
        self.waker.wake_all();
    }

    fn wait(&self) -> Result<(), GpuBackendSubmitError> {
        loop {
            if let Some(result) = *self.result.lock() {
                return result;
            }
            wait_for_progress(&self.waker);
        }
    }
}

enum SubmissionObserver {
    Async(GpuSubmission),
    Sync(Arc<SyncResult>),
}

struct PendingSubmission {
    prepared: PreparedSubmission,
    observer: SubmissionObserver,
    // The worker registry is weak. Accepted work itself keeps the device alive
    // after closing its session, queue, or last completion handle.
    _core: Arc<A618Core>,
}

impl PendingSubmission {
    // Only unaccepted async work reaches this path. Call with no pending or
    // hardware lock: dropping prepared ownership can unmap GPU resources.
    fn unaccepted(self, error: Option<GpuBackendSubmitError>) -> GpuBackendEnqueueError {
        let Self { prepared, observer, _core } = self;
        drop(prepared);
        drop(_core);
        let SubmissionObserver::Async(submission) = observer else {
            unreachable!("only async work can lose an admission race")
        };
        match error {
            Some(error) => GpuBackendEnqueueError::Rejected(error, submission),
            None => GpuBackendEnqueueError::Busy(submission),
        }
    }
}

struct PendingState {
    queue: PendingQueue<PendingSubmission>,
    failure: Option<GpuBackendSubmitError>,
}

/// Admission remains reserved after the checkpoint until the generic layer has
/// finished its CPU copy, cache maintenance, and backend callback.
struct A618CpuAccess {
    core: Arc<A618Core>,
}

impl GpuBackendCpuAccessGuard for A618CpuAccess {}

impl Drop for A618CpuAccess {
    fn drop(&mut self) {
        self.core.pending.lock().queue.end_cpu_access();
        self.core.capacity_waker.wake_all();
    }
}

static WORKER_DEVICES: IrqSpinLock<Vec<Weak<A618Core>>> = IrqSpinLock::new(Vec::new());
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static WORKER_WAKER: Waker = Waker::new_uninterruptible("a618-worker");

fn wait_for_progress(waker: &Waker) {
    if let Some(task) = scarlet::task::mytask() {
        let _ = waker.wait_with_timeout(
            task.get_id(),
            task.get_trapframe(),
            Some(PROGRESS_INTERVAL_NS),
        );
    } else {
        time::udelay(10);
    }
}

fn register_worker(core: &Arc<A618Core>) -> Result<(), &'static str> {
    let weak = Arc::downgrade(core);
    {
        let mut devices = WORKER_DEVICES.lock();
        if !devices.iter().any(|device| Weak::ptr_eq(device, &weak)) {
            devices
                .try_reserve(1)
                .map_err(|_| "qcom-adreno-a618: worker registry allocation failed")?;
            devices.push(weak);
        }
    }
    if WORKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let task = scarlet::task::new_kernel_task(String::from("a618-submit"), 1, worker_entry);
        task.init();
        scarlet::sched::scheduler::add_task(task, arch::get_cpu().get_cpuid());
    }
    Ok(())
}

fn worker_entry() {
    loop {
        let device_count = {
            let mut devices = WORKER_DEVICES.lock();
            devices.retain(|device| device.strong_count() != 0);
            devices.len()
        };
        let mut progressed = false;
        for index in 0..device_count {
            // Only this worker removes entries; registration appends them.
            // Clone one weak owner without allocating a registry snapshot.
            let device = WORKER_DEVICES.lock().get(index).cloned();
            let completed = if let Some(core) = device.and_then(|device| device.upgrade()) {
                core.process_submission()
            } else {
                false
            };
            progressed |= completed;
            if completed {
                reschedule_between_submissions();
            }
        }
        if !progressed {
            if let Some(task) = scarlet::task::mytask() {
                // The waker latches notifications racing this idle transition.
                WORKER_WAKER.wait(task.get_id(), task.get_trapframe());
            } else {
                time::udelay(10);
            }
        }
    }
}

fn reschedule_between_submissions() {
    // AArch64 defers timer/IPI preemption of a non-idle kernel task. Honor
    // that request between jobs, after process_submission released every GPU
    // lock, retired resources and notified observers. This never sleeps and
    // leaves the request pending if the context is not schedulable.
    if let Some(task) = scarlet::task::mytask() {
        let cpu = arch::get_cpu().get_cpuid();
        if scarlet::sched::scheduler::may_schedule_from_interrupt(cpu)
            && scarlet::sched::scheduler::take_deferred_reschedule(cpu)
        {
            scarlet::sched::scheduler::schedule(task.get_trapframe());
        }
    }
}

struct A618Core {
    registers: DwordRegisters,
    register_base: usize,
    interrupt_id: scarlet::interrupt::InterruptId,
    interrupt_snapshot: IrqSpinLock<RingInterruptSnapshot>,
    interrupt_waker: Arc<Waker>,
    interrupt_wait_armed: AtomicBool,
    dma_context: DmaContext,
    interconnect: GpuMemoryPath,
    peak_kbps: u32,
    gmu: Arc<Mutex<A618Gmu>>,
    hardware: Mutex<HardwareState>,
    resource_mapping_gate: Arc<MappingGate>,
    shader_pack_address: AtomicU64,
    // Last firmware-ACKed OPP, zero while unavailable/lost. Telemetry must not
    // acquire the hardware mutex held through a complete GPU fence interval.
    acknowledged_frequency_khz: AtomicU64,
    pending: Mutex<PendingState>,
    capacity_waker: Waker,
    resources: IrqSpinLock<Vec<Arc<ResourceEntry>>>,
    next_resource_token: AtomicU64,
    backend_cookie: u64,
    frequency_policy_registered: Mutex<bool>,
}

impl Drop for A618Core {
    fn drop(&mut self) {
        self.acknowledged_frequency_khz.store(0, Ordering::Release);
        let registers = self.registers;
        let hardware = self.hardware.get_mut();
        if hardware.quiesce_failed {
            Self::fail_stop_teardown("a prior GPU/GMU quiesce timed out");
        }
        // GX MMIO is unsafe while the rail is off. If OOB is held, serialize
        // teardown and quiesce the SQE before releasing power and DMA memory.
        if hardware.gpu_oob_held {
            if Self::force_stop_gpu(registers, hardware).is_err() {
                Self::fail_stop_teardown("GPU bus drain timed out");
            }
            let mut gmu = self.gmu.lock();
            if gmu.force_shutdown().is_err() {
                Self::fail_stop_teardown("GMU power-off timed out");
            }
            hardware.gpu_oob_held = false;
        }
        vm::iounmap(self.register_base);
    }
}

struct A618FrequencyDriver(Weak<A618Core>);

impl scarlet::device::devfreq::DeviceFrequencyDriver for A618FrequencyDriver {
    fn current_frequency_khz(&self) -> Result<u64, &'static str> {
        let core = self.0.upgrade().ok_or("qcom-adreno-a618: GPU removed")?;
        let frequency = core.acknowledged_frequency_khz.load(Ordering::Acquire);
        (frequency != 0).then_some(frequency)
            .ok_or("qcom-adreno-a618: GPU frequency is unavailable")
    }

    fn sample_utilization(
        &self,
    ) -> Result<scarlet::device::devfreq::DeviceFrequencyUtilization, &'static str> {
        let core = self.0.upgrade().ok_or("qcom-adreno-a618: GPU removed")?;
        core.gmu.lock().sample_utilization()
    }

    fn set_frequency_khz(&self, frequency: u64) -> Result<(), &'static str> {
        let core = self.0.upgrade().ok_or("qcom-adreno-a618: GPU removed")?;
        // The same locks cover the complete submit/fence interval. DCVS runs
        // between submissions, while the GMU owns voltage ordering and ACKs.
        let _mapping_gate = core.resource_mapping_gate.execution.lock();
        let mut hardware = core.hardware.lock();
        if hardware.boot != BootState::Ready {
            return Err("qcom-adreno-a618: GPU is not ready");
        }
        let result = core.gmu.lock().set_frequency_khz(frequency);
        if result.is_ok() {
            // The GMU method returns success only after the exact OPP's ACK.
            core.acknowledged_frequency_khz.store(frequency, Ordering::Release);
        } else if let Err(reason) = result {
            // A timed-out vote can have reached firmware. Do not continue
            // submitting work at an unknown voltage/frequency state.
            core.quiesce_lost(&mut hardware, reason);
        }
        result
    }
}

impl A618Core {
    fn register_frequency_policy(self: &Arc<Self>) {
        let mut registered = self.frequency_policy_registered.lock();
        if *registered {
            return;
        }
        let frequencies = match self.gmu.lock().frequencies_khz() {
            Ok(frequencies) => frequencies,
            Err(_) => return,
        };
        match scarlet::device::devfreq::register_firmware_managed(
            "gpu",
            &frequencies,
            Arc::new(A618FrequencyDriver(Arc::downgrade(self))),
        ) {
            Ok(()) => {
                *registered = true;
                println!(
                    "[qcom-adreno-a618] registered GPU devfreq policy (GMU-acknowledged OPPs)"
                );
                // Match Linux MSM's interactive GPU thresholds: vblank can
                // leave a double-buffered workload only 50% busy even when
                // more GPU performance is needed. Keep the common policy's
                // downscale hysteresis and firmware-acknowledged transitions.
                // Registration retains the boot OPP until valid samples can
                // drive ondemand; a failed capability probe leaves userspace
                // at that same rate, without inventing an activity reading.
                let automatic = scarlet::device::devfreq::configure_simple_ondemand(
                    "gpu",
                    scarlet::device::devfreq::SimpleOndemandConfig {
                        upthreshold_pct: 50,
                        downdifferential_pct: 10,
                    },
                )
                .and_then(|()| {
                    scarlet::device::devfreq::set_governor(
                        "gpu",
                        scarlet::device::devfreq::DeviceFrequencyGovernor::SimpleOndemand,
                    )
                });
                match automatic {
                    Ok(()) => println!(
                        "[qcom-adreno-a618] GPU governor=simple_ondemand up=50 down-differential=10"
                    ),
                    Err(reason) => println!(
                        "[qcom-adreno-a618] keeping boot GPU frequency policy: {}",
                        reason
                    ),
                }
            }
            Err(reason) => println!("[qcom-adreno-a618] devfreq registration failed: {}", reason),
        }
    }

    fn process_submission(&self) -> bool {
        let next = {
            let mut pending = self.pending.lock();
            pending.queue.start_next().map(|submission| {
                let queued = pending.queue.queued_count_where(|entry| !entry.prepared.words.is_empty());
                (submission, pending.failure, queued)
            })
        };
        let Some((mut submission, previous_failure, queued_gpu_jobs)) = next else {
            // Controllers can serve other devices which cannot ask our LRU to
            // reclaim banks. Never pin idle leases beyond an accepted burst.
            if let Err(error) = self.release_idle_leases() {
                self.pending.lock().failure.get_or_insert(GpuBackendSubmitError::DeviceLost(error));
                self.capacity_waker.wake_all();
            }
            return false;
        };
        // Real GPU work requests frequency headroom before lifecycle/mapping/
        // hardware/GMU locks. Empty CPU checkpoints need no activity boost.
        // Keep this guard outside submit_words' complete DMA retirement proof.
        let frequency_policy_registered = *self.frequency_policy_registered.lock();
        let activity = if previous_failure.is_none()
            && !submission.prepared.words.is_empty()
            && frequency_policy_registered
        {
            scarlet::device::devfreq::acquire_adaptive_activity("gpu", queued_gpu_jobs).map(Some)
        } else {
            Ok(None)
        };
        let (mut activity, activity_error) = match activity {
            Ok(guard) => (guard, None),
            Err(reason) => (None, Some(GpuBackendSubmitError::DeviceLost(reason))),
        };
        let mut work_sample = None;
        let result = match previous_failure.or(activity_error) {
            Some(error) => Err(error),
            None => self
                .ensure_hardware_ready()
                .and_then(|()| self.submit_words(&submission.prepared, &mut work_sample)),
        };
        let quarantined = self
            .resource_mapping_gate
            .quarantine
            .load(Ordering::Acquire);
        if let Err(error) = result {
            self.pending.lock().failure.get_or_insert(error);
        }
        // Never run resource destructors under the hardware, mapping, context,
        // or pending queue lock: they can acquire the mapping gate themselves.
        if quarantined {
            if let SubmissionObserver::Async(request) = &mut submission.observer {
                request.fail(GpuCompletionFailure::DeviceLost);
            }
            // There is no retirement proof. Keep command storage, mapping pins,
            // the generic backing snapshot, and capacity indefinitely. A sync
            // caller must remain asleep because its backing lives on that path.
            core::mem::forget(submission);
            core::mem::forget(activity);
        } else {
            let PendingSubmission {
                prepared,
                observer,
                _core,
            } = submission;
            drop(prepared);
            // submit_words returned only after fence/idle retirement or proven
            // quiescence. No execution or mapping locks remain held here.
            if result.is_ok() {
                if let (Some(guard), Some(sample)) = (activity.as_mut(), work_sample) {
                    guard.record_work(sample);
                }
            }
            drop(activity);
            match observer {
                SubmissionObserver::Async(request) => match result {
                    Ok(()) => request.complete(),
                    Err(GpuBackendSubmitError::DeviceLost(_)) => {
                        request.retire_failed(GpuCompletionFailure::DeviceLost)
                    }
                    Err(_) => request.retire_failed(GpuCompletionFailure::Execution),
                },
                SubmissionObserver::Sync(observer) => observer.complete(result),
            }
            drop(_core);
        }
        self.pending.lock().queue.finish(!quarantined);
        self.capacity_waker.wake_all();
        true
    }

    fn submit_prepared_sync(
        self: &Arc<Self>,
        prepared: PreparedSubmission,
        cpu_access_owner: bool,
    ) -> Result<(), GpuBackendSubmitError> {
        let result = Arc::new(SyncResult::new());
        let mut submission = PendingSubmission {
            prepared,
            observer: SubmissionObserver::Sync(Arc::clone(&result)),
            _core: Arc::clone(self),
        };
        loop {
            {
                let mut pending = self.pending.lock();
                if let Some(error) = pending.failure {
                    return Err(error);
                }
                let admission = if cpu_access_owner {
                    pending.queue.push_cpu_checkpoint(submission)
                } else {
                    pending.queue.push(submission)
                };
                match admission {
                    Ok(()) => break,
                    Err(returned) => submission = returned,
                }
            }
            wait_for_progress(&self.capacity_waker);
        }
        WORKER_WAKER.wake_one();
        result.wait()
    }

    fn begin_cpu_access(self: &Arc<Self>) -> Result<A618CpuAccess, GpuBackendSubmitError> {
        loop {
            {
                let mut pending = self.pending.lock();
                if let Some(error) = pending.failure {
                    return Err(error);
                }
                if pending.queue.try_begin_cpu_access() {
                    break;
                }
            }
            wait_for_progress(&self.capacity_waker);
        }
        // Construct the guard before fallible checkpoint admission so every
        // error clears the reservation. Only this checkpoint bypasses its own
        // gate; all ordinary sync/async submissions wait or return Busy.
        let guard = A618CpuAccess {
            core: Arc::clone(self),
        };
        self.submit_prepared_sync(
            PreparedSubmission {
                words: Vec::new(),
                isolated_ranges: None,
                _resources: Arc::new(Vec::new()),
                client_vm: None,
            },
            true,
        )?;
        Ok(guard)
    }

    fn ensure_shader_pack(&self) -> Result<(), &'static str> {
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        if self
            .resource_mapping_gate
            .quarantine
            .load(Ordering::Acquire)
        {
            return Err("qcom-adreno-a618: GPU mappings are quarantined");
        }
        let mut hardware = self.hardware.lock();
        if hardware.shader_pack.is_some() {
            return Ok(());
        }
        let mut pack = DmaAllocation::new(&self.dma_context, PACK_SIZE, IommuMapFlags::READ)?;
        if pack.dma_addr() & (SHADER_ALIGNMENT as u64 - 1) != 0 || !copy_pack(pack.as_bytes_mut()) {
            return Err("qcom-adreno-a618: canonical shader pack layout is invalid");
        }
        pack.clean_for_device();
        self.shader_pack_address
            .store(pack.dma_addr(), Ordering::Release);
        hardware.shader_pack = Some(pack);
        Ok(())
    }

    fn shader_pack_base(&self) -> Option<u64> {
        let address = self.shader_pack_address.load(Ordering::Acquire);
        (address != 0).then_some(address)
    }

    fn resolve_shader_at(pack_base: u64, variant: ShaderVariant) -> Option<ResolvedResource> {
        let offset = variant.offset();
        let end = offset.checked_add(variant.size())?;
        if end > PACK_SIZE {
            return None;
        }
        Some(ResolvedResource {
            attachment_token: 0,
            gpu_va: pack_base.checked_add(offset as u64)?,
            allocation_size: variant.size() as u64,
            allowed_access: adreno_a6xx_submit_wire::ACCESS_READ,
            linear_image: None,
        })
    }

    fn fail_stop_teardown(reason: &'static str) -> ! {
        println!(
            "[qcom-adreno-a618] refusing unsafe DMA/MMIO teardown: {}",
            reason
        );
        loop {
            time::udelay(1_000_000);
        }
    }

    fn poll_gpu(
        registers: DwordRegisters,
        register: usize,
        predicate: impl Fn(u32) -> bool,
    ) -> bool {
        let start = time::current_time();
        loop {
            if predicate(registers.read(register)) {
                return true;
            }
            if time::current_time().saturating_sub(start) >= GPU_QUIESCE_TIMEOUT_US {
                return false;
            }
            time::udelay(10);
        }
    }

    /// Stop command fetch and drain/halt A618 GBIF before power or DMA teardown.
    fn force_stop_gpu(
        registers: DwordRegisters,
        hardware: &HardwareState,
    ) -> Result<(), &'static str> {
        registers.write(CP_SQE_CNTL, 3);
        registers.write(RBBM_INT_0_MASK, 0);
        registers.write(RBBM_INT_CLEAR_CMD, u32::MAX);
        arch::io_wmb();
        // Best-effort bounded drain: a hung ring is expected on the fault path,
        // so the authoritative DMA stop is the subsequent GBIF halt/reset.
        let _ = Self::poll_gpu(registers, CP_RB_RPTR, |rptr| rptr == hardware.wptr);

        registers.write(RBBM_GBIF_HALT, 1);
        let gx_halted = Self::poll_gpu(registers, RBBM_GBIF_HALT_ACK, |value| value & 1 != 0);
        registers.write(GBIF_HALT, 1);
        let clients_halted = Self::poll_gpu(registers, GBIF_HALT_ACK, |value| value & 1 != 0);
        registers.write(GBIF_HALT, 2);
        let axi_halted = Self::poll_gpu(registers, GBIF_HALT_ACK, |value| value & 2 != 0);
        registers.write(GBIF_HALT, 0);
        arch::io_wmb();

        registers.write(RBBM_SW_RESET_CMD, 1);
        let _ = registers.read(RBBM_SW_RESET_CMD);
        time::udelay(100);
        arch::io_wmb();
        if gx_halted && clients_halted && axi_halted {
            Ok(())
        } else {
            Err("qcom-adreno-a618: GPU bus drain timed out")
        }
    }

    fn allocate_resource_token(&self) -> Result<u64, &'static str> {
        allocate_monotonic(
            &self.next_resource_token,
            "qcom-adreno-a618: resource token space exhausted",
        )
    }

    fn register_resource(&self, entry: Arc<ResourceEntry>) -> Result<(), &'static str> {
        let mut resources = self.resources.lock();
        resources
            .try_reserve(1)
            .map_err(|_| "qcom-adreno-a618: resource registry allocation failed")?;
        resources.push(entry);
        Ok(())
    }

    fn unregister_resource(&self, token: u64) {
        let mut resources = self.resources.lock();
        let removed = resources
            .iter()
            .position(|entry| entry.token == token)
            .map(|index| resources.swap_remove(index));
        drop(resources);
        drop(removed);
    }

    fn resource(&self, token: u64) -> Option<Arc<ResourceEntry>> {
        self.resources
            .lock()
            .iter()
            .find(|entry| entry.token == token)
            .cloned()
    }

    fn map_backing(
        &self,
        paddr: u64,
        allocation_size: u64,
    ) -> Result<(u64, DmaMapping, Vec<isolation::PhysicalSpan>), &'static str> {
        let allocation_size = usize::try_from(allocation_size)
            .map_err(|_| "qcom-adreno-a618: resource allocation exceeds kernel address size")?;
        if paddr == 0 || allocation_size == 0 || paddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err("qcom-adreno-a618: resource backing is invalid");
        }
        let mut spans = Vec::new();
        spans.try_reserve_exact(1).map_err(|_| "a618: physical metadata allocation failed")?;
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        if self
            .resource_mapping_gate
            .quarantine
            .load(Ordering::Acquire)
        {
            return Err("qcom-adreno-a618: GPU mappings are quarantined");
        }
        let mapping = self
            .dma_context
            // A618 advertises cached-coherent system memory. External CPU-mapped
            // resources therefore use coherent SMMU PTEs; transient command
            // allocations remain explicitly cache-maintained instead.
            .map_phys_owned(
                PhysAddr::new(paddr),
                allocation_size,
                IommuMapFlags::READ | IommuMapFlags::WRITE | IommuMapFlags::COHERENT,
            )
            .map_err(|_| "qcom-adreno-a618: resource IOMMU mapping failed")?;
        let iova = mapping.dma_addr().as_u64();
        spans.push(isolation::PhysicalSpan { iova, physical: paddr, size: allocation_size as u64 });
        Ok((iova, mapping, spans))
    }

    fn map_image_backing(
        &self,
        backing: &GpuImageBackingInfo,
    ) -> Result<(u64, DmaMapping, Vec<isolation::PhysicalSpan>), &'static str> {
        if backing.is_physically_contiguous() {
            return self.map_backing(backing.paddr, backing.allocation_size);
        }
        let allocation_size = usize::try_from(backing.allocation_size)
            .map_err(|_| "qcom-adreno-a618: image allocation exceeds kernel address size")?;
        let mut segments = Vec::new();
        let mut spans = Vec::new();
        spans.try_reserve_exact(backing.physical_segments().len())
            .map_err(|_| "a618: physical metadata allocation failed")?;
        segments
            .try_reserve_exact(backing.physical_segments().len())
            .map_err(|_| "qcom-adreno-a618: image segment metadata allocation failed")?;
        let mut total_size = 0usize;
        for segment in backing.physical_segments().iter().copied() {
            if segment.physical_addr() == 0
                || segment.physical_addr() & (PAGE_SIZE as u64 - 1) != 0
                || segment.length() == 0
                || segment.length() & (PAGE_SIZE - 1) != 0
            {
                return Err("qcom-adreno-a618: image backing segment is invalid");
            }
            total_size = total_size
                .checked_add(segment.length())
                .ok_or("qcom-adreno-a618: image backing size overflows")?;
            segments.push((PhysAddr::new(segment.physical_addr()), segment.length()));
        }
        if total_size < allocation_size {
            return Err("qcom-adreno-a618: image backing segments are undersized");
        }
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        if self
            .resource_mapping_gate
            .quarantine
            .load(Ordering::Acquire)
        {
            return Err("qcom-adreno-a618: GPU mappings are quarantined");
        }
        let mapping = self
            .dma_context
            .map_phys_segments_owned(
                &segments,
                IommuMapFlags::READ | IommuMapFlags::WRITE | IommuMapFlags::COHERENT,
            )
            .map_err(|_| "qcom-adreno-a618: segmented image IOMMU mapping failed")?;
        let iova = mapping.dma_addr().as_u64();
        let mut offset = 0;
        for (physical, len) in segments {
            spans.push(isolation::PhysicalSpan { iova: iova + offset, physical: physical.as_u64(), size: len as u64 });
            offset += len as u64;
        }
        Ok((iova, mapping, spans))
    }

    fn initialize_nonprivileged_mmio(&self, hardware: &HardwareState) {
        let registers = self.registers;
        registers.write(RBBM_INT_0_MASK, 0);
        registers.write(RBBM_INT_CLEAR_CMD, u32::MAX);
        arch::io_wmb();
        // Enable the same completion and fault sources as the registered
        // platform IRQ. The hard-IRQ path snapshots dependent CP diagnostics
        // before acknowledging this shared RBBM status word.
        registers.write(RBBM_INT_0_MASK, RBBM_INT_POLL_MASK);
        registers.write(RBBM_SECVID_TSB_CNTL, 0);
        registers.write64(RBBM_SECVID_TSB_TRUSTED_BASE, 0);
        registers.write(RBBM_SECVID_TSB_TRUSTED_SIZE, 0);

        for register in [
            CP_ADDR_MODE_CNTL,
            VSC_ADDR_MODE_CNTL,
            GRAS_ADDR_MODE_CNTL,
            RB_ADDR_MODE_CNTL,
            PC_ADDR_MODE_CNTL,
            HLSQ_ADDR_MODE_CNTL,
            VFD_ADDR_MODE_CNTL,
            VPC_ADDR_MODE_CNTL,
            UCHE_ADDR_MODE_CNTL,
            SP_ADDR_MODE_CNTL,
            TPL1_ADDR_MODE_CNTL,
            RBBM_SECVID_TSB_ADDR_MODE_CNTL,
        ] {
            registers.write(register, 1);
        }

        // A618 context/errata state from the same pinned Freedreno device
        // description used to build the canonical IR3 shader pack.  Linux
        // leaves these non-privileged registers to userspace; Scarlet owns
        // both sides of this backend, so establish them before CP protection
        // and before the first command buffer can execute.
        for (register, value) in [
            (UCHE_UNKNOWN_0E12, 0x0000_0001),
            (GRAS_SC_CNTL, 0x0000_0002),
            (GRAS_DBG_ECO_CNTL, 0x0000_0880),
            (RB_MODE_CNTL, 0x0000_0010),
            (RB_RBP_CNTL, 0x0000_0001),
            (RB_DBG_ECO_CNTL, 0x0410_0000),
            (VPC_DBG_ECO_CNTL, 0x0000_0000),
            (PC_MODE_CNTL, 0x0000_001f),
            (PC_POWER_CNTL, 0x0000_0000),
            (VFD_MODE_CNTL, 0x0000_0003),
            (VFD_POWER_CNTL, 0x0000_0000),
            (SP_GFX_USIZE, 0x0000_0000),
            (SP_DBG_ECO_CNTL, 0x0000_0000),
            (SP_CHICKEN_BITS, 0x0000_0430),
            (SP_PERFCTR_SHADER_MASK, 0x0000_003f),
            (TPL1_DBG_ECO_CNTL, 0x0010_8000),
            (TPL1_UNKNOWN_B605, 0x0000_0044),
            (HLSQ_SHARED_CONSTS, 0x0000_0000),
            (HLSQ_UNKNOWN_BE00, 0x0000_0080),
            (HLSQ_UNKNOWN_BE01, 0x0000_0000),
            (HLSQ_DBG_ECO_CNTL, 0x0008_0000),
        ] {
            registers.write(register, value);
        }
        registers.write(RBBM_VBIF_CLIENT_QOS_CNTL, 3);
        registers.write(RBBM_PERFCTR_GPU_BUSY_MASKED, u32::MAX);
        registers.write64(UCHE_WRITE_RANGE_MAX, UCHE_CACHED_WRITE_RANGE_MAX);
        registers.write64(UCHE_TRAP_BASE, UCHE_UNMAPPED_TRAP_BASE);
        registers.write64(UCHE_WRITE_THRU_BASE, UCHE_UNMAPPED_TRAP_BASE);
        registers.write64(UCHE_GMEM_RANGE_MIN, GMEM_BASE);
        registers.write64(UCHE_GMEM_RANGE_MAX, GMEM_BASE + GMEM_SIZE - 1);
        registers.write(UCHE_FILTER_CNTL, 0x804);
        registers.write(UCHE_CACHE_WAYS, 4);
        registers.write(CP_ROQ_THRESHOLDS_2, 0x0100_00c0);
        registers.write(CP_ROQ_THRESHOLDS_1, 0x8040_362c);
        registers.write(CP_MEM_POOL_SIZE, 128);
        registers.write(PC_DBG_ECO_CNTL, 0x0018_0000);
        registers.write(CP_AHB_CNTL, 1);
        registers.write(RBBM_PERFCTR_CNTL, 1);
        registers.write(CP_PERFCTR_CP_SEL_0, 0);
        registers.write(RBBM_INTERFACE_HANG_INT_CNTL, (1 << 30) | 0x1f_ffff);
        registers.write(UCHE_CLIENT_PF, 1);
        registers.write(CP_PROTECT_CNTL, 1 | (1 << 1) | (1 << 3));
        for (index, value) in CP_PROTECT.iter().enumerate() {
            registers.write(CP_PROTECT_BASE + index, *value);
        }
        if let Some(sqe) = &hardware.sqe {
            registers.write64(CP_SQE_INSTR_BASE, sqe.dma_addr());
        }
        registers.write64(CP_RB_BASE, hardware.ring.dma_addr());
        // Linux's 32 KiB ring and 32-byte block default, with RPTR shadow
        // updates disabled because this backend has no privileged shadow page.
        registers.write(CP_RB_CNTL, 12 | (2 << 8) | (1 << 27));
        registers.write(CP_RB_WPTR, 0);
        arch::io_wmb();
        println!(
            "[qcom-adreno-a618] UCHE range-max={:#014x} trap={:#014x} write-through={:#014x} client-pf={:#010x}",
            registers.read64(UCHE_WRITE_RANGE_MAX),
            registers.read64(UCHE_TRAP_BASE),
            registers.read64(UCHE_WRITE_THRU_BASE),
            registers.read(UCHE_CLIENT_PF),
        );
    }

    fn coachz_no_zap_configuration() -> bool {
        scarlet::device::fdt::FdtManager::get_manager()
            .get_fdt()
            .is_some_and(|fdt| fdt.root().model().starts_with("Google CoachZ"))
    }

    fn sqe_version_is_secure(instructions: &[u8]) -> bool {
        if instructions.len() < 12 || instructions.len() & 3 != 0 {
            return false;
        }
        let Some(version) = instructions
            .get(..4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
        else {
            return false;
        };
        let patchlevel = instructions
            .get(8..12)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0);
        ((version & 0xf) == 0xa && (patchlevel & 0xf) >= 1) || (version & 0xfff) >= 0x190
    }

    fn write_ring(&self, hardware: &mut HardwareState, words: &[u32]) -> Result<u32, &'static str> {
        let ring_words = hardware.ring.as_words_mut();
        let capacity = ring_words.len();
        if capacity == 0 || !capacity.is_power_of_two() || words.len() >= capacity {
            return Err("qcom-adreno-a618: kernel ring command is too large");
        }
        let dirty = ring_cache::dirty_ranges(hardware.wptr as usize, words.len(), capacity)
            .ok_or("qcom-adreno-a618: invalid ring cache range")?;
        for word in words {
            ring_words[hardware.wptr as usize] = *word;
            hardware.wptr = (hardware.wptr + 1) & (capacity as u32 - 1);
        }
        // The private-domain entry scrub already cleans the entire ring. Only
        // these newly written slots are dirty now (also true on the fixed path).
        for (offset, len) in dirty {
            if len != 0 {
                hardware.ring.clean_range_for_device(offset, len)?;
            }
        }
        self.clear_ring_interrupts()?;
        self.registers.write(CP_RB_WPTR, hardware.wptr);
        Ok(hardware.wptr)
    }

    /// Establish an unambiguous completion edge before advancing the ring.
    ///
    /// The userspace validator forbids IRQ-requesting CACHE_FLUSH_TS, so bit
    /// 20 can only be raised by the kernel-owned completion event appended by
    /// `execute_ring`. Native jobs may clean UCHE into a user sequence word
    /// with TIMESTAMP only; this cannot request the completion interrupt.
    /// Requiring a zero readback here makes the next observed bit fresh even
    /// on systems where the event's optional memory payload is unreliable.
    fn clear_ring_interrupts(&self) -> Result<(), &'static str> {
        let start = time::current_time();
        loop {
            let pending = self.registers.read(RBBM_INT_0_STATUS);
            if pending == 0 {
                return Ok(());
            }
            if pending & RBBM_INT_FATAL_MASK != 0 {
                return Err("qcom-adreno-a618: GPU fault pending before ring submit");
            }
            self.registers.write(RBBM_INT_CLEAR_CMD, pending);
            arch::io_wmb();
            if time::current_time().saturating_sub(start) >= GPU_INTERRUPT_CLEAR_TIMEOUT_US {
                return Err("qcom-adreno-a618: stale GPU interrupt did not clear");
            }
            time::udelay(1);
        }
    }

    /// Consume the shared A6xx interrupt status without confusing timestamp
    /// completion events with execution faults.
    ///
    /// Linux uses `CP_CACHE_FLUSH_TS` to retire fences from this same register;
    /// the status being non-zero is therefore not itself a device-loss signal.
    fn consume_ring_interrupts(&self) -> Result<RingInterruptSnapshot, RingInterruptSnapshot> {
        let rbbm = self.registers.read(RBBM_INT_0_STATUS);
        // CP_INTERRUPT_STATUS is no longer stable after the shared RBBM source
        // is acknowledged. Capture its dependent diagnostics before that W1C;
        // a second status read in the failure path reports a false zero.
        let cp = self.registers.read(CP_INTERRUPT_STATUS);
        let cp_hw_fault = if cp & CP_INT_HW_FAULT_ERROR != 0 {
            self.registers.read(CP_HW_FAULT)
        } else {
            0
        };
        let cp_protect_status = if cp & CP_INT_REGISTER_PROTECTION_ERROR != 0 {
            self.registers.read(CP_PROTECT_STATUS)
        } else {
            0
        };
        let cp_opcode = if cp & CP_INT_OPCODE_ERROR != 0 {
            self.registers.write(CP_SQE_STAT_ADDR, 1);
            arch::io_wmb();
            self.registers.read(CP_SQE_STAT_DATA)
        } else {
            0
        };
        if rbbm != 0 {
            self.registers.write(RBBM_INT_CLEAR_CMD, rbbm);
            // Commit the W1C before a following submit can report another
            // completion or fault through the same shared status register.
            arch::io_wmb();
        }
        let snapshot = RingInterruptSnapshot {
            rbbm,
            cp,
            cp_hw_fault,
            cp_protect_status,
            cp_opcode,
        };
        if snapshot.is_fatal() {
            Err(snapshot)
        } else {
            Ok(snapshot)
        }
    }

    fn take_interrupt_snapshot(&self) -> RingInterruptSnapshot {
        let mut pending = self.interrupt_snapshot.lock();
        let snapshot = *pending;
        *pending = RingInterruptSnapshot::default();
        snapshot
    }

    fn collect_polled_interrupts(&self) -> Result<RingInterruptSnapshot, RingInterruptSnapshot> {
        let mut combined = self.take_interrupt_snapshot();
        match self.consume_ring_interrupts() {
            Ok(snapshot) => combined.merge(snapshot),
            Err(snapshot) => combined.merge(snapshot),
        }
        if combined.is_fatal() {
            Err(combined)
        } else {
            Ok(combined)
        }
    }

    fn capture_ring_failure(
        &self,
        hardware: &HardwareState,
        reason: &'static str,
        sequence: u32,
        command_words: usize,
        target_wptr: u32,
        interrupts: RingInterruptSnapshot,
        fence: Option<(u32, u32)>,
    ) -> RingFailureSnapshot {
        let ib1_base = self.registers.read64(CP_IB1_BASE);
        // Capture the route used by this execution BEFORE quiesce/restore.
        // Base-domain translations alone cannot diagnose a private-domain DMA
        // write. These reads never authorize completion or change GPU state.
        let (active_ib1_translation, active_fence_translation, active_iommu_fault) =
            if let Some(owner) = &hardware.execution_domain {
                let domain = owner.diagnostic_domain();
                (
                    domain.translate(ib1_base),
                    domain.translate(hardware.fence.dma_addr()),
                    domain.fault_snapshot(),
                )
            } else if let Some(attachment) = &self.dma_context.iommu {
                let domain = IsolationDomainAdapter {
                    domain: Arc::clone(&attachment.domain),
                    streams: Vec::new(),
                };
                use isolation::Domain;
                (
                    domain.translate(ib1_base),
                    domain.translate(hardware.fence.dma_addr()),
                    domain.fault_snapshot(),
                )
            } else {
                (None, None, None)
            };
        let capacity = hardware.ring.requested_size() / core::mem::size_of::<u32>();
        // Failure-only reads: all CPU stores to these control pages were
        // cleaned before publication. Do not clean here and overwrite DMA.
        hardware.ring.invalidate_from_device();
        hardware.fence.invalidate_from_device();
        // Only execute_ring appends this seven-word tail. ME initialization
        // failures have no fence and must not label arbitrary ring data as one.
        let completion_tail = (fence.is_some() && capacity >= 7 && capacity.is_power_of_two())
            .then(|| core::array::from_fn(|index| {
                let slot = (target_wptr as usize).wrapping_sub(7).wrapping_add(index)
                    & (capacity - 1);
                hardware.ring.read_word_volatile(slot).unwrap_or(0)
            }));
        RingFailureSnapshot {
            reason,
            sequence,
            command_words,
            interrupt: interrupts.rbbm,
            rptr: self.registers.read(CP_RB_RPTR),
            wptr: self.registers.read(CP_RB_WPTR),
            target_wptr,
            status: self.registers.read(RBBM_STATUS),
            status1: self.registers.read(RBBM_STATUS1),
            status2: self.registers.read(RBBM_STATUS2),
            status3: self.registers.read(RBBM_STATUS3),
            cp_interrupt: interrupts.cp,
            cp_hw_fault: interrupts.cp_hw_fault,
            cp_protect_status: interrupts.cp_protect_status,
            cp_opcode: interrupts.cp_opcode,
            ib1_base,
            ib1_translation: self
                .dma_context
                .iommu
                .as_ref()
                .and_then(|attachment| attachment.domain.iova_to_phys(Iova::new(ib1_base)))
                .map(PhysAddr::as_u64),
            ib1_remaining: self.registers.read(CP_IB1_REM_SIZE),
            ib2_base: self.registers.read64(CP_IB2_BASE),
            ib2_remaining: self.registers.read(CP_IB2_REM_SIZE),
            roq_rb: self.registers.read(CP_ROQ_RB_STATUS),
            roq_ib1: self.registers.read(CP_ROQ_IB1_STATUS),
            roq_ib2: self.registers.read(CP_ROQ_IB2_STATUS),
            roq_sds: self.registers.read(CP_ROQ_SDS_STATUS),
            roq_mrb: self.registers.read(CP_ROQ_MRB_STATUS),
            roq_vsd: self.registers.read(CP_ROQ_VSD_STATUS),
            scratch2: self.registers.read(CP_SCRATCH_2),
            fence_dma: hardware.fence.dma_addr(),
            fence_paddr: hardware.fence.paddr(),
            fence_translation: self
                .dma_context
                .iommu
                .as_ref()
                .and_then(|attachment| {
                    attachment
                        .domain
                        .iova_to_phys(Iova::new(hardware.fence.dma_addr()))
                })
                .map(PhysAddr::as_u64),
            fence,
            private_domain: hardware.execution_domain.is_some(),
            active_ib1_translation,
            active_fence_translation,
            active_iommu_fault,
            fence_head: core::array::from_fn(|index| {
                hardware.fence.read_word_volatile(index).unwrap_or(0)
            }),
            completion_tail,
        }
    }

    fn print_ring_failure(snapshot: RingFailureSnapshot) {
        println!("[a618] ring failure={}", snapshot.reason);
        println!(
            "[a618] submit seq={} words={}",
            snapshot.sequence, snapshot.command_words,
        );
        println!(
            "[a618] irq={:#010x} complete={:#010x}",
            snapshot.interrupt,
            snapshot.interrupt & RBBM_INT_COMPLETION_MASK,
        );
        println!(
            "[a618] fatal={:#010x} cp-int={:#010x}",
            snapshot.interrupt & RBBM_INT_FATAL_MASK,
            snapshot.cp_interrupt,
        );
        println!(
            "[a618] rb rptr={:#x} wptr={:#x} target={:#x}",
            snapshot.rptr, snapshot.wptr, snapshot.target_wptr,
        );
        println!(
            "[a618] status={:#010x} status1={:#010x} status2={:#010x} status3={:#010x}",
            snapshot.status, snapshot.status1, snapshot.status2, snapshot.status3,
        );
        if snapshot.status3 & RBBM_STATUS3_SMMU_STALLED_ON_FAULT != 0 {
            println!("[a618] SMMU stalled on fault");
        }
        if snapshot.cp_interrupt != 0 {
            println!(
                "[a618] cp fault={:#010x} protect={:#010x} opcode={:#010x}",
                snapshot.cp_hw_fault, snapshot.cp_protect_status, snapshot.cp_opcode,
            );
        }
        println!(
            "[a618] ib1={:#x} translated={:#x} rem={:#x}",
            snapshot.ib1_base,
            snapshot.ib1_translation.unwrap_or(0),
            snapshot.ib1_remaining,
        );
        println!(
            "[a618] ib2={:#x} rem={:#x}",
            snapshot.ib2_base, snapshot.ib2_remaining,
        );
        println!(
            "[a618] roq rb={:#010x} ib1={:#010x} ib2={:#010x}",
            snapshot.roq_rb, snapshot.roq_ib1, snapshot.roq_ib2,
        );
        println!(
            "[a618] roq sds={:#010x} mrb={:#010x} vsd={:#010x}",
            snapshot.roq_sds, snapshot.roq_mrb, snapshot.roq_vsd,
        );
        println!("[a618] scratch2={:#x}", snapshot.scratch2);
        println!(
            "[a618] fence dma={:#x} paddr={:#x} translated={:#x}",
            snapshot.fence_dma,
            snapshot.fence_paddr,
            snapshot.fence_translation.unwrap_or(0),
        );
        if let Some((actual, expected)) = snapshot.fence {
            println!("[a618] fence actual={:#x} expected={:#x}", actual, expected,);
        }
        println!(
            "[a618] active-domain private={} ib1-translation={:#x?} fence-translation={:#x?}",
            snapshot.private_domain, snapshot.active_ib1_translation,
            snapshot.active_fence_translation,
        );
        if let Some(fault) = snapshot.active_iommu_fault {
            println!(
                "[a618] active-smmu global={:#x} context={:#x} far={:#x} fsynr0={:#x} fsynr1={:#x}",
                fault.global_status, fault.context_status, fault.fault_address,
                fault.syndrome0, fault.syndrome1,
            );
        }
        println!("[a618] fence-head={:#x?} completion-tail={:#x?}",
            snapshot.fence_head, snapshot.completion_tail);
    }

    /// Failure-only correlation while the accepted resources and active route
    /// are still pinned. Software translations are evidence, not hardware ATOS
    /// results or permission/retirement proofs. Never clear the SMMU fault here.
    fn print_fault_authority(&self, hardware: &HardwareState, prepared: &PreparedSubmission) {
        let Some(fault) = hardware.last_ring_failure.and_then(|s| s.active_iommu_fault) else {
            return;
        };
        // FSR.FORMAT alone does not indicate a fault (ARM SMMUv2 / Linux FSR).
        if fault.context_status & 0xc000_01fe == 0 {
            return;
        }
        let far = fault.fault_address;
        println!(
            "[a618] fault-decode tf={} pf={} ef={} ss={} multi={} format={} write={} plvl={} ptwf={}",
            fault.context_status & (1 << 1) != 0,
            fault.context_status & (1 << 3) != 0,
            fault.context_status & (1 << 4) != 0,
            fault.context_status & (1 << 30) != 0,
            fault.context_status & (1 << 31) != 0,
            (fault.context_status >> 9) & 3,
            fault.syndrome0 & (1 << 4) != 0,
            fault.syndrome0 & 3,
            fault.syndrome0 & (1 << 10) != 0,
        );
        let base = self.dma_context.iommu.as_ref()
            .and_then(|a| a.domain.iova_to_phys(Iova::new(far)))
            .map(PhysAddr::as_u64);
        let active = hardware.execution_domain.as_ref()
            .and_then(|owner| owner.diagnostic_domain().translate(far));
        println!("[a618] fault-far={:#x} active-sw={:#x?} base-sw={:#x?}", far, active, base);
        if let Some(owner) = &hardware.execution_domain {
            let pages = owner.diagnostic_pages(far);
            println!("[a618] fault-authority contains={}", pages[0].is_some());
            for (relation, page) in ["contains", "before", "after"].into_iter().zip(pages) {
                if let Some(page) = page {
                    println!(
                        "[a618] fault-page {} iova={:#x} size={:#x} phys={:#x} access={:#x} coherent={} kernel={}",
                        relation, page.range.iova, page.range.size, page.physical,
                        page.range.access, page.range.coherent, page.range.kernel,
                    );
                }
            }
        }
        if let Some(ranges) = &prepared.isolated_ranges {
            let nearby = isolation::nearby_ranges(far, ranges.iter().map(|r| (r.iova, r.size)));
            println!("[a618] fault-wire ranges={} contains={}", ranges.len(), nearby[0].is_some());
            for (relation, index) in ["contains", "before", "after"].into_iter().zip(nearby) {
                if let Some(index) = index {
                    let range = ranges[index];
                    let resource = prepared._resources.iter().find(|resource| {
                        range.iova.checked_sub(resource.gpu_va)
                            .is_some_and(|offset| offset < resource.allocation_size)
                    });
                    println!(
                        "[a618] fault-wire {} index={} iova={:#x} size={:#x} access={:#x} resource-token={:#x?} allocation-offset={:#x?}",
                        relation, index, range.iova, range.size, range.access,
                        resource.map(|resource| resource.token),
                        resource.map(|resource| range.iova - resource.gpu_va),
                    );
                }
            }
        }
        let resources = &prepared._resources;
        let nearby = isolation::nearby_ranges(
            far, resources.iter().map(|r| (r.gpu_va, r.allocation_size)),
        );
        println!("[a618] fault-resources count={} contains={}", resources.len(), nearby[0].is_some());
        for (relation, index) in ["contains", "before", "after"].into_iter().zip(nearby) {
            if let Some(index) = index {
                let resource = &resources[index];
                println!(
                    "[a618] fault-resource {} token={:#x} iova={:#x} size={:#x} access={:#x} image={} far-offset={:#x?}",
                    relation, resource.token, resource.gpu_va, resource.allocation_size,
                    resource.allowed_access, resource.linear_image.is_some(),
                    far.checked_sub(resource.gpu_va),
                );
            }
        }
        // MappingGate is held by submit_words, so map/unmap cannot race this
        // failure lookup. Copy only metadata under the IRQ registry lock: do
        // not print, allocate, clone/drop resource Arcs or acquire other locks.
        // A removed registry entry may still be retained by another accepted
        // submit; absence here does not prove that a base mapping is dangling.
        let (live_count, live) = {
            let registry = self.resources.lock();
            let nearby = isolation::nearby_ranges(
                far, registry.iter().map(|r| (r.gpu_va, r.allocation_size)),
            );
            let live = nearby.map(|index| index.map(|index| {
                let resource = &registry[index];
                (resource.token, resource.gpu_va, resource.allocation_size,
                 resource.allowed_access, resource.linear_image.is_some())
            }));
            (registry.len(), live)
        };
        println!("[a618] fault-live-registry count={} contains={}", live_count, live[0].is_some());
        for (relation, resource) in ["contains", "before", "after"].into_iter().zip(live) {
            if let Some((token, iova, size, access, image)) = resource {
                let in_submit = prepared._resources.iter().any(|resource| resource.token == token);
                println!(
                    "[a618] fault-live-resource {} token={:#x} iova={:#x} size={:#x} access={:#x} image={} in-submit={} far-offset={:#x?}",
                    relation, token, iova, size, access, image, in_submit, far.checked_sub(iova),
                );
            }
        }
    }

    fn record_ring_failure(
        &self,
        hardware: &mut HardwareState,
        reason: &'static str,
        sequence: u32,
        command_words: usize,
        target_wptr: u32,
        interrupts: RingInterruptSnapshot,
        fence: Option<(u32, u32)>,
    ) {
        let snapshot = self.capture_ring_failure(
            hardware,
            reason,
            sequence,
            command_words,
            target_wptr,
            interrupts,
            fence,
        );
        Self::print_ring_failure(snapshot);
        hardware.last_ring_failure = Some(snapshot);
        hardware.lost_reason = Some(reason);
    }

    fn wait_ring_idle(
        &self,
        hardware: &mut HardwareState,
        target_wptr: u32,
        command_words: usize,
    ) -> Result<(), &'static str> {
        let start = time::current_time();
        let mut observed_interrupts = RingInterruptSnapshot::default();
        loop {
            if self.registers.read(CP_RB_RPTR) == target_wptr
                && self.registers.read(RBBM_STATUS) & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0
            {
                return Ok(());
            }
            match self.collect_polled_interrupts() {
                Ok(interrupts) => observed_interrupts.merge(interrupts),
                Err(interrupts) => {
                    observed_interrupts.merge(interrupts);
                    self.record_ring_failure(
                        hardware,
                        "fatal interrupt during ME initialization",
                        0,
                        command_words,
                        target_wptr,
                        observed_interrupts,
                        None,
                    );
                    return Err("qcom-adreno-a618: GPU fault interrupt during GPU submit");
                }
            }
            if time::current_time().saturating_sub(start) >= GPU_TIMEOUT_US {
                self.record_ring_failure(
                    hardware,
                    "timeout during ME initialization",
                    0,
                    command_words,
                    target_wptr,
                    observed_interrupts,
                    None,
                );
                return Err("qcom-adreno-a618: GPU fence timed out");
            }
            time::udelay(10);
        }
    }

    fn execute_ring(
        &self,
        hardware: &mut HardwareState,
        words: &[u32],
        command_words: usize,
    ) -> Result<(), &'static str> {
        hardware.fence_sequence = hardware.fence_sequence.wrapping_add(1).max(1);
        let sequence = hardware.fence_sequence;
        hardware.fence.as_words_mut()[0] = 0;
        hardware.fence.clean_prefix_for_device(core::mem::size_of::<u32>())?;
        // Only the addressed CACHE_FLUSH_TS DMA sequence proves retirement.
        // IRQ, scratch, ring pointer, and idle observations cannot substitute
        // for it: a CP interrupt may arrive before writes reach system memory.
        let completion = completion_commands(hardware.fence.dma_addr(), sequence)?;
        // The trusted preamble has ten words; a checkpoint has none. Keep its
        // fence tail on the worker stack instead of allocating after admission.
        let mut ring = [0u32; 17];
        if words.len() > ring.len() - completion.len() {
            return Err("qcom-adreno-a618: kernel preamble exceeds ring packet storage");
        }
        let ring_len = words.len() + completion.len();
        ring[..words.len()].copy_from_slice(words);
        ring[words.len()..ring_len].copy_from_slice(&completion);
        let target_wptr = self.write_ring(hardware, &ring[..ring_len])?;
        let start = time::current_time();
        #[cfg(any(debug_assertions, feature = "submit-profile"))]
        let mut waits = 0u32;
        let mut wait_policy = completion_wait::CompletionWait::new(GPU_TIMEOUT_US);
        #[cfg(feature = "submit-profile")]
        let mut profile = (None::<u64>, 0u32, 0u32); // first IRQ observation, timeouts, IRQ tails
        let mut observed_interrupts = RingInterruptSnapshot::default();
        loop {
            let fence_value = hardware.fence.read_device_word_volatile(0).unwrap_or(0);
            observed_interrupts.merge(self.take_interrupt_snapshot());
            #[cfg(feature = "submit-profile")]
            if !observed_interrupts.is_empty() && profile.0.is_none() {
                profile.0 = Some(time::current_time().saturating_sub(start));
            }
            if observed_interrupts.is_fatal() {
                let reason = if observed_interrupts.cp & CP_INT_ILLEGAL_INSTR_ERROR != 0 {
                    "CP illegal instruction during submit"
                } else if observed_interrupts.cp & CP_INT_OPCODE_ERROR != 0 {
                    "CP opcode error during submit"
                } else if observed_interrupts.cp & CP_INT_REGISTER_PROTECTION_ERROR != 0 {
                    "CP register protection fault during submit"
                } else if observed_interrupts.cp & CP_INT_HW_FAULT_ERROR != 0 {
                    "CP hardware fault during submit"
                } else {
                    "fatal interrupt during submit"
                };
                self.record_ring_failure(
                    hardware,
                    reason,
                    sequence,
                    command_words,
                    target_wptr,
                    observed_interrupts,
                    Some((fence_value, sequence)),
                );
                return Err(
                    if observed_interrupts.cp & CP_INT_ILLEGAL_INSTR_ERROR != 0 {
                        "qcom-adreno-a618: CP illegal instruction during GPU submit"
                    } else if observed_interrupts.cp & CP_INT_OPCODE_ERROR != 0 {
                        "qcom-adreno-a618: CP opcode error during GPU submit"
                    } else if observed_interrupts.cp & CP_INT_REGISTER_PROTECTION_ERROR != 0 {
                        "qcom-adreno-a618: CP register protection fault during GPU submit"
                    } else if observed_interrupts.cp & CP_INT_HW_FAULT_ERROR != 0 {
                        "qcom-adreno-a618: CP hardware fault during GPU submit"
                    } else {
                        "qcom-adreno-a618: GPU fault interrupt during GPU submit"
                    },
                );
            }
            // Drain any status raised alongside the memory fence before
            // returning so the next ring kick starts from a clean status word.
            if fence_retired(
                sequence,
                fence_value,
                self.registers.read(CP_SCRATCH_2),
                self.registers.read(CP_RB_RPTR),
                target_wptr,
                self.registers.read(RBBM_STATUS) & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0,
            ) {
                // Exactly one FSR read after the DMA retirement predicate, not
                // a full fault snapshot on every wait/poll iteration. A fence
                // can retire despite a terminated/sticky SMMU transaction.
                let pending_fault = if let Some(owner) = &hardware.execution_domain {
                    owner.diagnostic_domain().pending_context_fault()
                } else {
                    self.dma_context.iommu.as_ref().is_some_and(|attachment| {
                        attachment.domain.pending_context_fault().unwrap_or(false)
                    })
                };
                if pending_fault {
                    self.record_ring_failure(
                        hardware, "IOMMU context fault at DMA retirement", sequence,
                        command_words, target_wptr, observed_interrupts,
                        Some((fence_value, sequence)),
                    );
                    return Err("qcom-adreno-a618: IOMMU context fault at DMA retirement");
                }
                #[cfg(debug_assertions)]
                if sequence <= 4 || sequence.is_power_of_two() {
                    let elapsed_us = time::current_time().saturating_sub(start);
                    println!(
                        "[a618] submit complete seq={} words={} elapsed_us={} waits={} irq={:#010x} scratch={:#010x} fence={:#010x}",
                        sequence,
                        command_words,
                        elapsed_us,
                        waits,
                        observed_interrupts.rbbm,
                        self.registers.read(CP_SCRATCH_2),
                        fence_value,
                    );
                }
                #[cfg(feature = "submit-profile")]
                if sequence <= 4 || sequence % 64 == 0 {
                    println!(
                        "[a618-profile] seq={} words={} us={} waits={} timeouts={} first_irq_us={:?} irq_tails={}",
                        sequence,
                        command_words,
                        time::current_time().saturating_sub(start),
                        waits,
                        profile.1,
                        profile.0,
                        profile.2
                    );
                }
                return Ok(());
            }
            #[cfg(feature = "submit-profile")]
            if !observed_interrupts.is_empty() {
                profile.2 = profile.2.saturating_add(1);
            }
            let action = wait_policy.action(
                time::current_time().saturating_sub(start),
                observed_interrupts.rbbm & RBBM_INT_CP_CACHE_FLUSH_TS != 0,
            );
            if action == completion_wait::Action::Deadline {
                self.record_ring_failure(
                    hardware,
                    "GPU submit timeout",
                    sequence,
                    command_words,
                    target_wptr,
                    observed_interrupts,
                    Some((fence_value, sequence)),
                );
                return Err("qcom-adreno-a618: GPU fence timed out");
            }
            if action == completion_wait::Action::Poll {
                time::udelay(1);
                continue;
            }

            // Arm before the final snapshot recheck. If the IRQ races this
            // transition, the handler either leaves a snapshot for this loop
            // or latches a Waker notification that the following wait consumes.
            self.interrupt_wait_armed.store(true, Ordering::Release);
            let late_interrupts = self.take_interrupt_snapshot();
            if !late_interrupts.is_empty() {
                self.interrupt_wait_armed.store(false, Ordering::Release);
                observed_interrupts.merge(late_interrupts);
                continue;
            }

            // Recheck the complete proof after arming: an IRQ/fence can arrive
            // between the first observation and wait registration.
            let ready = || {
                let fence_value = hardware.fence.read_device_word_volatile(0).unwrap_or(0);
                !self.interrupt_snapshot.lock().is_empty()
                    || fence_retired(
                        sequence,
                        fence_value,
                        self.registers.read(CP_SCRATCH_2),
                        self.registers.read(CP_RB_RPTR),
                        target_wptr,
                        self.registers.read(RBBM_STATUS) & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0,
                    )
            };
            if ready() {
                self.interrupt_wait_armed.store(false, Ordering::Release);
                continue;
            }
            if let Some(task) = scarlet::task::mytask() {
                let _woken = match action {
                    completion_wait::Action::Interrupt { timeout_ns } => {
                        self.interrupt_waker.wait_with_condition(
                            task.get_id(),
                            task.get_trapframe(),
                            Some(timeout_ns),
                            0,
                            ready,
                        ) != scarlet::sync::WaitResult::TimedOut
                    }
                    completion_wait::Action::Tail { timeout_ns } => {
                        self.interrupt_waker.wait_with_timeout_precision(
                            task.get_id(),
                            task.get_trapframe(),
                            Some(timeout_ns),
                            scarlet::timer::TimerPrecision::Exact,
                        )
                    }
                    _ => unreachable!(),
                };
                #[cfg(feature = "submit-profile")]
                if !_woken {
                    profile.1 = profile.1.saturating_add(1);
                }
            } else {
                time::udelay(10);
            }
            self.interrupt_wait_armed.store(false, Ordering::Release);
            #[cfg(any(debug_assertions, feature = "submit-profile"))]
            {
                waits = waits.saturating_add(1);
            }
        }
    }

    fn quiesce_lost(&self, hardware: &mut HardwareState, reason: &'static str) {
        self.acknowledged_frequency_khz.store(0, Ordering::Release);
        hardware.lost_reason = Some(reason);
        if hardware.gpu_oob_held {
            let gpu_stopped = Self::force_stop_gpu(self.registers, hardware).is_ok();
            let mut gmu = self.gmu.lock();
            let gmu_stopped = gmu.force_shutdown().is_ok();
            hardware.quiesce_failed = !(gpu_stopped && gmu_stopped);
            if !hardware.quiesce_failed {
                hardware.gpu_oob_held = false;
            }
        }
        hardware.boot = BootState::Lost;
        if hardware.quiesce_failed {
            self.resource_mapping_gate
                .quarantine
                .store(true, Ordering::Release);
        }
    }

    fn ensure_hardware_ready(&self) -> Result<(), GpuBackendSubmitError> {
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        let mut hardware = self.hardware.lock();
        match hardware.boot {
            BootState::Ready => return Ok(()),
            BootState::Lost => {
                if let Some(snapshot) = hardware.last_ring_failure {
                    Self::print_ring_failure(snapshot);
                }
                return Err(GpuBackendSubmitError::DeviceLost(
                    hardware.lost_reason.unwrap_or(
                        "qcom-adreno-a618: GPU was lost during a prior hardware operation",
                    ),
                ));
            }
            BootState::Cold => {}
        }
        self.acknowledged_frequency_khz.store(0, Ordering::Release);
        if !Self::coachz_no_zap_configuration() {
            return Err(GpuBackendSubmitError::Unavailable(
                "qcom-adreno-a618: no authenticated zap-shader path exists for this board",
            ));
        }

        self.interconnect
            .set_peak_kbps(self.peak_kbps)
            .map_err(GpuBackendSubmitError::Unavailable)?;

        let mut gmu = self.gmu.lock();
        gmu.ensure_ready()
            .map_err(GpuBackendSubmitError::Unavailable)?;
        if let Err(error) = gmu.begin_gpu_boot() {
            let gmu_stopped = gmu.force_shutdown().is_ok();
            hardware.quiesce_failed = !gmu_stopped;
            hardware.boot = if gmu_stopped {
                BootState::Cold
            } else {
                BootState::Lost
            };
            if !gmu_stopped {
                self.resource_mapping_gate
                    .quarantine
                    .store(true, Ordering::Release);
                hardware.lost_reason =
                    Some("qcom-adreno-a618: failed to quiesce GMU after GPU OOB timeout");
            }
            return Err(if gmu_stopped {
                GpuBackendSubmitError::Unavailable(error)
            } else {
                GpuBackendSubmitError::DeviceLost(
                    "qcom-adreno-a618: failed to quiesce GMU after GPU OOB timeout",
                )
            });
        }
        hardware.gpu_oob_held = true;
        println!("[qcom-adreno-a618] GPU OOB acknowledged");
        let mut sqe_started = false;
        let preparation = (|| {
            self.dma_context
                .restore_iommu()
                .map_err(|_| "qcom-adreno-a618: failed to restore GPU IOMMU")?;
            let firmware = firmware::load(firmware::SQE_FIRMWARE_PATH, SQE_FIRMWARE_MAX_SIZE)?;
            let instructions = firmware
                .get(4..)
                .filter(|bytes| !bytes.is_empty() && bytes.len() & 3 == 0)
                .ok_or("qcom-adreno-a618: a630 SQE firmware has no instruction payload")?;
            // Linux's adreno_fw_create_bo() strips the four-byte host header
            // before both checking the SQE version and mapping the ucode.
            if !Self::sqe_version_is_secure(instructions) {
                return Err("qcom-adreno-a618: a630 SQE firmware lacks required security fixes");
            }
            let version = u32::from_le_bytes([
                instructions[0],
                instructions[1],
                instructions[2],
                instructions[3],
            ]) & 0xfff;
            println!(
                "[qcom-adreno-a618] SQE payload bytes={} version={:#05x}",
                instructions.len(),
                version,
            );
            let mut sqe =
                DmaAllocation::new(&self.dma_context, instructions.len(), IommuMapFlags::READ)?;
            // Qualcomm firmware word 0 is a host-side version header; the CP
            // instruction base must point at the following payload only.
            sqe.as_bytes_mut().copy_from_slice(instructions);
            sqe.clean_for_device();
            hardware.sqe = Some(sqe);
            hardware.ring.as_bytes_mut().fill(0);
            hardware.ring.clean_for_device();
            hardware.wptr = 0;
            self.initialize_nonprivileged_mmio(&hardware);
            self.registers.write(CP_SQE_CNTL, 1);
            sqe_started = true;
            let me_init = [
                type7(opcode::ME_INIT, 8)
                    .map_err(|_| "qcom-adreno-a618: failed to encode CP initialization")?,
                0x0000_002f,
                0x0000_0003,
                0x2000_0000,
                0,
                0,
                0,
                0,
                0,
            ];
            // Secure-mode CP initialization may not write a host fence. Drain
            // the ring and hardware status exactly as the upstream path does.
            let target_wptr = self.write_ring(&mut hardware, &me_init)?;
            self.wait_ring_idle(&mut hardware, target_wptr, me_init.len())?;
            // CoachZ/Trogdor deletes the zap-shader node; this is Linux's
            // canonical no-zap secure-mode exit for that exact board family.
            self.registers.write(RBBM_SECVID_TRUST_CNTL, 0);
            arch::io_wmb();
            gmu.current_frequency_khz()
        })();
        match preparation {
            Ok(frequency) => {
                gmu.finish_initial_boot_keep_gpu_on();
                hardware.boot = BootState::Ready;
                hardware.lost_reason = None;
                hardware.last_ring_failure = None;
                self.acknowledged_frequency_khz.store(frequency, Ordering::Release);
                println!("[qcom-adreno-a618] SQE/ring ready in CoachZ no-zap mode");
                Ok(())
            }
            Err(error) => {
                let gpu_stopped =
                    !sqe_started || Self::force_stop_gpu(self.registers, &hardware).is_ok();
                let gmu_stopped = gmu.force_shutdown().is_ok();
                hardware.quiesce_failed = !(gpu_stopped && gmu_stopped);
                if hardware.quiesce_failed {
                    self.resource_mapping_gate
                        .quarantine
                        .store(true, Ordering::Release);
                }
                if !hardware.quiesce_failed {
                    hardware.gpu_oob_held = false;
                }
                hardware.boot = if sqe_started || hardware.quiesce_failed {
                    BootState::Lost
                } else {
                    BootState::Cold
                };
                if sqe_started {
                    hardware.lost_reason = Some(error);
                    Err(GpuBackendSubmitError::DeviceLost(error))
                } else {
                    Err(GpuBackendSubmitError::Unavailable(error))
                }
            }
        }
    }

    fn prepare_execution_domain(
        &self,
        hardware: &mut HardwareState,
        client: &Arc<A618ClientVm>,
        command_visible_size: usize,
        released_clients: &mut [Option<Arc<A618ClientVm>>; isolation::RETAINED_LEASE_LIMIT],
        #[cfg(feature = "submit-profile")] profile: &mut SubmissionProfile,
    ) -> Result<isolation::ExecutionDomain, &'static str> {
        let base = self
            .dma_context
            .iommu
            .as_ref()
            .ok_or("a618 isolation: translated IOMMU required")?;
        if !self.dma_context.additional_iommus.is_empty()
            || base.streams.is_empty()
            || base.streams.iter().any(|s| s.substream_id.is_some())
        {
            return Err("a618 isolation: unsupported stream topology");
        }
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(5)
            .map_err(|_| "a618 isolation: allocation failed")?;
        let mut physical = Vec::new();
        physical.try_reserve_exact(5)
            .map_err(|_| "a618 isolation: physical metadata allocation failed")?;
        // Client pages are already mapped by attachment generation. Only the
        // five privileged allocations and active IB size vary per execution.
        for allocation in [
            Some((&hardware.ring, hardware.ring.allocation_size())),
            Some((&hardware.command, command_visible_size)),
            hardware.sqe.as_ref().map(|a| (a, a.allocation_size())),
            hardware.shader_pack.as_ref().map(|a| (a, a.allocation_size())),
        ] {
            let (allocation, visible_size) = allocation.ok_or("a618 isolation: kernel GPU storage unavailable")?;
            physical.push(isolation::PhysicalSpan {
                iova: allocation.dma_addr(), physical: allocation.paddr() as u64,
                size: allocation.allocation_size() as u64,
            });
            ranges.push(isolation::Range {
                iova: allocation.dma_addr(),
                size: visible_size as u64,
                access: adreno_a6xx_submit_wire::ACCESS_READ,
                coherent: false,
                kernel: true,
            });
        }
        ranges.push(isolation::Range {
            iova: hardware.fence.dma_addr(),
            size: hardware.fence.allocation_size() as u64,
            access: adreno_a6xx_submit_wire::ACCESS_READ | adreno_a6xx_submit_wire::ACCESS_WRITE,
            coherent: false,
            kernel: true,
        });
        physical.push(isolation::PhysicalSpan {
            iova: hardware.fence.dma_addr(), physical: hardware.fence.paddr() as u64,
            size: hardware.fence.allocation_size() as u64,
        });
        let translations = isolation::PinnedTranslations::new(physical, PAGE_SIZE as u64)?;
        let base_adapter: Arc<dyn isolation::Domain> = Arc::new(IsolationDomainAdapter {
            domain: Arc::clone(&base.domain),
            streams: base.streams.clone(),
        });
        // Resolve only trusted transient storage. No per-job resource page walk
        // or exact-authority cache is involved in a persistent client VM.
        let authority = isolation::Authority::resolve_with(
            &ranges, PAGE_SIZE as u64, base.streams.len(), |iova| translations.translate(iova),
        )?;
        #[cfg(feature = "submit-profile")]
        {
            profile.stamp(3);
            profile.pages = authority.page_count();
        }
        let cached = if let Some(lease) = hardware.idle_leases
            .take_matching(|lease| Arc::ptr_eq(&lease.client, client))
        {
            // prepared owns this exact generation, so this Arc cannot be its
            // last reference while execution/mapping locks are held.
            Some(lease.owner)
        } else {
            client.domain.lock().take()
        };
        #[cfg(feature = "submit-profile")]
        {
            profile.stamp(4);
            profile.pages += client.authority.page_count();
        }
        if let Some(mut owner) = cached {
            #[cfg(feature = "submit-profile")]
            {
                profile.cache_hit = true;
                profile.retargeted = !owner.kernel_matches(&authority);
                profile.stamp(5);
            }
            if let Err(error) = owner.retarget(authority).and_then(|()| {
                self.acquire_idle_lease(hardware, &mut owner, released_clients)
            }) {
                hardware.execution_domain = Some(owner);
                return Err(error);
            }
            #[cfg(feature = "submit-profile")]
            profile.stamp(6);
            return Ok(owner);
        }
        let domain = base
            .controller
            .alloc_parked_domain(IommuDomainConfig {
                domain_type: IommuDomainType::Dma,
                iova_base: Iova::new(GPU_IOVA_BASE),
                iova_size: GPU_IOVA_SIZE,
            })
            .map_err(|_| "a618 isolation: private context bank allocation failed")?;
        #[cfg(feature = "submit-profile")]
        profile.stamp(5);
        let mut owner = isolation::ExecutionDomain::from_parked_client_authority(
            base_adapter,
            Arc::new(IsolationDomainAdapter {
                domain,
                streams: base.streams.clone(),
            }),
            Arc::clone(&client.authority),
            authority,
        )?;
        if let Err(error) = self.acquire_idle_lease(hardware, &mut owner, released_clients) {
            if owner.ownership_uncertain() {
                hardware.execution_domain = Some(owner);
            }
            return Err(error);
        }
        #[cfg(feature = "submit-profile")]
        profile.stamp(6);
        Ok(owner)
    }

    fn acquire_idle_lease(
        &self,
        hardware: &mut HardwareState,
        owner: &mut isolation::ExecutionDomain,
        released_clients: &mut [Option<Arc<A618ClientVm>>; isolation::RETAINED_LEASE_LIMIT],
    ) -> Result<(), &'static str> {
        // Include the new active lease within the fixed resident-bank limit.
        if hardware.idle_leases.is_full() {
            self.evict_idle_lease(hardware, released_clients)?;
        }
        loop {
            if owner.try_resume_idle()? {
                return Ok(());
            }
            if !self.evict_idle_lease(hardware, released_clients)? {
                return Err("a618 isolation: no reclaimable context bank available");
            }
        }
    }

    fn evict_idle_lease(
        &self,
        hardware: &mut HardwareState,
        released_clients: &mut [Option<Arc<A618ClientVm>>; isolation::RETAINED_LEASE_LIMIT],
    ) -> Result<bool, &'static str> {
        let Some(mut lease) = hardware.idle_leases.pop_oldest() else {
            return Ok(false);
        };
        if let Err(error) = lease.owner.suspend_idle(&lease.stopped) {
            // An uncertain bank must retain its own generation's backing too,
            // not just its page tables. Never retry that lease on another CB.
            core::mem::forget(lease);
            self.isolation_route_failure(hardware, error);
            return Err(error);
        }
        let IdleLease { owner, client, .. } = lease;
        let mut parked = client.domain.lock();
        if parked.is_some() {
            drop(parked);
            // Even an internal ownership violation must not run competing
            // domain or backing destructors under execution locks.
            core::mem::forget(owner);
            core::mem::forget(client);
            let error = "a618 isolation: duplicate generation domain owner";
            self.isolation_route_failure(hardware, error);
            return Err(error);
        }
        *parked = Some(owner);
        drop(parked);
        // Last-generation destruction can unmap base resources. Defer it until
        // after both hardware and mapping guards have been released.
        *released_clients.iter_mut().find(|slot| slot.is_none()).unwrap() = Some(client);
        Ok(true)
    }

    fn release_idle_leases(&self) -> Result<(), &'static str> {
        let mut released_clients = [const { None }; isolation::RETAINED_LEASE_LIMIT];
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        let mut hardware = self.hardware.lock();
        if self.resource_mapping_gate.quarantine.load(Ordering::Acquire) {
            return Ok(());
        }
        while self.evict_idle_lease(&mut hardware, &mut released_clients)? {}
        Ok(())
    }

    /// Linux a6xx_bus_clear_pending_transactions ordering, but keep the halt
    /// asserted across route changes and preserve SQE/ring/register state.
    /// This is deliberately separate from the destructive fault/reset path.
    fn halt_idle_dma(
        &self,
        hardware: &HardwareState,
    ) -> Result<isolation::DmaStopped, &'static str> {
        isolation::halt_for_switch(|phase| match phase {
            isolation::HaltPhase::Idle => {
                Self::poll_gpu(self.registers, CP_RB_RPTR, |rptr| rptr == hardware.wptr)
                    && Self::poll_gpu(self.registers, RBBM_STATUS, |status| {
                        status & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0
                    })
            }
            isolation::HaltPhase::Gx => {
                self.registers.write(RBBM_GBIF_HALT, 1);
                arch::io_wmb();
                Self::poll_gpu(self.registers, RBBM_GBIF_HALT_ACK, |v| v & 1 != 0)
            }
            isolation::HaltPhase::Clients => {
                self.registers.write(GBIF_HALT, 1);
                arch::io_wmb();
                Self::poll_gpu(self.registers, GBIF_HALT_ACK, |v| v & 1 != 0)
            }
            isolation::HaltPhase::Axi => {
                self.registers.write(GBIF_HALT, 2);
                arch::io_wmb();
                Self::poll_gpu(self.registers, GBIF_HALT_ACK, |v| v & 2 != 0)
            }
        })
    }

    fn resume_idle_dma(&self) {
        self.registers.write(GBIF_HALT, 0);
        let _ = self.registers.read(GBIF_HALT);
        self.registers.write(RBBM_GBIF_HALT, 0);
        let _ = self.registers.read(RBBM_GBIF_HALT);
        arch::io_wmb();
    }

    fn isolation_route_failure(
        &self,
        hardware: &mut HardwareState,
        error: &'static str,
    ) -> GpuBackendSubmitError {
        self.quiesce_lost(hardware, error);
        // A failed IOMMU synchronization can leave an uncertain route even
        // after the bus stops. Retain the private owner and every backing.
        hardware.quiesce_failed = true;
        self.resource_mapping_gate
            .quarantine
            .store(true, Ordering::Release);
        GpuBackendSubmitError::DeviceLost(error)
    }

    fn submit_words(
        &self,
        prepared: &PreparedSubmission,
        work_sample: &mut Option<scarlet::device::devfreq::DeviceFrequencyWork>,
    ) -> Result<(), GpuBackendSubmitError> {
        // Declared before lock guards so even early-return resource destruction
        // runs only after the hardware/mapping critical section has ended.
        let mut released_clients = [const { None }; isolation::RETAINED_LEASE_LIMIT];
        #[cfg(feature = "submit-profile")]
        let mut profile = SubmissionProfile::default();
        #[cfg(feature = "submit-profile")]
        {
            profile.stamp(0);
            if prepared.isolated_ranges.is_some() {
                profile.private_ordinal = PRIVATE_PHASE_SEQUENCE.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            }
        }
        let words = &prepared.words;
        let word_count = u32::try_from(words.len()).map_err(|_| {
            GpuBackendSubmitError::Rejected("qcom-adreno-a618: command stream is too large")
        })?;
        let byte_size = words.len().checked_mul(core::mem::size_of::<u32>()).ok_or(
            GpuBackendSubmitError::Rejected("qcom-adreno-a618: command byte size overflows"),
        )?;
        // Mapping changes invalidate the complete A618 SMMU context. Keep
        // resource map/unmap outside the interval in which the GPU can fetch
        // commands, shaders, or image data from that context.
        let _mapping_gate = self.resource_mapping_gate.execution.lock();
        let mut hardware = self.hardware.lock();
        #[cfg(feature = "submit-profile")]
        profile.stamp(1);
        // Readiness may have changed after ensure_hardware_ready released
        // the locks (for example, a failed devfreq vote on another thread).
        if hardware.boot != BootState::Ready
            || self
                .resource_mapping_gate
                .quarantine
                .load(Ordering::Acquire)
        {
            return Err(GpuBackendSubmitError::DeviceLost(
                hardware
                    .lost_reason
                    .unwrap_or("qcom-adreno-a618: GPU is not ready for submission"),
            ));
        }
        if byte_size > hardware.command.requested_size() {
            return Err(GpuBackendSubmitError::Rejected(
                "qcom-adreno-a618: command stream exceeds staging allocation",
            ));
        }
        if byte_size != 0 {
            hardware.command.as_words_mut()[..words.len()].copy_from_slice(words);
            hardware
                .command
                .clean_prefix_for_device(byte_size)
                .map_err(GpuBackendSubmitError::Unavailable)?;
        }
        let submission = submission_commands(hardware.command.dma_addr(), word_count)
            .map_err(GpuBackendSubmitError::Unavailable)?;
        // Empty checkpoints still append the real hardware fence, without an
        // invalid zero-length indirect-buffer packet.
        let ring_words = if words.is_empty() {
            &[][..]
        } else {
            &submission[..]
        };
        #[cfg(feature = "submit-profile")]
        {
            profile.stamp(2);
            // Non-private submissions have no authority or route phases.
            for index in 3..10 {
                profile.stamps[index] = profile.stamps[2];
            }
        }
        if prepared.isolated_ranges.is_some() {
            let command_visible_size = isolation::command_mapping_size(
                byte_size, hardware.command.allocation_size(), PAGE_SIZE,
            ).map_err(GpuBackendSubmitError::Unavailable)?;
            let owner = match self.prepare_execution_domain(
                    &mut hardware, prepared.client_vm.as_ref().unwrap(), command_visible_size,
                    &mut released_clients,
                    #[cfg(feature = "submit-profile")]
                    &mut profile,
                ) {
                Ok(owner) => owner,
                Err(error) if hardware.execution_domain.is_some() => {
                    let owner = hardware.execution_domain.as_ref().unwrap();
                    println!("[a618] private domain update failed executed=false last-seq={} reason={} pending-fault={:#x?}",
                        hardware.fence_sequence, error, owner.pending_bind_fault());
                    // A partial table/TLBI update is not an execution or
                    // retirement proof. Retain the bank and accepted backing.
                    return Err(self.isolation_route_failure(&mut hardware, error));
                }
                Err(error) => return Err(GpuBackendSubmitError::Unavailable(error)),
            };
            hardware.execution_domain = Some(owner);
            let stopped = match self.halt_idle_dma(&hardware) {
                Ok(proof) => proof,
                Err(error) => {
                    self.quiesce_lost(&mut hardware, error);
                    if !hardware.quiesce_failed {
                        drop(hardware.execution_domain.take());
                    }
                    return Err(GpuBackendSubmitError::DeviceLost(error));
                }
            };
            #[cfg(feature = "submit-profile")]
            profile.stamp(7);
            // These kernel-owned pages are readable by untrusted ISA. The
            // staging tail, consumed ring entries and fence padding can hold
            // a previous context's inline constants or shader-written data.
            // Erase them only after idle+halt, preserving this job's commands
            // and the hardware ring pointer/SQE state.
            // Only the page-rounded active IB is exposed in this domain.
            // Clear its final page's padding; the rest of the 2 MiB allocation
            // is unmapped, including any stale preceding context's commands.
            hardware.command.as_words_mut()[words.len()..command_visible_size / 4].fill(0);
            // command_mapping_size checked this against the owned allocation.
            arch::clean_dcache_to_poc_range(hardware.command.vaddr(), command_visible_size);
            hardware.ring.as_words_mut().fill(0);
            hardware.ring.clean_for_device();
            hardware.fence.as_words_mut().fill(0);
            hardware.fence.clean_for_device();
            #[cfg(feature = "submit-profile")]
            profile.stamp(8);
            if let Err(error) = hardware
                .execution_domain
                .as_mut()
                .unwrap()
                .enter_halted(&stopped)
            {
                if let Some((private, fault)) = hardware.execution_domain.as_ref()
                    .and_then(|owner| owner.pending_bind_fault())
                {
                    // No route was changed and this job never ran. Preserve
                    // the original bank's syndrome without assigning it to a
                    // newly emitted sequence or silently evicting a bad cache.
                    println!("[a618] pre-bind IOMMU fault private={} executed=false last-seq={} words={} snapshot={:#x?}",
                        private, hardware.fence_sequence, words.len(), fault);
                    self.quiesce_lost(&mut hardware, error);
                    if !hardware.quiesce_failed {
                        drop(hardware.execution_domain.take());
                    }
                    return Err(GpuBackendSubmitError::DeviceLost(error));
                }
                return Err(self.isolation_route_failure(&mut hardware, error));
            }
            self.resume_idle_dma();
            #[cfg(feature = "submit-profile")]
            profile.stamp(9);
        }
        // Stable OPP under execution+hardware locks. CPU validation/mapping is
        // already finished; only the real XO busy delta becomes GPU demand.
        // These raw reads do not consume the separate utilization sample.
        let work_start = if words.is_empty() { None } else {
            let gmu = self.gmu.lock();
            gmu.busy_ticks().ok().zip(gmu.current_frequency_khz().ok())
                .map(|(ticks, frequency)| (ticks, frequency, time::current_time_ns()))
        };
        let result = self.execute_ring(&mut hardware, ring_words, words.len());
        if result.is_ok() {
            if let Some((start_ticks, frequency_khz, start_ns)) = work_start {
                if let Ok(end_ticks) = self.gmu.lock().busy_ticks() {
                    let completed_ns = time::current_time_ns();
                    if let (Some(ticks), Some(elapsed)) = (
                        end_ticks.checked_sub(start_ticks), completed_ns.checked_sub(start_ns),
                    ) {
                        let busy_ns = (u128::from(ticks) * 1_000_000_000).div_ceil(19_200_000);
                        // Reject resets/impossible counters rather than treating
                        // CPU submission time as GPU work or reporting fake busy.
                        if busy_ns <= u128::from(elapsed.saturating_add(2_000)) {
                            *work_sample = Some(scarlet::device::devfreq::DeviceFrequencyWork {
                                busy_ns: busy_ns as u64, frequency_khz, completed_ns,
                            });
                        }
                    }
                }
            }
        }
        #[cfg(feature = "submit-profile")]
        profile.stamp(10);
        if let Err(error) = result {
            self.print_fault_authority(&hardware, prepared);
            // Stop/quiesce while the private route still exists. No base route
            // or resource teardown is permitted before this DMA-stop proof.
            self.quiesce_lost(&mut hardware, error);
        }
        if hardware.execution_domain.is_some() && !hardware.quiesce_failed {
            let stopped = if result.is_ok() {
                match self.halt_idle_dma(&hardware) {
                    Ok(proof) => proof,
                    Err(error) => {
                        self.quiesce_lost(&mut hardware, error);
                        if hardware.quiesce_failed {
                            return Err(GpuBackendSubmitError::DeviceLost(error));
                        }
                        isolation::DmaStopped::after_fault_shutdown()
                    }
                }
            } else {
                isolation::DmaStopped::after_fault_shutdown()
            };
            if let Err(error) = hardware
                .execution_domain
                .as_mut()
                .unwrap()
                .restore_halted(&stopped)
            {
                return Err(self.isolation_route_failure(&mut hardware, error));
            }
            if result.is_ok() && hardware.boot == BootState::Ready {
                if let Err(error) = hardware.execution_domain.as_ref().unwrap().retain_idle(&stopped) {
                    return Err(self.isolation_route_failure(&mut hardware, error));
                }
                let owner = hardware.execution_domain.take().unwrap();
                let lease = IdleLease {
                    owner,
                    client: Arc::clone(prepared.client_vm.as_ref().unwrap()),
                    stopped,
                };
                if let Err(lease) = hardware.idle_leases.push_newest(lease) {
                    core::mem::forget(lease);
                    return Err(self.isolation_route_failure(
                        &mut hardware, "a618 isolation: resident lease limit exceeded",
                    ));
                }
            } else {
                drop(hardware.execution_domain.take());
            }
            if hardware.boot == BootState::Ready {
                self.resume_idle_dma();
            }
        }
        if hardware.boot == BootState::Lost {
            return Err(GpuBackendSubmitError::DeviceLost(
                hardware
                    .lost_reason
                    .unwrap_or("a618 isolation: execution lost"),
            ));
        }
        #[cfg(feature = "submit-profile")]
        {
            profile.stamp(11);
            let sequence = hardware.fence_sequence;
            // Profiling output must not extend the mapping/hardware critical
            // section, nor hold resources through a potentially slow console.
            drop(hardware);
            drop(_mapping_gate);
            profile.report(sequence, words.len(), prepared.isolated_ranges.is_some());
        }
        result.map_err(GpuBackendSubmitError::DeviceLost)
    }
}

impl InterruptCapableDevice for A618Core {
    fn handle_interrupt(&self) -> scarlet::interrupt::InterruptResult<()> {
        let _ = self.claim_interrupt()?;
        Ok(())
    }

    fn interrupt_id(&self) -> Option<scarlet::interrupt::InterruptId> {
        Some(self.interrupt_id)
    }

    fn claim_interrupt(
        &self,
    ) -> scarlet::interrupt::InterruptResult<scarlet::interrupt::InterruptClaim> {
        if self.registers.read(RBBM_INT_0_STATUS) == 0 {
            return Ok(scarlet::interrupt::InterruptClaim::NotMine);
        }
        let snapshot = match self.consume_ring_interrupts() {
            Ok(snapshot) | Err(snapshot) => snapshot,
        };
        {
            let mut pending = self.interrupt_snapshot.lock();
            pending.merge(snapshot);
        }
        if self.interrupt_wait_armed.swap(false, Ordering::AcqRel) {
            self.interrupt_waker.wake_all();
        }
        Ok(scarlet::interrupt::InterruptClaim::Handled)
    }
}

struct A618Backend {
    core: Arc<A618Core>,
}

struct A618Resource {
    core: Arc<A618Core>,
    entry: Arc<ResourceEntry>,
}

impl Drop for A618Resource {
    fn drop(&mut self) {
        self.core.unregister_resource(self.entry.token);
    }
}

struct A618Buffer {
    resource: A618Resource,
}

struct A618Image {
    resource: A618Resource,
    create: GpuImageCreateInfo,
    layout: GpuBackendImageLayout,
}

impl A618Image {
    fn depth_readback(&self) -> Option<readback::DepthReadback> {
        let plane = self.layout.planes[0];
        if self.create.format != GPU_IMAGE_FORMAT_DEPTH32_FLOAT
            || self.create.usage & GPU_IMAGE_USAGE_TRANSFER_SRC == 0
            || self.create.mip_levels != 1 || self.create.array_layers != 1 || self.create.cube
            || self.layout.modifier != IMAGE_MODIFIER_TILE6_3_DEPTH
            || self.layout.plane_count != 1 || plane.offset != 0
            || plane.block_width != 1 || plane.block_height != 1 || plane.bytes_per_block != 4
            || plane.size != self.layout.total_size || u64::from(plane.array_pitch) != plane.size
            || self.resource.entry.allocation_size < self.layout.total_size
        {
            return None;
        }
        readback::DepthReadback::new(self.create.width, self.create.height, plane.row_pitch, plane.size)
    }
}

impl GpuBackendBuffer for A618Buffer {
    fn query_info(&self) -> GpuBackendBufferInfo {
        GpuBackendBufferInfo::new(
            self.resource.entry.token,
            self.resource.entry.allocation_size,
        )
    }

    fn backend_cookie(&self) -> u64 {
        self.resource.core.backend_cookie
    }
}

impl GpuBackendImage for A618Image {
    fn supports_native_readback_spans(&self) -> bool {
        self.depth_readback().is_some()
    }

    fn native_readback_span(&self, x: u32, y: u32, width: u32) -> Result<(u64, u32), &'static str> {
        self.depth_readback().and_then(|layout| layout.span(x, y, width))
            .ok_or("qcom-adreno-a618: invalid tiled D32 readback span")
    }

    fn query_info(&self) -> GpuBackendImageInfo {
        GpuBackendImageInfo::new(
            self.create,
            self.resource.entry.token,
            self.resource.entry.allocation_size,
        )
    }

    fn backend_cookie(&self) -> u64 {
        self.resource.core.backend_cookie
    }

    fn display_resource(&self) -> Option<GpuDisplayResource> {
        None
    }

    fn linear_display_info(&self) -> Option<GpuBackendLinearDisplayInfo> {
        (self.create.usage & GPU_IMAGE_USAGE_PRESENTABLE != 0).then(|| {
            GpuBackendLinearDisplayInfo::new(
                self.layout.planes[0].offset,
                self.layout.planes[0].row_pitch,
                PixelFormat::BGRA8888,
            )
        })
    }
}

#[derive(Clone)]
struct Attachment {
    attachment_token: u64,
    resource: Arc<ResourceEntry>,
    allowed_access: u32,
    backing: GpuBackingPin,
}

/// One immutable attachment generation, shared by its context and admitted
/// jobs. Field order drops detached page tables before mappings and real pins.
struct A618ClientVm {
    domain: Mutex<Option<isolation::ExecutionDomain>>,
    authority: Arc<isolation::Authority>,
    resolved: Vec<ResolvedResource>,
    resources: Arc<Vec<Arc<ResourceEntry>>>,
    _backing: Vec<GpuBackingPin>,
}

struct A618ContextInner {
    core: Arc<A618Core>,
    id: u64,
    next_attachment: AtomicU64,
    attachments: IrqSpinLock<Vec<Attachment>>,
    vm: Mutex<Option<Arc<A618ClientVm>>>,
    execution: Mutex<()>,
}

impl A618ContextInner {
    fn attach(&self, resource_token: u64, backing: GpuBackingPin) -> Result<u64, &'static str> {
        let resource = self
            .core
            .resource(resource_token)
            .ok_or("qcom-adreno-a618: resource no longer exists")?;
        let allowed_access = resource.allowed_access
            & if backing.allows_gpu_write() {
                adreno_a6xx_submit_wire::ACCESS_READ | adreno_a6xx_submit_wire::ACCESS_WRITE
            } else {
                adreno_a6xx_submit_wire::ACCESS_READ
            };
        if allowed_access == 0 {
            return Err("a618 VM: backing grants no GPU access");
        }
        let mut attachments = self.attachments.lock();
        if attachments
            .iter()
            .any(|entry| entry.resource.token == resource_token)
        {
            return Err("qcom-adreno-a618: resource is already attached");
        }
        let generation = allocate_monotonic(
            &self.next_attachment,
            "qcom-adreno-a618: attachment token space exhausted",
        )?;
        let attachment_token = self.id.rotate_left(17).wrapping_add(generation);
        if attachment_token == 0
            || attachments
                .iter()
                .any(|entry| entry.attachment_token == attachment_token)
        {
            return Err("qcom-adreno-a618: attachment token space exhausted");
        }
        attachments
            .try_reserve(1)
            .map_err(|_| "qcom-adreno-a618: attachment allocation failed")?;
        attachments.push(Attachment {
            attachment_token,
            resource,
            allowed_access,
            backing,
        });
        drop(attachments);
        drop(self.vm.lock().take());
        Ok(attachment_token)
    }

    fn detach(&self, resource_token: u64) -> Result<(), &'static str> {
        let mut attachments = self.attachments.lock();
        let index = attachments
            .iter()
            .position(|entry| entry.resource.token == resource_token)
            .ok_or("qcom-adreno-a618: resource is not attached")?;
        let removed = attachments.swap_remove(index);
        drop(attachments);
        drop(self.vm.lock().take());
        drop(removed);
        Ok(())
    }

    /// Build physical authority once per attachment generation while generic
    /// attach/submission ownership proves each real backing is still alive.
    fn client_vm(&self) -> Result<Arc<A618ClientVm>, &'static str> {
        if let Some(vm) = self.vm.lock().as_ref() {
            return Ok(Arc::clone(vm));
        }
        let count = self.attachments.lock().len();
        let mut attachments = Vec::new();
        attachments
            .try_reserve_exact(count)
            .map_err(|_| "a618 VM: attachment allocation failed")?;
        attachments.extend(self.attachments.lock().iter().cloned());
        let mut resolved = Vec::new();
        let mut resources = Vec::new();
        let mut backing = Vec::new();
        let mut ranges = Vec::new();
        let mut spans = Vec::new();
        resolved
            .try_reserve_exact(count)
            .map_err(|_| "a618 VM: allocation failed")?;
        resources
            .try_reserve_exact(count)
            .map_err(|_| "a618 VM: allocation failed")?;
        backing
            .try_reserve_exact(count)
            .map_err(|_| "a618 VM: allocation failed")?;
        ranges
            .try_reserve_exact(count)
            .map_err(|_| "a618 VM: allocation failed")?;
        for attachment in attachments {
            let entry = &attachment.resource;
            spans
                .try_reserve(entry.physical_spans.len())
                .map_err(|_| "a618 VM: extent allocation failed")?;
            spans.extend_from_slice(&entry.physical_spans);
            resolved.push(ResolvedResource {
                attachment_token: attachment.attachment_token,
                gpu_va: entry.gpu_va,
                allocation_size: entry.allocation_size,
                allowed_access: attachment.allowed_access,
                linear_image: entry.linear_image,
            });
            ranges.push(isolation::Range {
                iova: entry.gpu_va,
                size: entry.allocation_size,
                access: attachment.allowed_access,
                coherent: true,
                kernel: false,
            });
            resources.push(attachment.resource);
            backing.push(attachment.backing);
        }
        resolved.sort_unstable_by_key(|entry| entry.attachment_token);
        let translations = isolation::PinnedTranslations::new(spans, PAGE_SIZE as u64)?;
        let streams = self
            .core
            .dma_context
            .iommu
            .as_ref()
            .ok_or("a618 VM: translated IOMMU required")?
            .streams
            .len();
        let authority =
            isolation::Authority::resolve_with(&ranges, PAGE_SIZE as u64, streams, |iova| {
                translations.translate(iova)
            })?;
        let vm = Arc::new(A618ClientVm {
            domain: Mutex::new(None),
            authority: Arc::new(authority),
            resolved,
            resources: Arc::new(resources),
            _backing: backing,
        });
        *self.vm.lock() = Some(Arc::clone(&vm));
        Ok(vm)
    }

    fn prepare_submission(
        &self,
        commands: &[u8],
    ) -> Result<PreparedSubmission, GpuBackendSubmitError> {
        if commands.is_empty() {
            return Ok(PreparedSubmission {
                words: Vec::new(),
                isolated_ranges: None,
                _resources: Arc::new(Vec::new()),
                client_vm: None,
            });
        }
        let shader_pack_base =
            self.core
                .shader_pack_base()
                .ok_or(GpuBackendSubmitError::Unavailable(
                    "qcom-adreno-a618: canonical shader pack is unavailable",
                ))?;
        let decoded = adreno_a6xx_submit_wire::decode(commands).map_err(|_| {
            GpuBackendSubmitError::Rejected("qcom-adreno-a618: invalid A6xx submit wire")
        })?;
        let client_vm = self
            .client_vm()
            .map_err(GpuBackendSubmitError::Unavailable)?;
        let attachments = &client_vm.resolved;
        let words = validate_and_relocate_decoded(
            decoded,
            |token| {
                let index = attachments.binary_search_by_key(&token, |entry| entry.attachment_token).ok()?;
                attachments.get(index).copied()
            },
            |variant| A618Core::resolve_shader_at(shader_pack_base, variant),
        ).map_err(|error| {
            println!("[a618] submit rejected: {}", error);
            if let Some(packet) = diagnose_rejected_packet(commands) {
                println!(
                    "[a618] rejected PM4 packet kind={} word={} selector={:#06x} count={} first={:#010x}",
                    packet.kind.label(), packet.word_offset, packet.selector,
                    packet.payload_len, packet.first_value.unwrap_or(0),
                );
            }
            GpuBackendSubmitError::Rejected(error)
        })?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(decoded.resource_len())
            .map_err(|_| {
                GpuBackendSubmitError::Unavailable("a618 isolation: range allocation failed")
            })?;
        for index in 0..decoded.resource_len() {
            let wire = decoded.resource(index).unwrap();
            // The immutable VM snapshot is sorted by attachment token; the
            // validator above already proved every referenced token exists.
            let index = attachments
                .binary_search_by_key(&wire.attachment_token, |entry| entry.attachment_token)
                .unwrap();
            let resolved = &attachments[index];
            ranges.push(isolation::Range {
                iova: resolved.gpu_va + wire.range_offset,
                size: wire.range_size,
                access: wire.access,
                coherent: true,
                kernel: false,
            });
        }
        Ok(PreparedSubmission {
            words,
            isolated_ranges: Some(ranges),
            _resources: Arc::clone(&client_vm.resources),
            client_vm: Some(client_vm),
        })
    }
}

struct A618Context {
    inner: Arc<A618ContextInner>,
}

impl A618Context {
    fn validate_resource(
        &self,
        backend_cookie: u64,
        token: u64,
        backing: GpuBackingPin,
    ) -> Result<u64, &'static str> {
        if backend_cookie != self.inner.core.backend_cookie || token == 0 {
            return Err("qcom-adreno-a618: resource belongs to another backend");
        }
        self.inner.attach(token, backing)
    }
}

impl GpuBackendContext for A618Context {
    fn timestamp_ns(&self) -> Result<Option<u64>, GpuBackendSubmitError> {
        timestamp::sample(&self.inner.core).map(Some)
    }

    fn query_info(&self) -> GpuBackendContextInfo {
        GpuBackendContextInfo::new(0, DIALECT_TOKEN)
    }

    fn create_queue(&self) -> Result<Arc<dyn GpuBackendQueue>, &'static str> {
        Ok(Arc::new(A618Queue {
            context: Arc::clone(&self.inner),
        }))
    }

    fn begin_image_cpu_access(
        &self,
        image: &dyn GpuBackendImage,
    ) -> Result<Option<Box<dyn GpuBackendCpuAccessGuard + '_>>, &'static str> {
        if image.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: image belongs to another backend");
        }
        let guard = self
            .inner
            .core
            .begin_cpu_access()
            .map_err(|error| match error {
                GpuBackendSubmitError::Rejected(reason)
                | GpuBackendSubmitError::Unavailable(reason)
                | GpuBackendSubmitError::DeviceLost(reason) => reason,
            })?;
        Ok(Some(Box::new(guard)))
    }

    fn attach_image_pinned(
        &self,
        image: &dyn GpuBackendImage,
        backing: GpuBackingPin,
    ) -> Result<u64, &'static str> {
        let _execution = self.inner.execution.lock();
        self.validate_resource(
            image.backend_cookie(),
            image.query_info().command_resource_token,
            backing,
        )
    }

    fn detach_image(&self, image: &dyn GpuBackendImage) -> Result<(), &'static str> {
        let _execution = self.inner.execution.lock();
        if image.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: image belongs to another backend");
        }
        self.inner.detach(image.query_info().command_resource_token)
    }

    fn upload_image_bgra(
        &self,
        image: &dyn GpuBackendImage,
        _upload: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        let _execution = self.inner.execution.lock();
        if image.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: image belongs to another backend");
        }
        // The generic layer holds A618CpuAccess across its prior GPU drain,
        // CPU copy/cache clean, and this callback. No new GPU work can execute
        // until it drops that guard after this synchronous callback returns.
        Ok(())
    }

    fn readback_image_bgra(
        &self,
        image: &dyn GpuBackendImage,
        _readback: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        // The generic layer already drained the device FIFO and still holds
        // A618CpuAccess through its following cache invalidate and CPU copy.
        let _execution = self.inner.execution.lock();
        if image.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: image belongs to another backend");
        }
        let info = image.query_info();
        if info.format != GPU_IMAGE_FORMAT_BGRA8_UNORM
            || info.usage & GPU_IMAGE_USAGE_TRANSFER_SRC == 0
        {
            return Err("qcom-adreno-a618: image does not support BGRA readback");
        }
        // Order the completed GPU fence before the generic GPU layer
        // invalidates the requested backing rows and copies them to userspace.
        arch::io_rmb();
        Ok(())
    }

    fn upload_image(
        &self,
        image: &dyn GpuBackendImage,
        upload: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        if !matches!(
            image.query_info().format,
            GPU_IMAGE_FORMAT_BGRA8_UNORM
                | GPU_IMAGE_FORMAT_RGBA16_FLOAT
                | GPU_IMAGE_FORMAT_RGBA32_FLOAT
        ) {
            return Err("qcom-adreno-a618: unsupported native color upload");
        }
        // Generic resource code validates byte-sized rows and holds CPU access
        // across the copy/cache clean. The backend callback only checks owner.
        self.upload_image_bgra(image, upload)
    }

    fn readback_image(
        &self,
        image: &dyn GpuBackendImage,
        _readback: GpuImageUploadInfo,
    ) -> Result<(), &'static str> {
        let _execution = self.inner.execution.lock();
        if image.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: image belongs to another backend");
        }
        let info = image.query_info();
        let supported = matches!(
            info.format,
            GPU_IMAGE_FORMAT_BGRA8_UNORM
                | GPU_IMAGE_FORMAT_RGBA16_FLOAT
                | GPU_IMAGE_FORMAT_RGBA32_FLOAT
        ) || (info.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT && image.supports_native_readback_spans());
        if !supported || info.usage & GPU_IMAGE_USAGE_TRANSFER_SRC == 0
        {
            return Err("qcom-adreno-a618: unsupported native image readback");
        }
        // Generic CPU access has drained prior GPU work and blocks new work.
        // The generic layer invalidates and copies bounded backing spans for
        // D32, preserving the tiled source and its owner for the entire copy.
        arch::io_rmb();
        Ok(())
    }

    fn attach_buffer_pinned(
        &self,
        buffer: &dyn GpuBackendBuffer,
        backing: GpuBackingPin,
    ) -> Result<u64, &'static str> {
        let _execution = self.inner.execution.lock();
        self.validate_resource(
            buffer.backend_cookie(),
            buffer.query_info().command_resource_token,
            backing,
        )
    }

    fn detach_buffer(&self, buffer: &dyn GpuBackendBuffer) -> Result<(), &'static str> {
        let _execution = self.inner.execution.lock();
        if buffer.backend_cookie() != self.inner.core.backend_cookie {
            return Err("qcom-adreno-a618: buffer belongs to another backend");
        }
        self.inner
            .detach(buffer.query_info().command_resource_token)
    }
}

struct A618Queue {
    context: Arc<A618ContextInner>,
}

impl GpuBackendQueue for A618Queue {
    fn query_info(&self) -> GpuBackendQueueInfo {
        GpuBackendQueueInfo::new(A618_MAX_OPAQUE_COMMAND_SIZE)
    }

    fn submit(&self, commands: &[u8]) -> Result<(), GpuBackendSubmitError> {
        let prepared = {
            let _execution = self.context.execution.lock();
            self.context.prepare_submission(commands)?
        };
        // A CPU access callback can take context.execution while this submit
        // waits for its admission reservation, so never retain that gate here.
        self.context.core.submit_prepared_sync(prepared, false)
    }

    fn async_capacity(&self) -> u32 {
        ASYNC_CAPACITY as u32
    }

    fn enqueue(&self, submission: GpuSubmission) -> Result<(), GpuBackendEnqueueError> {
        // This gate only protects CPU-side validation/authority snapshots. The
        // worker never takes it while waiting for the GPU. Async admission
        // always uses try_lock so it never waits on an authority operation.
        let Some(_execution) = self.context.execution.try_lock() else {
            return Err(GpuBackendEnqueueError::Busy(submission));
        };
        {
            let Some(pending) = self.context.core.pending.try_lock() else {
                return Err(GpuBackendEnqueueError::Busy(submission));
            };
            if let Some(error) = pending.failure {
                return Err(GpuBackendEnqueueError::Rejected(error, submission));
            }
            if !pending.queue.has_capacity() {
                return Err(GpuBackendEnqueueError::Busy(submission));
            }
        }
        // Decode, relocation and attachment-generation VM construction are
        // CPU-only and context-serialized. Do not block retirement/dispatch of
        // already accepted jobs by retaining the device's global pending lock.
        let prepared = match self.context.prepare_submission(submission.commands()) {
            Ok(prepared) => prepared,
            Err(error) => return Err(GpuBackendEnqueueError::Rejected(error, submission)),
        };
        let work = PendingSubmission {
            prepared,
            observer: SubmissionObserver::Async(submission),
            _core: Arc::clone(&self.context.core),
        };
        // Validation grants no admission. Recheck atomically at publication:
        // another context may have filled the FIFO, reserved CPU access, or
        // reported a device failure while this context was preparing work.
        let Some(mut pending) = self.context.core.pending.try_lock() else {
            return Err(work.unaccepted(None));
        };
        if let Some(error) = pending.failure {
            drop(pending);
            return Err(work.unaccepted(Some(error)));
        }
        if !pending.queue.has_capacity() {
            drop(pending);
            return Err(work.unaccepted(None));
        }
        if let Err(work) = pending.queue.push(work) {
            drop(pending);
            return Err(work.unaccepted(None));
        }
        drop(pending);
        WORKER_WAKER.wake_one();
        Ok(())
    }
}

impl A618Backend {
    fn create_resource(
        &self,
        paddr: u64,
        allocation_size: u64,
        allowed_access: u32,
        linear_image: Option<LinearImage>,
    ) -> Result<A618Resource, &'static str> {
        let (gpu_va, mapping, physical_spans) = self.core.map_backing(paddr, allocation_size)?;
        let token = self.core.allocate_resource_token()?;
        let entry = Arc::new(ResourceEntry {
            token,
            gpu_va,
            allocation_size,
            allowed_access,
            linear_image,
            physical_spans,
            _mapping: ResourceMapping::new(Arc::clone(&self.core.resource_mapping_gate), mapping),
        });
        self.core.register_resource(Arc::clone(&entry))?;
        Ok(A618Resource {
            core: Arc::clone(&self.core),
            entry,
        })
    }

    fn create_image_resource(
        &self,
        backing: &GpuImageBackingInfo,
        allowed_access: u32,
        linear_image: LinearImage,
    ) -> Result<A618Resource, &'static str> {
        let (gpu_va, mapping, physical_spans) = self.core.map_image_backing(backing)?;
        let token = self.core.allocate_resource_token()?;
        let entry = Arc::new(ResourceEntry {
            token,
            gpu_va,
            allocation_size: backing.allocation_size,
            allowed_access,
            linear_image: Some(linear_image),
            physical_spans,
            _mapping: ResourceMapping::new(Arc::clone(&self.core.resource_mapping_gate), mapping),
        });
        self.core.register_resource(Arc::clone(&entry))?;
        Ok(A618Resource {
            core: Arc::clone(&self.core),
            entry,
        })
    }
}

impl GpuBackend for A618Backend {
    fn import_shared_image(
        &self,
        image: Arc<scarlet::device::graphics::shared_image::SharedImage>,
        color: scarlet::device::graphics::shared_image::ImageColor,
    ) -> Result<(Arc<dyn GpuBackendImage>, GpuBackendImageLayout), &'static str> {
        use scarlet::device::gpu::{GPU_IMAGE_FORMAT_NV12, GpuBackendImagePlaneLayout};
        use scarlet::device::graphics::shared_image::*;
        let d = image.descriptor();
        d.validate()?;
        if color.reserved != [0; 2]
            || d.format != IMAGE_FORMAT_NV12
            || d.modifier != 0
            || d.buffer_count != 1
            || d.visible.x != 0
            || d.visible.y != 0
            || d.visible.width != d.width
            || d.visible.height != d.height
            || d.width % 2 != 0
            || d.height % 2 != 0
            || !matches!(color.matrix, COLOR_MATRIX_BT601 | COLOR_MATRIX_BT709)
            || !matches!(color.range, COLOR_RANGE_LIMITED | COLOR_RANGE_FULL)
            || !matches!(color.chroma_x, CHROMA_COSITED | CHROMA_MIDPOINT)
            || !matches!(color.chroma_y, CHROMA_COSITED | CHROMA_MIDPOINT)
        {
            return Err("qcom-adreno-a618: unsupported NV12 import");
        }
        let y = d.planes[0];
        let uv = d.planes[1];
        if y.buffer_index != 0
            || uv.buffer_index != 0
            || y.offset != 0
            || y.row_pitch % 64 != 0
            || uv.row_pitch != y.row_pitch
            || y.size != u64::from(y.row_pitch) * u64::from(d.height)
            || uv.size != u64::from(uv.row_pitch) * u64::from(d.height / 2)
            || uv.offset < y.size
            || uv.offset % 4096 != 0
        {
            return Err("qcom-adreno-a618: unsupported NV12 plane layout");
        }
        let total = d.buffer_sizes[0];
        let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
        for (index, (out, plane)) in planes.iter_mut().zip(d.planes.iter()).take(2).enumerate() {
            *out = GpuBackendImagePlaneLayout {
                offset: plane.offset,
                row_pitch: plane.row_pitch,
                array_pitch: plane.size as u32,
                size: plane.size,
                block_width: if index == 0 { 1 } else { 2 },
                block_height: if index == 0 { 1 } else { 2 },
                bytes_per_block: if index == 0 { 1 } else { 2 },
            };
        }
        let layout = GpuBackendImageLayout {
            modifier: 0,
            total_size: total,
            alignment: 4096,
            plane_count: 2,
            planes,
        };
        let backing = GpuImageBackingInfo::new_segmented(image.backing().buffer_segments(0), total);
        let create = GpuImageCreateInfo {
            format: GPU_IMAGE_FORMAT_NV12,
            usage: GPU_IMAGE_USAGE_SAMPLED,
            width: d.width,
            height: d.height,
            mip_levels: 1,
            array_layers: 1,
            cube: false,
        };
        let resource = self.create_image_resource(
            &backing,
            adreno_a6xx_submit_wire::ACCESS_READ,
            LinearImage {
                texture: None,
                format: LinearImageFormat::Nv12 {
                    uv_offset: uv.offset,
                    uv_size: uv.size,
                },
                modifier: ImageModifier::Linear,
                width: d.width,
                height: d.height,
                row_pitch: y.row_pitch,
                visible_size: total,
            },
        )?;
        // GpuImageResource pins the shared producer lease through accepted submissions.
        Ok((
            Arc::new(A618Image {
                resource,
                create,
                layout,
            }),
            layout,
        ))
    }

    fn supports_segmented_image_backing(&self) -> bool {
        true
    }

    fn query_info(&self) -> scarlet::device::gpu::GpuBackendInfo {
        let ready = match self.core.ensure_hardware_ready() {
            Ok(()) => {
                self.core.register_frequency_policy();
                true
            }
            Err(error) => {
                let (class, detail) = match error {
                    GpuBackendSubmitError::Rejected(detail) => ("rejected", detail),
                    GpuBackendSubmitError::Unavailable(detail) => ("unavailable", detail),
                    GpuBackendSubmitError::DeviceLost(detail) => ("device-lost", detail),
                };
                println!("[a618] hardware class={}", class);
                println!("[a618] {}", detail);
                false
            }
        };
        scarlet::device::gpu::GpuBackendInfo::new(
            GpuDeviceInfo::new(
                if ready {
                    GpuDeviceState::Ready
                } else if self.core.hardware.lock().boot == BootState::Lost {
                    GpuDeviceState::Lost
                } else {
                    GpuDeviceState::Unavailable
                },
                if ready {
                    GPU_EXECUTION_SUPPORT_ADDRESS_SPACE
                        | GPU_EXECUTION_SUPPORT_MEMORY
                        | GPU_EXECUTION_SUPPORT_QUEUE
                        | GPU_EXECUTION_SUPPORT_TIMELINE
                        | GPU_EXECUTION_SUPPORT_PRESENTATION
                        | GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD
                        | GPU_EXECUTION_SUPPORT_IMAGE_READBACK
                        | GPU_EXECUTION_SUPPORT_IMAGE_MIPS
                        | GPU_EXECUTION_SUPPORT_TEXTURE_ARRAYS
                        | scarlet::device::gpu::GPU_EXECUTION_SUPPORT_TIMESTAMPS
                } else {
                    0
                },
                if ready {
                    A618_MAX_OPAQUE_COMMAND_SIZE
                } else {
                    0
                },
            ),
            0,
            BACKEND_ID,
            if ready {
                b"A618 CoachZ linear BGRA8 + TILE6_3 D32"
            } else {
                b"A618 unavailable"
            },
        )
    }

    fn query_dialect(&self, index: u32) -> Result<GpuBackendDialectInfo, &'static str> {
        if index != 0 {
            return Err("qcom-adreno-a618: dialect index is unavailable");
        }
        Ok(GpuBackendDialectInfo::new(0, DIALECT_TOKEN, DIALECT_ID))
    }

    fn create_context(
        &self,
        dialect: GpuBackendDialectDescriptor,
    ) -> Result<Arc<dyn GpuBackendContext>, &'static str> {
        if dialect.index != 0 || dialect.token != DIALECT_TOKEN {
            return Err("qcom-adreno-a618: dialect descriptor does not match");
        }
        // Canonical shader mappings are immutable after context creation.
        // Async enqueue must never wait for a mapping change or GPU execution.
        self.core.ensure_shader_pack()?;
        register_worker(&self.core)?;
        let id = allocate_monotonic(
            &NEXT_CONTEXT_ID,
            "qcom-adreno-a618: context ID space exhausted",
        )?;
        #[cfg(debug_assertions)]
        if let Some(task) = scarlet::task::mytask() {
            println!(
                "[a618-context] create id={} task={} tgid={}",
                id,
                task.get_id(),
                task.get_thread_group_id(),
            );
        } else {
            println!("[a618-context] create id={} task=0 tgid=0", id);
        }
        Ok(Arc::new(A618Context {
            inner: Arc::new(A618ContextInner {
                core: Arc::clone(&self.core),
                id,
                next_attachment: AtomicU64::new(1),
                attachments: IrqSpinLock::new(Vec::new()),
                vm: Mutex::new(None),
                execution: Mutex::new(()),
            }),
        }))
    }

    fn plan_image(
        &self,
        create: GpuImageCreateInfo,
    ) -> Result<GpuBackendImageLayout, &'static str> {
        if !matches!(
            create.format,
            GPU_IMAGE_FORMAT_BGRA8_UNORM
                | GPU_IMAGE_FORMAT_DEPTH32_FLOAT
                | GPU_IMAGE_FORMAT_RGBA16_FLOAT
                | GPU_IMAGE_FORMAT_RGBA32_FLOAT
        ) {
            return Err("qcom-adreno-a618: image format is not supported");
        }
        let texel_bytes = match create.format {
            GPU_IMAGE_FORMAT_RGBA16_FLOAT => 8,
            GPU_IMAGE_FORMAT_RGBA32_FLOAT => 16,
            _ => 4,
        };
        if texel_bytes != 4 && create.usage & GPU_IMAGE_USAGE_PRESENTABLE != 0 {
            return Err("qcom-adreno-a618: float images cannot be presented");
        }
        // Keep scanout, CPU transfers, and mip/array textures linear. Internal
        // render-and-sample attachments benefit from CCU/texture tile locality
        // without any detiling copy or a change to blending semantics.
        let tiled_color = create.format == GPU_IMAGE_FORMAT_BGRA8_UNORM
            && create.usage == (GPU_IMAGE_USAGE_RENDER_TARGET | GPU_IMAGE_USAGE_SAMPLED)
            && create.mip_levels == 1
            && create.array_layers == 1
            && create.width >= 64;
        if create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT || tiled_color {
            let tiled = depth32_tile6_3_layout(create.width, create.height)
                .ok_or("qcom-adreno-a618: tiled depth layout overflows")?;
            let array_pitch = u32::try_from(tiled.layer_size)
                .map_err(|_| "qcom-adreno-a618: depth layer pitch exceeds the backend ABI")?;
            let total_size = tiled
                .layer_size
                .checked_mul(u64::from(create.array_layers))
                .ok_or("qcom-adreno-a618: depth array size overflows")?;
            let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
            planes[0] = GpuBackendImagePlaneLayout {
                offset: 0,
                size: total_size,
                row_pitch: tiled.row_pitch,
                array_pitch,
                block_width: 1,
                block_height: 1,
                bytes_per_block: 4,
            };
            return Ok(GpuBackendImageLayout {
                modifier: if tiled_color {
                    IMAGE_MODIFIER_TILE6_3_COLOR
                } else {
                    IMAGE_MODIFIER_TILE6_3_DEPTH
                },
                total_size,
                alignment: DEPTH32_LAYER_ALIGNMENT,
                plane_count: 1,
                planes,
            });
        }
        if create.mip_levels > 1 || create.array_layers > 1 {
            let texture = adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(
                create.width,
                create.height,
                create.mip_levels,
                create.array_layers,
                texel_bytes,
            )
            .ok_or("qcom-adreno-a618: texture layout overflows")?;
            let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
            planes[0] = GpuBackendImagePlaneLayout {
                offset: 0,
                size: texture.total_size,
                row_pitch: texture.row_pitch,
                array_pitch: texture.layer_stride,
                block_width: 1,
                block_height: 1,
                bytes_per_block: texel_bytes as u16,
            };
            return Ok(GpuBackendImageLayout {
                modifier: 0,
                total_size: texture.total_size,
                alignment: PAGE_SIZE as u64,
                plane_count: 1,
                planes,
            });
        }
        let row_bytes = create
            .width
            .checked_mul(texel_bytes)
            .ok_or("qcom-adreno-a618: image row size overflows")?;
        let row_pitch = row_bytes
            .checked_add(63)
            .map(|value| value & !63)
            .ok_or("qcom-adreno-a618: image row pitch overflows")?;
        if create.width == 0 || create.height == 0 {
            return Err("qcom-adreno-a618: image extent is empty");
        }
        let size = u64::from(row_pitch)
            .checked_mul(u64::from(create.height))
            .ok_or("qcom-adreno-a618: image layout size overflows")?;
        let array_pitch = u32::try_from(size)
            .map_err(|_| "qcom-adreno-a618: image layer pitch exceeds the backend ABI")?;
        // The generic variable-texel constructor is private to Scarlet. Build
        // the public layout with the checked native byte pitch instead.
        let mut planes = [GpuBackendImagePlaneLayout::EMPTY; 4];
        planes[0] = GpuBackendImagePlaneLayout {
            offset: 0,
            size,
            row_pitch,
            array_pitch,
            block_width: 1,
            block_height: 1,
            bytes_per_block: texel_bytes as u16,
        };
        Ok(GpuBackendImageLayout {
            modifier: 0,
            total_size: size,
            alignment: PAGE_SIZE as u64,
            plane_count: 1,
            planes,
        })
    }

    fn create_image_with_layout(
        &self,
        create: GpuImageCreateInfo,
        layout: GpuBackendImageLayout,
        backing: GpuImageBackingInfo,
    ) -> Result<Arc<dyn GpuBackendImage>, &'static str> {
        let planned = self.plan_image(create)?;
        if layout != planned || backing.allocation_size < layout.total_size {
            return Err("qcom-adreno-a618: image backing does not match its layout plan");
        }
        Ok(Arc::new(A618Image {
            resource: self.create_image_resource(
                &backing,
                (if create.usage
                    & (GPU_IMAGE_USAGE_PRESENTABLE
                        | GPU_IMAGE_USAGE_SAMPLED
                        | GPU_IMAGE_USAGE_TRANSFER_SRC)
                    != 0
                {
                    adreno_a6xx_submit_wire::ACCESS_READ
                } else {
                    0
                }) | (if create.usage
                    & (GPU_IMAGE_USAGE_RENDER_TARGET
                        | GPU_IMAGE_USAGE_TRANSFER_DST
                        | GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT)
                    != 0
                {
                    adreno_a6xx_submit_wire::ACCESS_WRITE
                } else {
                    0
                }) | (if create.usage & GPU_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT != 0 {
                    adreno_a6xx_submit_wire::ACCESS_READ
                } else {
                    0
                }),
                LinearImage {
                    texture: if create.format != GPU_IMAGE_FORMAT_DEPTH32_FLOAT
                        && (create.mip_levels > 1 || create.array_layers > 1)
                    {
                        adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(
                            create.width,
                            create.height,
                            create.mip_levels,
                            create.array_layers,
                            match create.format {
                                GPU_IMAGE_FORMAT_RGBA16_FLOAT => 8,
                                GPU_IMAGE_FORMAT_RGBA32_FLOAT => 16,
                                _ => 4,
                            },
                        )
                    } else {
                        None
                    },
                    format: match create.format {
                        GPU_IMAGE_FORMAT_DEPTH32_FLOAT => LinearImageFormat::Depth32Float,
                        GPU_IMAGE_FORMAT_RGBA16_FLOAT => LinearImageFormat::Rgba16Float,
                        GPU_IMAGE_FORMAT_RGBA32_FLOAT => LinearImageFormat::Rgba32Float,
                        _ => LinearImageFormat::Bgra8Unorm,
                    },
                    modifier: if create.format == GPU_IMAGE_FORMAT_DEPTH32_FLOAT {
                        ImageModifier::Tile6_3Depth
                    } else if layout.modifier == IMAGE_MODIFIER_TILE6_3_COLOR {
                        ImageModifier::Tile6_3Color
                    } else {
                        ImageModifier::Linear
                    },
                    width: create.width,
                    height: create.height,
                    row_pitch: layout.planes[0].row_pitch,
                    // A relocation exposes exactly the immutable plane
                    // footprint. Linear color excludes backing tail padding;
                    // TILE6_3 depth includes its padded rows and layer tail.
                    visible_size: if create.mip_levels > 1
                        || create.array_layers > 1
                        || layout.modifier != 0
                    {
                        layout.planes[0].size
                    } else {
                        u64::from(layout.planes[0].row_pitch)
                            .checked_mul(u64::from(create.height))
                            .ok_or("qcom-adreno-a618: image visible size overflows")?
                    },
                },
            )?,
            create,
            layout,
        }))
    }

    fn create_buffer(
        &self,
        create: GpuBufferCreateInfo,
    ) -> Result<Arc<dyn GpuBackendBuffer>, &'static str> {
        Ok(Arc::new(A618Buffer {
            resource: self.create_resource(
                create.paddr,
                create.allocation_size,
                adreno_a6xx_submit_wire::ACCESS_READ | adreno_a6xx_submit_wire::ACCESS_WRITE,
                None,
            )?,
        }))
    }
}

struct RegisteredGpu {
    phandle: u32,
    device_id: usize,
    _core: Arc<A618Core>,
}

static GPUS: IrqSpinLock<Vec<RegisteredGpu>> = IrqSpinLock::new(Vec::new());

fn property_phandle(device: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    let bytes = device.property(name)?.value();
    let value = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    (value != 0).then_some(value)
}

fn own_phandle(device: &PlatformDeviceInfo) -> Option<u32> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
}

pub(crate) fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let gmu_phandle = property_phandle(device, "qcom,gmu")
        .or_else(|| property_phandle(device, "gmu"))
        .ok_or("qcom-adreno-a618: GPU is missing its GMU phandle")?;
    let gmu = match gmu::get_by_phandle(gmu_phandle) {
        Some(gmu) => gmu,
        None => return probe_defer(),
    };
    let gpu_operating_points = read_gpu_operating_points(device)?;
    let peak_kbps = gpu_operating_points
        .iter()
        .filter_map(|point| point.peak_kbps)
        .max()
        .ok_or("qcom-adreno-a618: GPU OPP table has no peak bandwidth")?;
    let interconnect = qcom_sc7180_interconnect::gpu_memory_path(device)?;
    gmu.lock()
        .configure_gpu_operating_points(gpu_operating_points)?;
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-adreno-a618: missing GPU register resource")?;
    let interrupt_resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("qcom-adreno-a618: missing GPU interrupt resource")?;
    let interrupt_id = scarlet::interrupt::resolve_platform_irq(interrupt_resource)
        .map_err(|_| "qcom-adreno-a618: failed to resolve GPU interrupt")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-adreno-a618: GPU register resource overflows")?;
    if resource_size < GPU_RESOURCE_SIZE as u64 {
        return Err("qcom-adreno-a618: GPU register resource is too small");
    }
    let register_base = vm::ioremap(resource.start, GPU_RESOURCE_SIZE)
        .map_err(|_| "qcom-adreno-a618: failed to map GPU registers")?;
    let result = (|| {
        let dma_context = DeviceManager::get_manager().resolve_platform_dma_context(
            device,
            IommuDomainConfig {
                domain_type: IommuDomainType::Dma,
                iova_base: Iova::new(GPU_IOVA_BASE),
                iova_size: GPU_IOVA_SIZE,
            },
        )?;
        let ring = DmaAllocation::new(&dma_context, RING_SIZE, bidirectional_flags())?;
        let command = DmaAllocation::new(
            &dma_context,
            A618_MAX_OPAQUE_COMMAND_SIZE as usize,
            IommuMapFlags::READ,
        )?;
        let fence = DmaAllocation::new(&dma_context, PAGE_SIZE, bidirectional_flags())?;
        let backend_cookie = allocate_monotonic(
            &NEXT_BACKEND_COOKIE,
            "qcom-adreno-a618: backend cookie space exhausted",
        )?;
        let resource_mapping_gate = Arc::new(MappingGate {
            execution: Mutex::new(()),
            quarantine: AtomicBool::new(false),
        });
        let core = Arc::new(A618Core {
            registers: DwordRegisters::new(register_base),
            register_base,
            interrupt_id,
            interrupt_snapshot: IrqSpinLock::new(RingInterruptSnapshot::default()),
            interrupt_waker: Arc::new(Waker::new_uninterruptible("a618-submit")),
            interrupt_wait_armed: AtomicBool::new(false),
            dma_context,
            interconnect,
            peak_kbps,
            gmu,
            hardware: Mutex::new(HardwareState {
                boot: BootState::Cold,
                ring,
                command,
                sqe: None,
                shader_pack: None,
                fence,
                wptr: 0,
                fence_sequence: 0,
                gpu_oob_held: false,
                quiesce_failed: false,
                lost_reason: None,
                last_ring_failure: None,
                execution_domain: None,
                idle_leases: isolation::IdleLeases::new(),
            }),
            resource_mapping_gate,
            shader_pack_address: AtomicU64::new(0),
            acknowledged_frequency_khz: AtomicU64::new(0),
            pending: Mutex::new(PendingState {
                queue: PendingQueue::try_new(ASYNC_CAPACITY)?,
                failure: None,
            }),
            capacity_waker: Waker::new_uninterruptible("a618-capacity"),
            resources: IrqSpinLock::new(Vec::new()),
            next_resource_token: AtomicU64::new(1),
            backend_cookie,
            frequency_policy_registered: Mutex::new(false),
        });
        scarlet::interrupt::register_and_enable_platform_irq_device(
            interrupt_resource,
            core.clone(),
            arch::get_cpu().get_cpuid() as u32,
        )
        .map_err(|_| "qcom-adreno-a618: failed to register GPU interrupt")?;
        let (device_id, gpu_name) = register_gpu_control_device(Arc::new(A618Backend {
            core: Arc::clone(&core),
        }))?;
        let phandle = own_phandle(device).unwrap_or(0);
        GPUS.lock().push(RegisteredGpu {
            phandle,
            device_id,
            _core: core,
        });
        println!(
            "[qcom-adreno-a618] registered lazy CoachZ GPU backend as {} paddr={:#x} irq={}",
            gpu_name, resource.start, interrupt_id,
        );
        Ok(())
    })();
    if result.is_err() {
        vm::iounmap(register_base);
    }
    result
}

pub(crate) fn remove(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = own_phandle(device).unwrap_or(0);
    let mut gpus = GPUS.lock();
    let index = gpus
        .iter()
        .position(|gpu| gpu.phandle == phandle)
        .ok_or("qcom-adreno-a618: GPU was not registered")?;
    let gpu = gpus.swap_remove(index);
    DeviceManager::get_manager().unregister_device(gpu.device_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use adreno_a6xx_pm4::{opcode, type4, type7};

    use super::{
        A618Core, CP_EVENT_WRITE_IRQ, CP_INDIRECT_BUFFER, EVENT_CACHE_FLUSH_TS,
        EVENT_CACHE_INVALIDATE, EVENT_CCU_INVALIDATE_COLOR, EVENT_CCU_INVALIDATE_DEPTH,
        completion_commands, submission_commands,
    };
    use crate::registers::CP_SCRATCH_2;

    #[test]
    fn checks_a630_sqe_version_after_the_linux_firmware_host_header() {
        // First four bytes are the host-only word stripped by Linux's
        // adreno_fw_create_bo().  The payload then starts with v2.07.
        let firmware_prefix = [
            0x00, 0x00, 0x00, 0x00, 0x07, 0xe2, 0x6e, 0x01, 0xe2, 0x20, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01,
        ];
        assert!(!A618Core::sqe_version_is_secure(&firmware_prefix));
        assert!(A618Core::sqe_version_is_secure(&firmware_prefix[4..]));
    }

    #[test]
    fn completion_commands_match_the_polled_a6xx_fence_sequence() {
        let commands = completion_commands(0x1_2345_6000, 7).unwrap();
        assert_eq!(commands[0], type4(CP_SCRATCH_2 as u32, 1).unwrap());
        assert_eq!(commands[1], 7);
        assert_eq!(commands[2], type7(opcode::EVENT_WRITE, 4).unwrap());
        assert_eq!(commands[3], EVENT_CACHE_FLUSH_TS | CP_EVENT_WRITE_IRQ);
        assert_eq!(commands[4], 0x2345_6000);
        assert_eq!(commands[5], 1);
        assert_eq!(commands[6], 7);
    }

    #[test]
    fn submission_commands_match_the_linux_a6xx_trusted_preamble() {
        let commands = submission_commands(0x1_2345_6000, 0x123).unwrap();
        assert_eq!(commands[0], type7(opcode::EVENT_WRITE, 1).unwrap());
        assert_eq!(commands[1], EVENT_CACHE_INVALIDATE);
        assert_eq!(commands[2], type7(opcode::EVENT_WRITE, 1).unwrap());
        assert_eq!(commands[3], EVENT_CCU_INVALIDATE_DEPTH);
        assert_eq!(commands[4], type7(opcode::EVENT_WRITE, 1).unwrap());
        assert_eq!(commands[5], EVENT_CCU_INVALIDATE_COLOR);
        assert_eq!(commands[6], type7(CP_INDIRECT_BUFFER, 3).unwrap());
        assert_eq!(commands[7], 0x2345_6000);
        assert_eq!(commands[8], 1);
        assert_eq!(commands[9], 0x123);
        assert_eq!(commands.len(), 10);
    }
}
