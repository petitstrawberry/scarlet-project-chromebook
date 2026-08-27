// SPDX-License-Identifier: GPL-2.0-only

//! Scarlet generic GPU backend for the SC7180 Adreno 618.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use scarlet::{
    arch,
    device::{
        gpu::{
            GPU_EXECUTION_SUPPORT_ADDRESS_SPACE, GPU_EXECUTION_SUPPORT_IMAGE_UPLOAD,
            GPU_EXECUTION_SUPPORT_MEMORY, GPU_EXECUTION_SUPPORT_PRESENTATION,
            GPU_EXECUTION_SUPPORT_QUEUE, GPU_EXECUTION_SUPPORT_TIMELINE,
            GPU_IMAGE_FORMAT_BGRA8_UNORM, GPU_IMAGE_USAGE_PRESENTABLE,
            GPU_IMAGE_USAGE_RENDER_TARGET, GPU_IMAGE_USAGE_SAMPLED, GPU_IMAGE_USAGE_TRANSFER_DST,
            GPU_MAX_OPAQUE_COMMAND_SIZE, GpuBackend, GpuBackendBuffer, GpuBackendBufferInfo,
            GpuBackendContext, GpuBackendContextInfo, GpuBackendDialectDescriptor,
            GpuBackendDialectInfo, GpuBackendImage, GpuBackendImageInfo, GpuBackendImageLayout,
            GpuBackendLinearDisplayInfo, GpuBackendQueue, GpuBackendQueueInfo,
            GpuBackendSubmitError, GpuBufferCreateInfo, GpuDeviceInfo, GpuDeviceState,
            GpuImageBackingInfo, GpuImageCreateInfo, GpuImageUploadInfo,
            register_gpu_control_device,
        },
        graphics::{GpuDisplayResource, PixelFormat},
        iommu::{DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags},
        manager::{DeviceManager, probe_defer},
        platform::{PlatformDeviceInfo, resource::PlatformDeviceResourceType},
    },
    early_println,
    environment::PAGE_SIZE,
    sync::{IrqSpinLock, Mutex},
    time, vm,
};

use adreno_a6xx_pm4::{opcode, type4, type7};
use adreno_a6xx_shader_pack::{PACK_SIZE, SHADER_ALIGNMENT, SHADER_SIZE, ShaderVariant, copy_pack};

use crate::{
    firmware,
    gmu::{self, A618Gmu},
    memory::{DmaAllocation, bidirectional_flags},
    registers::*,
    submit::{LinearImage, ResolvedResource, diagnose_rejected_packet, validate_and_relocate},
};

