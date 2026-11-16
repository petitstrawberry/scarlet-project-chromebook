// SPDX-License-Identifier: GPL-2.0-only
//! SC7180's MBA/RMB boot path, following Linux qcom_q6v5_mss.c.
use crate::mdt::Image;
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use qcom_scm::{Host, Permission};
use scarlet::{
    arch::{self, mmio},
    device::{
        clk::ClkHandle,
        iommu::{
            DmaAddr, DmaContext, DmaMapping, IommuDomainConfig, IommuDomainType, IommuMapFlags,
            Iova,
        },
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        power::{PowerDomain, PowerManager},
        remoteproc::{
            RemoteProcessor, RemoteprocCrashHandler, RemoteprocError, RemoteprocFirmware,
            RemoteprocService, RemoteprocServiceId, RemoteprocState,
        },
        reset::ResetHandle,
    },
    fs::manager::get_global_vfs_manager_safe,
    mem::{address::PhysAddr, page::ContiguousPages},
    object::KernelObject,
    sync::{IrqSpinLock, Mutex},
    time, vm,
};

const MBA_PATH: &str = "/lib/firmware/qcom/sc7180-trogdor/modem-nolte/mba.mbn";
const MPSS_PATH: &str = "/lib/firmware/qcom/sc7180-trogdor/modem-nolte/qdsp6sw.mbn";
const METADATA_BYTES: usize = 0x4000;
const HLOS: u64 = 1 << Host::Hlos as u32;
const MSS: u64 = 1 << Host::Mss as u32;

struct Mapping {
    base: usize,
    physical: u64,
    size: usize,
}
impl Mapping {
    fn device(physical: u64, size: usize) -> Result<Self, &'static str> {
        Ok(Self {
            base: vm::ioremap(physical, size)?,
            physical,
            size,
        })
    }
    fn ram(physical: u64, size: usize) -> Result<Self, &'static str> {
        Ok(Self {
            base: vm::memremap_normal(physical, size)?,
            physical,
            size,
        })
    }
    fn word(&self, offset: usize) -> u32 {
        assert!(offset % 4 == 0 && offset.checked_add(4).is_some_and(|end| end <= self.size));
        unsafe { mmio::read32(self.base + offset) }
    }
    fn write(&self, offset: usize, value: u32) {
        assert!(offset % 4 == 0 && offset.checked_add(4).is_some_and(|end| end <= self.size));
        unsafe { mmio::write32(self.base + offset, value) };
        arch::io_wmb();
    }
    fn update(&self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.word(offset) & !clear) | set);
    }
    fn copy(&self, offset: usize, bytes: &[u8], memory: usize) -> Result<(), &'static str> {
        if bytes.len() > memory || offset.checked_add(memory).is_none_or(|end| end > self.size) {
            return Err("mss: copy outside reserved RAM");
        }
        // SAFETY: this controller owns HLOS access before any CPU copy; bounds
        // were validated by the MDT planner and against this normal RAM map.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.base + offset) as *mut u8,
                bytes.len(),
            );
            core::ptr::write_bytes(
                (self.base + offset + bytes.len()) as *mut u8,
                0,
                memory - bytes.len(),
            );
        }
        arch::clean_dcache_to_poc_range(self.base + offset, memory);
        arch::io_mb();
        Ok(())
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

struct Metadata {
    _mapping: DmaMapping,
    pages: ContiguousPages,
}
// Unique page ownership is kept behind Mss::state. Firmware ownership changes
// are serialized there; no mutable reference to shared firmware RAM escapes.
unsafe impl Send for Metadata {}
struct State {
    lifecycle: RemoteprocState,
    authenticated: bool,
    quarantined: bool,
    mba_owners: u64,
    mpss_owners: u64,
    metadata_owners: u64,
    metadata: Metadata,
    handlers: Vec<Arc<dyn RemoteprocCrashHandler>>,
}
pub struct Mss {
    q6: Mapping,
    rmb: Mapping,
    halt: Mapping,
    halt_offsets: [usize; 3],
    spare: Mapping,
    spare_offset: usize,
    mba: Mapping,
    mpss: Mapping,
    _dma: DmaContext,
    _mappings: Vec<DmaMapping>,
    _domains: Vec<Arc<dyn PowerDomain>>,
    smp: Arc<qcom_smp2p::Smp2p>,
    // Logical handles remain prepared through authentication and quarantine;
    // releasing a failed request must not collapse an executing DSP's fabric.
    clocks: Vec<ClkHandle>,
    reset: ResetHandle,
    pdc: ResetHandle,
    state: Mutex<State>,
}
static CONTROLLER: IrqSpinLock<Option<Arc<Mss>>> = IrqSpinLock::new(None);
pub fn controller() -> Option<Arc<Mss>> {
    CONTROLLER.lock().clone()
}

