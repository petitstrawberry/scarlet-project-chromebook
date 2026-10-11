//! Dynamically loaded Adreno backend. Adapted from SGFX 20bb5b8c (MIT). This crate alone owns all Rust objects on
//! the driver side. The exported table uses the versioned SGFX C ABI.
#![deny(unsafe_op_in_unsafe_fn)]

// ThinLTO merges std into this object's code, so --exclude-libs alone cannot
// hide std's native startup exports. They must remain private to this runtime;
// scarlet-ld resolves the SGFX entry only and must not interpose another std.
#[cfg(target_os = "scarlet")]
core::arch::global_asm!(".hidden __scarlet_getauxval", ".hidden __scarlet_start");

use abi::{Object, Span};
use sgfx_backend_abi as abi;
use sgfx_backend_scarlet_adreno as adreno;
use sgfx_core::{
    backend::{CommandExecutor, CommandSubmitter, Completion, CompletionStatus, SubmitError},
    ir,
};
use std::{rc::Rc, time::Duration};

struct Session {
    inner: adreno::MappedTargetSession,
    table: Rc<ir::ResourceTable>,
    source: u64,
    revision: u64,
    poisoned: bool,
}

// Core retains its pinned wire-ABI source identity. Reconstruct its record
// without accessing the borrowed command words or changing their lifetime.
fn core_batch(batch: abi::Batch) -> ir::abi::Batch {
    ir::abi::Batch {
        table: batch.table,
        words: ir::abi::Span {
            data: batch.words.data,
            len: batch.words.len,
        },
        count: batch.count,
    }
}

