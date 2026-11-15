//! Retired-context sampling of A618's fixed-frequency GPU clock.
use super::*;

#[path = "timestamp_clock.rs"]
mod clock;

// Mesa a6xx.xml CP_ALWAYS_ON_COUNTER; Linux a6xx_gmu_get_timestamp reads
// this same 64-bit register while holding GPU power. Units are 19.2 MHz,
// independent of GMU DVFS (Mesa tu_device.cc ALWAYS_ON_FREQUENCY).
const CP_ALWAYS_ON_COUNTER: usize = 0x0980;

pub(super) fn sample(core: &Arc<A618Core>) -> Result<u64, GpuBackendSubmitError> {
    // The checkpoint is placed behind all accepted work. Its existing full
    // fence/scratch/RPTR/idle proof retires DMA, and this reservation prevents
    // later submissions from running until the counter has been sampled.
    // It also boots a cold device through the ordinary worker path.
    let _reservation = core.begin_cpu_access()?;
    let _mapping = core.resource_mapping_gate.execution.lock();
    let hardware = core.hardware.lock();
    if hardware.boot != BootState::Ready
        || !hardware.gpu_oob_held
        || core
            .resource_mapping_gate
            .quarantine
            .load(Ordering::Acquire)
    {
        return Err(GpuBackendSubmitError::DeviceLost(
            "qcom-adreno-a618: GPU clock is unavailable after retirement",
        ));
    }
    // OOB remains held for the lifetime of Ready; hardware/mapping locks also
    // exclude DVFS, reset and teardown. Never read powered-off GX MMIO.
    let ticks = clock::read_stable(|high| {
        core.registers
            .read(CP_ALWAYS_ON_COUNTER + usize::from(high))
    })
    .ok_or(GpuBackendSubmitError::DeviceLost(
        "qcom-adreno-a618: unstable GPU clock",
    ))?;
    clock::ticks_to_ns(ticks).ok_or(GpuBackendSubmitError::Unavailable(
        "qcom-adreno-a618: GPU clock exceeds nanosecond range",
    ))
}