struct MssSignals(Weak<Mss>);
impl qcom_smp2p::StateListener for MssSignals {
    fn changed(&self, value: u32, changed: u32) {
        let Some(core) = self.0.upgrade() else {
            return;
        };
        if value & changed & 1 != 0 {
            core.crashed(1);
        } else if value & changed & 2 != 0 {
            match core.ready() {
                Ok(()) => scarlet::println!("[sc7180-mss] SMP2P ready; remote processor running"),
                Err(error) => {
                    scarlet::println!("[sc7180-mss] rejected ready indication: {:?}", error)
                }
            }
        }
    }
    fn restarted(&self) {
        if let Some(core) = self.0.upgrade() {
            if core.state() == RemoteprocState::Running {
                core.crashed(2);
            }
        }
    }
    fn failed(&self) {
        if let Some(core) = self.0.upgrade() {
            core.crashed(3);
        }
    }
}

fn poll(
    map: &Mapping,
    offset: usize,
    timeout_us: u64,
    condition: impl Fn(u32) -> bool,
) -> Result<u32, &'static str> {
    let start = time::current_time();
    loop {
        let value = map.word(offset);
        if condition(value) {
            return Ok(value);
        }
        if time::current_time().wrapping_sub(start) >= timeout_us {
            return Err("mss: register deadline expired");
        }
        time::udelay(10);
    }
}
fn mba_status(map: &Mapping, status: u32, timeout_us: u64) -> Result<u32, &'static str> {
    let value = poll(map, 0x0c, timeout_us, |value| {
        if (value as i32) < 0 {
            true
        } else if status == 0 {
            value != 0
        } else {
            value == status
        }
    })?;
    if (value as i32) < 0 {
        scarlet::println!("[sc7180-mss] MBA error {}", value as i32);
        return Err("mss: MBA rejected firmware");
    }
    Ok(value)
}
fn transfer(
    physical: u64,
    size: usize,
    owners: &mut u64,
    local: bool,
    remote: bool,
) -> Result<(), &'static str> {
    let desired = (if local { HLOS } else { 0 }) | (if remote { MSS } else { 0 });
    if *owners == desired {
        return Ok(());
    }
    let mut permissions = Vec::with_capacity(2);
    if local {
        permissions.push(Permission {
            host: Host::Hlos,
            bits: 7,
        });
    }
    if remote {
        permissions.push(Permission {
            host: Host::Mss,
            bits: 6,
        });
    }
    // SAFETY: the caller retains all backing RAM and mappings in the permanent
    // controller, stops host accesses before handoff, and quarantines on error.
    unsafe { qcom_scm::assign_memory(physical, size, owners, &permissions) }.map_err(|error| {
        scarlet::println!(
            "[sc7180-mss] memory ownership failed paddr={:#x} bytes={:#x}: {:?}",
            physical,
            size,
            error
        );
        "mss: SCM ownership transfer failed"
    })
}