fn error(e: adreno::IrSubmitError) -> i32 {
    use adreno::IrSubmitError as E;
    match e {
        E::OutOfMemory => abi::OUT_OF_MEMORY,
        E::Unsupported(_) | E::AsyncUnsupported => abi::UNSUPPORTED,
        E::InvalidIr(_)
        | E::ResourceTableMismatch
        | E::ContextMismatch
        | E::TargetExtentMismatch
        | E::ImageNotMapped
        | E::TextureAlreadyMapped
        | E::ImageAlreadyMapped => abi::INVALID,
        E::Codegen(e) => match e {
            sgfx_codegen_adreno_a6xx::CompileError::OutOfMemory => abi::OUT_OF_MEMORY,
            sgfx_codegen_adreno_a6xx::CompileError::UnsupportedFeature => abi::UNSUPPORTED,
            _ => abi::INVALID,
        },
        E::SubmitWire(_) => abi::INVALID,
        E::SubmissionTooLarge => abi::OUT_OF_MEMORY,
        E::Backend(e) => handle_error(e),
        _ => abi::DEVICE_LOST,
    }
}
fn handle_error(e: adreno::HandleError) -> i32 {
    use adreno::HandleError as E;
    match e {
        E::InvalidHandle | E::InvalidParameter => abi::INVALID,
        E::Unsupported => abi::UNSUPPORTED,
        E::OutOfResources => abi::OUT_OF_MEMORY,
        E::NotFound => abi::INITIALIZATION_FAILED,
        _ => abi::DEVICE_LOST,
    }
}
fn status(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(abi::OK)
}
unsafe fn object<'a, T>(p: Object) -> Result<&'a mut T, i32> {
    // SAFETY: the ABI requires a live object of the indicated kind and serialized calls.
    unsafe { p.cast::<T>().as_mut() }.ok_or(abi::INVALID)
}
unsafe fn span<'a, T>(s: Span<T>) -> Result<&'a [T], i32> {
    if s.len > isize::MAX as usize / core::mem::size_of::<T>().max(1)
        || (s.len > 0
            && (s.data.is_null() || !(s.data as usize).is_multiple_of(core::mem::align_of::<T>())))
    {
        return Err(abi::INVALID);
    }
    // SAFETY: the caller supplies initialized storage valid for this call.
    Ok(unsafe { s.as_slice() })
}
unsafe extern "C" fn open(path: Span<u8>, out: *mut Object, caps: *mut u64) -> i32 {
    if out.is_null() || caps.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
        caps.write(0);
    }
    status((|| {
        let path = std::str::from_utf8(unsafe { span(path) }?).map_err(|_| abi::INVALID)?;
        let device = adreno::Device::open(path).map_err(handle_error)?;
        let c = device.capabilities();
        let flags = if c.supports_rendering() {
            abi::RENDERING
        } else {
            0
        } | if c.supports_presentation() {
            abi::PRESENTATION
        } else {
            0
        } | if c.supports_image_upload() {
            abi::IMAGE_UPLOAD
        } else {
            0
        } | if c.supports_image_readback() {
            abi::IMAGE_READBACK
        } else {
            0
        } | if c.supports_depth() { abi::DEPTH } else { 0 }
            | if c.supports_texture_arrays() {
                abi::TEXTURE_ARRAYS
            } else {
                0
            }
            | if c.supports_image_mips() {
                abi::IMAGE_MIPS | abi::IMAGE_BLITS
            } else {
                0
            }
            | abi::RGBA8_COLOR_ATTACHMENT;
        unsafe {
            caps.write(flags);
            out.write(Box::into_raw(Box::new(device)).cast());
        }
        Ok(())
    })())
}
unsafe extern "C" fn drop_device(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<adreno::Device>()));
        }
    }
}
unsafe extern "C" fn create_context(p: Object, out: *mut Object) -> i32 {
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
    }
    status((|| {
        let value = unsafe { object::<adreno::Device>(p) }?
            .create_context()
            .map_err(handle_error)?;
        unsafe {
            out.write(Box::into_raw(Box::new(value)).cast());
        }
        Ok(())
    })())
}
unsafe extern "C" fn drop_context(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<adreno::Context>()));
        }
    }
}
unsafe extern "C" fn create_session(
    p: Object,
    metadata: Span<u64>,
    targets: Span<u32>,
    out: *mut Object,
    caps: *mut u64,
) -> i32 {
    if out.is_null() || caps.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
        caps.write(0);
    }
    status((|| {
        let context = unsafe { object::<adreno::Context>(p) }?;
        let table = Rc::new(ir::ResourceTable::new());
        let (source, revision) = table
            .sync_abi_snapshot(unsafe { span(metadata) }?)
            .map_err(|_| abi::INVALID)?;
        let targets = unsafe { span(targets) }?
            .iter()
            .map(|slot| table.abi_texture(*slot).map(|r| r.id()))
            .collect::<ir::Result<Vec<_>>>()
            .map_err(|_| abi::INVALID)?;
        let mut inner = context
            .create_mapped_target_session(table.clone(), &targets)
            .map_err(error)?;
        let flags = if inner.executor().supports_async_submission() {
            abi::ASYNC
        } else {
            0
        };
        let session = Session {
            inner,
            table,
            source,
            revision,
            poisoned: false,
        };
        unsafe {
            caps.write(flags);
            out.write(Box::into_raw(Box::new(session)).cast());
        }
        Ok(())
    })())
}
unsafe extern "C" fn drop_session(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<Session>()));
        }
    }
}
unsafe extern "C" fn sync_resources(p: Object, metadata: Span<u64>) -> i32 {
    status((|| {
        let s = unsafe { object::<Session>(p) }?;
        if s.poisoned {
            return Err(abi::DEVICE_LOST);
        }
        let words = unsafe { span(metadata) }?;
        if words.len() < 3 || words[1] != s.source || words[2] < s.revision {
            return Err(abi::INVALID);
        }
        if words[2] == s.revision {
            return Ok(());
        }
        let (textures, buffers) = s
            .table
            .abi_retired_resources(words)
            .map_err(|_| abi::INVALID)?;
        // Never turn resource retirement into a hidden GPU wait in submit.
        if (!textures.is_empty() || !buffers.is_empty()) && !s.inner.is_idle().map_err(error)? {
            return Err(abi::BUSY);
        }
        for id in textures {
            s.inner.release_texture(id).map_err(error)?;
        }
        for id in buffers {
            s.inner.release_buffer(id).map_err(error)?;
        }
        match s.table.sync_abi_snapshot(words) {
            Ok((_, revision)) => s.revision = revision,
            Err(_) => {
                s.poisoned = true;
                return Err(abi::INVALID);
            }
        }
        Ok(())
    })())
}
unsafe extern "C" fn image(p: Object, slot: u32, out: *mut abi::ImageInfo) -> i32 {
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(abi::ImageInfo {
            handle: -1,
            ..Default::default()
        });
    }
    status((|| {
        let s = unsafe { object::<Session>(p) }?;
        let id = s.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        let image = s.inner.image(id).map_err(error)?;
        let handle = image.shared_handle().duplicate().map_err(handle_error)?;
        let info = abi::ImageInfo {
            width: image.width(),
            height: image.height(),
            handle: handle.as_raw(),
            reserved: 0,
        };
        std::mem::forget(handle);
        unsafe {
            out.write(info);
        }
        Ok(())
    })())
}
unsafe extern "C" fn readback(
    p: Object,
    slot: u32,
    out: *mut u8,
    len: usize,
    stride: u32,
    rect: abi::Rect,
) -> i32 {
    status((|| {
        if out.is_null() || len > isize::MAX as usize {
            return Err(abi::INVALID);
        }
        let s = unsafe { object::<Session>(p) }?;
        let id = s.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        let rect = ir::PixelRect::new(rect.x, rect.y, rect.width, rect.height)
            .map_err(|_| abi::INVALID)?;
        s.inner
            .readback_bgra(
                id,
                unsafe { std::slice::from_raw_parts_mut(out, len) },
                stride,
                rect,
            )
            .map_err(error)
    })())
}
unsafe extern "C" fn import_bgra(p: Object, slot: u32, raw: i32) -> i32 {
    status((|| {
        // Ownership transfers even if subsequent validation fails.
        let handle = unsafe { adreno::Handle::from_raw(raw) }.map_err(handle_error)?;
        let s = unsafe { object::<Session>(p) }?;
        let id = s.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        s.inner
            .import_shared_bgra_texture(id, handle)
            .map_err(error)
    })())
}
unsafe extern "C" fn release_import(p: Object, slot: u32) -> i32 {
    status((|| {
        let s = unsafe { object::<Session>(p) }?;
        let id = s.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        s.inner.release_imported_texture(id).map_err(error)
    })())
}
unsafe extern "C" fn execute(p: Object, batch: *const abi::Batch) -> i32 {
    status((|| {
        let s = unsafe { object::<Session>(p) }?;
        if s.poisoned {
            return Err(abi::DEVICE_LOST);
        }
        let batch = unsafe { batch.as_ref() }.ok_or(abi::INVALID)?;
        let commands =
            unsafe { ir::CommandBuffer::from_abi(&s.table, s.source, core_batch(*batch)) }
                .map_err(|_| abi::INVALID)?;
        s.inner.executor().execute(&commands).map_err(error)
    })())
}
unsafe extern "C" fn submit(p: Object, batch: *const abi::Batch, out: *mut abi::SubmitResult) {
    if out.is_null() {
        return;
    }
    unsafe {
        out.write(abi::SubmitResult::default());
    }
    let result = (|| {
        let s = unsafe { object::<Session>(p) }?;
        if s.poisoned {
            return Err(abi::DEVICE_LOST);
        }
        let batch = unsafe { batch.as_ref() }.ok_or(abi::INVALID)?;
        let commands =
            unsafe { ir::CommandBuffer::from_abi(&s.table, s.source, core_batch(*batch)) }
                .map_err(|_| abi::INVALID)?;
        let (disposition, error, receipt) = match s.inner.executor().submit(&commands) {
            Ok(receipt) => (abi::ACCEPTED, abi::OK, receipt),
            Err(SubmitError::Busy) => return Err(abi::BUSY),
            Err(SubmitError::Rejected(e)) => return Err(error(e)),
            Err(SubmitError::Failed {
                error: e,
                completion,
            }) => (abi::PARTIAL, error(e), completion),
            Err(_) => return Err(abi::DEVICE_LOST),
        };
        Ok(abi::SubmitResult {
            disposition,
            error,
            receipt: Box::into_raw(Box::new(receipt)).cast(),
        })
    })();
    unsafe {
        out.write(match result {
            Ok(value) => value,
            Err(error) => abi::SubmitResult {
                error,
                ..Default::default()
            },
        });
    }
}
unsafe extern "C" fn wait(p: Object, timeout: u64, out: *mut u32) -> i32 {
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(abi::PENDING);
    }
    status((|| {
        if p.is_null() {
            return Err(abi::INVALID);
        }
        let state = unsafe {
            object::<adreno::Submission>(p)?.wait(if timeout == u64::MAX {
                None
            } else {
                Some(Duration::from_nanos(timeout))
            })
        }
        .map_err(error)?;
        unsafe {
            out.write(if state == CompletionStatus::Complete {
                abi::COMPLETE
            } else {
                abi::PENDING
            });
        }
        Ok(())
    })())
}
unsafe extern "C" fn drop_receipt(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<adreno::Submission>()));
        }
    }
}

