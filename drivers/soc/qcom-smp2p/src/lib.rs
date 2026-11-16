// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! IRQ-driven Qualcomm SMP2P v1 state transport (Linux soc/qcom/smp2p.c).
//! A sleeping worker validates peer metadata and publishes callbacks; IRQ
//! context only transfers a oneshot completion token and wakes that worker.
extern crate alloc;
use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use scarlet::{
    arch,
    device::{
        DeviceInfo,
        fdt::FdtManager,
        mailbox::{MailboxChannel, MailboxMessage, MailboxSpec},
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    interrupt::{
        DeferredInterruptCompletion, InterruptClaim, InterruptId, InterruptManager,
        InterruptResult, InterruptSource,
    },
    sync::{IrqSpinLock, Mutex, Waker},
};
const MAGIC: u32 = 0x504d5324;
const MAX_ENTRIES: usize = 16;
const BYTES: usize = 20 + MAX_ENTRIES * 20;

/// Callbacks run in task context without the SMP2P transport lock held.
pub trait StateListener: Send + Sync {
    fn changed(&self, value: u32, changed: u32);
    fn restarted(&self) {}
    fn failed(&self) {}
}
struct Listener {
    name: [u8; 16],
    mask: u32,
    last: AtomicU32,
    callback: Arc<dyn StateListener>,
}
struct Incoming {
    item: Option<qcom_smem::Item>,
    negotiated: bool,
    ssr_supported: bool,
    ack: bool,
}
pub struct Smp2p {
    remote: u16,
    inbound_id: u16,
    outbound: qcom_smem::Item,
    output_names: Vec<([u8; 16], usize)>,
    output_lock: IrqSpinLock<()>,
    mailbox: Arc<dyn MailboxChannel>,
    irq: InterruptId,
    incoming: Mutex<Incoming>,
    listeners: IrqSpinLock<Vec<Arc<Listener>>>,
    pending: AtomicBool,
    failed: AtomicBool,
    completion: IrqSpinLock<Option<DeferredInterruptCompletion>>,
}
static PROVIDERS: IrqSpinLock<Vec<Arc<Smp2p>>> = IrqSpinLock::new(Vec::new());
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static WAKER: Waker = Waker::new_uninterruptible("qcom-smp2p");
fn wake() {
    SEQUENCE.fetch_add(1, Ordering::Release);
    WAKER.wake_one();
}
pub fn provider(remote: u16) -> Option<Arc<Smp2p>> {
    PROVIDERS
        .lock()
        .iter()
        .find(|provider| provider.remote == remote && !provider.failed.load(Ordering::Acquire))
        .cloned()
}
fn name_bytes(name: &str) -> Result<[u8; 16], &'static str> {
    if name.is_empty() || name.len() >= 16 || !name.is_ascii() || name.as_bytes().contains(&0) {
        return Err("smp2p: invalid entry name");
    }
    let mut bytes = [0; 16];
    bytes[..name.len()].copy_from_slice(name.as_bytes());
    Ok(bytes)
}
impl Smp2p {
    pub fn subscribe(
        &self,
        name: &str,
        mask: u32,
        callback: Arc<dyn StateListener>,
    ) -> Result<(), &'static str> {
        if mask == 0 || self.failed.load(Ordering::Acquire) {
            return Err("smp2p: unavailable subscription");
        }
        let name = name_bytes(name)?;
        let mut listeners = self.listeners.lock();
        if listeners.len() >= 16 {
            return Err("smp2p: subscriber limit reached");
        }
        listeners.push(Arc::new(Listener {
            name,
            mask,
            last: AtomicU32::new(0),
            callback,
        }));
        drop(listeners);
        self.pending.store(true, Ordering::Release);
        wake();
        Ok(())
    }
    pub fn update(&self, name: &str, mask: u32, value: u32) -> Result<(), &'static str> {
        if self.failed.load(Ordering::Acquire) {
            return Err("smp2p: transport failed");
        }
        let key = name_bytes(name)?;
        let index = self
            .output_names
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, index)| *index)
            .ok_or("smp2p: no outbound entry")?;
        let _guard = self.output_lock.lock();
        let offset = 20 + index * 20 + 16;
        let old = self
            .outbound
            .read32(offset)
            .map_err(|_| "smp2p: outbound read failed")?;
        let next = (old & !mask) | (value & mask);
        if old != next {
            self.outbound
                .write32(offset, next)
                .map_err(|_| "smp2p: outbound write failed")?;
            self.kick()?;
        }
        Ok(())
    }
    fn kick(&self) -> Result<(), &'static str> {
        self.mailbox
            .try_send(&MailboxMessage::one(0))
            .map_err(|_| "smp2p: IPC doorbell failed")
    }
    fn pump(&self) -> Result<(), &'static str> {
        let mut incoming = self.incoming.lock();
        if incoming.item.is_none() {
            match qcom_smem::get(self.remote, self.inbound_id) {
                Ok(item) => incoming.item = Some(item),
                Err(qcom_smem::Error::Layout(qcom_smem::layout::Error::NotFound)) => return Ok(()),
                Err(_) => return Err("smp2p: cannot acquire remote SMEM item"),
            }
        }
        let item = incoming.item.as_ref().unwrap().clone();
        if item.len() < 20 {
            return Err("smp2p: short remote header");
        }
        let version = item.read32(4).map_err(|_| "smp2p: unreadable version")?;
        if version & 0xff == 0 {
            return Ok(());
        }
        if version & 0xff != 1
            || item.read32(0).map_err(|_| "smp2p: unreadable magic")? != MAGIC
            || item.read32(8).map_err(|_| "smp2p: unreadable peer IDs")? != u32::from(self.remote)
        {
            return Err("smp2p: incompatible remote header");
        }
        let counts = item
            .read32(12)
            .map_err(|_| "smp2p: unreadable entry count")?;
        let total = (counts & 0xffff) as usize;
        let valid = (counts >> 16) as usize;
        if total > 16 || valid > total || 20 + valid * 20 > item.len() {
            return Err("smp2p: invalid remote entry count");
        }
        if !incoming.negotiated {
            let features = (version >> 8) & 1;
            self.outbound
                .write32(4, 1 | features << 8)
                .map_err(|_| "smp2p: feature negotiation failed")?;
            incoming.ssr_supported = features & 1 != 0;
            incoming.negotiated = true;
        }
        let flags = item
            .read32(16)
            .map_err(|_| "smp2p: unreadable restart flags")?;
        let restart = flags & 1 != 0;
        let restarted = incoming.ssr_supported && restart != incoming.ack;
        if restarted {
            let _guard = self.output_lock.lock();
            let old = self
                .outbound
                .read32(16)
                .map_err(|_| "smp2p: unreadable local flags")?;
            self.outbound
                .write32(16, (old & !2) | if restart { 2 } else { 0 })
                .map_err(|_| "smp2p: restart acknowledgement failed")?;
            incoming.ack = restart;
            self.kick()?;
        }
        let mut values = Vec::with_capacity(valid);
        for index in 0..valid {
            let mut name = [0; 16];
            item.read(20 + index * 20, &mut name)
                .map_err(|_| "smp2p: unreadable entry name")?;
            if !name.contains(&0) {
                return Err("smp2p: unterminated remote entry name");
            }
            let value = item
                .read32(20 + index * 20 + 16)
                .map_err(|_| "smp2p: unreadable entry value")?;
            values.push((name, value));
        }
        drop(incoming);
        let listeners = self.listeners.lock().clone();
        for listener in listeners {
            if restarted {
                listener.callback.restarted();
            }
            if let Some((_, value)) = values.iter().find(|(name, _)| *name == listener.name) {
                let previous = listener.last.swap(*value, Ordering::AcqRel);
                let changed = (previous ^ value) & listener.mask;
                if changed != 0 {
                    listener.callback.changed(*value, changed);
                }
            }
        }
        Ok(())
    }
    fn process(&self) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.pump() {
            self.failed.store(true, Ordering::Release);
            let _ = InterruptManager::global()
                .disable_external_interrupt(self.irq, arch::get_cpu().get_cpuid() as u32);
            scarlet::println!("[qcom-smp2p] peer {} failed: {}", self.remote, error);
            let listeners = self.listeners.lock().clone();
            for listener in listeners {
                listener.callback.failed();
            }
            // Retain an outstanding oneshot hold on failure; don't unmask a
            // malformed source or recycle shared protocol state.
            return;
        }
        let completion = self.completion.lock().take();
        if let Some(mut completion) = completion {
            if completion.complete().is_err() {
                self.failed.store(true, Ordering::Release);
                *self.completion.lock() = Some(completion);
            }
        }
    }
}
impl InterruptSource for Smp2p {
    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.irq)
    }
    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        Ok(if self.failed.load(Ordering::Acquire) {
            InterruptClaim::NotMine
        } else {
            InterruptClaim::Deferred
        })
    }
    fn deferred_interrupt_ready(&self, completion: DeferredInterruptCompletion) {
        let mut slot = self.completion.lock();
        assert!(slot.is_none(), "smp2p: duplicate oneshot token");
        *slot = Some(completion);
        drop(slot);
        self.pending.store(true, Ordering::Release);
        wake();
    }
}
fn worker() {
    let task = scarlet::task::mytask().expect("smp2p: worker has no task");
    loop {
        let seen = SEQUENCE.load(Ordering::Acquire);
        let providers = PROVIDERS.lock().clone();
        for provider in providers {
            if provider.pending.swap(false, Ordering::AcqRel) {
                provider.process();
            }
        }
        WAKER.wait_with_condition(task.get_id(), task.get_trapframe(), None, 0, || {
            SEQUENCE.load(Ordering::Acquire) != seen
        });
    }
}
fn be(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}
fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if qcom_smem::physical_region().is_none() {
        return Err(PROBE_DEFER);
    }
    let local = device
        .property("qcom,local-pid")
        .and_then(|property| property.as_usize())
        .ok_or("smp2p: missing local PID")?;
    let remote = device
        .property("qcom,remote-pid")
        .and_then(|property| property.as_usize())
        .ok_or("smp2p: missing remote PID")?;
    if local != 0 || remote >= 20 {
        return Err("smp2p: unsupported processor IDs");
    }
    let remote = remote as u16;
    if provider(remote).is_some() {
        return Err("smp2p: duplicate peer");
    }
    let ids = device
        .property("qcom,smem")
        .ok_or("smp2p: missing SMEM IDs")?
        .value();
    if ids.len() != 8 {
        return Err("smp2p: malformed SMEM IDs");
    }
    let inbound_id =
        u16::try_from(be(ids).unwrap()).map_err(|_| "smp2p: invalid inbound SMEM ID")?;
    let outbound_id =
        u16::try_from(be(&ids[4..]).unwrap()).map_err(|_| "smp2p: invalid outbound SMEM ID")?;
    let irq_resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::IRQ)
        .ok_or("smp2p: missing IRQ")?;
    let irq = scarlet::interrupt::resolve_platform_irq(irq_resource).map_err(|_| PROBE_DEFER)?;
    let mboxes = device
        .property("mboxes")
        .ok_or("smp2p: missing APCS doorbell")?
        .value();
    if mboxes.len() != 8 {
        return Err("smp2p: invalid mailbox specifier");
    }
    let mailbox = DeviceManager::get_manager().request_mailbox_channel(
        &MailboxSpec {
            controller_phandle: be(mboxes).unwrap(),
            cells: vec![be(&mboxes[4..]).unwrap()],
        },
        None,
    )?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("smp2p: missing FDT")?;
    let node = fdt
        .all_nodes()
        .find(|node| {
            node.name == device.name()
                && node
                    .property("qcom,remote-pid")
                    .and_then(|property| be(property.value))
                    == Some(u32::from(remote))
        })
        .ok_or("smp2p: FDT node not found")?;
    let mut names = Vec::new();
    for child in node
        .children()
        .filter(|child| child.property("interrupt-controller").is_none())
    {
        let name = child
            .property("qcom,entry-name")
            .and_then(|property| property.as_str())
            .ok_or("smp2p: outbound name missing")?;
        if names.len() >= 16 {
            return Err("smp2p: too many output entries");
        }
        let bytes = name_bytes(name)?;
        if names.iter().any(|(previous, _)| *previous == bytes) {
            return Err("smp2p: duplicate outbound name");
        }
        names.push((bytes, names.len()));
    }
    let outbound = qcom_smem::allocate(remote, outbound_id, BYTES)
        .map_err(|_| "smp2p: cannot allocate local SMEM item")?;
    // Invalidate version before clearing an old AP-owned transport instance.
    outbound
        .write32(4, 0)
        .map_err(|_| "smp2p: cannot invalidate version")?;
    for offset in (0..BYTES).step_by(4) {
        outbound
            .write32(offset, 0)
            .map_err(|_| "smp2p: cannot initialize local SMEM")?;
    }
    outbound
        .write32(0, MAGIC)
        .map_err(|_| "smp2p: cannot initialize magic")?;
    outbound
        .write32(8, u32::from(remote) << 16)
        .map_err(|_| "smp2p: cannot initialize peer IDs")?;
    for (name, index) in &names {
        outbound
            .write(20 + index * 20, name)
            .map_err(|_| "smp2p: cannot initialize entry")?;
    }
    outbound
        .write32(12, 16 | ((names.len() as u32) << 16))
        .map_err(|_| "smp2p: cannot publish entries")?;
    outbound
        .write32(4, 1 | (1 << 8))
        .map_err(|_| "smp2p: cannot publish version")?;
    let provider = Arc::new(Smp2p {
        remote,
        inbound_id,
        outbound,
        output_names: names,
        output_lock: IrqSpinLock::new(()),
        mailbox,
        irq,
        incoming: Mutex::new(Incoming {
            item: None,
            negotiated: false,
            ssr_supported: false,
            ack: false,
        }),
        listeners: IrqSpinLock::new(Vec::new()),
        pending: AtomicBool::new(true),
        failed: AtomicBool::new(false),
        completion: IrqSpinLock::new(None),
    });
    InterruptManager::global()
        .register_interrupt_source(irq, provider.clone())
        .map_err(|_| "smp2p: cannot register IRQ")?;
    PROVIDERS.lock().push(provider.clone());
    if !WORKER_STARTED.swap(true, Ordering::AcqRel) {
        let task = scarlet::task::new_kernel_task(String::from("qcom-smp2p"), 1, worker);
        task.init();
        scarlet::sched::scheduler::add_task(task, 0);
    }
    if let Err(error) = InterruptManager::global()
        .enable_external_interrupt(irq, arch::get_cpu().get_cpuid() as u32)
    {
        provider.failed.store(true, Ordering::Release);
        scarlet::println!("[qcom-smp2p] peer {} IRQ setup failed: {:?}", remote, error);
        return Err("smp2p: cannot enable IRQ");
    }
    provider.kick()?;
    wake();
    scarlet::println!(
        "[qcom-smp2p] peer={} IRQ={} entries={}",
        remote,
        irq,
        provider.output_names.len()
    );
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("smp2p: retained shared-memory transport is permanent")
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-smp2p",
            probe,
            remove,
            vec!["qcom,smp2p"],
        )),
        DriverPriority::Standard,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_SMP2P_ANCHOR: fn() = force_link;
pub fn force_link() {}
