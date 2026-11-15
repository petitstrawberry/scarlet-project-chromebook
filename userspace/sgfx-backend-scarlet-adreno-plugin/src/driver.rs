//! SGFX v2 low-level resource/queue and generational program-release adapters.
//! Ownership remains in this DSO. Extension availability does not advertise
//! programmable/compute execution; device capability bits remain independent.
use super::*;

struct Resources {
    inner: adreno::IrResources,
    context: adreno::Context,
    table: Rc<ir::ResourceTable>,
    source: u64,
    revision: u64,
    poisoned: bool,
}

struct Queue {
    inner: adreno::Queue,
    context: adreno::Context,
}

unsafe fn resources<'a>(p: Object) -> Result<&'a mut Resources, i32> {
    let r = unsafe { object::<Resources>(p) }?;
    if r.poisoned {
        return Err(abi::DEVICE_LOST);
    }
    Ok(r)
}

fn ready(context: &adreno::Context) -> Result<(), i32> {
    if context.is_idle().map_err(handle_error)? {
        Ok(())
    } else {
        Err(abi::BUSY)
    }
}

unsafe extern "C" fn create_resources(p: Object, metadata: Span<u64>, out: *mut Object) -> i32 {
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
    }
    status((|| {
        let context = unsafe { object::<adreno::Context>(p) }?;
        let table = Rc::new(ir::ResourceTable::new());
        let (source, revision) = table
            .sync_abi_snapshot(unsafe { span(metadata) }?)
            .map_err(|_| abi::INVALID)?;
        let inner = context
            .create_ir_resources(Rc::clone(&table))
            .map_err(error)?;
        let value = Resources {
            inner,
            context: context.clone(),
            table,
            source,
            revision,
            poisoned: false,
        };
        unsafe {
            out.write(Box::into_raw(Box::new(value)).cast());
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_resources(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<Resources>()));
        }
    }
}

fn check_revision(words: &[u64], source: u64, revision: u64) -> Result<bool, i32> {
    if words.len() < 3 || words[1] != source || words[2] < revision {
        return Err(abi::INVALID);
    }
    Ok(words[2] != revision)
}

unsafe extern "C" fn sync_resources(p: Object, metadata: Span<u64>) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let words = unsafe { span(metadata) }?;
        if !check_revision(words, r.source, r.revision)? {
            return Ok(());
        }
        let (textures, buffers) = r
            .table
            .abi_retired_resources(words)
            .map_err(|_| abi::INVALID)?;
        let (shaders, compute, render) = r
            .table
            .abi_retired_programs(words)
            .map_err(|_| abi::INVALID)?;
        // Preserve old metadata and caches on Busy; never hide a GPU wait in
        // metadata admission. Release pipelines before their shader identities.
        if !textures.is_empty()
            || !buffers.is_empty()
            || !shaders.is_empty()
            || !compute.is_empty()
            || !render.is_empty()
        {
            ready(&r.context)?;
        }
        let result = (|| {
            for id in render {
                r.inner
                    .release_programmable_render_pipeline(id)
                    .map_err(error)?;
            }
            for id in compute {
                r.inner.release_compute_pipeline(id).map_err(error)?;
            }
            for id in shaders {
                r.inner.release_shader_module(id).map_err(error)?;
            }
            for id in textures {
                r.inner.release_texture(id).map_err(error)?;
            }
            for id in buffers {
                r.inner.release_buffer(id).map_err(error)?;
            }
            let (_, revision) = r.table.sync_abi_snapshot(words).map_err(|_| abi::INVALID)?;
            r.revision = revision;
            Ok(())
        })();
        // A partial cache retirement/import cannot be rolled back. Subsequent
        // calls must not execute against inconsistent logical/native metadata.
        if result.is_err() {
            r.poisoned = true;
        }
        result
    })())
}

unsafe extern "C" fn release_buffer(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_buffer(slot).map_err(|_| abi::INVALID)?.id();
        ready(&r.context)?;
        r.inner.release_buffer(id).map_err(error)
    })())
}

unsafe extern "C" fn release_texture(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        ready(&r.context)?;
        r.inner.release_texture(id).map_err(error)
    })())
}

unsafe extern "C" fn release_bind_group(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_bind_group(slot).map_err(|_| abi::INVALID)?.id();
        r.inner.release_bind_group(id).map_err(error)
    })())
}

#[derive(Clone, Copy)]
enum Program {
    Shader(ir::ShaderModuleId),
    Compute(ir::ComputePipelineId),
    Render(ir::ProgrammableRenderPipelineId),
}