const fn name<const N: usize>(s: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < s.len() {
        out[i] = s[i];
        i += 1;
    }
    out
}

/// Obtain the C ABI table. All callable addresses belong to this library.
/// # Safety
/// `out` must reference writable storage for `size` bytes when non-null.
/// `host` must be readable and its feature bits must describe the actual CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sgfx_backend_get_api_v2(
    version: u32,
    size: usize,
    host: *const abi::HostInfo,
    out: *mut abi::BackendApi,
) -> i32 {
    if version != abi::VERSION || size < core::mem::size_of::<abi::BackendApi>() {
        return abi::ABI_MISMATCH;
    }
    if out.is_null() {
        return abi::INVALID;
    }
    let Some(host) = (unsafe { host.as_ref() }) else {
        return abi::INVALID;
    };
    if host.size as usize != core::mem::size_of::<abi::HostInfo>() || host.reserved != 0 {
        return abi::ABI_MISMATCH;
    }
    // A cdylib does not run std's executable startup and has no private auxv.
    // Keep outline atomics at the same CPU capability as the initialized host.
    // This compiler-builtins hook updates its feature flag atomically.
    #[cfg(all(target_arch = "aarch64", target_os = "scarlet"))]
    if host.cpu_features & abi::CPU_AARCH64_LSE != 0 {
        unsafe extern "C" {
            fn __rust_enable_lse();
        }
        unsafe {
            __rust_enable_lse();
        }
    }
    unsafe {
        out.write(abi::BackendApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::BackendApi>() as u32,
            name: name(b"scarlet-adreno"),
            gpu_backend: name(b"qcom-adreno"),
            open,
            drop_device,
            create_context,
            drop_context,
            create_session,
            drop_session,
            sync_resources,
            image,
            readback,
            import_bgra,
            release_import,
            execute,
            submit,
            wait,
            drop_receipt,
        });
    }
    abi::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{MaybeUninit, size_of};

    fn host() -> abi::HostInfo {
        abi::HostInfo {
            size: size_of::<abi::HostInfo>() as u32,
            reserved: 0,
            cpu_features: 0,
        }
    }

    #[test]
    fn negotiates_adreno_identity_without_opening_a_gpu() {
        let mut table = MaybeUninit::<abi::BackendApi>::uninit();
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    &host(),
                    table.as_mut_ptr(),
                )
            },
            abi::OK
        );
        let table = unsafe { table.assume_init() };
        assert_eq!(table.name, name(b"scarlet-adreno"));
        assert_eq!(table.gpu_backend, name(b"qcom-adreno"));
        assert_eq!(table.version, abi::VERSION);
        assert_eq!(table.size as usize, size_of::<abi::BackendApi>());
    }

    #[test]
    fn rejects_mismatched_abi_before_writing_a_table() {
        let mut table = MaybeUninit::<abi::BackendApi>::uninit();
        for (version, size) in [(1, size_of::<abi::BackendApi>()), (abi::VERSION, 1)] {
            assert_eq!(
                unsafe { sgfx_backend_get_api_v2(version, size, &host(), table.as_mut_ptr()) },
                abi::ABI_MISMATCH
            );
        }
        let mut invalid = host();
        invalid.reserved = 1;
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    &invalid,
                    table.as_mut_ptr(),
                )
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_api_v2(
                    abi::VERSION,
                    size_of::<abi::BackendApi>(),
                    &host(),
                    core::ptr::null_mut(),
                )
            },
            abi::INVALID
        );
    }

    #[test]
    fn invalid_ffi_inputs_initialize_outputs_and_validate_spans() {
        let mut object = core::ptr::dangling_mut::<core::ffi::c_void>();
        let mut caps = u64::MAX;
        assert_eq!(
            unsafe {
                open(
                    Span {
                        data: core::ptr::null(),
                        len: 1,
                    },
                    &mut object,
                    &mut caps,
                )
            },
            abi::INVALID
        );
        assert!(object.is_null());
        assert_eq!(caps, 0);
        assert!(
            unsafe {
                span::<u64>(Span {
                    data: core::ptr::null(),
                    len: 0,
                })
            }
            .unwrap()
            .is_empty()
        );
        assert!(
            unsafe {
                span::<u64>(Span {
                    data: core::ptr::dangling(),
                    len: usize::MAX,
                })
            }
            .is_err()
        );
        let mut result = abi::SubmitResult::default();
        unsafe { submit(core::ptr::null_mut(), core::ptr::null(), &mut result) };
        assert_eq!(result.error, abi::INVALID);
        assert_eq!(result.disposition, abi::REJECTED);
        assert!(result.receipt.is_null());
    }

    #[test]
    fn validation_failures_do_not_report_device_loss() {
        use sgfx_codegen_adreno_a6xx::CompileError as C;
        assert_eq!(
            error(adreno::IrSubmitError::Codegen(C::OutOfBounds)),
            abi::INVALID
        );
        assert_eq!(
            error(adreno::IrSubmitError::Codegen(C::OutOfMemory)),
            abi::OUT_OF_MEMORY
        );
        assert_eq!(
            error(adreno::IrSubmitError::Codegen(C::UnsupportedFeature)),
            abi::UNSUPPORTED
        );
        assert_eq!(
            error(adreno::IrSubmitError::CompletionUnavailable),
            abi::DEVICE_LOST
        );
    }
}
