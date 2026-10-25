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

The adjacent `sgfx-backend-scarlet-adreno-plugin` exposes this implementation
as a cdylib using SGFX ABI v2. It owns sessions, resource tables, imported handles
and completion receipts inside the driver. Distribution integration uses
published dependency revisions; the CoachZ image does not inject local clients
or drivers into the standard Scarlet bundles.

Tracked submission supports the existing fixed pipelines, textures, depth,
scissor, draw/indexed draw, texture copy and uploads. Color textures allocate
real mip chains and array/cube layers using the shared A6xx layer-first layout.
Uploads address the selected mip and layer in both execution paths. GPU blits
support mip generation, nearest/linear filtering, bounded rectangles and flips;
2D sampling encodes mip counts, LOD clamps and mip filtering. Explicit resource
barriers use the ordered queue's existing CCU retirement and invalidation.
Buffer uploads require
four-byte-aligned offsets and byte counts, matching A6xx `CP_MEMCPY`; unaligned
writes are rejected during preflight. The synchronous executor retains its
byte-granular CPU upload path. Programmable pipelines, bind groups, compute,
and new buffer-copy IR are rejected before any upload or draw
is accepted.

Use the checked-in lockfiles as a compatible source set. The standalone
userspace locks select SGFX core `040dbb5c` and Scarlet runtime `d3b1c051`,
matching the source identities used by the current SGFX facade `20bb5b8c`.
The driver uses the current Scarlet kernel's typed physical/DMA address APIs
and requires its `begin_image_cpu_access` guard and synchronous backing
retention. The standalone driver selects kernel `72e68549`; older kernels without
those hooks do not provide the required lifetime and ordering guarantees.

## Validation

Portable production-source harnesses run with no GPU:

```sh
cargo test --locked --manifest-path tests/a618-async-queue/Cargo.toml
cargo test --locked --manifest-path tests/a618-async-preparation/Cargo.toml
cargo test --locked --manifest-path tests/a618-submit-validation/Cargo.toml
cargo test --locked --manifest-path userspace/sgfx-codegen-adreno-a6xx/Cargo.toml
cargo test --locked --manifest-path shared/adreno-a6xx-layout/Cargo.toml
cargo test --locked --manifest-path userspace/sgfx-backend-scarlet-adreno-plugin/Cargo.toml
```

Use the Scarlet toolchain for the backend target checks:

```sh
cargo check --locked --manifest-path userspace/sgfx-backend-scarlet-adreno/Cargo.toml --no-default-features --features std --target aarch64-unknown-scarlet
cargo check --locked --manifest-path userspace/sgfx-backend-scarlet-adreno/Cargo.toml --no-default-features --features std --target riscv64gc-unknown-scarlet
cargo check --locked --manifest-path drivers/gpu/qcom-adreno-a618/Cargo.toml --target aarch64-unknown-none --features scarlet/network
```

The driver [hardware validation checklist](../../drivers/gpu/qcom-adreno-a618/README.md)
covers real DMA fence visibility, IRQ delivery, pending close/detach, and fault
shutdown. Those behaviors require a CoachZ/A618 boot and have not been verified
by host tests or target compilation alone.

## Shared NV12 video

The dynamic driver exposes the SGFX v2 YCbCr import extension. Immutable linear
NV12 image capabilities are sampled as native two-plane A618 textures; BT.601
and BT.709, limited/full range, and cosited/midpoint chroma are explicit import
parameters. The GPU performs color conversion and scaling while rendering the
video plane. Imported memory is read-only and retained through queued work.

Venus publishes an owned NV12 snapshot of each completed output (one CPU copy
of the planar bytes). The reusable decoder mmap is never shared with the GPU,
so decoding the next frame cannot overwrite a frame still being displayed.
There is no CPU NV12-to-BGRA conversion or CPU resizing on this path.
