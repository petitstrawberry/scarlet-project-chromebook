// SPDX-License-Identifier: GPL-2.0-only
use crate::{Error, Metadata, Stage};
use std::{
    ffi::{CStr, CString, c_char},
    ptr,
    sync::Mutex,
};
static COMPILER: Mutex<()> = Mutex::new(());
#[repr(C)]
struct NativeResult {
    allocation: *mut u8,
    allocation_size: usize,
    metadata: *mut c_char,
    metadata_size: usize,
    error: *mut c_char,
}
unsafe extern "C" {
    fn scarlet_ir3_compile_with_samples(
        words: *const u32,
        count: usize,
        stage: u32,
        entry: *const c_char,
        push_bytes: u32,
        rasterization_samples: u32,
        out: *mut NativeResult,
    ) -> i32;
    fn scarlet_ir3_compile_with_nv12_samplers(words:*const u32,count:usize,stage:u32,entry:*const c_char,push_bytes:u32,rasterization_samples:u32,profiles:*const crate::Nv12Profile,profile_count:u32,samplers:*const crate::Nv12SamplerProfile,sampler_count:u32,out:*mut NativeResult)->i32;
    fn scarlet_ir3_result_free(out: *mut NativeResult);
}
impl Drop for NativeResult {
    fn drop(&mut self) {
        unsafe { scarlet_ir3_result_free(self) };
    }
}
pub(crate) fn compile(
    words: &[u32],
    stage: Stage,
    entry: &str,
    push_bytes: u32,
) -> Result<(Vec<u8>, Metadata), Error> {
    compile_ycbcr(words,stage,entry,push_bytes,&[],&[])
}
pub(crate) fn compile_ycbcr(words:&[u32],stage:Stage,entry:&str,push_bytes:u32,profiles:&[crate::Nv12Profile],samplers:&[crate::Nv12SamplerProfile])->Result<(Vec<u8>,Metadata),Error> {
    crate::ycbcr::validate_profiles(profiles)?;
    let _lock = COMPILER
        .lock()
        .map_err(|_| Error("compiler mutex poisoned".into()))?;
    let entry = CString::new(entry).map_err(|_| Error("invalid entry".into()))?;
    let mut out = NativeResult {
        allocation: ptr::null_mut(),
        allocation_size: 0,
        metadata: ptr::null_mut(),
        metadata_size: 0,
        error: ptr::null_mut(),
    };
    let status = unsafe {
        if profiles.is_empty() {scarlet_ir3_compile_with_samples(
            words.as_ptr(),
            words.len(),
            stage as u32,
            entry.as_ptr(),
            push_bytes,
            if stage == Stage::Fragment {
                crate::FRAGMENT_RASTERIZATION_SAMPLES
            } else {
                0
            },
            &mut out,
        )} else {scarlet_ir3_compile_with_nv12_samplers(words.as_ptr(),words.len(),stage as u32,entry.as_ptr(),push_bytes,
            if stage==Stage::Fragment {crate::FRAGMENT_RASTERIZATION_SAMPLES}else{0},profiles.as_ptr(),profiles.len() as u32,samplers.as_ptr(),samplers.len() as u32,&mut out)}
    };
    if status != 0 {
        let message = if out.error.is_null() {
            format!("Mesa IR3 compilation failed ({status})")
        } else {
            unsafe { CStr::from_ptr(out.error) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(Error(message));
    }
    if out.allocation.is_null()
        || out.metadata.is_null()
        || out.allocation_size == 0
        || out.allocation_size > 16 * 1024 * 1024
        || out.metadata_size > 1024 * 1024
    {
        return Err(Error("invalid native compiler result".into()));
    }
    let metadata: Metadata = serde_json::from_slice(unsafe {
        std::slice::from_raw_parts(out.metadata.cast(), out.metadata_size)
    })
    .map_err(|e| Error(format!("IR3 metadata: {e}")))?;
    let code_end = metadata.sizedwords as usize * 4;
    let const_end = metadata.constant_data_offset as usize + metadata.constant_data_bytes as usize;
    if metadata.abi_version != 1
        || metadata.gpu_id != 618
        || metadata.allocation_bytes as usize != out.allocation_size
        || code_end > out.allocation_size
        || const_end > out.allocation_size
        || metadata.const_allocations.len() != 13
        || metadata.rasterization_samples
            != Some(if stage == Stage::Fragment {
                crate::FRAGMENT_RASTERIZATION_SAMPLES
            } else {
                0
            })
    {
        return Err(Error(
            "inconsistent native shader allocation metadata".into(),
        ));
    }
    let allocation =
        unsafe { std::slice::from_raw_parts(out.allocation, out.allocation_size) }.to_vec();
    Ok((allocation, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_entry_rejects_unsupported_sample_counts_before_parsing() {
        for (stage, samples) in [(1, 0), (1, 2), (1, 4), (0, 1), (2, 1)] {
            let mut out = NativeResult {
                allocation: ptr::null_mut(),
                allocation_size: 0,
                metadata: ptr::null_mut(),
                metadata_size: 0,
                error: ptr::null_mut(),
            };
            let status = unsafe {
                scarlet_ir3_compile_with_samples(
                    ptr::null(),
                    0,
                    stage,
                    c"main".as_ptr(),
                    0,
                    samples,
                    &mut out,
                )
            };
            assert_ne!(status, 0);
            assert!(out.allocation.is_null() && out.metadata.is_null());
        }
    }
}
