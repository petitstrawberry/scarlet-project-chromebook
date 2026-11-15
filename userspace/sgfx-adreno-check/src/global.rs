// SPDX-License-Identifier: GPL-2.0-only
use sgfx_core::ir::{CommandEncoder, ResourceBarrier, ResourceTable};
pub fn check(library: &str) -> Result<(), String> {
    let table = ResourceTable::new();
    let mut session = crate::abi_session::Session::new(library, &table, &[])?;
    if session.capabilities & sgfx_backend_abi::GLOBAL_RESOURCE_BARRIERS == 0 {
        return Err("global barrier capability missing".into());
    }
    let mut encoder = CommandEncoder::new(&table);
    encoder
        .resource_barrier(ResourceBarrier::Global)
        .map_err(|e| format!("barrier: {e:?}"))?;
    let commands = encoder.finish().map_err(|e| format!("finish: {e:?}"))?;
    session.submit(&commands)?;
    println!("SGFX_ADRENO_GLOBAL_BARRIER_OK");
    Ok(())
}
