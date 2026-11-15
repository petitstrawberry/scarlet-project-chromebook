//! A618 scratch layout and address-free private-memory register emission.
//!
//! Mesa 3f1b217b: tu_setup_pvtmem, freedreno_devices.py (618), a6xx.xml.
//! Scratch is a separate caller-owned allocation, never an upload-arena object.
//! Cache ownership and completion retention belong to the backend.
use super::Emitter;
use crate::{Access, CompileError, CompileInput, ObjectId, ObjectRef, ResourceKind, compiler};
use sgfx_core::ir::BufferUsage;
use sgfx_shader_ir3::Metadata;

pub const PRIVATE_MEMORY_ALIGNMENT: u64 = 4096;
const FIBERS_PER_SP: u32 = 128 * 16;
const SP_CORES: u32 = 1;

/// A distinct scratch buffer; do not alias code, uniforms, user buffers or
/// scratch allocated for a different shader/layout. Reusing this exact shader's
/// allocation is safe with A618's hardware wave/fiber indexing. Keep it alive
/// until every referencing submission completes, including cache retirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativePrivateMemory {
    pub object: ObjectId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrivateMemoryLayout {
    pub per_fiber_bytes: u32,
    pub per_sp_bytes: u32,
    pub allocation_bytes: u64,
    pub per_wave: bool,
}

impl PrivateMemoryLayout {
    pub fn for_shader(metadata: &Metadata) -> Result<Option<Self>, CompileError> {
        if metadata.gpu_id != 618 || metadata.abi_version != 1 {
            return Err(CompileError::InvalidResource);
        }
        Self::new(metadata.pvtmem_size, metadata.pvtmem_per_wave)
    }

    pub fn new(bytes_per_fiber: u32, per_wave: bool) -> Result<Option<Self>, CompileError> {
        if bytes_per_fiber == 0 {
            return Ok(None);
        }
        let aligned = bytes_per_fiber
            .checked_add(511)
            .ok_or(CompileError::Overflow)?
            & !511;
        let per_fiber_bytes = aligned
            .checked_next_power_of_two()
            .ok_or(CompileError::Overflow)?;
        // MEMSIZEPERITEM is an eight-bit count of 512-byte blocks. Do not
        // silently wrap its maximum or enable the separate hardware call stack.
        if per_fiber_bytes >> 9 > 0xff {
            return Err(CompileError::OutOfBounds);
        }
        let per_sp_bytes = per_fiber_bytes
            .checked_mul(FIBERS_PER_SP)
            .ok_or(CompileError::Overflow)?;
        if per_sp_bytes % 4096 != 0 || per_sp_bytes >> 12 > 0x3ffff || per_sp_bytes >> 11 > 0x7ffff
        {
            return Err(CompileError::OutOfBounds);
        }
        Ok(Some(Self {
            per_fiber_bytes,
            per_sp_bytes,
            allocation_bytes: u64::from(per_sp_bytes) * u64::from(SP_CORES),
            per_wave,
        }))
    }
    pub fn parameter_word(self) -> u32 {
        self.per_fiber_bytes >> 9
    }
    pub fn size_word(self) -> u32 {
        (self.per_sp_bytes >> 12) | (u32::from(self.per_wave) << 31)
    }
    pub fn stack_word(self) -> u32 {
        self.per_sp_bytes >> 11
    }
}

pub fn validate_private_memory(
    metadata: &Metadata,
    memory: Option<&NativePrivateMemory>,
) -> Result<(), CompileError> {
    if PrivateMemoryLayout::for_shader(metadata)?.is_some() != memory.is_some() {
        return Err(CompileError::UnsupportedFeatureReason(
            "native private memory allocation missing or unexpected",
        ));
    }
    Ok(())
}

impl Emitter {
    /// Emit the final four words of the seven-register shader-program burst:
    /// PARAM, BASE_LO, BASE_HI, SIZE. Caller then writes returned STACK_OFFSET.
    /// All address words remain zero until the normal attachment relocation.
    pub(super) fn native_private_memory(
        &mut self,
        metadata: &Metadata,
        memory: Option<&NativePrivateMemory>,
        input: CompileInput<'_>,
    ) -> Result<u32, CompileError> {
        validate_private_memory(metadata, memory)?;
        let Some(layout) = PrivateMemoryLayout::for_shader(metadata)? else {
            self.extend_words(&[0; 4])?;
            return Ok(0);
        };
        let memory = memory.ok_or(CompileError::InvalidResource)?;
        let resource = compiler::find_resource(input.resources, memory.object)?;
        if !matches!(resource.kind, ResourceKind::Buffer { usage } if usage.contains(BufferUsage::STORAGE))
        {
            return Err(CompileError::InvalidResource);
        }
        compiler::validate_range(resource.size, 0, layout.allocation_bytes)?;
        self.push_word(layout.parameter_word())?;
        self.address_words(
            ObjectRef::External(memory.object),
            0,
            layout.allocation_bytes,
            Access::READ | Access::WRITE,
        )?;
        self.push_word(layout.size_word())?;
        Ok(layout.stack_word())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a618_rounding_and_wave_layout_match_mesa() {
        for (bytes, rounded) in [
            (1, 512),
            (512, 512),
            (513, 1024),
            (1024, 1024),
            (1025, 2048),
            (65536, 65536),
        ] {
            for wave in [false, true] {
                let l = PrivateMemoryLayout::new(bytes, wave).unwrap().unwrap();
                assert_eq!(l.per_fiber_bytes, rounded);
                assert_eq!(l.allocation_bytes, u64::from(rounded) * 2048);
                assert_eq!(l.parameter_word(), rounded / 512);
                assert_eq!(l.size_word() & 0x7fffffff, rounded / 2);
                assert_eq!(l.size_word() >> 31, u32::from(wave));
                assert_eq!(l.stack_word(), rounded);
                assert_eq!(l.allocation_bytes % PRIVATE_MEMORY_ALIGNMENT, 0);
            }
        }
        assert_eq!(PrivateMemoryLayout::new(0, true).unwrap(), None);
        assert!(PrivateMemoryLayout::new(65537, false).is_err());
        assert!(PrivateMemoryLayout::new(u32::MAX, false).is_err());
    }
}