const GPU_IOVA_BASE: u64 = 0x1_0000_0000;
const GPU_IOVA_SIZE: u64 = 0x1_0000_0000;
const DIALECT_TOKEN: u64 = 0x4136_5858_0001_0000;
const BACKEND_ID: &[u8] = b"qcom-adreno";
const DIALECT_ID: &[u8] = b"adreno-a6xx-pm4-reloc-v1";
const SQE_FIRMWARE_MAX_SIZE: usize = 256 * 1024;
const RING_SIZE: usize = 32 * 1024;
const GMEM_BASE: u64 = 0x10_0000;
const GMEM_SIZE: u64 = 512 * 1024;
const GPU_TIMEOUT_US: u64 = 1_000_000;
const GPU_QUIESCE_TIMEOUT_US: u64 = 10_000;
const GPU_INTERRUPT_CLEAR_TIMEOUT_US: u64 = 1_000;
const CP_INDIRECT_BUFFER: u8 = 0x3f;
const EVENT_CACHE_FLUSH_TS: u32 = 0x04;
const EVENT_CCU_INVALIDATE_DEPTH: u32 = 0x18;
const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
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
    let wait_mem_writes_header = type7(opcode::WAIT_MEM_WRITES, 0)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel memory-write barrier")?;
    let wait_for_me_header = type7(opcode::WAIT_FOR_ME, 0)
        .map_err(|_| "qcom-adreno-a618: failed to encode kernel ME barrier")?;

    // Linux's A6xx submit path unconditionally invalidates both CCUs before
    // entering a userspace IB.  After the IB, serialize asynchronous CP memory
    // writes and then make the parser wait for the micro-engine.  Mesa uses the
    // same WAIT_MEM_WRITES -> WAIT_FOR_ME order before a following memory
    // signal; without the front-end barrier, CACHE_FLUSH_TS can overtake the ME.
    Ok([
        event_header,
        EVENT_CCU_INVALIDATE_DEPTH,
        event_header,
        EVENT_CCU_INVALIDATE_COLOR,
        indirect_header,
        command_address as u32,
        (command_address >> 32) as u32,
        word_count,
        wait_mem_writes_header,
        wait_for_me_header,
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
    ib1_base: u64,
    ib1_translation: Option<usize>,
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
    fence_paddr: usize,
    fence_translation: Option<usize>,
    fence: Option<(u32, u32)>,
}

struct HardwareState {
    boot: BootState,
    ring: DmaAllocation,
    sqe: Option<DmaAllocation>,
    shader_pack: Option<DmaAllocation>,
    fence: DmaAllocation,
    wptr: u32,
    fence_sequence: u32,
    gpu_oob_held: bool,
    quiesce_failed: bool,
    lost_reason: Option<&'static str>,
    last_ring_failure: Option<RingFailureSnapshot>,
}

struct ResourceEntry {
    token: u64,
    gpu_va: u64,
    allocation_size: u64,
    allowed_access: u32,
    linear_image: Option<LinearImage>,
    _mapping: DmaMapping,
}

struct A618Core {
    registers: DwordRegisters,
    register_base: usize,
    dma_context: DmaContext,
    gmu: Arc<Mutex<A618Gmu>>,
    hardware: Mutex<HardwareState>,
    resources: IrqSpinLock<Vec<Arc<ResourceEntry>>>,
    next_resource_token: AtomicU64,
    backend_cookie: u64,
}

impl Drop for A618Core {
    fn drop(&mut self) {
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

impl A618Core {
    fn ensure_shader_pack(&self) -> Result<(), &'static str> {
        let mut hardware = self.hardware.lock();
        if hardware.shader_pack.is_some() {
            return Ok(());
        }
        let mut pack = DmaAllocation::new(&self.dma_context, PACK_SIZE, IommuMapFlags::READ)?;
        if pack.dma_addr() as usize & (SHADER_ALIGNMENT - 1) != 0 || !copy_pack(pack.as_bytes_mut())
        {
            return Err("qcom-adreno-a618: canonical shader pack layout is invalid");
        }
        pack.clean_for_device();
        hardware.shader_pack = Some(pack);
        Ok(())
    }

    fn resolve_shader(&self, variant: ShaderVariant) -> Option<ResolvedResource> {
        let hardware = self.hardware.lock();
        let pack = hardware.shader_pack.as_ref()?;
        let offset = variant.offset();
        let end = offset.checked_add(SHADER_SIZE)?;
        if end > pack.requested_size() {
            return None;
        }
        Some(ResolvedResource {
            attachment_token: 0,
            gpu_va: pack.dma_addr().checked_add(offset as u64)?,
            allocation_size: SHADER_SIZE as u64,
            allowed_access: adreno_a6xx_submit_wire::ACCESS_READ,
            linear_image: None,
        })
    }

    fn fail_stop_teardown(reason: &'static str) -> ! {
        early_println!(
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
        if let Some(index) = resources.iter().position(|entry| entry.token == token) {
            resources.swap_remove(index);
        }
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
        paddr: usize,
        allocation_size: u64,
    ) -> Result<(u64, DmaMapping), &'static str> {
        let allocation_size = usize::try_from(allocation_size)
            .map_err(|_| "qcom-adreno-a618: resource allocation exceeds kernel address size")?;
        if paddr == 0 || allocation_size == 0 || paddr & (PAGE_SIZE - 1) != 0 {
            return Err("qcom-adreno-a618: resource backing is invalid");
        }
        let mapping = self
            .dma_context
            // A618 advertises cached-coherent system memory. External CPU-mapped
            // resources therefore use coherent SMMU PTEs; transient command
            // allocations remain explicitly cache-maintained instead.
            .map_phys_owned(
                paddr,
                allocation_size,
                IommuMapFlags::READ | IommuMapFlags::WRITE | IommuMapFlags::COHERENT,
            )
            .map_err(|_| "qcom-adreno-a618: resource IOMMU mapping failed")?;
        Ok((mapping.dma_addr(), mapping))
    }

    fn initialize_nonprivileged_mmio(&self, hardware: &HardwareState) {
        let registers = self.registers;
        registers.write(RBBM_INT_0_MASK, 0);
        registers.write(RBBM_INT_CLEAR_CMD, u32::MAX);
        arch::io_wmb();
        // Scarlet keeps the platform IRQ disabled and polls the RBBM status,
        // but A6xx still needs the same internal completion source enabled as
        // Linux so CACHE_FLUSH_TS can be observed and W1C-acknowledged before
        // the next submit.
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
        early_println!(
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
        for word in words {
            ring_words[hardware.wptr as usize] = *word;
            hardware.wptr = (hardware.wptr + 1) & (capacity as u32 - 1);
        }
        hardware.ring.clean_for_device();
        self.clear_ring_interrupts()?;
        self.registers.write(CP_RB_WPTR, hardware.wptr);
        Ok(hardware.wptr)
    }

    /// Establish an unambiguous completion edge before advancing the ring.
    ///
    /// The userspace validator forbids CACHE_FLUSH_TS, so bit 20 can only be
    /// raised by the kernel-owned completion event appended by `execute_ring`.
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
    fn consume_ring_interrupts(&self) -> Result<u32, u32> {
        let interrupt = self.registers.read(RBBM_INT_0_STATUS);
        let cp_interrupt = self.registers.read(CP_INTERRUPT_STATUS);
        if interrupt != 0 {
            self.registers.write(RBBM_INT_CLEAR_CMD, interrupt);
            // Commit the W1C before a following submit can report another
            // completion or fault through the same shared status register.
            arch::io_wmb();
        }
        let completion = interrupt & RBBM_INT_COMPLETION_MASK;
        let fatal = (interrupt & !completion) & RBBM_INT_FATAL_MASK;
        if fatal != 0 || cp_interrupt & CP_INT_FATAL_MASK != 0 {
            Err(interrupt)
        } else {
            Ok(interrupt)
        }
    }

    fn capture_ring_failure(
        &self,
        hardware: &HardwareState,
        reason: &'static str,
        sequence: u32,
        command_words: usize,
        target_wptr: u32,
        interrupt: u32,
        fence: Option<(u32, u32)>,
    ) -> RingFailureSnapshot {
        let cp_interrupt = self.registers.read(CP_INTERRUPT_STATUS);
        let ib1_base = self.registers.read64(CP_IB1_BASE);
        RingFailureSnapshot {
            reason,
            sequence,
            command_words,
            interrupt,
            rptr: self.registers.read(CP_RB_RPTR),
            wptr: self.registers.read(CP_RB_WPTR),
            target_wptr,
            status: self.registers.read(RBBM_STATUS),
            status1: self.registers.read(RBBM_STATUS1),
            status2: self.registers.read(RBBM_STATUS2),
            status3: self.registers.read(RBBM_STATUS3),
            cp_interrupt,
            cp_hw_fault: if cp_interrupt != 0 {
                self.registers.read(CP_HW_FAULT)
            } else {
                0
            },
            cp_protect_status: if cp_interrupt != 0 {
                self.registers.read(CP_PROTECT_STATUS)
            } else {
                0
            },
            ib1_base,
            ib1_translation: self
                .dma_context
                .iommu
                .as_ref()
                .and_then(|attachment| attachment.domain.iova_to_phys(ib1_base)),
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
                .and_then(|attachment| attachment.domain.iova_to_phys(hardware.fence.dma_addr())),
            fence,
        }
    }

    fn print_ring_failure(snapshot: RingFailureSnapshot) {
        early_println!("[a618] ring failure={}", snapshot.reason);
        early_println!(
            "[a618] submit seq={} words={}",
            snapshot.sequence,
            snapshot.command_words,
        );
        early_println!(
            "[a618] irq={:#010x} complete={:#010x}",
            snapshot.interrupt,
            snapshot.interrupt & RBBM_INT_COMPLETION_MASK,
        );
        early_println!(
            "[a618] fatal={:#010x} cp-int={:#010x}",
            snapshot.interrupt & RBBM_INT_FATAL_MASK,
            snapshot.cp_interrupt,
        );
        early_println!(
            "[a618] rb rptr={:#x} wptr={:#x} target={:#x}",
            snapshot.rptr,
            snapshot.wptr,
            snapshot.target_wptr,
        );
        early_println!(
            "[a618] status={:#010x} status1={:#010x} status2={:#010x} status3={:#010x}",
            snapshot.status,
            snapshot.status1,
            snapshot.status2,
            snapshot.status3,
        );
        if snapshot.status3 & RBBM_STATUS3_SMMU_STALLED_ON_FAULT != 0 {
            early_println!("[a618] SMMU stalled on fault");
        }
        if snapshot.cp_interrupt != 0 {
            early_println!(
                "[a618] cp fault={:#010x} protect={:#010x}",
                snapshot.cp_hw_fault,
                snapshot.cp_protect_status,
            );
        }
        early_println!(
            "[a618] ib1={:#x} translated={:#x} rem={:#x}",
            snapshot.ib1_base,
            snapshot.ib1_translation.unwrap_or(0),
            snapshot.ib1_remaining,
        );
        early_println!(
            "[a618] ib2={:#x} rem={:#x}",
            snapshot.ib2_base,
            snapshot.ib2_remaining,
        );
        early_println!(
            "[a618] roq rb={:#010x} ib1={:#010x} ib2={:#010x}",
            snapshot.roq_rb,
            snapshot.roq_ib1,
            snapshot.roq_ib2,
        );
        early_println!(
            "[a618] roq sds={:#010x} mrb={:#010x} vsd={:#010x}",
            snapshot.roq_sds,
            snapshot.roq_mrb,
            snapshot.roq_vsd,
        );
        early_println!("[a618] scratch2={:#x}", snapshot.scratch2);
        early_println!(
            "[a618] fence dma={:#x} paddr={:#x} translated={:#x}",
            snapshot.fence_dma,
            snapshot.fence_paddr,
            snapshot.fence_translation.unwrap_or(0),
        );
        if let Some((actual, expected)) = snapshot.fence {
            early_println!("[a618] fence actual={:#x} expected={:#x}", actual, expected,);
        }
    }

    fn record_ring_failure(
        &self,
        hardware: &mut HardwareState,
        reason: &'static str,
        sequence: u32,
        command_words: usize,
        target_wptr: u32,
        interrupt: u32,
        fence: Option<(u32, u32)>,
    ) {
        let snapshot = self.capture_ring_failure(
            hardware,
            reason,
            sequence,
            command_words,
            target_wptr,
            interrupt,
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
        let mut observed_interrupts = 0;
        loop {
            if self.registers.read(CP_RB_RPTR) == target_wptr
                && self.registers.read(RBBM_STATUS) & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0
            {
                return Ok(());
            }
            match self.consume_ring_interrupts() {
                Ok(interrupt) => observed_interrupts |= interrupt,
                Err(interrupt) => {
                    observed_interrupts |= interrupt;
                    self.record_ring_failure(
                        hardware,
                        "fatal interrupt during ME initialization",
                        0,
                        command_words,
                        target_wptr,
                        observed_interrupts,
                        None,
                    );
                    return Err("qcom-adreno-a618: GPU fault interrupt during synchronous submit");
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
                return Err("qcom-adreno-a618: synchronous GPU fence timed out");
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
        hardware.fence.clean_for_device();
        // Match Linux's A6xx completion source. The addressed timestamp remains
        // useful diagnostic state, but this serialized single-ring backend
        // retires from a fresh RBBM completion bit plus the sequence-specific
        // scratch register. Some SC7180 firmware paths acknowledge the event
        // without making a repeated timestamp write CPU-visible.
        let completion = completion_commands(hardware.fence.dma_addr(), sequence)?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(words.len() + completion.len())
            .map_err(|_| "qcom-adreno-a618: kernel fence command allocation failed")?;
        ring.extend_from_slice(words);
        ring.extend_from_slice(&completion);
        let target_wptr = self.write_ring(hardware, &ring)?;
        let start = time::current_time();
        let mut observed_interrupts = 0;
        loop {
            hardware.fence.invalidate_from_device();
            let fence_value = hardware.fence.read_word_volatile(0).unwrap_or(0);
            match self.consume_ring_interrupts() {
                Ok(interrupt) => observed_interrupts |= interrupt,
                Err(interrupt) => {
                    observed_interrupts |= interrupt;
                    let cp_interrupt = self.registers.read(CP_INTERRUPT_STATUS);
                    let reason = if cp_interrupt & CP_INT_ILLEGAL_INSTR_ERROR != 0 {
                        "CP illegal instruction during submit"
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
                    return Err(if cp_interrupt & CP_INT_ILLEGAL_INSTR_ERROR != 0 {
                        "qcom-adreno-a618: CP illegal instruction during synchronous submit"
                    } else {
                        "qcom-adreno-a618: GPU fault interrupt during synchronous submit"
                    });
                }
            }
            // Drain any status raised alongside the memory fence before
            // returning so the next ring kick starts from a clean status word.
            if observed_interrupts & RBBM_INT_CP_CACHE_FLUSH_TS != 0
                && self.registers.read(CP_SCRATCH_2) == sequence
                && self.registers.read(CP_RB_RPTR) == target_wptr
                && self.registers.read(RBBM_STATUS) & !RBBM_STATUS_CP_AHB_BUSY_CX_MASTER == 0
            {
                if sequence <= 2 {
                    early_println!(
                        "[a618] submit complete seq={} irq={:#010x} scratch={:#010x} fence={:#010x}",
                        sequence,
                        observed_interrupts,
                        self.registers.read(CP_SCRATCH_2),
                        fence_value,
                    );
                }
                return Ok(());
            }
            if time::current_time().saturating_sub(start) >= GPU_TIMEOUT_US {
                self.record_ring_failure(
                    hardware,
                    "synchronous submit timeout",
                    sequence,
                    command_words,
                    target_wptr,
                    observed_interrupts,
                    Some((fence_value, sequence)),
                );
                return Err("qcom-adreno-a618: synchronous GPU fence timed out");
            }
            time::udelay(10);
        }
    }

    fn quiesce_lost(&self, hardware: &mut HardwareState, reason: &'static str) {
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
    }

    fn ensure_hardware_ready(&self) -> Result<(), GpuBackendSubmitError> {
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
        if !Self::coachz_no_zap_configuration() {
            return Err(GpuBackendSubmitError::Unavailable(
                "qcom-adreno-a618: no authenticated zap-shader path exists for this board",
            ));
        }

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
        early_println!("[qcom-adreno-a618] GPU OOB acknowledged");
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
            early_println!(
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
            Ok::<(), &'static str>(())
        })();
        match preparation {
            Ok(()) => {
                gmu.finish_initial_boot_keep_gpu_on();
                hardware.boot = BootState::Ready;
                hardware.lost_reason = None;
                hardware.last_ring_failure = None;
                early_println!("[qcom-adreno-a618] SQE/ring ready in CoachZ no-zap mode");
                Ok(())
            }
            Err(error) => {
                let gpu_stopped =
                    !sqe_started || Self::force_stop_gpu(self.registers, &hardware).is_ok();
                let gmu_stopped = gmu.force_shutdown().is_ok();
                hardware.quiesce_failed = !(gpu_stopped && gmu_stopped);
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

    fn submit_command(&self, command: &DmaAllocation) -> Result<(), GpuBackendSubmitError> {
        let word_count = u32::try_from(command.as_words().len()).map_err(|_| {
            GpuBackendSubmitError::Rejected("qcom-adreno-a618: command stream is too large")
        })?;
        let submission = submission_commands(command.dma_addr(), word_count)
            .map_err(GpuBackendSubmitError::Unavailable)?;
        let mut hardware = self.hardware.lock();
        self.execute_ring(&mut hardware, &submission, command.as_words().len())
            .map_err(|error| {
                self.quiesce_lost(&mut hardware, error);
                GpuBackendSubmitError::DeviceLost(error)
            })
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
}

struct A618ContextInner {
    core: Arc<A618Core>,
    id: u64,
    next_attachment: AtomicU64,
    attachments: IrqSpinLock<Vec<Attachment>>,
    execution: Mutex<()>,
}

impl A618ContextInner {
    fn attach(&self, resource_token: u64) -> Result<u64, &'static str> {
        let resource = self
            .core
            .resource(resource_token)
            .ok_or("qcom-adreno-a618: resource no longer exists")?;
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
        });
        Ok(attachment_token)
    }

    fn detach(&self, resource_token: u64) -> Result<(), &'static str> {
        let mut attachments = self.attachments.lock();
        let index = attachments
            .iter()
            .position(|entry| entry.resource.token == resource_token)
            .ok_or("qcom-adreno-a618: resource is not attached")?;
        attachments.swap_remove(index);
        Ok(())
    }

    fn resolve(&self, attachment_token: u64) -> Option<ResolvedResource> {
        let entry = self
            .attachments
            .lock()
            .iter()
            .find(|entry| entry.attachment_token == attachment_token)?
            .resource
            .clone();
        Some(ResolvedResource {
            attachment_token,
            gpu_va: entry.gpu_va,
            allocation_size: entry.allocation_size,
            allowed_access: entry.allowed_access,
            linear_image: entry.linear_image,
        })
    }
}

struct A618Context {
    inner: Arc<A618ContextInner>,
}

impl A618Context {
    fn validate_resource(&self, backend_cookie: u64, token: u64) -> Result<u64, &'static str> {
        if backend_cookie != self.inner.core.backend_cookie || token == 0 {
            return Err("qcom-adreno-a618: resource belongs to another backend");
        }
        self.inner.attach(token)
    }
}

impl GpuBackendContext for A618Context {
    fn query_info(&self) -> GpuBackendContextInfo {
        GpuBackendContextInfo::new(0, DIALECT_TOKEN)
    }

    fn create_queue(&self) -> Result<Arc<dyn GpuBackendQueue>, &'static str> {
        Ok(Arc::new(A618Queue {
            context: Arc::clone(&self.inner),
        }))
    }

    fn attach_image(&self, image: &dyn GpuBackendImage) -> Result<u64, &'static str> {
        let _execution = self.inner.execution.lock();
        self.validate_resource(
            image.backend_cookie(),
            image.query_info().command_resource_token,
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
        // Scarlet copied into the kernel-owned linear backing and cleaned the
        // corresponding cache rows before entering this synchronous callback.
        Ok(())
    }

    fn attach_buffer(&self, buffer: &dyn GpuBackendBuffer) -> Result<u64, &'static str> {
        let _execution = self.inner.execution.lock();
        self.validate_resource(
            buffer.backend_cookie(),
            buffer.query_info().command_resource_token,
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
        GpuBackendQueueInfo::new(GPU_MAX_OPAQUE_COMMAND_SIZE)
    }

    fn submit(&self, commands: &[u8]) -> Result<(), GpuBackendSubmitError> {
        let _execution = self.context.execution.lock();
        self.context
            .core
            .ensure_shader_pack()
            .map_err(GpuBackendSubmitError::Unavailable)?;
        let words = validate_and_relocate(
            commands,
            |token| self.context.resolve(token),
            |variant| self.context.core.resolve_shader(variant),
        )
        .map_err(|error| {
            early_println!("[a618] submit rejected: {}", error);
            if let Some(packet) = diagnose_rejected_packet(commands) {
                early_println!(
                    "[a618] rejected PM4 packet kind={} word={} selector={:#06x} count={} first={:#010x}",
                    packet.kind.label(),
                    packet.word_offset,
                    packet.selector,
                    packet.payload_len,
                    packet.first_value.unwrap_or(0),
                );
            }
            GpuBackendSubmitError::Rejected(error)
        })?;
        let byte_size = words.len().checked_mul(core::mem::size_of::<u32>()).ok_or(
            GpuBackendSubmitError::Rejected("qcom-adreno-a618: command byte size overflows"),
        )?;
        let mut command = DmaAllocation::new(
            &self.context.core.dma_context,
            byte_size,
            IommuMapFlags::READ,
        )
        .map_err(GpuBackendSubmitError::Unavailable)?;
        command.as_words_mut().copy_from_slice(&words);
        command.clean_for_device();

        // The validated command allocation deliberately stays alive across
        // lazy boot and synchronous kernel-owned fence completion.
        self.context.core.ensure_hardware_ready()?;
        self.context.core.submit_command(&command)
    }
}

impl A618Backend {
    fn create_resource(
        &self,
        paddr: usize,
        allocation_size: u64,
        allowed_access: u32,
        linear_image: Option<LinearImage>,
    ) -> Result<A618Resource, &'static str> {
        let (gpu_va, mapping) = self.core.map_backing(paddr, allocation_size)?;
        let token = self.core.allocate_resource_token()?;
        let entry = Arc::new(ResourceEntry {
            token,
            gpu_va,
            allocation_size,
            allowed_access,
            linear_image,
            _mapping: mapping,
        });
        self.core.register_resource(Arc::clone(&entry))?;
        Ok(A618Resource {
            core: Arc::clone(&self.core),
            entry,
        })
    }
}

impl GpuBackend for A618Backend {
    fn query_info(&self) -> scarlet::device::gpu::GpuBackendInfo {
        let ready = match self.core.ensure_hardware_ready() {
            Ok(()) => true,
            Err(error) => {
                let (class, detail) = match error {
                    GpuBackendSubmitError::Rejected(detail) => ("rejected", detail),
                    GpuBackendSubmitError::Unavailable(detail) => ("unavailable", detail),
                    GpuBackendSubmitError::DeviceLost(detail) => ("device-lost", detail),
                };
                early_println!("[a618] hardware class={}", class);
                early_println!("[a618] {}", detail);
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
                } else {
                    0
                },
                if ready {
                    GPU_MAX_OPAQUE_COMMAND_SIZE
                } else {
                    0
                },
            ),
            0,
            BACKEND_ID,
            if ready {
                b"A618 CoachZ linear BGRA8"
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
        let id = allocate_monotonic(
            &NEXT_CONTEXT_ID,
            "qcom-adreno-a618: context ID space exhausted",
        )?;
        Ok(Arc::new(A618Context {
            inner: Arc::new(A618ContextInner {
                core: Arc::clone(&self.core),
                id,
                next_attachment: AtomicU64::new(1),
                attachments: IrqSpinLock::new(Vec::new()),
                execution: Mutex::new(()),
            }),
        }))
    }

    fn plan_image(
        &self,
        create: GpuImageCreateInfo,
    ) -> Result<GpuBackendImageLayout, &'static str> {
        if create.format != GPU_IMAGE_FORMAT_BGRA8_UNORM {
            return Err("qcom-adreno-a618: only BGRA8 images are supported");
        }
        let row_bytes = create
            .width
            .checked_mul(4)
            .ok_or("qcom-adreno-a618: image row size overflows")?;
        let row_pitch = row_bytes
            .checked_add(63)
            .map(|value| value & !63)
            .ok_or("qcom-adreno-a618: image row pitch overflows")?;
        GpuBackendImageLayout::linear_32bpp(create, row_pitch, PAGE_SIZE as u64)
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
            resource: self.create_resource(
                backing.paddr,
                backing.allocation_size,
                (if create.usage & (GPU_IMAGE_USAGE_PRESENTABLE | GPU_IMAGE_USAGE_SAMPLED) != 0 {
                    adreno_a6xx_submit_wire::ACCESS_READ
                } else {
                    0
                }) | (if create.usage
                    & (GPU_IMAGE_USAGE_RENDER_TARGET | GPU_IMAGE_USAGE_TRANSFER_DST)
                    != 0
                {
                    adreno_a6xx_submit_wire::ACCESS_WRITE
                } else {
                    0
                }),
                Some(LinearImage {
                    width: create.width,
                    height: create.height,
                    row_pitch: layout.planes[0].row_pitch,
                    // Exclude page-alignment tail padding: A2D authorizes the
                    // complete visible plane, while the mapping retains the
                    // separately tracked allocation_size.
                    visible_size: u64::from(layout.planes[0].row_pitch)
                        .checked_mul(u64::from(create.height))
                        .ok_or("qcom-adreno-a618: image visible size overflows")?,
                }),
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
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-adreno-a618: missing GPU register resource")?;
    let resource_size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-adreno-a618: GPU register resource overflows")?;
    if resource_size < GPU_RESOURCE_SIZE {
        return Err("qcom-adreno-a618: GPU register resource is too small");
    }
    let register_base = vm::ioremap(resource.start, GPU_RESOURCE_SIZE)
        .map_err(|_| "qcom-adreno-a618: failed to map GPU registers")?;
    let result = (|| {
        let dma_context = DeviceManager::get_manager().resolve_platform_dma_context(
            device,
            IommuDomainConfig {
                domain_type: IommuDomainType::Dma,
                iova_base: GPU_IOVA_BASE,
                iova_size: GPU_IOVA_SIZE,
            },
        )?;
        let ring = DmaAllocation::new(&dma_context, RING_SIZE, bidirectional_flags())?;
        let fence = DmaAllocation::new(&dma_context, PAGE_SIZE, bidirectional_flags())?;
        let backend_cookie = allocate_monotonic(
            &NEXT_BACKEND_COOKIE,
            "qcom-adreno-a618: backend cookie space exhausted",
        )?;
        let core = Arc::new(A618Core {
            registers: DwordRegisters::new(register_base),
            register_base,
            dma_context,
            gmu,
            hardware: Mutex::new(HardwareState {
                boot: BootState::Cold,
                ring,
                sqe: None,
                shader_pack: None,
                fence,
                wptr: 0,
                fence_sequence: 0,
                gpu_oob_held: false,
                quiesce_failed: false,
                lost_reason: None,
                last_ring_failure: None,
            }),
            resources: IrqSpinLock::new(Vec::new()),
            next_resource_token: AtomicU64::new(1),
            backend_cookie,
        });
        let (device_id, gpu_name) = register_gpu_control_device(Arc::new(A618Backend {
            core: Arc::clone(&core),
        }))?;
        let phandle = own_phandle(device).unwrap_or(0);
        GPUS.lock().push(RegisteredGpu {
            phandle,
            device_id,
            _core: core,
        });
        early_println!(
            "[qcom-adreno-a618] registered lazy CoachZ GPU backend as {} paddr={:#x}",
            gpu_name,
            resource.start,
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
        EVENT_CCU_INVALIDATE_COLOR, EVENT_CCU_INVALIDATE_DEPTH, completion_commands,
        submission_commands,
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
        assert_eq!(commands[1], EVENT_CCU_INVALIDATE_DEPTH);
        assert_eq!(commands[2], type7(opcode::EVENT_WRITE, 1).unwrap());
        assert_eq!(commands[3], EVENT_CCU_INVALIDATE_COLOR);
        assert_eq!(commands[4], type7(CP_INDIRECT_BUFFER, 3).unwrap());
        assert_eq!(commands[5], 0x2345_6000);
        assert_eq!(commands[6], 1);
        assert_eq!(commands[7], 0x123);
        assert_eq!(commands[8], type7(opcode::WAIT_MEM_WRITES, 0).unwrap());
        assert_eq!(commands[9], type7(opcode::WAIT_FOR_ME, 0).unwrap());
    }
}