fn program(
    table: &ir::ResourceTable,
    kind: u32,
    slot: u32,
    generation: Option<u64>,
) -> Result<Program, i32> {
    let (program, current) = match kind {
        abi::VALIDATE_SHADER => {
            let r = table.abi_shader_module(slot).map_err(|_| abi::INVALID)?;
            (Program::Shader(r.id()), r.generation())
        }
        abi::VALIDATE_COMPUTE_PIPELINE => {
            let r = table.abi_compute_pipeline(slot).map_err(|_| abi::INVALID)?;
            (Program::Compute(r.id()), r.generation())
        }
        abi::VALIDATE_RENDER_PIPELINE => {
            let r = table
                .abi_programmable_render_pipeline(slot)
                .map_err(|_| abi::INVALID)?;
            (Program::Render(r.id()), r.generation())
        }
        _ => return Err(abi::INVALID),
    };
    if generation.is_some_and(|generation| generation != current) {
        return Err(abi::INVALID);
    }
    Ok(program)
}

unsafe extern "C" fn validate(p: Object, kind: u32, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        match program(&r.table, kind, slot, None)? {
            Program::Shader(id) => r.inner.validate_shader_module(id),
            Program::Compute(id) => r.inner.validate_compute_pipeline(id),
            Program::Render(id) => r.inner.validate_programmable_render_pipeline(id),
        }
        .map_err(error)
    })())
}

unsafe fn release_program(p: Object, kind: u32, slot: u32, generation: u64) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let program = program(&r.table, kind, slot, Some(generation))?;
        ready(&r.context)?;
        match program {
            Program::Shader(id) => r.inner.release_shader_module(id),
            Program::Compute(id) => r.inner.release_compute_pipeline(id),
            Program::Render(id) => r.inner.release_programmable_render_pipeline(id),
        }
        .map_err(error)
    })())
}

unsafe extern "C" fn release_shader_module(p: Object, slot: u32, generation: u64) -> i32 {
    unsafe { release_program(p, abi::VALIDATE_SHADER, slot, generation) }
}
unsafe extern "C" fn release_compute_pipeline(p: Object, slot: u32, generation: u64) -> i32 {
    unsafe { release_program(p, abi::VALIDATE_COMPUTE_PIPELINE, slot, generation) }
}
unsafe extern "C" fn release_programmable_render_pipeline(
    p: Object,
    slot: u32,
    generation: u64,
) -> i32 {
    unsafe { release_program(p, abi::VALIDATE_RENDER_PIPELINE, slot, generation) }
}

unsafe fn output<'a>(ptr: *mut u8, len: usize) -> Result<&'a mut [u8], i32> {
    if len == 0 {
        return Ok(&mut []);
    }
    if ptr.is_null() || len > isize::MAX as usize {
        return Err(abi::INVALID);
    }
    // SAFETY: the ABI caller promises exclusive initialized writable storage.
    Ok(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
}

unsafe extern "C" fn read_buffer(
    p: Object,
    slot: u32,
    offset: u64,
    out: *mut u8,
    len: usize,
) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_buffer(slot).map_err(|_| abi::INVALID)?.id();
        r.inner
            .read_buffer_into(id, offset, unsafe { output(out, len) }?)
            .map_err(error)
    })())
}

unsafe extern "C" fn create_image(
    p: Object,
    width: u32,
    height: u32,
    out: *mut Object,
    info: *mut abi::ImageInfo,
) -> i32 {
    if out.is_null() || info.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
        info.write(abi::ImageInfo {
            handle: -1,
            ..Default::default()
        });
    }
    status((|| {
        let context = unsafe { object::<adreno::Context>(p) }?;
        let image = context
            .create_shared_image(width, height)
            .map_err(handle_error)?;
        let handle = image.shared_handle().duplicate().map_err(handle_error)?;
        let value = abi::ImageInfo {
            width: image.width(),
            height: image.height(),
            handle: handle.as_raw(),
            reserved: 0,
        };
        unsafe {
            out.write(Box::into_raw(Box::new(Arc::new(image))).cast());
            info.write(value);
        }
        // The caller owns the duplicated capability, separately from the image.
        std::mem::forget(handle);
        Ok(())
    })())
}

unsafe extern "C" fn drop_image(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<Arc<adreno::Image>>()));
        }
    }
}

unsafe extern "C" fn map_image(p: Object, slot: u32, image: Object) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let image = unsafe { object::<Arc<adreno::Image>>(image) }?;
        let id = r.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        r.inner.map_image(id, Arc::clone(image)).map_err(error)
    })())
}

unsafe extern "C" fn unmap_image(p: Object, slot: u32) -> i32 {
    status((|| {
        let r = unsafe { resources(p) }?;
        let id = r.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        ready(&r.context)?;
        r.inner.unmap_image(id).map_err(error)
    })())
}

unsafe extern "C" fn create_queue(p: Object, out: *mut Object) -> i32 {
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(core::ptr::null_mut());
    }
    status((|| {
        let context = unsafe { object::<adreno::Context>(p) }?;
        let inner = context.create_queue().map_err(handle_error)?;
        unsafe {
            out.write(
                Box::into_raw(Box::new(Queue {
                    inner,
                    context: context.clone(),
                }))
                .cast(),
            );
        }
        Ok(())
    })())
}

