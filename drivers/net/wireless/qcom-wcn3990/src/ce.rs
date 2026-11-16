// SPDX-License-Identifier: GPL-2.0-only
//! WCN3990 64-bit Copy Engines. Descriptors are noncacheable; packet memory
//! uses explicit ownership/cache transitions. Queues stay owned after failure.
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use scarlet::{
    arch::{self, mmio},
    device::{
        iommu::{DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags, Iova},
        manager::DeviceManager,
        platform::{PlatformDeviceInfo, resource::PlatformDeviceResourceType},
    },
    interrupt::{
        DeferredInterruptCompletion, InterruptClaim, InterruptId, InterruptManager,
        InterruptResult, InterruptSource,
    },
    mem::{address::PhysAddr, page::ContiguousPages},
    sync::{IrqSpinLock, Mutex},
    vm::{self, vmem::MemoryAttribute},
};
const BASE: usize = 0x240000;
const STRIDE: usize = 0x1000;
const DESCRIPTOR: usize = 16;
const SOURCE: [usize; 12] = [16, 0, 0, 32, 2048, 0, 0, 2, 0, 0, 0, 0];
const DESTINATION: [usize; 12] = [0, 512, 64, 0, 0, 512, 0, 2, 128, 512, 512, 512];
const PACKET: [usize; 12] = [
    2048, 2048, 2048, 2048, 256, 512, 0, 2048, 2048, 2048, 2048, 2048,
];
struct Memory {
    mapping: DmaMapping,
    pages: ContiguousPages,
    len: usize,
    noncacheable: bool,
}
// Allocations are uniquely owned inside Bus::rings. No borrowed DMA slice
// escapes; packet results are copied after the hardware completes ownership.
unsafe impl Send for Memory {}
impl Memory {
    fn new(context: &DmaContext, len: usize, noncacheable: bool) -> Result<Self, &'static str> {
        let count = len.checked_add(4095).ok_or("ce: allocation overflow")? / 4096;
        let mut pages = ContiguousPages::new(count).ok_or("ce: DMA allocation failed")?;
        unsafe { core::ptr::write_bytes(pages.as_vaddr() as *mut u8, 0, count * 4096) };
        arch::clean_dcache_to_poc_range(pages.as_vaddr(), count * 4096);
        if noncacheable {
            pages
                .retag_memory_attribute(MemoryAttribute::NonCacheable)
                .map_err(|_| "ce: descriptor attribute change failed")?;
        }
        let mapping = context
            .map_phys_owned(
                PhysAddr::new(pages.as_paddr()),
                count * 4096,
                IommuMapFlags::READ | IommuMapFlags::WRITE,
            )
            .map_err(|_| "ce: DMA mapping failed")?;
        if mapping
            .dma_addr()
            .as_u64()
            .checked_add(count as u64 * 4096)
            .is_none_or(|end| end > 1u64 << 35)
        {
            return Err("ce: DMA exceeds 35-bit hardware aperture");
        }
        Ok(Self {
            mapping,
            pages,
            len,
            noncacheable,
        })
    }
    fn address(&self) -> u64 {
        self.mapping.dma_addr().as_u64()
    }
    fn ptr(&self, offset: usize, len: usize) -> Result<usize, &'static str> {
        if offset.checked_add(len).is_none_or(|end| end > self.len) {
            return Err("ce: DMA bounds violation");
        }
        Ok(self.pages.as_vaddr() + offset)
    }
    fn clean(&self, offset: usize, len: usize) -> Result<(), &'static str> {
        let ptr = self.ptr(offset, len)?;
        if !self.noncacheable {
            arch::clean_dcache_to_poc_range(ptr, len);
        }
        arch::io_wmb();
        Ok(())
    }
    fn invalidate(&self, offset: usize, len: usize) -> Result<(), &'static str> {
        let ptr = self.ptr(offset, len)?;
        if !self.noncacheable {
            arch::invalidate_dcache_to_poc_range(ptr, len);
        }
        arch::io_mb();
        Ok(())
    }
}
struct Ring {
    descriptors: Memory,
    packets: Memory,
    count: usize,
    size: usize,
    read: usize,
    write: usize,
    source: bool,
    owners: Vec<usize>,
    reusable_at: Vec<u64>,
    sensitive: Vec<bool>,
}
impl Ring {
    fn new(
        context: &DmaContext,
        count: usize,
        size: usize,
        source: bool,
    ) -> Result<Self, &'static str> {
        if count < 2 || !count.is_power_of_two() || size == 0 || size % 64 != 0 {
            return Err("ce: invalid ring geometry");
        }
        Ok(Self {
            descriptors: Memory::new(context, count * DESCRIPTOR, true)?,
            packets: Memory::new(context, count * size, false)?,
            count,
            size,
            read: 0,
            write: 0,
            source,
            owners: (0..count).collect(),
            reusable_at: (0..count).map(|_| 0).collect(),
            sensitive: (0..count).map(|_| false).collect(),
        })
    }
    fn descriptor(
        &self,
        index: usize,
        address: u64,
        length: u16,
        metadata: u16,
    ) -> Result<(), &'static str> {
        let p = self.descriptors.ptr(index * DESCRIPTOR, DESCRIPTOR)?;
        unsafe {
            core::ptr::write_volatile(p as *mut u64, address.to_le());
            core::ptr::write_volatile((p + 8) as *mut u16, length.to_le());
            core::ptr::write_volatile((p + 10) as *mut u16, metadata.to_le());
            core::ptr::write_volatile((p + 12) as *mut u32, 0);
        }
        arch::io_wmb();
        Ok(())
    }
}
struct Shared {
    pending: AtomicU32,
    failed: AtomicBool,
    completion: IrqSpinLock<[Option<DeferredInterruptCompletion>; 12]>,
    wake: fn(),
}
struct Source {
    irq: InterruptId,
    id: usize,
    shared: Arc<Shared>,
}
impl InterruptSource for Source {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.irq)
    }
    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        Ok(if self.shared.failed.load(Ordering::Acquire) {
            InterruptClaim::NotMine
        } else {
            InterruptClaim::Deferred
        })
    }
    fn deferred_interrupt_ready(&self, token: DeferredInterruptCompletion) {
        let mut slots = self.shared.completion.lock();
        assert!(
            slots[self.id].is_none(),
            "ce: duplicate interrupt completion"
        );
        slots[self.id] = Some(token);
        drop(slots);
        self.shared
            .pending
            .fetch_or(1 << self.id, Ordering::Release);
        (self.shared.wake)();
    }
}
struct Rings {
    source: [Option<Ring>; 12],
    destination: [Option<Ring>; 12],
    active: bool,
    next: usize,
}
struct Registers(usize);
impl Drop for Registers {
    fn drop(&mut self) {
        vm::iounmap(self.0);
    }
}
pub struct Bus {
    base: usize,
    _registers: Registers,
    _context: DmaContext,
    rri: Memory,
    rings: Mutex<Rings>,
    shared: Arc<Shared>,
    irqs: Vec<Arc<Source>>,
    retry_at: AtomicU64,
}
// RRI memory is owned for the bus lifetime and accessed only with volatile
// loads; its device writes are independent of the software ring mutex.
unsafe impl Sync for Memory {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    Full,
    TooLarge,
    Unavailable,
}
pub struct Activity {
    pub received: Vec<(usize, Vec<u8>)>,
    pub tx_progress: bool,
}
pub struct HostMemory(Memory);
impl HostMemory {
    pub fn address(&self) -> u64 {
        self.0.address()
    }
    pub fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>, &'static str> {
        let ptr = self.0.ptr(offset, len)?;
        self.0.invalidate(offset, len)?;
        let mut bytes = Vec::with_capacity(len);
        for i in 0..len {
            bytes.push(unsafe { core::ptr::read_volatile((ptr + i) as *const u8) });
        }
        Ok(bytes)
    }
    pub fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), &'static str> {
        let ptr = self.0.ptr(offset, bytes.len())?;
        for (i, value) in bytes.iter().enumerate() {
            unsafe { core::ptr::write_volatile((ptr + i) as *mut u8, *value) };
        }
        self.0.clean(offset, bytes.len())
    }
    pub fn write_index(&self, index: u32) -> Result<(), &'static str> {
        let ptr = self.0.ptr(0, 4)?;
        arch::io_wmb();
        unsafe { core::ptr::write_volatile(ptr as *mut u32, index.to_le()) };
        self.0.clean(0, 4)
    }
    pub fn description(&self, id: u32) -> crate::wmi::MemoryChunk {
        crate::wmi::MemoryChunk {
            id,
            address: self.0.address(),
            bytes: self.0.len as u32,
        }
    }
}
impl Bus {
    fn retry_no_later_than(&self, deadline: u64) {
        let _ = self
            .retry_at
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some(if old == 0 {
                    deadline
                } else {
                    old.min(deadline)
                })
            });
    }
    pub fn retry_deadline(&self) -> Option<u64> {
        let deadline = self.retry_at.load(Ordering::Acquire);
        (deadline != 0).then_some(deadline)
    }
    pub fn host_memory(&self, bytes: usize) -> Result<HostMemory, &'static str> {
        Memory::new(&self._context, bytes, true).map(HostMemory)
    }
    pub fn startup_snapshot(&self) {
        let rings = self.rings.lock();
        if !rings.active {
            return;
        }
        for id in [0, 2, 3, 4] {
            scarlet::println!(
                "[wcn3990-ce] id={} source={:#x}/{:#x} dest={:#x}/{:#x} src-index={}/{} dst-index={}/{} status={:#x} errors={:#x}",
                id,
                self.read(id, 4),
                self.read(id, 0),
                self.read(id, 16),
                self.read(id, 12),
                self.read(id, 0x44),
                self.read(id, 0x3c),
                self.read(id, 0x48),
                self.read(id, 0x40),
                self.read(id, 0x30),
                self.read(id, 0x38)
            );
        }
        scarlet::println!(
            "[wcn3990-ce] IOMMU={:?}",
            self._context.primary_iommu_fault_snapshot()
        );
    }
    pub fn process(&self) -> Result<Activity, &'static str> {
        self.retry_at.store(0, Ordering::Release);
        if self.shared.failed.load(Ordering::Acquire) {
            return Err("ce: quarantined");
        }
        let mut rings = self.rings.lock();
        let mut activity = Activity {
            received: Vec::new(),
            tx_progress: false,
        };
        if !rings.active {
            return Ok(activity);
        }
        let pending = self.shared.pending.swap(0, Ordering::AcqRel);
        let result = (|| {
            let start = rings.next;
            let mut budget = 64;
            let mut remaining = 0u32;
            for n in 0..12 {
                let id = (start + n) % 12;
                if SOURCE[id] == 0 && DESTINATION[id] == 0 {
                    continue;
                }
                let errors = self.read(id, 0x38) & 0x3e0;
                if errors != 0 {
                    scarlet::println!("[wcn3990-ce] engine {} DMA error {:#x}", id, errors);
                    return Err("ce: hardware DMA error");
                }
                // Clear the cause before draining, so a later completion stays
                // pending through the final GIC oneshot unmask.
                if pending & (1 << id) != 0 {
                    self.write(id, 0x30, 0x1f);
                }
                if let Some(ring) = rings.source[id].as_mut() {
                    if id == 3 {
                        let now = scarlet::time::current_time();
                        for index in 0..ring.count {
                            if !ring.sensitive[index] || ring.reusable_at[index] == 0 {
                                continue;
                            }
                            if ring.reusable_at[index] <= now {
                                let ptr = ring.packets.ptr(index * ring.size, ring.size)?;
                                let bytes = unsafe {
                                    core::slice::from_raw_parts_mut(ptr as *mut u8, ring.size)
                                };
                                crate::secret::erase(bytes);
                                ring.packets.clean(index * ring.size, ring.size)?;
                                ring.sensitive[index] = false;
                                ring.reusable_at[index] = 0;
                            } else {
                                self.retry_no_later_than(ring.reusable_at[index]);
                            }
                        }
                    }
                    let hardware = self.read(id, 0x44) as usize;
                    if hardware >= ring.count {
                        return Err("ce: invalid source completion index");
                    }
                    let done = (hardware + ring.count - ring.read) & (ring.count - 1);
                    let outstanding = (ring.write + ring.count - ring.read) & (ring.count - 1);
                    if done > outstanding {
                        return Err("ce: source completion exceeds submitted descriptors");
                    }
                    if done != 0 {
                        arch::io_mb();
                        if id == 3 {
                            // WCN3990 can report WMI copy completion early.
                            // Keep its slot unchanged for Linux's 2ms guard;
                            // other slots and engines continue independently.
                            let deadline = scarlet::time::current_time() + 2000;
                            for offset in 0..done {
                                let index = (ring.read + offset) & (ring.count - 1);
                                ring.reusable_at[index] = deadline;
                                if ring.sensitive[index] {
                                    self.retry_no_later_than(deadline);
                                }
                            }
                        }
                        ring.read = hardware;
                        activity.tx_progress = true;
                    }
                }
                if let Some(ring) = rings.destination[id].as_mut() {
                    let previous_write = ring.write;
                    while budget != 0 && ring.read != ring.write {
                        let index = ring.read;
                        let descriptor = ring.descriptors.ptr(index * DESCRIPTOR, DESCRIPTOR)?;
                        let length = u16::from_le(unsafe {
                            core::ptr::read_volatile((descriptor + 8) as *const u16)
                        }) as usize;
                        if length == 0 {
                            break;
                        }
                        arch::io_mb();
                        if length > ring.size {
                            return Err("ce: destination DMA exceeded packet buffer");
                        }
                        let buffer = ring.owners[index];
                        let offset = buffer * ring.size;
                        let expected = ring.packets.address() + offset as u64;
                        let address = u64::from_le(unsafe {
                            core::ptr::read_volatile(descriptor as *const u64)
                        }) & ((1u64 << 35) - 1);
                        if address != expected {
                            return Err("ce: destination descriptor ownership changed");
                        }
                        ring.packets.invalidate(offset, ring.size)?;
                        let ptr = ring.packets.ptr(offset, length)?;
                        let bytes =
                            unsafe { core::slice::from_raw_parts(ptr as *const u8, length) }
                                .to_vec();
                        activity.received.push((id, bytes));
                        unsafe { core::ptr::write_volatile((descriptor + 8) as *mut u16, 0) };
                        ring.read = (index + 1) & (ring.count - 1);
                        ring.packets.invalidate(offset, ring.size)?;
                        let post = ring.write;
                        ring.owners[post] = buffer;
                        ring.descriptor(post, expected, 0, 0)?;
                        ring.write = (post + 1) & (ring.count - 1);
                        budget -= 1;
                    }
                    if ring.write != previous_write {
                        self.write(id, 0x40, ring.write as u32);
                    }
                    if ring.read != ring.write {
                        let descriptor =
                            ring.descriptors.ptr(ring.read * DESCRIPTOR, DESCRIPTOR)?;
                        if unsafe { core::ptr::read_volatile((descriptor + 8) as *const u16) } != 0
                        {
                            remaining |= 1 << id;
                            if budget == 0 {
                                rings.next = (id + 1) % 12;
                            }
                        }
                    }
                }
            }
            Ok(remaining)
        })();
        drop(rings);
        let remaining = match result {
            Ok(mask) => mask,
            Err(error) => {
                self.quarantine();
                return Err(error);
            }
        };
        let tokens: Vec<_> = {
            let mut slots = self.shared.completion.lock();
            (0..12)
                .filter(|id| remaining & (1 << id) == 0)
                .filter_map(|id| slots[id].take())
                .collect()
        };
        for mut token in tokens {
            if token.complete().is_err() {
                self.quarantine();
                return Err("ce: interrupt completion failed");
            }
        }
        if remaining != 0 {
            self.shared.pending.fetch_or(remaining, Ordering::Release);
            (self.shared.wake)();
        }
        Ok(activity)
    }
    pub fn prepare(device: &PlatformDeviceInfo, wake: fn()) -> Result<Arc<Self>, &'static str> {
        let manager = DeviceManager::get_manager();
        let context = manager.resolve_platform_dma_context(
            device,
            IommuDomainConfig {
                domain_type: IommuDomainType::Dma,
                iova_base: Iova::new(0x10000000),
                iova_size: 0x70000000,
            },
        )?;
        if context.iommu.is_none() {
            return Err("ce: missing DMA SMMU context");
        }
        let resource = device
            .get_resources()
            .iter()
            .find(|r| r.res_type == PlatformDeviceResourceType::MEM)
            .ok_or("ce: missing registers")?;
        let size = resource.size()?;
        if size < 0x24c010 {
            return Err("ce: truncated register aperture");
        }
        let registers = Registers(vm::ioremap(resource.start, size)?);
        let base = registers.0;
        let rri = Memory::new(&context, 12 * 4, true)?;
        let mut source = core::array::from_fn(|_| None);
        let mut destination = core::array::from_fn(|_| None);
        for id in 0..12 {
            if SOURCE[id] != 0 {
                source[id] = Some(Ring::new(&context, SOURCE[id], PACKET[id], true)?);
            }
            if DESTINATION[id] != 0 {
                destination[id] = Some(Ring::new(&context, DESTINATION[id], PACKET[id], false)?);
            }
        }
        context
            .restore_iommu()
            .map_err(|_| "ce: DMA context synchronization failed")?;
        let resources: Vec<_> = device
            .get_resources()
            .iter()
            .filter(|r| r.res_type == PlatformDeviceResourceType::IRQ)
            .collect();
        if resources.len() != 12 {
            return Err("ce: expected twelve IRQs");
        }
        let shared = Arc::new(Shared {
            pending: AtomicU32::new(0),
            failed: AtomicBool::new(false),
            completion: IrqSpinLock::new(core::array::from_fn(|_| None)),
            wake,
        });
        let mut irqs = Vec::new();
        for (id, resource) in resources.iter().enumerate() {
            let irq = scarlet::interrupt::resolve_platform_irq(resource)
                .map_err(|_| "ce: IRQ mapping failed")?;
            irqs.push(Arc::new(Source {
                irq,
                id,
                shared: shared.clone(),
            }));
        }
        let bus = Arc::new(Self {
            base,
            _registers: registers,
            _context: context,
            rri,
            rings: Mutex::new(Rings {
                source,
                destination,
                active: false,
                next: 0,
            }),
            shared,
            irqs,
            retry_at: AtomicU64::new(0),
        });
        for source in &bus.irqs {
            InterruptManager::global()
                .register_interrupt_source(source.irq, source.clone())
                .map_err(|_| "ce: IRQ source registration failed")?;
        }
        Ok(bus)
    }
    fn read(&self, id: usize, offset: usize) -> u32 {
        assert!(id < 12 && offset < STRIDE && offset % 4 == 0);
        unsafe { mmio::read32(self.base + BASE + id * STRIDE + offset) }
    }
    fn write(&self, id: usize, offset: usize, value: u32) {
        assert!(id < 12 && offset < STRIDE && offset % 4 == 0);
        unsafe { mmio::write32(self.base + BASE + id * STRIDE + offset, value) };
        arch::io_wmb();
    }
    fn set_base(&self, id: usize, ring: &Ring) {
        let (low, high, size) = if ring.source { (0, 4, 8) } else { (12, 16, 20) };
        let dma = ring.descriptors.address();
        self.write(id, low, dma as u32);
        self.write(
            id,
            high,
            (self.read(id, high) & !0x1f) | ((dma >> 32) as u32 & 0x1f),
        );
        self.write(id, size, ring.count as u32);
    }
    /// Called only after WLFW acknowledges mission mode. No MMIO is touched by
    /// prepare(), while the WLAN clock/rails may still be off.
    pub fn activate(&self) -> Result<(), &'static str> {
        let mut rings = self.rings.lock();
        if rings.active {
            return Err("ce: already active");
        }
        if self.shared.failed.load(Ordering::Acquire) {
            return Err("ce: quarantined");
        }
        let result = (|| {
            unsafe {
                mmio::write32(self.base + 0x24c004, self.rri.address() as u32);
                mmio::write32(
                    self.base + 0x24c008,
                    (self.rri.address() >> 32) as u32 & 0x1f,
                )
            };
            for id in 0..12 {
                if SOURCE[id] == 0 && DESTINATION[id] == 0 {
                    continue;
                }
                self.write(id, 0x2c, 0);
                self.write(id, 0x34, 0);
                self.write(id, 0x30, 0x1f);
                self.write(id, 0x38, 0x3e0);
                let control = (self.read(id, 0x18) & !((1 << 17) | (1 << 18) | 0xffff))
                    | PACKET[id] as u32
                    | (1 << 19);
                self.write(id, 0x18, control);
                if let Some(ring) = rings.source[id].as_mut() {
                    let read = self.read(id, 0x44) as usize;
                    let write = self.read(id, 0x3c) as usize;
                    if read >= ring.count || write != read {
                        return Err("ce: source ring not empty at startup");
                    }
                    ring.read = read;
                    ring.write = write;
                    self.set_base(id, ring);
                    self.write(id, 0x4c, ring.count as u32);
                }
                if let Some(ring) = rings.destination[id].as_mut() {
                    let read = self.read(id, 0x48) as usize;
                    let write = self.read(id, 0x40) as usize;
                    if read >= ring.count || write != read {
                        return Err("ce: destination ring not empty at startup");
                    }
                    ring.read = read;
                    ring.write = write;
                    self.set_base(id, ring);
                    self.write(id, 0x50, ring.count as u32);
                    ring.packets.invalidate(0, ring.packets.len)?;
                    for _ in 0..ring.count - 1 {
                        let index = ring.write;
                        let buffer = index;
                        ring.owners[index] = buffer;
                        ring.descriptor(
                            index,
                            ring.packets.address() + (buffer * ring.size) as u64,
                            0,
                            0,
                        )?;
                        ring.write = (index + 1) & (ring.count - 1);
                    }
                    self.write(id, 0x40, ring.write as u32);
                }
            }
            rings.active = true;
            for source in &self.irqs {
                if SOURCE[source.id] == 0 && DESTINATION[source.id] == 0 {
                    continue;
                }
                InterruptManager::global()
                    .enable_external_interrupt(source.irq, arch::get_cpu().get_cpuid() as u32)
                    .map_err(|_| "ce: IRQ enable failed")?;
                self.write(source.id, 0x34, 0x3e0);
                // CE4 retirement accompanies HTT completion IRQs; it does not
                // need a polling task or one interrupt per transmitted frame.
                self.write(source.id, 0x2c, if source.id == 4 { 0 } else { 1 });
            }
            Ok(())
        })();
        if result.is_err() {
            self.quarantine();
        }
        result
    }
    pub fn quarantine(&self) {
        self.shared.failed.store(true, Ordering::Release);
        for source in &self.irqs {
            let _ = InterruptManager::global()
                .disable_external_interrupt(source.irq, arch::get_cpu().get_cpuid() as u32);
        }
    }
    pub fn send(&self, id: usize, bytes: &[u8], metadata: u16) -> Result<(), SendError> {
        if id >= 12 || bytes.is_empty() || self.shared.failed.load(Ordering::Acquire) {
            return Err(SendError::Unavailable);
        }
        // The STA admission barrier can be an IRQ spinlock. Never sleep while
        // holding it: contention means this packet was not submitted.
        let mut rings = self.rings.try_lock().ok_or(SendError::Full)?;
        if !rings.active {
            return Err(SendError::Unavailable);
        }
        let ring = rings.source[id].as_mut().ok_or(SendError::Unavailable)?;
        if bytes.len() > ring.size || metadata > 0xfff {
            return Err(SendError::TooLarge);
        }
        if (ring.write + 1) & (ring.count - 1) == ring.read {
            return Err(SendError::Full);
        }
        let index = ring.write;
        if ring.reusable_at[index] > scarlet::time::current_time() {
            self.retry_no_later_than(ring.reusable_at[index]);
            return Err(SendError::Full);
        }
        ring.sensitive[index] = id == 3
            && bytes.get(8..12).is_some_and(|header| {
                u32::from_le_bytes(header.try_into().unwrap()) & 0xffffff == 0x5009
            });
        ring.reusable_at[index] = 0;
        let offset = index * ring.size;
        let ptr = ring
            .packets
            .ptr(offset, bytes.len())
            .map_err(|_| SendError::Unavailable)?;
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len()) };
        ring.packets
            .clean(offset, bytes.len())
            .map_err(|_| SendError::Unavailable)?;
        ring.descriptor(
            index,
            ring.packets.address() + offset as u64,
            bytes.len() as u16,
            metadata << 4,
        )
        .map_err(|_| SendError::Unavailable)?;
        ring.write = (index + 1) & (ring.count - 1);
        unsafe { mmio::write32(self.base + 0x32000 + id * 4, ring.write as u32) };
        arch::io_wmb();
        Ok(())
    }
}