impl Mss {
    pub fn firmware_authenticated(&self) -> bool {
        self.state.lock().authenticated
    }
    /// Only an observed firmware SMP2P ready indication may publish Running.
    pub fn ready(&self) -> Result<(), RemoteprocError> {
        let mut state = self.state.lock();
        if state.lifecycle == RemoteprocState::Running {
            return Ok(());
        }
        if !state.authenticated || state.quarantined {
            return Err(RemoteprocError::InvalidState);
        }
        transfer(
            self.mba.physical,
            self.mba.size,
            &mut state.mba_owners,
            true,
            false,
        )
        .map_err(|_| RemoteprocError::BootFailed)?;
        state.lifecycle = RemoteprocState::Running;
        if let Some(edge) = qcom_glink::modem_edge() {
            edge.startup_snapshot();
            edge.start().map_err(|_| RemoteprocError::BootFailed)?;
        }
        Ok(())
    }
    fn crashed(&self, reason: u32) {
        if let Ok(item) = qcom_smem::get(u16::MAX,421) {
            let mut bytes=[0u8;512];let n=item.len().min(bytes.len());
            if item.read(0,&mut bytes[..n]).is_ok() {
                let end=bytes[..n].iter().position(|b|*b==0).unwrap_or(n);
                scarlet::println!("[sc7180-mss] firmware crash reason {:?}",core::str::from_utf8(&bytes[..end]));
            }
        }
        let handlers = {
            let mut state = self.state.lock();
            if state.quarantined {
                return;
            }
            state.quarantined = true;
            state.lifecycle = RemoteprocState::Crashed;
            state.handlers.clone()
        };
        scarlet::println!(
            "[sc7180-mss] remote failure {}; retaining owned DMA",
            reason
        );
        let _ = self.quiesce();
        for handler in handlers {
            handler.crashed(RemoteprocServiceId(reason), reason);
        }
    }
    fn authenticate(&self, mba: &[u8], modem: &[u8], plan: &Image) -> Result<(), &'static str> {
        let mut state = self.state.lock();
        if state.lifecycle != RemoteprocState::Offline {
            return Err("mss: controller is not offline");
        }
        state.lifecycle = RemoteprocState::Loading;
        let result = self.authenticate_inner(&mut state, mba, modem, plan);
        if result.is_err() {
            state.quarantined = true;
            state.lifecycle = RemoteprocState::Crashed;
            // Do not drop DMA/ownership just because a timeout happened.
            // Try the SC7180 reset workaround, retaining every mapping either way.
            let _ = self.quiesce();
        }
        result
    }
    fn quiesce(&self) -> Result<(), &'static str> {
        for offset in self.halt_offsets {
            if self.halt.word(offset + 8) == 0 {
                self.halt.write(offset, 1);
                let _ = poll(&self.halt, offset + 4, 100_000, |value| value != 0);
                self.halt.write(offset, 0);
            }
        }
        self.pdc.assert()?;
        self.spare.update(self.spare_offset, 0, 1);
        self.reset.assert()?;
        self.pdc.deassert()?;
        self.spare.update(self.spare_offset, 1, 0);
        self.reset.deassert()
    }
    fn authenticate_inner(
        &self,
        state: &mut State,
        mba: &[u8],
        modem: &[u8],
        plan: &Image,
    ) -> Result<(), &'static str> {
        self.mba.copy(0, mba, self.mba.size)?;
        // xo, iface, bus, snoc_axi precede the reset pulse; mnoc_axi/nav follow.
        for clock in self.clocks.iter().take(4) {
            clock
                .prepare_enable()
                .map_err(|_| "mss: reset clock failed")?;
        }
        self.reset.reset()?;
        for clock in self.clocks.iter().skip(4) {
            clock
                .prepare_enable()
                .map_err(|_| "mss: active clock failed")?;
        }
        transfer(
            self.mpss.physical,
            self.mpss.size,
            &mut state.mpss_owners,
            false,
            true,
        )?;
        transfer(
            self.mba.physical,
            self.mba.size,
            &mut state.mba_owners,
            false,
            true,
        )?;
        self.rmb.write(0, self.mba.physical as u32);
        for register in [0x3c, 0x38] {
            self.q6.update(register, 0, 1);
            poll(&self.q6, register, 200, |value| value & (1 << 31) == 0)?;
        }
        self.q6.update(0x20, 0, 1);
        self.q6.write(0x400, 1);
        time::udelay(10);
        self.q6.write(0x404, 1);
        poll(&self.rmb, 0x40, 10_000, |value| value & 1 != 0)?;
        let pbl = poll(&self.rmb, 4, 1_000_000, |value| value != 0)?;
        if pbl != 1 {
            scarlet::println!("[sc7180-mss] PBL status={:#x}", pbl);
            return Err("mss: PBL rejected MBA");
        }
        let unlocked = mba_status(&self.rmb, 0, 5_000_000)?;
        if unlocked != 1 && unlocked != 2 {
            scarlet::println!(
                "[sc7180-mss] unexpected MBA status={:#x} PBL={:#x} FSM={:#x}",
                unlocked,
                self.rmb.word(4),
                self.rmb.word(0x40)
            );
            return Err("mss: MBA did not unlock firmware region");
        }
        scarlet::println!("[sc7180-mss] MBA booted; authenticating modem metadata");
        self.rmb.write(0x18, 0);
        let metadata = state.metadata.pages.as_vaddr();
        if plan.metadata.len() > METADATA_BYTES {
            return Err("mss: oversized signed metadata");
        }
        unsafe {
            core::ptr::write_bytes(metadata as *mut u8, 0, METADATA_BYTES);
            core::ptr::copy_nonoverlapping(
                plan.metadata.as_ptr(),
                metadata as *mut u8,
                plan.metadata.len(),
            );
        }
        arch::clean_dcache_to_poc_range(metadata, METADATA_BYTES);
        arch::io_mb();
        transfer(
            state.metadata.pages.as_paddr(),
            METADATA_BYTES,
            &mut state.metadata_owners,
            false,
            true,
        )?;
        // MBA authenticates through a physical-address path. Ownership and
        // the firmware IOMMU mapping must identify that same physical range.
        self.rmb.write(0x10, state.metadata.pages.as_paddr() as u32);
        self.rmb.write(8, 1);
        let auth = mba_status(&self.rmb, 3, 1_000_000);
        if auth.is_err() {
            scarlet::println!(
                "[sc7180-mss] metadata phys={:#x} bytes={} RMB={:#x} command={} status={:#x} IOMMU={:?}",
                state.metadata.pages.as_paddr(),
                plan.metadata.len(),
                self.rmb.word(0x10),
                self.rmb.word(8),
                self.rmb.word(0x0c),
                self._dma.primary_iommu_fault_snapshot()
            );
        }
        transfer(
            state.metadata.pages.as_paddr(),
            METADATA_BYTES,
            &mut state.metadata_owners,
            true,
            false,
        )?;
        auth?;
        transfer(
            self.mpss.physical,
            self.mpss.size,
            &mut state.mpss_owners,
            true,
            false,
        )?;
        arch::invalidate_dcache_to_poc_range(self.mpss.base, self.mpss.size);
        transfer(
            self.mpss.physical,
            self.mpss.size,
            &mut state.mpss_owners,
            true,
            true,
        )?;
        let mut length = 0u32;
        for segment in &plan.segments {
            self.mpss.copy(
                segment.offset,
                &modem[segment.source.clone()],
                segment.memory_size,
            )?;
            if length == 0 {
                self.rmb.write(0x14, plan.boot_address);
                self.rmb.write(8, 2);
            }
            length = length
                .checked_add(segment.memory_size as u32)
                .ok_or("mss: code length overflow")?;
            self.rmb.write(0x18, length);
            if (self.rmb.word(0x0c) as i32) < 0 {
                return Err("mss: modem segment authentication failed");
            }
        }
        if length != plan.code_length {
            return Err("mss: code length differs from checked plan");
        }
        transfer(
            self.mpss.physical,
            self.mpss.size,
            &mut state.mpss_owners,
            false,
            true,
        )?;
        mba_status(&self.rmb, 4, 10_000_000)?;
        state.authenticated = true;
        scarlet::println!("[sc7180-mss] modem authenticated; waiting for SMP2P ready");
        Ok(())
    }
}
impl RemoteProcessor for Mss {
    fn name(&self) -> &'static str {
        "sc7180-mss"
    }
    fn state(&self) -> RemoteprocState {
        self.state.lock().lifecycle
    }
    fn load(&self, firmware: &RemoteprocFirmware) -> Result<(), RemoteprocError> {
        if self.state() != RemoteprocState::Offline {
            return Err(RemoteprocError::InvalidState);
        }
        if firmware.regions.len() != 2
            || firmware.regions[0].paddr != self.mba.physical
            || firmware.regions[0].size != self.mba.size
            || firmware.regions[1].paddr != self.mpss.physical
            || firmware.regions[1].size != self.mpss.size
        {
            return Err(RemoteprocError::LoadFailed);
        }
        Ok(())
    }
    fn boot(&self) -> Result<(), RemoteprocError> {
        if self.state() == RemoteprocState::Running {
            Ok(())
        } else {
            Err(RemoteprocError::Busy)
        }
    }
    fn shutdown(&self) -> Result<(), RemoteprocError> {
        Err(RemoteprocError::NotSupported)
    }
    fn suspend(&self) -> Result<(), RemoteprocError> {
        Err(RemoteprocError::NotSupported)
    }
    fn resume(&self) -> Result<(), RemoteprocError> {
        Err(RemoteprocError::NotSupported)
    }
    fn register_crash_handler(
        &self,
        handler: Arc<dyn RemoteprocCrashHandler>,
    ) -> Result<(), RemoteprocError> {
        let mut state = self.state.lock();
        if state.handlers.len() >= 4 {
            return Err(RemoteprocError::Busy);
        }
        state.handlers.push(handler);
        Ok(())
    }
    fn get_service(&self, _: RemoteprocServiceId) -> Option<Arc<dyn RemoteprocService>> {
        None
    }
}

