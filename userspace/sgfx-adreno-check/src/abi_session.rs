// SPDX-License-Identifier: GPL-2.0-only
//! Exercise current wire extensions even when the facade pins an older core.
use sgfx_backend_abi as abi;
use sgfx_backend_loader::{LoadedBackend, Manifest};
use sgfx_core::ir::{CommandBuffer, PixelRect, ResourceTable};
struct Owned(abi::Object, unsafe extern "C" fn(abi::Object));
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // The matching destructor runs once while the library is loaded.
            unsafe { (self.1)(self.0) }
        }
    }
}
fn status(value: i32) -> Result<(), String> {
    if value == abi::OK {
        Ok(())
    } else {
        Err(format!("driver ABI status={value}"))
    }
}
pub struct Session {
    // Fields drop in this order: all objects before unloading their library.
    session: Owned,
    _context: Owned,
    _device: Owned,
    driver: std::sync::Arc<LoadedBackend>,
    pub capabilities: u64,
}
impl Session {
    pub fn new(library: &str, table: &ResourceTable, targets: &[u32]) -> Result<Self, String> {
        let driver = LoadedBackend::load(&Manifest {
            name: "scarlet-adreno".into(),
            gpu_backend: "qcom-adreno".into(),
            library: library.into(),
        })
        .map_err(|e| format!("driver: {e}"))?;
        let api = &driver.api;
        let mut device = Owned(core::ptr::null_mut(), api.drop_device);
        let mut capabilities = 0;
        // Each span remains live for the synchronous ABI call; outputs are
        // initialized before use and owned by the matching destructor guards.
        unsafe {
            status((api.open)(
                abi::Span::from_slice(b"/dev/gpu0"),
                &mut device.0,
                &mut capabilities,
            ))?
        };
        if device.0.is_null() {
            return Err("null device".into());
        }
        let mut context = Owned(core::ptr::null_mut(), api.drop_context);
        unsafe { status((api.create_context)(device.0, &mut context.0))? };
        if context.0.is_null() {
            return Err("null context".into());
        }
        table.enable_abi_commands();
        let metadata = table
            .abi_snapshot()
            .map_err(|e| format!("snapshot: {e:?}"))?;
        let mut session = Owned(core::ptr::null_mut(), api.drop_session);
        let mut session_caps = 0;
        unsafe {
            status((api.create_session)(
                context.0,
                abi::Span::from_slice(&metadata),
                abi::Span::from_slice(targets),
                &mut session.0,
                &mut session_caps,
            ))?
        };
        if session.0.is_null() {
            return Err("null session".into());
        }
        Ok(Self {
            session,
            _context: context,
            _device: device,
            driver,
            capabilities,
        })
    }
    pub fn submit(&mut self, commands: &CommandBuffer<'_, '_>) -> Result<(), String> {
        self.submit_measured(commands).map(|_| ())
    }
    /// Wall time for ABI admission and retirement, excluding command recording.
    pub fn submit_measured(
        &mut self,
        commands: &CommandBuffer<'_, '_>,
    ) -> Result<[u64; 3], String> {
        self.submit_burst(commands, 1)
    }
    /// Admit a bounded sequence before waiting, to expose transport overlap.
    pub fn submit_burst(
        &mut self,
        commands: &CommandBuffer<'_, '_>,
        count: usize,
    ) -> Result<[u64; 3], String> {
        self.submit_sequence(&vec![commands; count])
    }
    pub fn submit_sequence(
        &mut self,
        commands: &[&CommandBuffer<'_, '_>],
    ) -> Result<[u64; 3], String> {
        let api = &self.driver.api;
        let mut receipts = Vec::with_capacity(commands.len());
        let started = std::time::Instant::now();
        let mut retries = 0;
        for command in commands {
            let encoded = command.abi_batch().ok_or("missing wire commands")?;
            let batch = abi::Batch {
                table: encoded.table,
                words: abi::Span {
                    data: encoded.words.data,
                    len: encoded.words.len,
                },
                count: encoded.count,
            };
            let mut result = abi::SubmitResult::default();
            loop {
                unsafe { (api.submit)(self.session.0, &batch, &mut result) };
                if result.error != abi::BUSY
                    || result.disposition != abi::REJECTED
                    || !result.receipt.is_null()
                {
                    break;
                }
                if started.elapsed() >= std::time::Duration::from_secs(10) {
                    return Err("admission remained busy for ten seconds".into());
                }
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let receipt = Owned(result.receipt, api.drop_receipt);
            status(result.error)?;
            if result.disposition != abi::ACCEPTED || receipt.0.is_null() {
                return Err("submission not accepted with receipt".into());
            }
            receipts.push(receipt);
        }
        let admitted = std::time::Instant::now();
        for receipt in &receipts {
            let mut state = abi::PENDING;
            unsafe { status((api.wait)(receipt.0, 10_000_000_000, &mut state))? };
            if state != abi::COMPLETE {
                return Err("submission did not retire".into());
            }
        }
        Ok([
            admitted.duration_since(started).as_nanos() as u64,
            admitted.elapsed().as_nanos() as u64,
            retries,
        ])
    }
    /// Resolve the session-specific extension before transferring an owned
    /// producer handle. The closure runs only when the import entry exists.
    pub fn import_ycbcr(&mut self,slot:u32,conversion:abi::YcbcrConversion,transfer:impl FnOnce()->i32)->Result<(),String> {
        let import=self.driver.ycbcr.as_ref().ok_or("missing YCbCr import API")?.import_ycbcr;
        let raw=transfer();
        // The plugin consumes the handle on success and on every error path.
        unsafe {status(import(self.session.0,slot,raw,conversion))}
    }
    /// Release the imported mapping while retaining its logical resource ID.
    /// The plugin drains accepted work and detaches before clearing the slot.
    pub fn release_import(&mut self,slot:u32)->Result<(),String> {
        unsafe {status((self.driver.api.release_import)(self.session.0,slot))}
    }
    pub fn readback(
        &mut self,
        slot: u32,
        pixels: &mut [u8],
        stride: u32,
        rect: PixelRect,
    ) -> Result<(), String> {
        unsafe {
            status((self.driver.api.readback)(
                self.session.0,
                slot,
                pixels.as_mut_ptr(),
                pixels.len(),
                stride,
                abi::Rect {
                    x: rect.x(),
                    y: rect.y(),
                    width: rect.width(),
                    height: rect.height(),
                },
            ))
        }
    }
}
