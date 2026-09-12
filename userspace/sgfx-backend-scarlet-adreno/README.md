# Scarlet Adreno execution

`Executor` implements both the synchronous `CommandExecutor` and asynchronous
`CommandSubmitter`. The backend negotiates the existing A6xx relocation dialect
and queries the additive Scarlet async queue ABI. An older kernel or a queue
with zero async capacity returns `IrSubmitError::AsyncUnsupported` before work
is accepted; there is no synchronous fallback inside `submit`.

## Admission and completion

A call validates and lowers the complete fixed-function IR stream, copies all
borrowed uploads, and makes one bounded logical admission. The context's worker
then submits native chunks in order. Native `Busy` leaves the exact next chunk
queued for retry. A stream may exceed the native queue capacity and staging
arena count without exposing intermediate capacity pressure to its caller.
A logical `Busy` means nothing from that call was accepted. A later native or
completion error fails the owned receipt and stops dispatch; accepted work is
never replayed.

One context admits at most 16 logical streams with a combined 64 MiB budget for
encoded commands and owned upload bytes. A larger individual stream is rejected
before admission. Four persistent 8 MiB upload arenas are created on the first
tracked call. Chunk data is copied into an arena only after the previous native
completion using that arena reports success. Buffer uploads and texture rows
are split independently; render-pass continuation preserves color/depth stores,
loads, and draw state. Kernel admission separately limits the device to eight
native requests, including the running request.

First-use resource and arena creation may synchronize while SMMU mappings are
installed. Reusing those resources performs CPU validation/lowering and logical
admission without waiting for GPU completion or capacity. `submit` can return a
receipt before the native worker has submitted its first chunk. An empty stream
contains a real ordered kernel checkpoint.

`Submission::poll` and `wait` observe the whole logical stream, including earlier
context-ordered work. A wait timeout returns Pending and does not cancel work.
Dropping a receipt or session does not wait or cancel: queued jobs keep physical
resources, context attachments, arenas and queues alive. Attachments detach only
when their final owner is released. The kernel independently pins accepted
commands/backing/mappings through its DMA fence or safe fault retirement.

All sessions share the context dispatcher. Synchronous execution, direct context
readback, mapped-target readback and explicit imported-image release drain it
before accessing or detaching resources. GPU completion is separate from display
presentation, shared-image leases and CPU cache visibility. Raw image upload
and readback controls use the companion kernel CPU-access guard: A618 reserves
device admission and drains its FIFO before the generic layer copies or reads
pixels, retaining that reservation through cache maintenance and callbacks.
Direct writes through a CPU mapping still require the caller to wait for work
using that backing.

## Implemented subset

Tracked submission supports the existing fixed pipelines, textures, depth,
scissor, draw/indexed draw, texture copy and uploads. Buffer uploads require
four-byte-aligned offsets and byte counts, matching A6xx `CP_MEMCPY`; unaligned
writes are rejected during preflight. The synchronous executor retains its
byte-granular CPU upload path. Programmable pipelines, bind groups, compute,
explicit barriers and new buffer-copy IR are rejected before any upload or draw
is accepted.

The standalone userspace lock selects SGFX `30da20a6` and Scarlet runtime
`c0f334b7`. The driver lock selects the compatible Scarlet 0.16 kernel
`f7adec9` as its base, which contains owned async submission and its command-limit
fix. The final driver also requires the companion `begin_image_cpu_access`
kernel hook on that compatible branch. Until that companion source is published
and the production pin advances, use the integration script below with the
coordinated Scarlet checkout; the base lock alone does not supply the hook.
These use the same fixed-width GPU ABI; updating this driver to later kernel
platform/address APIs is separate from async execution.

## Validation

Portable production-source harnesses run with no GPU:

```sh
cargo test --locked --manifest-path tests/a618-async-queue/Cargo.toml
cargo test --locked --manifest-path tests/a618-async-preparation/Cargo.toml
cargo test --locked --manifest-path tests/a618-submit-validation/Cargo.toml
cargo test --locked --manifest-path userspace/sgfx-codegen-adreno-a6xx/Cargo.toml
```

Use the Scarlet toolchain for the backend target checks:

```sh
cargo check --locked --manifest-path userspace/sgfx-backend-scarlet-adreno/Cargo.toml --no-default-features --features std --target aarch64-unknown-scarlet
cargo check --locked --manifest-path userspace/sgfx-backend-scarlet-adreno/Cargo.toml --no-default-features --features std --target riscv64gc-unknown-scarlet
scripts/check-a618-kernel-integration.sh /path/to/compatible-Scarlet-checkout
```

The driver [hardware validation checklist](../../drivers/gpu/qcom-adreno-a618/README.md)
covers real DMA fence visibility, IRQ delivery, pending close/detach, and fault
shutdown. Those behaviors require a CoachZ/A618 boot and have not been verified
by host tests or target compilation alone.
