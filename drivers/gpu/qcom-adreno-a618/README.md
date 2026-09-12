# A618 queue execution

The kernel accepts at most eight submissions per device, including the active
submission. Admission validates and relocates PM4 on the CPU, retains attachment
mapping owners and the generic `GpuSubmission`, and returns before GPU execution.
A full queue or a contended admission gate returns the unchanged request as
`Busy`; it neither waits for capacity nor executes a prefix.

One kernel task drains the device FIFO independently of completion observers.
Legacy synchronous submissions enter the same FIFO and wait for their own
retirement. Empty submissions and BGRA readback use an ordered hardware
checkpoint. Closing a queue, context, session, or completion handle cannot cancel
accepted work. Detaching an attachment only removes authority for future work;
accepted requests retain its mapping and backing.

Only the addressed `CACHE_FLUSH_TS` DMA sequence can establish successful
retirement. The scratch sequence, consumed ring pointer, and idle state provide
additional checks. An IRQ wakes the task but cannot substitute for the DMA
sequence. A one-millisecond timed poll also drives completion if an IRQ is lost.
The GPU deadline is one second. All resource map/unmap operations serialize
against command execution because a mapping change invalidates the complete
A618 SMMU context.

A fault or timeout stops command fetch, drains/halts the GPU bus, and shuts down
the GMU before releasing retained owners. If quiescence cannot be proved, async
observers receive `DeviceLost`, while commands, backing, mappings, and admission
capacity remain quarantined. A legacy synchronous caller remains asleep in
that exceptional case so it cannot release its caller-owned backing. Subsequent
admission fails, and accepted work already in the FIFO is settled as failure.

Managed image uploads and readbacks use the generic `begin_image_cpu_access`
guard. It reserves device-wide admission, drains earlier work with a real
checkpoint, and remains held across the CPU copy, cache maintenance, and backend
callback. Async admission returns `Busy` and synchronous admission waits until
the guard drops. Earlier accepted work can still drain while the reservation is
held. Direct writes to caller-mapped shared buffers still require caller
synchronization; the async userspace backend uses retained staging arenas and
GPU transfers for its uploads.

## Validation

The driver uses Scarlet's 64-bit physical-address API, explicit `PhysAddr`,
`Iova`, and `DmaAddr` boundaries, and generic CPU-access guards. Use Scarlet
commit `d8a199815249c784a04c9dec60b6efedbb79d84e` or a compatible successor.
The checked-in lockfile selects the published kernel revision:

```sh
cargo test --locked --manifest-path tests/a618-async-queue/Cargo.toml
cargo check --locked --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml --target aarch64-unknown-none --features scarlet/network
cargo check --locked --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml --target aarch64-unknown-none --release --features scarlet/network,strict-command-validation
```

The host tests cover admission ownership, running and quarantined capacity,
FIFO checkpoints, observer-independent lifetime, CPU-access reservation and
error cleanup, and exact fence matching. The target checks use the real
kernel interfaces in debug and release with strict command validation.
Hardware validation remains necessary on CoachZ: burst admission until Busy,
drop all observers while work is pending, detach/close resources before the
fence, mix sync/async work with raw managed uploads/readback, suppress IRQ delivery,
and inject a GPU fault/timeout to verify bus quiescence and quarantine. Host checks cannot prove
DMA visibility, interrupt routing, or the GMU/GBIF stop sequence.
