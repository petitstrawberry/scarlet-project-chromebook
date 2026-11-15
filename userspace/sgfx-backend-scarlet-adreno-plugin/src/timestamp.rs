//! Optional SGFX v2 GPU timing extension. Capability advertising is separate.
use super::*;

unsafe extern "C" fn timestamp_ns(context: Object, out: *mut u64) -> i32 {
    if out.is_null() || !(out as usize).is_multiple_of(core::mem::align_of::<u64>()) {
        return abi::INVALID;
    }
    status((|| {
        let context = unsafe { object::<adreno::Context>(context) }?;
        let ns = context.timestamp_ns().map_err(handle_error)?;
        // SAFETY: ABI caller owns aligned writable output throughout the call.
        unsafe { out.write(ns) };
        Ok(())
    })())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sgfx_backend_get_timestamp_api_v2(
    version: u32,
    size: usize,
    out: *mut abi::TimestampApi,
) -> i32 {
    if version != abi::VERSION
        || size < core::mem::size_of::<abi::TimestampApi>()
        || out.is_null()
        || !(out as usize).is_multiple_of(core::mem::align_of::<abi::TimestampApi>())
    {
        return abi::INVALID;
    }
    // SAFETY: ABI caller supplies aligned storage at least the table's size.
    unsafe {
        out.write(abi::TimestampApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::TimestampApi>() as u32,
            timestamp_ns,
        });
    }
    abi::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        mem::{MaybeUninit, size_of},
        ptr,
    };

    #[test]
    fn real_dso_exports_timestamp_table_to_loader() {
        use sgfx_backend_loader::{LoadedBackend, Manifest};
        let library = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!(
                "libsgfx_scarlet_adreno.{}",
                std::env::consts::DLL_EXTENSION
            ));
        let loaded = LoadedBackend::load(&Manifest {
            name: "scarlet-adreno".into(),
            gpu_backend: "qcom-adreno".into(),
            library: library.to_str().unwrap().into(),
        })
        .unwrap();
        let table = loaded
            .timestamp
            .expect("TimestampApi export must be loadable");
        let mut untouched = 123;
        assert_eq!(
            unsafe { (table.timestamp_ns)(ptr::null_mut(), &mut untouched) },
            abi::INVALID
        );
        assert_eq!(untouched, 123);
    }

    #[test]
    fn timestamp_table_negotiates_and_rejects_invalid_storage() {
        let mut out = MaybeUninit::<abi::TimestampApi>::uninit();
        unsafe {
            assert_eq!(
                sgfx_backend_get_timestamp_api_v2(
                    abi::VERSION,
                    size_of::<abi::TimestampApi>(),
                    out.as_mut_ptr()
                ),
                abi::OK
            );
            let table = out.assume_init();
            assert_eq!(table.version, abi::VERSION);
            assert_eq!(table.size as usize, size_of::<abi::TimestampApi>());
            assert_eq!(
                sgfx_backend_get_timestamp_api_v2(
                    0,
                    size_of::<abi::TimestampApi>(),
                    out.as_mut_ptr()
                ),
                abi::INVALID
            );
            assert_eq!(
                sgfx_backend_get_timestamp_api_v2(abi::VERSION, 1, out.as_mut_ptr()),
                abi::INVALID
            );
            assert_eq!(
                sgfx_backend_get_timestamp_api_v2(
                    abi::VERSION,
                    size_of::<abi::TimestampApi>(),
                    ptr::null_mut()
                ),
                abi::INVALID
            );
            assert_eq!(
                (table.timestamp_ns)(ptr::null_mut(), ptr::null_mut()),
                abi::INVALID
            );
            let mut value = 123;
            assert_eq!(
                (table.timestamp_ns)(ptr::null_mut(), &mut value),
                abi::INVALID
            );
            assert_eq!(value, 123);
        }
    }
}