unsafe extern "C" fn drop_queue(p: Object) {
    if !p.is_null() {
        unsafe {
            drop(Box::from_raw(p.cast::<Queue>()));
        }
    }
}

// Classify decoded IR before admission; the backend still validates the stream.
// Transfers and barriers surrounding compute passes belong to the same receipt
// and must retire with the dispatch.
fn compute_only<'r, 'data>(
    commands: impl IntoIterator<Item = Result<ir::Command<'r, 'data>, ir::Error>>,
) -> Result<bool, i32> {
    let mut compute = false;
    for command in commands {
        match command.map_err(|_| abi::INVALID)? {
            ir::Command::BeginComputePass => compute = true,
            ir::Command::BeginRenderPass(_) => return Ok(false),
            _ => {}
        }
    }
    Ok(compute)
}

fn retire_compute_submission(
    compute_only: bool,
    receipt: &impl Completion<Error = adreno::IrSubmitError>,
    disposition: u32,
    submit_error: i32,
) -> (u32, i32) {
    if !compute_only {
        return (disposition, submit_error);
    }
    // COMPUTE_BUFFERS requires compute-only streams to retire before returning.
    // Wait on this whole-stream receipt, not context idle (which includes later
    // work). Graphics, mixed streams and transfer-only submissions stay async.
    match receipt.wait(None) {
        Ok(CompletionStatus::Complete) => (disposition, submit_error),
        // A failed observation cannot turn possibly accepted work into a
        // rejection. Keep its receipt and never report success or retryable Busy.
        Err(e) => (abi::PARTIAL, error(e)),
        Ok(_) => (abi::PARTIAL, abi::DEVICE_LOST),
    }
}