fn load_file(path: &str, limit: usize) -> Result<Vec<u8>, &'static str> {
    let vfs = get_global_vfs_manager_safe().ok_or(PROBE_DEFER)?;
    let KernelObject::File(file) = vfs
        .open(path, 0)
        .map_err(|_| "mss: required local firmware is missing")?
    else {
        return Err("mss: firmware path is not a regular file");
    };
    let size = file
        .metadata()
        .map_err(|_| "mss: firmware metadata unavailable")?
        .size;
    if size == 0 || size > limit {
        return Err("mss: invalid firmware size");
    }
    let mut bytes = vec![0; size];
    let mut offset = 0;
    while offset < size {
        let read = file
            .read(&mut bytes[offset..])
            .map_err(|_| "mss: firmware read failed")?;
        if read == 0 {
            return Err("mss: truncated firmware");
        }
        offset += read;
    }
    Ok(bytes)
}
fn cells(device: &PlatformDeviceInfo, name: &str) -> Result<Vec<u32>, &'static str> {
    let bytes = device
        .property(name)
        .ok_or("mss: required DT property missing")?
        .value();
    if bytes.len() % 4 != 0 {
        return Err("mss: malformed DT cells");
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|cell| u32::from_be_bytes(cell.try_into().unwrap()))
        .collect())
}
fn memory(device: &PlatformDeviceInfo, index: usize) -> Result<Mapping, &'static str> {
    let resource = DeviceManager::get_manager().resolve_platform_memory_region(
        device,
        "memory-region",
        index,
    )?;
    let size = resource.size()?;
    if resource.start % 4096 != 0 || size == 0 || size % 4096 != 0 || resource.end >= 1u64 << 32 {
        return Err("mss: reserved RAM is not a 32-bit page range");
    }
    Mapping::ram(resource.start, size)
}
fn syscon(
    device: &PlatformDeviceInfo,
    name: &str,
    count: usize,
) -> Result<(Mapping, Vec<usize>), &'static str> {
    let spec = cells(device, name)?;
    if spec.len() != count + 1 {
        return Err("mss: malformed syscon offsets");
    }
    let resource = DeviceManager::get_manager().resolve_platform_memory_region(device, name, 0)?;
    let size = resource.size()?;
    let offsets: Vec<_> = spec[1..].iter().map(|value| *value as usize).collect();
    if offsets
        .iter()
        .any(|offset| offset % 4 != 0 || offset.checked_add(12).is_none_or(|end| end > size))
    {
        return Err("mss: syscon offset outside mapping");
    }
    Ok((Mapping::device(resource.start, size)?, offsets))
}
fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if CONTROLLER.lock().is_some() {
        return Err("mss: controller already exists");
    }
    if !qcom_scm::memory_assignment_available() {
        return Err(PROBE_DEFER);
    }
    let (smem_phys, smem_size) = qcom_smem::physical_region().ok_or(PROBE_DEFER)?;
    let smp = qcom_smp2p::provider(1).ok_or(PROBE_DEFER)?;
    let mba_file = load_file(MBA_PATH, 1024 * 1024)?;
    let modem_file = load_file(MPSS_PATH, 16 * 1024 * 1024)?;
    let _glink = qcom_glink::prepare()?;
    let manager = DeviceManager::get_manager();
    let phandle = *cells(device, "phandle")?.first().ok_or("mss: no phandle")?;
    let mba = memory(device, 0)?;
    let mpss = memory(device, 1)?;
    let plan = Image::parse(&modem_file, mpss.physical, mpss.size)
        .map_err(|_| "mss: invalid modem ELF/MDT image")?;
    if mba_file.len() > mba.size || mba.size > 0x200000 || mpss.size > 0x2000000 {
        return Err("mss: unexpected non-LTE reserved memory");
    }
    let resources: Vec<_> = device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .collect();
    if resources.len() != 2 || resources[0].size()? < 0x4040 || resources[1].size()? < 0x48 {
        return Err("mss: invalid Q6/RMB registers");
    }
    let q6 = Mapping::device(resources[0].start, resources[0].size()?)?;
    let rmb = Mapping::device(resources[1].start, resources[1].size()?)?;
    let (halt, halt_offsets) = syscon(device, "qcom,halt-regs", 3)?;
    let (spare, spare_offset) = syscon(device, "qcom,spare-regs", 1)?;
    let reset = manager.resolve_reset(device, "mss_restart")?;
    let pdc = manager.resolve_reset(device, "pdc_reset")?;
    let mut domains = Vec::new();
    let power = cells(device, "power-domains")?;
    if power.len() != 6 {
        return Err("mss: expected CX/MX/MSS power domains");
    }
    for spec in power.chunks_exact(2) {
        if !PowerManager::has_provider(spec[0]) {
            return Err(PROBE_DEFER);
        }
        let domain = PowerManager::resolve_domain(spec[0], &spec[1..])?;
        domain.enable()?;
        domains.push(domain);
    }
    let mut clocks = Vec::new();
    for name in ["xo", "iface", "bus", "snoc_axi", "mnoc_axi", "nav"] {
        clocks.push(manager.resolve_clk(device, name)?);
    }
    let dma = manager.resolve_platform_dma_context(
        device,
        IommuDomainConfig {
            // Linux's Qualcomm SMMU policy selects IDENTITY for sc7180-mss-pil:
            // Q6 firmware owns its address space, including peripheral accesses.
            // SCM/XPU ownership remains enforced independently of this routing.
            domain_type: IommuDomainType::Identity,
            iova_base: Iova::ZERO,
            iova_size: 1u64 << 32,
        },
    )?;
    if dma.iommu.is_none() {
        return Err("mss: missing firmware IOMMU attachments");
    }
    let mut mappings = Vec::new();
    // Q6 firmware issues unprivileged transactions too. Restrict access by
    // the MSS stream IDs and these owned ranges, not by the CPU privilege bit.
    let rw = IommuMapFlags::READ | IommuMapFlags::WRITE;
    for (physical, size, flags) in [
        (mba.physical, mba.size, rw | IommuMapFlags::EXECUTE),
        (mpss.physical, mpss.size, rw | IommuMapFlags::EXECUTE),
        (smem_phys, smem_size, rw),
    ] {
        mappings.push(
            dma.map_phys_at_owned(DmaAddr::new(physical), PhysAddr::new(physical), size, flags)
                .map_err(|_| "mss: reserved firmware IOMMU mapping failed")?,
        );
    }
    let pages =
        ContiguousPages::new(METADATA_BYTES / 4096).ok_or("mss: cannot allocate metadata pages")?;
    if pages
        .as_paddr()
        .checked_add(METADATA_BYTES as u64)
        .is_none_or(|end| end > 1u64 << 32)
    {
        return Err("mss: metadata requires 32-bit physical RAM");
    }
    let metadata_mapping = dma
        .map_phys_at_owned(
            DmaAddr::new(pages.as_paddr()),
            PhysAddr::new(pages.as_paddr()),
            METADATA_BYTES,
            IommuMapFlags::READ,
        )
        .map_err(|_| "mss: metadata IOMMU mapping failed")?;
    dma.restore_iommu()
        .map_err(|_| "mss: cannot synchronize firmware IOMMU")?;
    let core = Arc::new(Mss {
        q6,
        rmb,
        halt,
        halt_offsets: halt_offsets
            .try_into()
            .map_err(|_| "mss: invalid halt offsets")?,
        spare,
        spare_offset: spare_offset[0],
        mba,
        mpss,
        _dma: dma,
        _mappings: mappings,
        _domains: domains,
        smp,
        clocks,
        reset,
        pdc,
        state: Mutex::new(State {
            lifecycle: RemoteprocState::Offline,
            authenticated: false,
            quarantined: false,
            mba_owners: HLOS,
            mpss_owners: HLOS,
            metadata_owners: HLOS,
            metadata: Metadata {
                pages,
                _mapping: metadata_mapping,
            },
            handlers: Vec::new(),
        }),
    });
    // Publish retention before the first reset or ownership transfer. A failed
    // start remains observable/quarantined, never freed by probe unwinding.
    *CONTROLLER.lock() = Some(core.clone());
    manager.register_remote_processor(phandle, core.clone());
    if let Err(error) = core.authenticate(&mba_file, &modem_file, &plan) {
        scarlet::println!(
            "[sc7180-mss] boot failed; controller quarantined: {}",
            error
        );
    } else {
        // Subscription publishes an already-high ready value as well as new
        // edges, so an indication arriving during authentication is retained.
        if let Err(error) = core.smp.subscribe(
            "slave-kernel",
            0x83,
            Arc::new(MssSignals(Arc::downgrade(&core))),
        ) {
            scarlet::println!("[sc7180-mss] state notification unavailable: {}", error);
            core.crashed(3);
        }
    }
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("mss: retained firmware controller cannot be removed")
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-mss",
            probe,
            remove,
            vec!["qcom,sc7180-mss-pil"],
        )),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_SC7180_MSS_ANCHOR: fn() = crate::force_link;
