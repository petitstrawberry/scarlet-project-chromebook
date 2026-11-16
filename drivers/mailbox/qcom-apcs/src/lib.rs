// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! SC7180 APCS shared doorbells. Payloads live in SMEM; a mailbox send only
//! asserts the selected write-one IPC bit (Linux qcom-apcs-ipc-mailbox.c).
extern crate alloc;
use alloc::{boxed::Box, sync::Arc, vec};
use scarlet::{
    arch::{self, mmio},
    device::{
        mailbox::{
            MailboxChannel, MailboxChannelId, MailboxClient, MailboxController, MailboxError,
            MailboxMessage, MailboxSpec,
        },
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    sync::IrqSpinLock,
    vm,
};
struct Shared {
    base: usize,
    leases: IrqSpinLock<([u64; 32], u64)>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}
struct Controller {
    phandle: u32,
    shared: Arc<Shared>,
}
struct Channel {
    id: usize,
    generation: u64,
    shared: Arc<Shared>,
    client: IrqSpinLock<Option<Arc<dyn MailboxClient>>>,
}
impl Drop for Channel {
    fn drop(&mut self) {
        let mut leases = self.shared.leases.lock();
        if leases.0[self.id] == self.generation {
            leases.0[self.id] = 0;
        }
    }
}
impl MailboxChannel for Channel {
    fn id(&self) -> MailboxChannelId {
        MailboxChannelId(self.id as u32)
    }
    fn try_send(&self, message: &MailboxMessage) -> Result<(), MailboxError> {
        if message.len > 1 || (message.len == 1 && message.words[0] != 0) {
            return Err(MailboxError::NotSupported);
        }
        {
            let leases = self.shared.leases.lock();
            if leases.0[self.id] != self.generation {
                return Err(MailboxError::InvalidChannel);
            }
            arch::io_wmb();
            // W1S: no read/modify/write, and no acknowledgement polling.
            unsafe { mmio::write32(self.shared.base + 12, 1 << self.id) };
        }
        let client = self.client.lock().clone();
        if let Some(client) = client {
            client.tx_done(self.id());
        }
        Ok(())
    }
    fn try_recv(&self) -> Result<Option<MailboxMessage>, MailboxError> {
        Ok(None)
    }
    fn send_timeout(&self, message: &MailboxMessage, _: u64) -> Result<(), MailboxError> {
        self.try_send(message)
    }
    fn set_client(&self, client: Option<Arc<dyn MailboxClient>>) -> Result<(), MailboxError> {
        let leases = self.shared.leases.lock();
        if leases.0[self.id] != self.generation {
            return Err(MailboxError::InvalidChannel);
        }
        *self.client.lock() = client;
        Ok(())
    }
    fn poll(&self) -> Result<(), MailboxError> {
        Ok(())
    }
}
impl MailboxController for Controller {
    fn name(&self) -> &'static str {
        "sc7180-apcs"
    }
    fn request_channel(
        &self,
        spec: &MailboxSpec,
        client: Option<Arc<dyn MailboxClient>>,
    ) -> Result<Arc<dyn MailboxChannel>, MailboxError> {
        let [id] = spec.cells.as_slice() else {
            return Err(MailboxError::InvalidChannel);
        };
        if spec.controller_phandle != self.phandle || *id >= 32 {
            return Err(MailboxError::InvalidChannel);
        }
        let mut leases = self.shared.leases.lock();
        let id = *id as usize;
        if leases.0[id] != 0 {
            return Err(MailboxError::Busy);
        }
        let generation = leases.1.checked_add(1).ok_or(MailboxError::Busy)?;
        leases.1 = generation;
        leases.0[id] = generation;
        Ok(Arc::new(Channel {
            id,
            generation,
            shared: self.shared.clone(),
            client: IrqSpinLock::new(client),
        }))
    }
    fn release_channel(&self, channel: MailboxChannelId) {
        if let Some(lease) = self.shared.leases.lock().0.get_mut(channel.0 as usize) {
            *lease = 0;
        }
    }
}
fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("apcs: missing phandle")?;
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("apcs: missing registers")?;
    let size = resource.size()?;
    if size < 16 || resource.start % 4 != 0 {
        return Err("apcs: invalid register window");
    }
    let base = vm::ioremap(resource.start, size)?;
    DeviceManager::get_manager().register_mailbox_controller(
        phandle,
        Arc::new(Controller {
            phandle,
            shared: Arc::new(Shared {
                base,
                leases: IrqSpinLock::new(([0; 32], 0)),
            }),
        }),
    );
    Ok(())
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("apcs: shared firmware doorbell provider is permanent")
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "sc7180-apcs",
            probe,
            remove,
            vec!["qcom,sc7180-apss-shared"],
        )),
        DriverPriority::Critical,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_APCS_ANCHOR: fn() = force_link;
pub fn force_link() {}