unsafe extern "C" fn submit(
    p: Object,
    resource_object: Object,
    batch: *const abi::Batch,
    out: *mut abi::SubmitResult,
) {
    if out.is_null() {
        return;
    }
    unsafe {
        out.write(abi::SubmitResult::default());
    }
    let result = (|| {
        let q = unsafe { object::<Queue>(p) }?;
        let r = unsafe { resources(resource_object) }?;
        let batch = unsafe { input(batch) }?;
        let commands =
            unsafe { ir::CommandBuffer::from_abi(&r.table, r.source, core_batch(*batch)) }
                .map_err(|_| abi::INVALID)?;
        // Header preflight avoids decoding graphics operands a second time.
        // Classification happens before admission and never allocates a mirror.
        let retire = commands.has_compute_commands().map_err(|_| abi::INVALID)?
            && compute_only(commands.iter_commands())?;
        let (disposition, error, receipt) =
            match q.inner.submit_ir_async(&q.context, &mut r.inner, &commands) {
                Ok(receipt) => (abi::ACCEPTED, abi::OK, receipt),
                Err(SubmitError::Busy) => return Err(abi::BUSY),
                Err(SubmitError::Rejected(e)) => return Err(error(e)),
                Err(SubmitError::Failed {
                    error: e,
                    completion,
                }) => (abi::PARTIAL, error(e), completion),
                Err(_) => return Err(abi::DEVICE_LOST),
            };
        let (disposition, error) = retire_compute_submission(retire, &receipt, disposition, error);
        Ok(abi::SubmitResult {
            disposition,
            error,
            receipt: into_receipt(receipt),
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

unsafe extern "C" fn read_texture(
    p: Object,
    resource_object: Object,
    slot: u32,
    out: *mut u8,
    len: usize,
) -> i32 {
    status((|| {
        let context = unsafe { object::<adreno::Context>(p) }?;
        let r = unsafe { resources(resource_object) }?;
        let id = r.table.abi_texture(slot).map_err(|_| abi::INVALID)?.id();
        context
            .read_texture_into(&mut r.inner, id, unsafe { output(out, len) }?)
            .map_err(error)
    })())
}

unsafe extern "C" fn clone_receipt(p: Object) {
    // Both base and extension tables publish Arc<Submission> data pointers.
    unsafe {
        retain_shared::<adreno::Submission>(p);
    }
}

/// Negotiate the low-level extension without changing device capability bits.
/// # Safety
/// `out` must be valid and aligned for a DriverApi when the size is sufficient.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sgfx_backend_get_driver_api_v2(
    version: u32,
    size: usize,
    out: *mut abi::DriverApi,
) -> i32 {
    if version != abi::VERSION || size < core::mem::size_of::<abi::DriverApi>() {
        return abi::ABI_MISMATCH;
    }
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(abi::DriverApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::DriverApi>() as u32,
            create_resources,
            drop_resources,
            sync_resources,
            release_buffer,
            validate,
            read_buffer,
            create_image,
            drop_image,
            map_image,
            unmap_image,
            create_queue,
            drop_queue,
            submit,
            read_texture,
            clone_receipt,
            release_texture,
            release_bind_group,
        });
    }
    abi::OK
}

/// Negotiate generation-checked program-cache retirement.
/// # Safety
/// `out` must be valid and aligned for ProgramReleaseApi when size is sufficient.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sgfx_backend_get_program_release_api_v2(
    version: u32,
    size: usize,
    out: *mut abi::ProgramReleaseApi,
) -> i32 {
    if version != abi::VERSION || size < core::mem::size_of::<abi::ProgramReleaseApi>() {
        return abi::ABI_MISMATCH;
    }
    if out.is_null() {
        return abi::INVALID;
    }
    unsafe {
        out.write(abi::ProgramReleaseApi {
            version: abi::VERSION,
            size: core::mem::size_of::<abi::ProgramReleaseApi>() as u32,
            release_shader_module,
            release_compute_pipeline,
            release_programmable_render_pipeline,
        });
    }
    abi::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use core::mem::{MaybeUninit, size_of};

    struct TestCompletion {
        waits: Cell<usize>,
        outcome: RefCell<Option<Result<CompletionStatus, adreno::IrSubmitError>>>,
    }

    impl TestCompletion {
        fn new(outcome: Result<CompletionStatus, adreno::IrSubmitError>) -> Self {
            Self {
                waits: Cell::new(0),
                outcome: RefCell::new(Some(outcome)),
            }
        }
    }

    impl Completion for TestCompletion {
        type Error = adreno::IrSubmitError;

        fn poll(&self) -> Result<CompletionStatus, Self::Error> {
            panic!("a poll is not compute retirement")
        }

        fn wait(&self, timeout: Option<Duration>) -> Result<CompletionStatus, Self::Error> {
            assert_eq!(timeout, None);
            self.waits.set(self.waits.get() + 1);
            self.outcome.borrow_mut().take().expect("one receipt wait")
        }
    }

    #[test]
    fn only_compute_streams_wait_including_surrounding_transfers() {
        fn only(commands: &[ir::Command<'_, '_>]) -> bool {
            compute_only(commands.iter().copied().map(Ok)).unwrap()
        }
        assert_eq!(
            compute_only([Err(ir::Error::InvalidDescriptor)]),
            Err(abi::INVALID)
        );
        let table = ir::ResourceTable::new();
        let buffer = table
            .define_buffer(
                ir::BufferDesc::new(16, ir::BufferUsage::COPY_SRC | ir::BufferUsage::COPY_DST)
                    .unwrap(),
            )
            .unwrap();
        let texture = table
            .define_texture(
                ir::TextureDesc::new(
                    ir::TextureFormat::Bgra8Unorm,
                    ir::Extent2D::new(4, 4).unwrap(),
                    ir::TextureUsage::RENDER_ATTACHMENT,
                )
                .unwrap(),
            )
            .unwrap();
        let render = || {
            ir::Command::BeginRenderPass(
                ir::RenderPassDesc::new(
                    &table,
                    texture,
                    ir::PixelRect::new(0, 0, 4, 4).unwrap(),
                    ir::LoadOp::Load,
                    ir::StoreOp::Store,
                )
                .unwrap(),
            )
        };
        let mut commands = vec![
            ir::Command::WriteBuffer {
                buffer,
                offset: 0,
                data: &[1, 2, 3, 4],
            },
            ir::Command::BeginComputePass,
            ir::Command::Dispatch { x: 1, y: 1, z: 1 },
            ir::Command::EndComputePass,
            ir::Command::CopyBufferToBuffer {
                source: buffer,
                source_offset: 0,
                destination: buffer,
                destination_offset: 4,
                size: 4,
            },
            ir::Command::BeginComputePass,
            ir::Command::EndComputePass,
        ];
        assert!(only(&commands));
        assert!(!only(&commands[..1])); // upload-only stays async
        assert!(!only(&[])); // empty queue checkpoint stays async
        assert!(!only(&[render(), ir::Command::EndRenderPass]));
        commands.push(render());
        commands.push(ir::Command::EndRenderPass);
        assert!(!only(&commands)); // mixed, compute first
        commands.insert(0, ir::Command::EndRenderPass);
        commands.insert(0, render());
        assert!(!only(&commands)); // mixed, graphics first
    }

    #[test]
    fn compute_retirement_preserves_acceptance_and_partial_error() {
        for (disposition, error) in [(abi::ACCEPTED, abi::OK), (abi::PARTIAL, abi::OUT_OF_MEMORY)] {
            let receipt = TestCompletion::new(Ok(CompletionStatus::Complete));
            assert_eq!(
                retire_compute_submission(true, &receipt, disposition, error),
                (disposition, error),
            );
            assert_eq!(receipt.waits.get(), 1);
        }
    }

    #[test]
    fn non_compute_submission_does_not_observe_or_wait_for_gpu() {
        for disposition in [abi::ACCEPTED, abi::PARTIAL] {
            let receipt = TestCompletion::new(Err(adreno::IrSubmitError::CompletionUnavailable));
            assert_eq!(
                retire_compute_submission(false, &receipt, disposition, abi::OK),
                (disposition, abi::OK),
            );
            assert_eq!(receipt.waits.get(), 0);
        }
    }

    #[test]
    fn compute_wait_failure_never_reports_success_or_rejection() {
        for outcome in [
            Err(adreno::IrSubmitError::CompletionUnavailable),
            Ok(CompletionStatus::Pending),
        ] {
            let receipt = TestCompletion::new(outcome);
            assert_eq!(
                retire_compute_submission(true, &receipt, abi::ACCEPTED, abi::OK),
                (abi::PARTIAL, abi::DEVICE_LOST),
            );
            assert_eq!(receipt.waits.get(), 1);
        }
    }

    #[test]
    fn compute_publication_precedes_read_without_a_caller_receipt_wait() {
        // Host ordering witness, not a GPU-coherency test: publication occurs
        // only when the adapter waits on the whole logical stream. The modeled
        // stream includes an upload, GPU write and a later buffer copy.
        struct PublishingCompletion {
            words: RefCell<[u32; 4]>,
            waits: Cell<usize>,
        }
        impl Completion for PublishingCompletion {
            type Error = adreno::IrSubmitError;
            fn poll(&self) -> Result<CompletionStatus, Self::Error> {
                Ok(if self.waits.get() == 0 {
                    CompletionStatus::Pending
                } else {
                    CompletionStatus::Complete
                })
            }
            fn wait(&self, timeout: Option<Duration>) -> Result<CompletionStatus, Self::Error> {
                assert_eq!(timeout, None);
                self.waits.set(self.waits.get() + 1);
                let mut words = self.words.borrow_mut();
                words[0] = 17; // deferred CPU upload
                words[1] = words[0] * 3; // GPU storage write
                words[2] = words[1]; // trailing copy must also be published
                Ok(CompletionStatus::Complete)
            }
        }
        let receipt = PublishingCompletion {
            words: RefCell::new([0xcccccccc; 4]),
            waits: Cell::new(0),
        };
        assert_eq!(receipt.poll().unwrap(), CompletionStatus::Pending);
        assert_eq!(
            retire_compute_submission(true, &receipt, abi::ACCEPTED, abi::OK),
            (abi::ACCEPTED, abi::OK),
        );
        // Read immediately after the adapter returns; the caller never waits.
        assert_eq!(*receipt.words.borrow(), [17, 51, 51, 0xcccccccc]);
        assert_eq!(receipt.waits.get(), 1);
        assert_eq!(receipt.poll().unwrap(), CompletionStatus::Complete);
        assert_eq!(receipt.waits.get(), 1);
    }

    #[test]
    fn portable_graphics_metadata_preserves_push_storage_and_divisors() {
        let table = ir::ResourceTable::new();
        let (shader, _, _) = define_programs(&table);
        let mut layouts = Vec::new();
        let mut group_slots = Vec::new();
        for stage in [ir::ShaderStages::VERTEX, ir::ShaderStages::FRAGMENT] {
            let layout = ir::BindGroupLayoutDesc::new(
                (0..4)
                    .map(|binding| {
                        ir::BindGroupLayoutEntry::new(
                            binding,
                            stage,
                            ir::BindingType::StorageBuffer { read_only: true },
                        )
                    })
                    .collect(),
            )
            .unwrap();
            let entries = (0..4)
                .map(|binding| {
                    let buffer = table
                        .define_buffer(
                            ir::BufferDesc::new(
                                256 * 1024 + 64,
                                ir::BufferUsage::STORAGE | ir::BufferUsage::COPY_DST,
                            )
                            .unwrap(),
                        )
                        .unwrap();
                    ir::BindGroupEntry::new(
                        binding,
                        ir::BindingResource::Buffer {
                            buffer: buffer.id(),
                            // Portable storage bindings permit four-byte offsets.
                            offset: if binding % 2 == 0 { 4 } else { 64 },
                            size: 256 * 1024,
                        },
                    )
                })
                .collect();
            let group = table
                .define_bind_group(ir::BindGroupDesc::new(&table, layout.clone(), entries).unwrap())
                .unwrap();
            group_slots.push(group.slot() as u32);
            layouts.push(layout);
        }
        let layout = ir::PipelineLayoutDesc::new(layouts)
            .unwrap()
            .with_push_constant_ranges(vec![
                ir::PushConstantRange::new(ir::ShaderStages::VERTEX, 0, 128).unwrap(),
                ir::PushConstantRange::new(ir::ShaderStages::FRAGMENT, 0, 128).unwrap(),
            ])
            .unwrap();
        let vertex = ir::VertexBufferLayout::new(
            16,
            vec![ir::VertexAttribute::new(0, ir::VertexFormat::Float32x4, 0)],
        )
        .unwrap()
        .with_step_mode(ir::VertexStepMode::Instance)
        .with_instance_divisor(3)
        .unwrap();
        let shader = table.shader_module_ref(shader).unwrap();
        let pipeline = table
            .define_programmable_render_pipeline(
                ir::ProgrammableRenderPipelineDesc::new(
                    ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Vertex, "vs".into())
                        .unwrap(),
                    ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Fragment, "fs".into())
                        .unwrap(),
                    layout.clone(),
                    ir::TextureFormat::Bgra8Unorm,
                    Some(vertex),
                    ir::PrimitiveTopology::TriangleList,
                    ir::BlendState::REPLACE,
                    ir::RasterState::new(ir::CullMode::None, ir::FrontFace::CounterClockwise),
                )
                .unwrap(),
            )
            .unwrap();
        // This is the import used by DriverApi create/sync_resources. It must
        // retain these contracts even while their capability flags remain off.
        let imported = ir::ResourceTable::new();
        imported
            .sync_abi_snapshot(&table.abi_snapshot().unwrap())
            .unwrap();
        let decoded = imported
            .programmable_render_pipeline(
                imported
                    .abi_programmable_render_pipeline(pipeline.slot() as u32)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(decoded.layout(), &layout);
        assert_eq!(decoded.vertex_buffers()[0].instance_divisor(), 3);
        assert_eq!(
            decoded.vertex_buffers()[0].step_mode(),
            ir::VertexStepMode::Instance
        );
        for slot in group_slots {
            let group = imported
                .bind_group(imported.abi_bind_group(slot).unwrap())
                .unwrap();
            assert_eq!(group.entries().len(), 4);
            for entry in group.entries() {
                let ir::BindingResource::Buffer {
                    buffer,
                    offset,
                    size,
                } = entry.resource()
                else {
                    panic!("buffer binding changed during ABI import")
                };
                assert_eq!(offset, if entry.binding() % 2 == 0 { 4 } else { 64 });
                assert_eq!(size, 256 * 1024);
                assert_eq!(
                    imported
                        .buffer(imported.buffer_ref(buffer).unwrap())
                        .unwrap()
                        .size(),
                    256 * 1024 + 64
                );
            }
        }
        assert!(
            decoded
                .layout()
                .validate_push_constants(
                    ir::ShaderStages::VERTEX | ir::ShaderStages::FRAGMENT,
                    124,
                    &[1; 4]
                )
                .is_ok()
        );
        assert!(
            decoded
                .layout()
                .validate_push_constants(
                    ir::ShaderStages::VERTEX | ir::ShaderStages::FRAGMENT,
                    0,
                    &[2; 128]
                )
                .is_ok()
        );
        // Overlapping ranges require every affected stage in the update mask.
        assert!(
            decoded
                .layout()
                .validate_push_constants(ir::ShaderStages::VERTEX, 124, &[1; 4])
                .is_err()
        );
        assert!(
            decoded
                .layout()
                .validate_push_constants(ir::ShaderStages::VERTEX, 128, &[1; 4])
                .is_err()
        );
        assert!(
            decoded
                .layout()
                .validate_push_constants(ir::ShaderStages::COMPUTE, 0, &[1; 4])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_dso_exports_both_extensions_to_the_loader() {
        use sgfx_backend_loader::{LoadedBackend, discover};
        let directory =
            std::env::temp_dir().join(format!("adreno-driver-api-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let library = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!(
                "libsgfx_scarlet_adreno.{}",
                std::env::consts::DLL_EXTENSION
            ));
        assert!(library.is_file());
        std::os::unix::fs::symlink(library, directory.join("libsgfx_scarlet_adreno.so")).unwrap();
        std::fs::write(
            directory.join("scarlet-adreno.sgfx-driver"),
            include_str!("../scarlet-adreno.sgfx-driver"),
        )
        .unwrap();
        let manifest = discover(
            &[directory.to_str().unwrap().to_owned()],
            "qcom-adreno",
            None,
        )
        .unwrap();
        let loaded = LoadedBackend::load(&manifest).unwrap();
        let driver = loaded.driver.expect("DriverApi must be exported");
        let programs = loaded
            .program_release
            .expect("ProgramReleaseApi must be exported");
        let mut result = abi::SubmitResult::default();
        unsafe {
            (driver.submit)(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null(),
                &mut result,
            );
            assert_eq!(
                (programs.release_shader_module)(core::ptr::null_mut(), 0, 1),
                abi::INVALID
            );
        }
        assert_eq!(result.error, abi::INVALID);
        assert!(result.receipt.is_null());
    }

    fn api() -> abi::DriverApi {
        let mut out = MaybeUninit::uninit();
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>(),
                    out.as_mut_ptr(),
                )
            },
            abi::OK
        );
        unsafe { out.assume_init() }
    }

    fn program_api() -> abi::ProgramReleaseApi {
        let mut out = MaybeUninit::uninit();
        assert_eq!(
            unsafe {
                sgfx_backend_get_program_release_api_v2(
                    abi::VERSION,
                    size_of::<abi::ProgramReleaseApi>(),
                    out.as_mut_ptr(),
                )
            },
            abi::OK
        );
        unsafe { out.assume_init() }
    }

    #[test]
    fn negotiates_both_complete_extensions_and_rejects_short_or_wrong_versions() {
        let table = api();
        assert_eq!(table.version, abi::VERSION);
        assert_eq!(table.size as usize, size_of::<abi::DriverApi>());
        let release = program_api();
        assert_eq!(release.version, abi::VERSION);
        assert_eq!(release.size as usize, size_of::<abi::ProgramReleaseApi>());
        let mut out = MaybeUninit::<abi::DriverApi>::uninit();
        let mut programs = MaybeUninit::<abi::ProgramReleaseApi>::uninit();
        for version in [0, abi::VERSION + 1] {
            assert_eq!(
                unsafe {
                    sgfx_backend_get_driver_api_v2(
                        version,
                        size_of::<abi::DriverApi>(),
                        out.as_mut_ptr(),
                    )
                },
                abi::ABI_MISMATCH
            );
            assert_eq!(
                unsafe {
                    sgfx_backend_get_program_release_api_v2(
                        version,
                        size_of::<abi::ProgramReleaseApi>(),
                        programs.as_mut_ptr(),
                    )
                },
                abi::ABI_MISMATCH
            );
        }
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>() - 1,
                    out.as_mut_ptr(),
                )
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_program_release_api_v2(
                    abi::VERSION,
                    size_of::<abi::ProgramReleaseApi>() - 1,
                    programs.as_mut_ptr(),
                )
            },
            abi::ABI_MISMATCH
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<abi::DriverApi>(),
                    core::ptr::null_mut(),
                )
            },
            abi::INVALID
        );
        assert_eq!(
            unsafe {
                sgfx_backend_get_program_release_api_v2(
                    abi::VERSION,
                    size_of::<abi::ProgramReleaseApi>(),
                    core::ptr::null_mut(),
                )
            },
            abi::INVALID
        );
        // A host may provide a larger table allocation; write only our prefix.
        #[repr(C)]
        struct Extended {
            table: MaybeUninit<abi::DriverApi>,
            tail: u64,
        }
        let mut larger = Extended {
            table: MaybeUninit::uninit(),
            tail: 0x12345678,
        };
        assert_eq!(
            unsafe {
                sgfx_backend_get_driver_api_v2(
                    abi::VERSION,
                    size_of::<Extended>(),
                    larger.table.as_mut_ptr(),
                )
            },
            abi::OK
        );
        assert_eq!(larger.tail, 0x12345678);
    }

    #[test]
    fn invalid_calls_clear_owned_outputs_and_never_issue_gpu_work() {
        let api = api();
        let null = core::ptr::null_mut();
        let mut object = core::ptr::dangling_mut::<core::ffi::c_void>();
        let metadata = Span {
            data: core::ptr::null(),
            len: 0,
        };
        assert_eq!(
            unsafe { (api.create_resources)(null, metadata, &mut object) },
            abi::INVALID
        );
        assert!(object.is_null());
        object = core::ptr::dangling_mut();
        assert_eq!(
            unsafe { (api.create_queue)(null, &mut object) },
            abi::INVALID
        );
        assert!(object.is_null());
        let mut image = abi::ImageInfo {
            width: 123,
            height: 456,
            handle: 100,
            reserved: 1,
        };
        object = core::ptr::dangling_mut();
        assert_eq!(
            unsafe { (api.create_image)(null, 16, 16, &mut object, &mut image) },
            abi::INVALID
        );
        assert!(object.is_null());
        assert_eq!(
            (image.width, image.height, image.handle, image.reserved),
            (0, 0, -1, 0)
        );
        let mut result = abi::SubmitResult {
            disposition: abi::ACCEPTED,
            error: abi::OK,
            receipt: core::ptr::dangling_mut(),
        };
        unsafe {
            (api.submit)(null, null, core::ptr::null(), &mut result);
        }
        assert_eq!(
            (result.disposition, result.error),
            (abi::REJECTED, abi::INVALID)
        );
        assert!(result.receipt.is_null());
        assert_eq!(
            unsafe { (api.sync_resources)(null, metadata) },
            abi::INVALID
        );
        assert_eq!(
            unsafe { (api.validate)(null, abi::VALIDATE_SHADER, 0) },
            abi::INVALID
        );
        assert_eq!(
            unsafe { (api.read_buffer)(null, 0, 0, null.cast(), 0) },
            abi::INVALID
        );
        assert_eq!(
            unsafe { (api.read_texture)(null, null, 0, null.cast(), 0) },
            abi::INVALID
        );
        assert_eq!(unsafe { (api.map_image)(null, 0, null) }, abi::INVALID);
        for release in [
            api.release_buffer,
            api.release_texture,
            api.release_bind_group,
            api.unmap_image,
        ] {
            assert_eq!(unsafe { release(null, 0) }, abi::INVALID);
        }
        let programs = program_api();
        for release in [
            programs.release_shader_module,
            programs.release_compute_pipeline,
            programs.release_programmable_render_pipeline,
        ] {
            assert_eq!(unsafe { release(null, 0, 1) }, abi::INVALID);
        }
        unsafe {
            (api.drop_resources)(null);
            (api.drop_queue)(null);
            (api.drop_image)(null);
            (api.clone_receipt)(null);
            drop_receipt(null);
            (api.submit)(null, null, core::ptr::null(), null.cast());
        }
    }

    #[test]
    fn buffers_and_metadata_reject_invalid_ranges_without_dereferencing() {
        assert!(
            unsafe { output(core::ptr::null_mut(), 0) }
                .unwrap()
                .is_empty()
        );
        assert!(unsafe { output(core::ptr::null_mut(), 1) }.is_err());
        assert!(unsafe { output(core::ptr::dangling_mut(), usize::MAX) }.is_err());
        assert!(
            unsafe {
                span(Span::<u64> {
                    data: 1usize as *const u64,
                    len: 1,
                })
            }
            .is_err()
        );
        assert!(unsafe { object::<Resources>(1usize as Object) }.is_err());
        assert!(unsafe { input::<abi::Batch>(1usize as *const abi::Batch) }.is_err());
        let mut completion = abi::COMPLETE;
        assert_eq!(
            unsafe { wait(1usize as Object, 0, &mut completion) },
            abi::INVALID
        );
        assert_eq!(completion, abi::PENDING);
        assert_eq!(check_revision(&[], 7, 9), Err(abi::INVALID));
        assert_eq!(check_revision(&[0, 8, 10], 7, 9), Err(abi::INVALID));
        assert_eq!(check_revision(&[0, 7, 8], 7, 9), Err(abi::INVALID));
        assert_eq!(check_revision(&[0, 7, 9], 7, 9), Ok(false));
        assert_eq!(check_revision(&[0, 7, 10], 7, 9), Ok(true));
    }

    fn define_programs(
        table: &ir::ResourceTable,
    ) -> (
        ir::ShaderModuleId,
        ir::ComputePipelineId,
        ir::ProgrammableRenderPipelineId,
    ) {
        let shader = table
            .define_shader_module(
                ir::ShaderModuleDesc::wgsl("// ABI metadata fixture".into()).unwrap(),
            )
            .unwrap();
        let compute = table
            .define_compute_pipeline(
                ir::ComputePipelineDesc::new(
                    ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Compute, "cs".into())
                        .unwrap(),
                    ir::PipelineLayoutDesc::new(vec![]).unwrap(),
                )
                .unwrap(),
            )
            .unwrap()
            .id();
        let render = table
            .define_programmable_render_pipeline(
                ir::ProgrammableRenderPipelineDesc::new(
                    ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Vertex, "vs".into())
                        .unwrap(),
                    ir::ShaderEntryPoint::new(shader, ir::ShaderStage::Fragment, "fs".into())
                        .unwrap(),
                    ir::PipelineLayoutDesc::new(vec![]).unwrap(),
                    ir::TextureFormat::Rgba8Unorm,
                    None,
                    ir::PrimitiveTopology::TriangleList,
                    ir::BlendState::REPLACE,
                    ir::RasterState::new(ir::CullMode::None, ir::FrontFace::CounterClockwise),
                )
                .unwrap(),
            )
            .unwrap()
            .id();
        (shader.id(), compute, render)
    }

    #[test]
    fn program_release_resolves_generations_before_touching_native_caches() {
        let table = ir::ResourceTable::new();
        let (shader, compute, render) = define_programs(&table);
        let generation = table.abi_shader_module(0).unwrap().generation();
        for kind in [
            abi::VALIDATE_SHADER,
            abi::VALIDATE_COMPUTE_PIPELINE,
            abi::VALIDATE_RENDER_PIPELINE,
        ] {
            assert!(program(&table, kind, 0, Some(generation)).is_ok());
            assert!(program(&table, kind, 0, Some(generation + 1)).is_err());
            assert!(program(&table, kind, 1, None).is_err());
        }
        assert!(program(&table, 0, 0, None).is_err());
        table.release_programmable_render_pipeline(render).unwrap();
        table.release_compute_pipeline(compute).unwrap();
        table.release_shader_module(shader).unwrap();
        for kind in [
            abi::VALIDATE_SHADER,
            abi::VALIDATE_COMPUTE_PIPELINE,
            abi::VALIDATE_RENDER_PIPELINE,
        ] {
            assert!(program(&table, kind, 0, None).is_err());
        }
        define_programs(&table);
        let replacement = table.abi_shader_module(0).unwrap().generation();
        assert_ne!(generation, replacement);
        for kind in [
            abi::VALIDATE_SHADER,
            abi::VALIDATE_COMPUTE_PIPELINE,
            abi::VALIDATE_RENDER_PIPELINE,
        ] {
            assert!(program(&table, kind, 0, Some(generation)).is_err());
            assert!(program(&table, kind, 0, Some(replacement)).is_ok());
        }
    }
}
