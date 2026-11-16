// SPDX-License-Identifier: GPL-2.0-only
use crate::protocol::{self, Event, Link};
use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use scarlet::{
    arch,
    device::{
        fdt::FdtManager,
        mailbox::{MailboxChannel, MailboxMessage, MailboxSpec},
        manager::{DeviceManager, PROBE_DEFER},
        platform::resource::{IrqMetadata, PlatformDeviceResource, PlatformDeviceResourceType},
    },
    interrupt::{
        DeferredInterruptCompletion, InterruptClaim, InterruptId, InterruptManager,
        InterruptResult, InterruptSource,
    },
    sync::{IrqSpinLock, Mutex, Waker},
};

pub trait Listener: Send + Sync {
    fn event(&self, event: Event);
    fn failed(&self);
}
struct State {
    link: Link,
    rx: Option<qcom_smem::Item>,
    blocked: bool,
}
pub struct Edge {
    descriptors: qcom_smem::Item,
    tx: qcom_smem::Item,
    doorbell: Arc<dyn MailboxChannel>,
    irq: InterruptId,
    state: Mutex<State>,
    listener: IrqSpinLock<Option<Arc<dyn Listener>>>,
    pending: AtomicBool,
    failed: AtomicBool,
    completion: IrqSpinLock<Option<DeferredInterruptCompletion>>,
}
static EDGE: IrqSpinLock<Option<Arc<Edge>>> = IrqSpinLock::new(None);
static WAKER: Waker = Waker::new_uninterruptible("qcom-glink");
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn wake() {
    SEQUENCE.fetch_add(1, Ordering::Release);
    WAKER.wake_one();
}
fn io<T>(value: Result<T, qcom_smem::Error>) -> Result<T, &'static str> {
    value.map_err(|_| "glink: SMEM access failed")
}
fn ring_read(item: &qcom_smem::Item, offset: usize, bytes: &mut [u8]) -> Result<(), &'static str> {
    let n = bytes.len().min(item.len() - offset);
    let (a, b) = bytes.split_at_mut(n);
    io(item.read(offset, a))?;
    if !b.is_empty() {
        io(item.read(0, b))?;
    }
    Ok(())
}
fn ring_write(item: &qcom_smem::Item, offset: usize, bytes: &[u8]) -> Result<(), &'static str> {
    let n = bytes.len().min(item.len() - offset);
    io(item.write(offset, &bytes[..n]))?;
    if n != bytes.len() {
        io(item.write(0, &bytes[n..]))?;
    }
    Ok(())
}
fn indices(
    desc: &qcom_smem::Item,
    tail: usize,
    head: usize,
    len: usize,
) -> Result<(usize, usize), &'static str> {
    let t = io(desc.read32(tail))? as usize;
    let h = io(desc.read32(head))? as usize;
    if len < 32 || len > 1 << 20 || len % 8 != 0 || t >= len || h >= len || t % 8 != 0 || h % 8 != 0
    {
        return Err("glink: invalid FIFO indices");
    }
    Ok((t, h))
}
impl Edge {
    /// Ring only after Q6 is running: pre-reset mailbox edges can be lost.
    pub fn start(&self) -> Result<(), &'static str> {
        self.pending.store(true, Ordering::Release);
        wake();
        self.kick()
    }
    pub fn startup_snapshot(&self) {
        scarlet::println!(
            "[qcom-glink] descriptors TX={:?}/{:?} RX={:?}/{:?} remote-fifo={:?}",
            self.descriptors.read32(0),
            self.descriptors.read32(4),
            self.descriptors.read32(8),
            self.descriptors.read32(12),
            qcom_smem::get(1, 480).map(|item| item.len())
        );
        if let (Ok(rx), Ok(tail)) = (qcom_smem::get(1, 480), self.descriptors.read32(8)) {
            if (tail as usize) < rx.len() {
                let mut b = [0; 16];
                if ring_read(&rx, tail as usize, &mut b).is_ok() {
                    scarlet::println!(
                        "[qcom-glink] next cmd={} p1={} p2={} pending={} failed={}",
                        protocol::u16le(&b),
                        protocol::u16le(&b[2..]),
                        protocol::u32le(&b[4..]),
                        self.pending.load(Ordering::Acquire),
                        self.failed.load(Ordering::Acquire)
                    );
                }
            }
        }
    }
    pub fn bind(&self, listener: Arc<dyn Listener>) -> Result<(), &'static str> {
        let mut slot = self.listener.lock();
        if slot.is_some() {
            return Err("glink: consumer already bound");
        }
        if self.failed.load(Ordering::Acquire) {
            return Err("glink: transport failed");
        }
        *slot = Some(listener);
        drop(slot);
        self.pending.store(true, Ordering::Release);
        wake();
        Ok(())
    }
    pub fn send(&self, channel: &str, packet: &[u8]) -> Result<(), protocol::Error> {
        if self.failed.load(Ordering::Acquire) {
            return Err(protocol::Error::Closed);
        }
        self.state.lock().link.send(channel, packet)?;
        self.pending.store(true, Ordering::Release);
        wake();
        Ok(())
    }
    fn kick(&self) -> Result<(), &'static str> {
        self.doorbell
            .try_send(&MailboxMessage::one(0))
            .map_err(|_| "glink: doorbell failed")
    }
    fn pump(&self) -> Result<(), &'static str> {
        let mut state = self.state.lock();
        if state.rx.is_none() {
            match qcom_smem::get(1, 480) {
                Ok(item) => state.rx = Some(item),
                Err(qcom_smem::Error::Layout(qcom_smem::layout::Error::NotFound)) => {}
                Err(_) => return Err("glink: RX FIFO unavailable"),
            }
        }
        let mut kick = false;
        let mut read_notification = false;
        if let Some(rx) = state.rx.clone() {
            let mut consumed = 0;
            for _ in 0..256 {
                let (tail, head) = indices(&self.descriptors, 8, 12, rx.len())?;
                let available = (head + rx.len() - tail) % rx.len();
                if available < 8 {
                    break;
                }
                let mut header = [0; 16];
                let count = available.min(16);
                ring_read(&rx, tail, &mut header[..count])?;
                let Some(size) = protocol::frame_size(&header[..count], rx.len())
                    .map_err(|_| "glink: invalid frame header")?
                else {
                    break;
                };
                if size > available {
                    break;
                }
                let mut frame = vec![0; size];
                ring_read(&rx, tail, &mut frame)?;
                if protocol::u16le(&frame) == 2 {
                    let n = protocol::u32le(&frame[4..]) as usize;
                    scarlet::println!(
                        "[qcom-glink] peer opens {:?}",
                        core::str::from_utf8(&frame[8..8 + n])
                    );
                }
                if protocol::u16le(&frame) == 13 {
                    read_notification = true;
                }
                state
                    .link
                    .receive(&frame)
                    .map_err(|_| "glink: invalid protocol state")?;
                io(self
                    .descriptors
                    .write32(8, ((tail + size) % rx.len()) as u32))?;
                consumed += 1;
            }
            let (tail, head) = indices(&self.descriptors, 8, 12, rx.len())?;
            if consumed == 256 && tail != head {
                self.pending.store(true, Ordering::Release);
                wake();
            }
        }
        state
            .link
            .flush()
            .map_err(|_| "glink: output queue failure")?;
        while let Some(frame) = state.link.output.front() {
            let (tail, head) = indices(&self.descriptors, 0, 4, self.tx.len())?;
            let free = (tail + self.tx.len() - head - 8) % self.tx.len();
            if free < frame.len() + 8 {
                if !state.blocked && free >= 8 {
                    let notification = protocol::frame(13, 0, 0, &[]);
                    ring_write(&self.tx, head, &notification)?;
                    io(self
                        .descriptors
                        .write32(4, ((head + 8) % self.tx.len()) as u32))?;
                    state.blocked = true;
                    kick = true;
                }
                break;
            }
            let frame = state.link.output.pop_front().unwrap();
            ring_write(&self.tx, head, &frame)?;
            io(self
                .descriptors
                .write32(4, ((head + frame.len()) % self.tx.len()) as u32))?;
            state.blocked = false;
            kick = true;
        }
        let listener = self.listener.lock().clone();
        let events: Vec<_> = if listener.is_some() {
            state.link.events.drain(..).collect()
        } else {
            Vec::new()
        };
        drop(state);
        if kick || read_notification {
            self.kick()?;
        }
        if let Some(listener) = listener {
            for event in events {
                listener.event(event);
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
            scarlet::println!("[qcom-glink] {}", error);
            let listener = self.listener.lock().clone();
            if let Some(listener) = listener {
                listener.failed();
            }
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
pub fn modem_edge() -> Option<Arc<Edge>> {
    EDGE.lock()
        .as_ref()
        .filter(|edge| !edge.failed.load(Ordering::Acquire))
        .cloned()
}
impl InterruptSource for Edge {
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
        assert!(slot.is_none(), "glink: duplicate oneshot token");
        *slot = Some(completion);
        drop(slot);
        self.pending.store(true, Ordering::Release);
        wake();
    }
}
fn worker() {
    let task = scarlet::task::mytask().expect("glink: worker has no task");
    loop {
        let seen = SEQUENCE.load(Ordering::Acquire);
        let edge = EDGE.lock().clone();
        if let Some(edge) = edge {
            if edge.pending.swap(false, Ordering::AcqRel) {
                edge.process();
            }
        }
        WAKER.wait_with_condition(task.get_id(), task.get_trapframe(), None, 0, || {
            SEQUENCE.load(Ordering::Acquire) != seen
        });
    }
}
/// Prepare the modem's DT-described edge before releasing its reset.
pub fn prepare() -> Result<Arc<Edge>, &'static str> {
    if let Some(edge) = EDGE.lock().clone() {
        return if edge.failed.load(Ordering::Acquire) {
            Err("glink: retained failed edge")
        } else {
            Ok(edge)
        };
    }
    let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
    let node = fdt
        .all_nodes()
        .find(|node| {
            node.name == "glink-edge"
                && node.property("qcom,remote-pid").and_then(|p| p.as_usize()) == Some(1)
        })
        .ok_or("glink: missing modem edge")?;
    let ints = node
        .property("interrupts")
        .ok_or("glink: missing IRQ")?
        .value;
    if ints.len() != 12 || u32::from_be_bytes(ints[..4].try_into().unwrap()) != 0 {
        return Err("glink: expected GIC SPI");
    }
    let number = u32::from_be_bytes(ints[4..8].try_into().unwrap());
    let flags = u32::from_be_bytes(ints[8..12].try_into().unwrap());
    let irq = scarlet::interrupt::resolve_platform_irq(&PlatformDeviceResource {
        res_type: PlatformDeviceResourceType::IRQ,
        start: u64::from(number + 32),
        end: u64::from(number + 32),
        irq_parent: None,
        irq_metadata: Some(IrqMetadata {
            irq_type: 0,
            irq_number: number,
            irq_flags: flags,
        }),
    })
    .map_err(|_| PROBE_DEFER)?;
    let mbox = node
        .property("mboxes")
        .ok_or("glink: missing mailbox")?
        .value;
    if mbox.len() != 8 {
        return Err("glink: invalid mailbox");
    }
    let doorbell = DeviceManager::get_manager().request_mailbox_channel(
        &MailboxSpec {
            controller_phandle: u32::from_be_bytes(mbox[..4].try_into().unwrap()),
            cells: vec![u32::from_be_bytes(mbox[4..].try_into().unwrap())],
        },
        None,
    )?;
    let descriptors =
        qcom_smem::allocate(1, 478, 32).map_err(|_| "glink: cannot allocate descriptors")?;
    let tx = qcom_smem::allocate(1, 479, 16384).map_err(|_| "glink: cannot allocate TX FIFO")?;
    io(descriptors.write32(4, 0))?;
    io(descriptors.write32(8, 0))?;
    let edge = Arc::new(Edge {
        descriptors,
        tx,
        doorbell,
        irq,
        state: Mutex::new(State {
            link: Link::new(),
            rx: None,
            blocked: false,
        }),
        listener: IrqSpinLock::new(None),
        pending: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        completion: IrqSpinLock::new(None),
    });
    InterruptManager::global()
        .register_interrupt_source(irq, edge.clone())
        .map_err(|_| "glink: IRQ registration failed")?;
    *EDGE.lock() = Some(edge.clone());
    if InterruptManager::global()
        .enable_external_interrupt(irq, arch::get_cpu().get_cpuid() as u32)
        .is_err()
    {
        edge.failed.store(true, Ordering::Release);
        return Err("glink: IRQ enable failed");
    }
    let task = scarlet::task::new_kernel_task(String::from("qcom-glink"), 1, worker);
    task.init();
    scarlet::sched::scheduler::add_task(task, 0);
    wake();
    scarlet::println!("[qcom-glink] modem edge prepared IRQ={}", irq);
    Ok(edge)
}
