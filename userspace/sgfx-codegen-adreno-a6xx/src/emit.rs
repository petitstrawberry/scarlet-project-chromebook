//! A6xx PM4 emission with symbolic address placeholders.

use alloc::vec::Vec;

use adreno_a6xx_pm4::{opcode, type4, type7};
use adreno_a6xx_shader_pack::{
    FragmentMeta, LinkMeta, PipelineVariant, SAMPLER_CLAMP_LINEAR, SAMPLER_CLAMP_NEAREST,
    SHADER_SIZE, ShaderMeta, VertexMeta, link_meta, shader_meta,
};
use sgfx_core::ir::{Color, CompareFunction, IndexFormat, PixelRect};

use crate::model::{
    Access, AddressEncoding, CompileError, GeneratedObject, GeneratedObjectId, GeneratedObjectKind,
    ObjectId, ObjectRef, RelocatablePm4, ResourceAccess, SymbolicAddress,
};

const CP_MEMCPY: u8 = 0x75;
const CP_MEM_WRITE: u8 = 0x3d;
// A single 252-dword transfer has stalled the CoachZ A618 command parser. Keep
// copies well below that observed failure size, while making a 256x256 BGRA
// upload fit the 64 KiB submit wire. The kernel validator enforces the same
// limit and a memory-write barrier orders every chunk.
const CP_MEMCPY_MAX_DWORDS: u32 = 128;
const CP_DRAW_INDX_OFFSET: u8 = 0x38;
const CP_LOAD_STATE6_GEOM: u8 = 0x32;
const CP_LOAD_STATE6_FRAG: u8 = 0x34;
const CP_SET_VISIBILITY_OVERRIDE: u8 = 0x64;
const CP_REG_WRITE: u8 = 0x6d;
const CP_SKIP_IB2_ENABLE_GLOBAL: u8 = 0x1d;
const CP_SKIP_IB2_ENABLE_LOCAL: u8 = 0x23;

const EVENT_CCU_INVALIDATE_DEPTH: u32 = 0x18;
const EVENT_CCU_FLUSH_COLOR_TS: u32 = 0x1d;
const EVENT_CCU_FLUSH_DEPTH_TS: u32 = 0x1c;
const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
const EVENT_CACHE_INVALIDATE: u32 = 0x31;
const EVENT_WRITE_TIMESTAMP: u32 = 1 << 30;

const FORMAT_8_8_8_8_UNORM: u32 = 0x30;
const FORMAT_32_FLOAT: u32 = 0x4a;
const DEPTH_FORMAT_32: u32 = 4;
const A2D_IFMT_FLOAT32: u32 = 4;
// Mesa fd6_format_table maps PIPE_FORMAT_B8G8R8A8_UNORM to WXYZ (1).
const A2D_BGRA_COLOR_SWAP: u32 = 1 << 10;
const MRT_BGRA_COLOR_SWAP: u32 = 1 << 13;
const BLIT_CHANNEL_MASK: u32 = 0xf << 20;
const BLIT_SOLID_COLOR: u32 = 1 << 7;
const BLIT_SCISSOR: u32 = 1 << 16;
const SOURCE_TEXTURE_REQUIRED: u32 = (1 << 20) | (1 << 22);
const RB_A2D_BLT_CNTL: u32 = 0x8c00;
const RB_A2D_PIXEL_CNTL: u32 = 0x8c01;
const RB_A2D_DEST_BUFFER_INFO: u32 = 0x8c17;
const RB_A2D_DEST_BUFFER_BASE: u32 = 0x8c18;
const RB_A2D_DEST_BUFFER_PITCH: u32 = 0x8c1a;
const RB_A2D_CLEAR_COLOR_DW0: u32 = 0x8c2c;
const GRAS_A2D_BLT_CNTL: u32 = 0x8400;
const GRAS_A2D_SRC_XMIN: u32 = 0x8401;
const GRAS_A2D_DEST_TL: u32 = 0x8405;
const GRAS_A2D_SCISSOR_TL: u32 = 0x840a;
const RB_DBG_ECO_CNTL: u32 = 0x8e04;
const RB_CCU_CNTL: u32 = 0x8e07;
const SP_A2D_OUTPUT_INFO: u32 = 0xacc0;
const TPL1_A2D_SRC_TEXTURE_INFO: u32 = 0xb4c0;
const TPL1_A2D_SRC_TEXTURE_SIZE: u32 = 0xb4c1;
const TPL1_A2D_SRC_TEXTURE_BASE: u32 = 0xb4c2;
const TPL1_A2D_SRC_TEXTURE_PITCH: u32 = 0xb4c4;

const GRAS_CL_CNTL: u32 = 0x8000;
const GRAS_CL_VS_CLIP_CULL_DISTANCE: u32 = 0x8001;
const GRAS_CL_ARRAY_SIZE: u32 = 0x8004;
const GRAS_CL_INTERP_CNTL: u32 = 0x8005;
const GRAS_CL_GUARDBAND_CLIP_ADJ: u32 = 0x8006;
const GRAS_CL_VIEWPORT_XOFFSET: u32 = 0x8010;
const GRAS_SU_POINT_MINMAX: u32 = 0x8091;
const GRAS_SU_POINT_SIZE: u32 = 0x8092;
const GRAS_SU_POLY_OFFSET_SCALE: u32 = 0x8095;
const GRAS_SU_DEPTH_BUFFER_INFO: u32 = 0x8098;
const GRAS_SC_CNTL: u32 = 0x80a0;
const GRAS_SC_RAS_MSAA_CNTL: u32 = 0x80a2;
const GRAS_SC_SCREEN_SCISSOR_CNTL: u32 = 0x80af;
const GRAS_SC_SCREEN_SCISSOR_TL: u32 = 0x80b0;
const GRAS_SC_VIEWPORT_SCISSOR_TL: u32 = 0x80d0;
const GRAS_SC_WINDOW_SCISSOR_TL: u32 = 0x80f0;
const GRAS_SC_BIN_CNTL: u32 = 0x80a1;
const GRAS_LRZ_CNTL: u32 = 0x8100;
const GRAS_LRZ_MRT_BUFFER_INFO_0: u32 = 0x8102;
const GRAS_SU_CNTL: u32 = 0x8090;
const GRAS_SU_DEPTH_PLANE_CNTL: u32 = 0x8094;
const GRAS_SU_VS_SIV_CNTL: u32 = 0x809b;
const GRAS_LRZ_PS_INPUT_CNTL: u32 = 0x8101;
const GRAS_LRZ_PS_SAMPLEFREQ_CNTL: u32 = 0x8109;
const GRAS_SU_DEPTH_CNTL: u32 = 0x8114;
const GRAS_SU_STENCIL_CNTL: u32 = 0x8115;
const RB_CNTL: u32 = 0x8800;
const RB_RENDER_CNTL: u32 = 0x8801;
const RB_RAS_MSAA_CNTL: u32 = 0x8802;
const RB_INTERP_CNTL: u32 = 0x8809;
const RB_PS_INPUT_CNTL: u32 = 0x880a;
const RB_PS_OUTPUT_CNTL: u32 = 0x880b;
const RB_PS_MRT_CNTL: u32 = 0x880c;
const RB_PS_OUTPUT_MASK: u32 = 0x880d;
const RB_DITHER_CNTL: u32 = 0x880e;
const RB_SRGB_CNTL: u32 = 0x880f;
const RB_MRT_CONTROL: u32 = 0x8820;
const RB_MRT_BUF_INFO: u32 = 0x8822;
const RB_MRT_PITCH: u32 = 0x8823;
const RB_MRT_BASE: u32 = 0x8825;
const RB_MRT_BASE_GMEM: u32 = 0x8827;
const RB_ALPHA_TEST_CNTL: u32 = 0x8864;
const RB_BLEND_CNTL: u32 = 0x8865;
const RB_DEPTH_PLANE_CNTL: u32 = 0x8870;
const RB_DEPTH_CNTL: u32 = 0x8871;
const RB_DEPTH_BUFFER_INFO: u32 = 0x8872;
const RB_DEPTH_BOUND_MIN: u32 = 0x8878;
const RB_STENCIL_CNTL: u32 = 0x8880;
const RB_STENCIL_BUFFER_INFO: u32 = 0x8881;
const RB_STENCIL_REF_CNTL: u32 = 0x8887;
const RB_STENCIL_MASK: u32 = 0x8888;
const RB_MODE_CNTL: u32 = 0x8811;
const RB_PS_SAMPLEFREQ_CNTL: u32 = 0x8810;
const RB_WINDOW_OFFSET: u32 = 0x8890;
const RB_LRZ_CNTL: u32 = 0x8898;
const RB_BIN_CONTROL2: u32 = 0x88d3;
const RB_WINDOW_OFFSET2: u32 = 0x88d4;
const RB_RESOLVE_GMEM_BUFFER_INFO: u32 = 0x88d5;
const RB_COLOR_FLAG_BUFFER_ADDR: u32 = 0x8903;
const RB_DEPTH_FLAG_BUFFER_BASE: u32 = 0x8900;
const VPC_RAST_CNTL: u32 = 0x9108;
const VPC_VARYING_INTERP_MODE: u32 = 0x9200;
const VPC_VARYING_REPLACE_MODE: u32 = 0x9208;
const VPC_VARYING_LM_TRANSFER_CNTL_DISABLE: u32 = 0x9212;
const VPC_VS_CLIP_CULL_CNTL: u32 = 0x9101;
const VPC_VS_SIV_CNTL: u32 = 0x9104;
const VPC_VS_CNTL: u32 = 0x9301;
const VPC_PS_CNTL: u32 = 0x9304;
const VPC_SO_OVERRIDE: u32 = 0x9306;
const VPC_VS_CLIP_CULL_CNTL_V2: u32 = 0x9311;
const VPC_VS_SIV_CNTL_V2: u32 = 0x9314;
const PC_MODE_CNTL: u32 = 0x9804;
const PC_PS_CNTL: u32 = 0x9806;
const PC_DGEN_RAST_CNTL: u32 = 0x9981;
const PC_CNTL: u32 = 0x9b00;
const PC_VS_CNTL: u32 = 0x9b01;
const PC_STEREO_RENDERING_CNTL: u32 = 0x9b07;
const VFD_CNTL_0: u32 = 0xa000;
const VFD_CNTL_1: u32 = 0xa001;
const VFD_RENDER_MODE: u32 = 0xa007;
const VFD_MODE_CNTL: u32 = 0xa009;
const VFD_INDEX_OFFSET: u32 = 0xa00e;
const VFD_VERTEX_BUFFER_BASE: u32 = 0xa010;
const VFD_VERTEX_BUFFER_SIZE: u32 = 0xa012;
const VFD_FETCH_INSTR: u32 = 0xa090;
const VFD_DEST_CNTL: u32 = 0xa0d0;
const SP_VS_CNTL_0: u32 = 0xa800;
const SP_VS_OUTPUT_CNTL: u32 = 0xa802;
const SP_VS_OUTPUT_REG: u32 = 0xa803;
const SP_VS_VPC_DEST_REG: u32 = 0xa813;
const SP_VS_PROGRAM_COUNTER_OFFSET: u32 = 0xa81b;
const SP_VS_PVT_MEM_STACK_OFFSET: u32 = 0xa825;
const SP_VS_CONFIG: u32 = 0xa823;
const SP_VS_INSTR_SIZE: u32 = 0xa824;
const SP_HS_CONFIG: u32 = 0xa83b;
const SP_DS_CONFIG: u32 = 0xa863;
const SP_GS_CONFIG: u32 = 0xa894;
const SP_PS_CNTL_0: u32 = 0xa980;
const SP_PS_PROGRAM_COUNTER_OFFSET: u32 = 0xa982;
const SP_PS_PVT_MEM_STACK_OFFSET: u32 = 0xa9a9;
const SP_BLEND_CNTL: u32 = 0xa989;
const SP_SRGB_CNTL: u32 = 0xa98a;
const SP_PS_OUTPUT_MASK: u32 = 0xa98b;
const SP_PS_OUTPUT_CNTL: u32 = 0xa98c;
const SP_PS_MRT_CNTL: u32 = 0xa98d;
const SP_PS_OUTPUT_REG: u32 = 0xa98e;
const SP_PS_MRT_REG: u32 = 0xa996;
const SP_PS_INITIAL_TEX_LOAD_CNTL: u32 = 0xa99e;
const SP_PS_INITIAL_TEX_INDEX_CMD: u32 = 0xa9a3;
const SP_PS_TSIZE: u32 = 0xa9a7;
const SP_PS_SAMPLER_BASE: u32 = 0xa9e0;
const SP_PS_TEXMEMOBJ_BASE: u32 = 0xa9e4;
const SP_PS_CONFIG: u32 = 0xab04;
const SP_PS_INSTR_SIZE: u32 = 0xab05;
const SP_MODE_CNTL: u32 = 0xab00;
const SP_GFX_USIZE: u32 = 0xab20;
const SP_VS_CONST_CONFIG: u32 = 0xb800;
const SP_PS_WAVE_CNTL: u32 = 0xb980;
const SP_LB_PARAM_LIMIT: u32 = 0xb982;
const SP_REG_PROG_ID_0: u32 = 0xb983;
const TPL1_RAS_MSAA_CNTL: u32 = 0xb300;
const TPL1_MSAA_SAMPLE_POS_CNTL: u32 = 0xb304;
const TPL1_WINDOW_OFFSET: u32 = 0xb307;
const TPL1_MODE_CNTL: u32 = 0xb309;
const SP_WINDOW_OFFSET: u32 = 0xb4d1;
const SP_UPDATE_CNTL: u32 = 0xbb08;
const SP_PS_CONST_CONFIG: u32 = 0xbb10;

#[derive(Clone, Copy)]
pub(crate) struct DrawState {
    pub(crate) variant: PipelineVariant,
    pub(crate) target: Surface,
    pub(crate) area: PixelRect,
    pub(crate) scissor: PixelRect,
    pub(crate) vertex: ObjectId,
    pub(crate) vertex_offset: u64,
    pub(crate) vertex_size: u64,
    pub(crate) stride: u32,
    pub(crate) attributes: &'static [(u32, u32)],
    pub(crate) uniforms: [u32; 20],
    pub(crate) texture: Option<Surface>,
    pub(crate) linear_sampler: bool,
    pub(crate) source_over: bool,
    pub(crate) cull: u32,
    pub(crate) depth: Option<DepthDrawState>,
    pub(crate) draw: DrawCall,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct DepthDrawState {
    pub(crate) target: Surface,
    pub(crate) compare: CompareFunction,
    pub(crate) write_enabled: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum DrawCall {
    NonIndexed {
        first_vertex: u32,
        vertex_count: u32,
    },
    Indexed(IndexedDraw),
}

#[derive(Clone, Copy)]
pub(crate) struct IndexedDraw {
    pub(crate) index: ObjectId,
    pub(crate) index_offset: u64,
    pub(crate) index_size: u64,
    pub(crate) format: IndexFormat,
    pub(crate) index_count: u32,
    pub(crate) first_index: u32,
    pub(crate) base_vertex: u32,
    pub(crate) max_indices: u32,
    pub(crate) vertex_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Surface {
    pub(crate) object: ObjectId,
    pub(crate) plane_offset: u64,
    pub(crate) plane_size: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
    pub(crate) tile_mode: u32,
    pub(crate) alpha_mask: bool,
}

pub(crate) struct Emitter {
    words: Vec<u32>,
    fixups: Vec<SymbolicAddress>,
    accesses: Vec<ResourceAccess>,
    generated_objects: Vec<GeneratedObject>,
    ccu_sequence: Option<GeneratedObjectId>,
    event_sequence: u32,
    draw_batch_active: bool,
    last_draw: Option<DrawState>,
    max_words: usize,
}

impl Emitter {
    pub(crate) fn new(max_words: u32) -> Self {
        Self {
            words: Vec::new(),
            fixups: Vec::new(),
            accesses: Vec::new(),
            generated_objects: Vec::new(),
            ccu_sequence: None,
            event_sequence: 0,
            draw_batch_active: false,
            last_draw: None,
            max_words: max_words as usize,
        }
    }

    fn reserve_words(&mut self, additional: usize) -> Result<(), CompileError> {
        if self
            .words
            .len()
            .checked_add(additional)
            .is_none_or(|length| length > self.max_words)
        {
            return Err(CompileError::CommandBudgetExceeded);
        }
        self.words
            .try_reserve(additional)
            .map_err(|_| CompileError::OutOfMemory)
    }

    fn push_word(&mut self, word: u32) -> Result<(), CompileError> {
        self.reserve_words(1)?;
        self.words.push(word);
        Ok(())
    }

    fn extend_words(&mut self, words: &[u32]) -> Result<(), CompileError> {
        self.reserve_words(words.len())?;
        self.words.extend_from_slice(words);
        Ok(())
    }

    fn packet4(&mut self, register: u32, values: &[u32]) -> Result<(), CompileError> {
        let count = u16::try_from(values.len()).map_err(|_| CompileError::InvalidPm4)?;
        self.push_word(type4(register, count).map_err(|_| CompileError::InvalidPm4)?)?;
        self.extend_words(values)
    }

    fn packet7(&mut self, opcode: u8, values: &[u32]) -> Result<(), CompileError> {
        let count = u16::try_from(values.len()).map_err(|_| CompileError::InvalidPm4)?;
        self.push_word(type7(opcode, count).map_err(|_| CompileError::InvalidPm4)?)?;
        self.extend_words(values)
    }

    fn wait_for_idle(&mut self) -> Result<(), CompileError> {
        self.packet7(opcode::WAIT_FOR_IDLE, &[])
    }

    fn record_access(
        &mut self,
        object: ObjectRef,
        offset: u64,
        size: u64,
        access: Access,
    ) -> Result<(), CompileError> {
        if matches!(object, ObjectRef::CanonicalShader(_)) {
            return Ok(());
        }
        let mut merged_offset = offset;
        let mut merged_end = offset.checked_add(size).ok_or(CompileError::Overflow)?;
        let mut index = 0;
        while index < self.accesses.len() {
            let existing = self.accesses[index];
            let existing_end = existing
                .offset
                .checked_add(existing.size)
                .ok_or(CompileError::Overflow)?;
            if existing.object == object
                && existing.access == access
                && existing.offset <= merged_end
                && merged_offset <= existing_end
            {
                merged_offset = merged_offset.min(existing.offset);
                merged_end = merged_end.max(existing_end);
                self.accesses.remove(index);
            } else {
                index += 1;
            }
        }
        self.accesses
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.accesses.push(ResourceAccess {
            object,
            offset: merged_offset,
            size: merged_end
                .checked_sub(merged_offset)
                .ok_or(CompileError::Overflow)?,
            access,
        });
        Ok(())
    }

    fn address_words(
        &mut self,
        object: ObjectRef,
        object_offset: u64,
        required_size: u64,
        access: Access,
    ) -> Result<(), CompileError> {
        let word_offset = u32::try_from(self.words.len()).map_err(|_| CompileError::Overflow)?;
        self.extend_words(&[0, 0])?;
        self.fixups
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.fixups.push(SymbolicAddress {
            word_offset,
            object,
            object_offset,
            required_size,
            access,
            encoding: AddressEncoding::GpuVa64,
        });
        self.record_access(object, object_offset, required_size, access)
    }

    fn address_register(
        &mut self,
        register: u32,
        object: ObjectRef,
        object_offset: u64,
        required_size: u64,
        access: Access,
    ) -> Result<(), CompileError> {
        self.push_word(type4(register, 2).map_err(|_| CompileError::InvalidPm4)?)?;
        self.address_words(object, object_offset, required_size, access)
    }

    fn load_direct(
        &mut self,
        opcode: u8,
        block: u32,
        state_type: u32,
        units: u32,
        values: &[u32],
    ) -> Result<(), CompileError> {
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(3 + values.len())
            .map_err(|_| CompileError::OutOfMemory)?;
        payload.push((state_type << 14) | (block << 18) | (units << 22));
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(values);
        self.packet7(opcode, &payload)
    }

    fn preload_shader(
        &mut self,
        opcode: u8,
        block: u32,
        units: u32,
        variant: adreno_a6xx_shader_pack::ShaderVariant,
    ) -> Result<(), CompileError> {
        // SS6_INDIRECT is encoded as 2 in STATE_SRC.  A6xx requires this
        // CP_LOAD_STATE6 operation in addition to programming SP_xS_BASE;
        // SP_UPDATE_CNTL deliberately invalidates the previous shader load.
        self.push_word(type7(opcode, 3).map_err(|_| CompileError::InvalidPm4)?)?;
        self.push_word((2 << 16) | (block << 18) | (units << 22))?;
        self.address_words(
            ObjectRef::CanonicalShader(variant),
            0,
            SHADER_SIZE as u64,
            Access::READ,
        )
    }

    fn load_indirect(
        &mut self,
        opcode: u8,
        block: u32,
        state_type: u32,
        units: u32,
        object: ObjectRef,
        object_offset: u64,
        required_size: u64,
    ) -> Result<(), CompileError> {
        self.push_word(type7(opcode, 3).map_err(|_| CompileError::InvalidPm4)?)?;
        self.push_word((state_type << 14) | (2 << 16) | (block << 18) | (units << 22))?;
        self.address_words(object, object_offset, required_size, Access::READ)
    }

    fn texture_descriptor_prefix(texture: Surface) -> Result<[u32; 4], CompileError> {
        // A6XX_TEX_MEMOBJ, single-level explicit-layout linear 2D BGRA8.
        // Alpha masks force RGB to one and retain physical W as alpha so the
        // canonical RGBA vertex-color shader has exact alpha-mask semantics,
        // including at linearly filtered atlas edges. Other images use the
        // identity view swizzle. Freedreno assigns explicit linear BGRA8 a
        // 64-byte minimum pitch alignment (PITCHALIGN encoding zero); a wider
        // incidental stride alignment must not change the descriptor.
        Ok([
            if texture.alpha_mask {
                0x4c00_76d0
            } else {
                0x4c00_6880
            },
            texture.width | (texture.height << 15),
            (texture.stride << 7) | (1 << 29),
            u32::try_from(
                texture
                    .plane_size
                    .checked_add(0xfff)
                    .ok_or(CompileError::Overflow)?
                    >> 12,
            )
            .map_err(|_| CompileError::Overflow)?,
        ])
    }

    fn texture_descriptor_address(&mut self, texture: Surface) -> Result<(), CompileError> {
        let word_offset = u32::try_from(self.words.len()).map_err(|_| CompileError::Overflow)?;
        self.extend_words(&[0, 1 << 17])?; // BASE placeholder + DEPTH=1.
        self.fixups
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.fixups.push(SymbolicAddress {
            word_offset,
            object: ObjectRef::External(texture.object),
            object_offset: texture.plane_offset,
            required_size: texture.plane_size,
            access: Access::READ,
            encoding: AddressEncoding::GpuVa49TexDescriptor,
        });
        self.record_access(
            ObjectRef::External(texture.object),
            texture.plane_offset,
            texture.plane_size,
            Access::READ,
        )
    }

    fn texture_state_backing(
        &mut self,
        texture: Surface,
        sampler: [u32; 4],
    ) -> Result<GeneratedObjectId, CompileError> {
        // Sampler pre-dispatch dereferences the SP base registers even when
        // CP_LOAD_STATE6 also receives direct state.  Materialize the exact
        // Mesa layout (16 descriptor dwords followed by four sampler dwords)
        // in a 64-byte aligned GPU-visible object.  The texture IOVA remains a
        // kernel relocation inside CP_MEM_WRITE data; userspace never learns
        // or forges the final GPU address.
        let state = self.add_generated(
            &[0; 80],
            64,
            Access::READ | Access::WRITE,
            GeneratedObjectKind::TextureState,
        )?;
        let descriptor = Self::texture_descriptor_prefix(texture)?;
        self.push_word(type7(CP_MEM_WRITE, 22).map_err(|_| CompileError::InvalidPm4)?)?;
        self.address_words(ObjectRef::Generated(state), 0, 80, Access::WRITE)?;
        self.extend_words(&descriptor)?;
        self.texture_descriptor_address(texture)?;
        self.extend_words(&[0; 10])?;
        self.extend_words(&sampler)?;
        self.packet7(opcode::WAIT_MEM_WRITES, &[])?;
        // The descriptor and sampler are consumed indirectly through the SP
        // base registers below.  WAIT_MEM_WRITES orders the CP store itself,
        // but it does not make an earlier cached state-block lookup observe
        // the newly written backing object.  Invalidate that cache before the
        // indirect CP_LOAD_STATE6 packets dereference it.
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])?;
        self.wait_for_idle()?;
        self.address_register(
            SP_PS_SAMPLER_BASE,
            ObjectRef::Generated(state),
            64,
            16,
            Access::READ,
        )?;
        self.address_register(
            SP_PS_TEXMEMOBJ_BASE,
            ObjectRef::Generated(state),
            0,
            64,
            Access::READ,
        )?;
        Ok(state)
    }

    pub(crate) fn begin_draw_batch(&mut self) -> Result<(), CompileError> {
        if self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        self.submission_begin_3d()?;
        self.draw_batch_active = true;
        self.last_draw = None;
        Ok(())
    }

    pub(crate) fn end_draw_batch(&mut self) -> Result<(), CompileError> {
        if !self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        self.submission_end()?;
        self.draw_batch_active = false;
        self.last_draw = None;
        Ok(())
    }

    pub(crate) fn continue_draw_batch(&mut self) -> Result<(), CompileError> {
        if !self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        // A6xx graphics state persists across draws in one IB. The first draw
        // establishes the complete sysmem baseline; subsequent draws emit the
        // bounded safety state required by the kernel plus actual deltas.
        // Replaying the restore baseline here made a retained UI frame scale
        // with hundreds of redundant register writes.
        Ok(())
    }

    pub(crate) fn draw(&mut self, draw: DrawState) -> Result<(), CompileError> {
        if !self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        if self
            .last_draw
            .is_some_and(|previous| Self::fixed_draw_state_matches(previous, draw))
        {
            self.draw_with_retained_fixed_state(draw)?;
            self.last_draw = Some(draw);
            return Ok(());
        }
        let link = link_meta(draw.variant);
        let ShaderMeta::Vertex(vs) = shader_meta(link.vs) else {
            return Err(CompileError::InvalidPm4);
        };
        let ShaderMeta::Fragment(fs) = shader_meta(link.fs) else {
            return Err(CompileError::InvalidPm4);
        };

        self.emit_program_config(vs, fs)?;
        // CP tracks RB_RENDER_CNTL and inserts a required hang-workaround WFI
        // when render modes change.  A raw type-4 write bypasses that tracker.
        self.packet7(CP_REG_WRITE, &[2, RB_RENDER_CNTL, 0x10])?;
        self.packet4(
            RB_MRT_CONTROL,
            &[
                if draw.source_over { 0x7e3 } else { 0x7e0 },
                if draw.source_over {
                    0x0701_0706
                } else {
                    0x0001_0001
                },
            ],
        )?;
        self.packet4(
            RB_MRT_BUF_INFO,
            &[FORMAT_8_8_8_8_UNORM | MRT_BGRA_COLOR_SWAP],
        )?;
        self.packet4(
            RB_MRT_PITCH,
            &[
                draw.target.stride >> 6,
                u32::try_from(draw.target.plane_size >> 6).map_err(|_| CompileError::Overflow)?,
            ],
        )?;
        self.address_register(
            RB_MRT_BASE,
            ObjectRef::External(draw.target.object),
            draw.target.plane_offset,
            draw.target.plane_size,
            Access::WRITE,
        )?;
        // This backend renders directly to linear system memory.  Program the
        // complete no-GMEM/no-UBWC MRT state instead of inheriting flag-buffer
        // or GMEM state from firmware or an earlier operation.
        self.packet4(RB_MRT_BASE_GMEM, &[0])?;
        self.packet4(RB_COLOR_FLAG_BUFFER_ADDR, &[0; 3])?;
        self.packet4(GRAS_LRZ_MRT_BUFFER_INFO_0, &[FORMAT_8_8_8_8_UNORM])?;
        self.packet4(RB_SRGB_CNTL, &[0])?;
        self.packet4(SP_SRGB_CNTL, &[0])?;
        self.packet4(GRAS_CL_ARRAY_SIZE, &[0])?;
        self.packet4(
            RB_BLEND_CNTL,
            &[if draw.source_over {
                0xffff_0001
            } else {
                0xffff_0000
            }],
        )?;
        self.packet4(SP_BLEND_CNTL, &[if draw.source_over { 1 } else { 0 }])?;
        self.packet4(RB_DITHER_CNTL, &[0])?;
        self.packet4(SP_PS_MRT_CNTL, &[1])?;
        self.packet4(RB_PS_MRT_CNTL, &[1])?;
        self.packet4(SP_PS_MRT_REG, &[FORMAT_8_8_8_8_UNORM])?;
        self.emit_depth_state(draw.depth)?;
        self.emit_single_sample_state()?;

        let target_area = PixelRect::new(0, 0, draw.target.width, draw.target.height)
            .map_err(|_| CompileError::InvalidResource)?;
        let target_br = pack_xy(draw.target.width - 1, draw.target.height - 1);
        let area_br = pack_xy(
            draw.area.x() + draw.area.width() - 1,
            draw.area.y() + draw.area.height() - 1,
        );
        let scissor_br = pack_xy(
            draw.scissor.x() + draw.scissor.width() - 1,
            draw.scissor.y() + draw.scissor.height() - 1,
        );
        // SGFX vertices are transformed to clip space against the complete
        // render target.  A render-pass area is damage, not a second logical
        // framebuffer: shrinking the hardware viewport to that area remaps
        // the whole scene into each dirty rectangle.  VirGL/wgpu keep the
        // target viewport stable and apply damage only through scissors.
        self.packet4(GRAS_CL_VIEWPORT_XOFFSET, &viewport_transform(target_area))?;
        self.packet4(
            GRAS_CL_GUARDBAND_CLIP_ADJ,
            &[guardband_clip_adj(target_area)],
        )?;
        self.packet4(
            GRAS_SC_SCREEN_SCISSOR_TL,
            &[pack_xy(draw.area.x(), draw.area.y()), area_br],
        )?;
        self.packet4(GRAS_SC_VIEWPORT_SCISSOR_TL, &[pack_xy(0, 0), target_br])?;
        self.packet4(
            GRAS_SC_WINDOW_SCISSOR_TL,
            &[pack_xy(draw.scissor.x(), draw.scissor.y()), scissor_br],
        )?;
        self.packet4(GRAS_CL_CNTL, &[0x80])?;
        self.packet4(GRAS_SU_CNTL, &[0x2010 | draw.cull])?;
        self.packet4(GRAS_SU_POINT_MINMAX, &[0x0010_0010])?;
        self.packet4(GRAS_SU_POINT_SIZE, &[0x10])?;
        self.packet4(GRAS_SU_POLY_OFFSET_SCALE, &[0, 0, 0])?;
        self.packet4(PC_CNTL, &[0])?;
        self.packet4(VPC_RAST_CNTL, &[3])?;
        self.packet4(PC_DGEN_RAST_CNTL, &[3])?;

        self.packet4(
            VFD_CNTL_0,
            &[(draw.attributes.len() as u32) | ((draw.attributes.len() as u32) << 8)],
        )?;
        self.packet4(VFD_CNTL_1, &vs.vfd_cntl_1_6)?;
        let vertex_offset = match draw.draw {
            DrawCall::NonIndexed { first_vertex, .. } => first_vertex,
            DrawCall::Indexed(indexed) => indexed.base_vertex,
        };
        self.packet4(VFD_INDEX_OFFSET, &[vertex_offset, 0])?;
        self.address_register(
            VFD_VERTEX_BUFFER_BASE,
            ObjectRef::External(draw.vertex),
            draw.vertex_offset,
            draw.vertex_size,
            Access::READ,
        )?;
        self.packet4(
            VFD_VERTEX_BUFFER_SIZE,
            &[
                u32::try_from(draw.vertex_size).map_err(|_| CompileError::Overflow)?,
                draw.stride,
            ],
        )?;
        let mut fetch = Vec::new();
        for &(format, offset) in draw.attributes {
            fetch.push(0xc000_0000 | (format << 20) | (offset << 5));
            fetch.push(1);
        }
        self.packet4(VFD_FETCH_INSTR, &fetch)?;
        self.packet4(VFD_DEST_CNTL, vs.vfd_dest_cntl)?;

        self.emit_vs(vs, link)?;
        self.emit_fs(fs)?;
        self.emit_vs_program(vs, link.vs)?;
        self.emit_fs_program(fs, link.fs)?;
        self.load_direct(CP_LOAD_STATE6_GEOM, 8, 1, 5, &draw.uniforms)?;
        self.load_direct(CP_LOAD_STATE6_FRAG, 12, 1, 5, &draw.uniforms)?;
        if let Some(texture) = draw.texture {
            // ST6_SHADER sampler units are four dwords each. Texture
            // descriptors use sixteen dwords under ST6_CONSTANTS, but padding
            // a sampler to that size makes CP_LOAD_STATE6 consume an invalid
            // direct-state payload and raises CP_ILLEGAL_INSTR_ERROR.
            let sampler = if draw.linear_sampler {
                // clamp-to-edge, linear min/mag, no mip chain, chroma-linear
                SAMPLER_CLAMP_LINEAR
            } else {
                // clamp-to-edge, nearest min/mag, no mip chain
                SAMPLER_CLAMP_NEAREST
            };
            let state = self.texture_state_backing(texture, sampler)?;
            self.load_indirect(
                CP_LOAD_STATE6_FRAG,
                4,
                0,
                1,
                ObjectRef::Generated(state),
                64,
                16,
            )?;
            self.load_indirect(
                CP_LOAD_STATE6_FRAG,
                4,
                1,
                1,
                ObjectRef::Generated(state),
                0,
                64,
            )?;
        }
        self.wait_for_idle()?;
        match draw.draw {
            DrawCall::NonIndexed { vertex_count, .. } => {
                self.packet7(CP_DRAW_INDX_OFFSET, &[0x84, 1, vertex_count])?;
            }
            DrawCall::Indexed(indexed) => {
                let initiator = match indexed.format {
                    // CP_DRAW_INDX_OFFSET uses the A4xx+ initiator layout:
                    // TRIANGLE_LIST | DMA | INDEX4_SIZE_16/32_BIT.
                    IndexFormat::Uint16 => 0x404,
                    IndexFormat::Uint32 => 0x804,
                };
                self.push_word(
                    type7(CP_DRAW_INDX_OFFSET, 7).map_err(|_| CompileError::InvalidPm4)?,
                )?;
                self.extend_words(&[initiator, 1, indexed.index_count, indexed.first_index])?;
                self.address_words(
                    ObjectRef::External(indexed.index),
                    indexed.index_offset,
                    indexed.index_size,
                    Access::READ,
                )?;
                self.push_word(indexed.max_indices)?;
            }
        }
        self.last_draw = Some(draw);
        Ok(())
    }

    fn fixed_draw_state_matches(previous: DrawState, next: DrawState) -> bool {
        previous.variant == next.variant
            && previous.target == next.target
            && previous.area == next.area
            && previous.stride == next.stride
            && previous.attributes == next.attributes
            && previous.source_over == next.source_over
            && previous.cull == next.cull
            && previous.depth == next.depth
    }

    fn draw_with_retained_fixed_state(&mut self, draw: DrawState) -> Result<(), CompileError> {
        // The complete first draw authorizes and establishes the target,
        // viewport, render-area, pipeline, and shader state. Compatibility
        // includes the exact target and pass area, so replaying those immutable
        // registers and their target relocation for every UI primitive only
        // bloats the command stream. The kernel permits this inheritance only
        // inside the same decoded submit after validating that complete draw.
        let scissor_br = pack_xy(
            draw.scissor.x() + draw.scissor.width() - 1,
            draw.scissor.y() + draw.scissor.height() - 1,
        );
        self.packet4(
            GRAS_SC_WINDOW_SCISSOR_TL,
            &[pack_xy(draw.scissor.x(), draw.scissor.y()), scissor_br],
        )?;

        let vertex_offset = match draw.draw {
            DrawCall::NonIndexed { first_vertex, .. } => first_vertex,
            DrawCall::Indexed(indexed) => indexed.base_vertex,
        };
        self.packet4(VFD_INDEX_OFFSET, &[vertex_offset, 0])?;
        self.address_register(
            VFD_VERTEX_BUFFER_BASE,
            ObjectRef::External(draw.vertex),
            draw.vertex_offset,
            draw.vertex_size,
            Access::READ,
        )?;
        self.packet4(
            VFD_VERTEX_BUFFER_SIZE,
            &[
                u32::try_from(draw.vertex_size).map_err(|_| CompileError::Overflow)?,
                draw.stride,
            ],
        )?;

        // Shader program layout and instruction preloads remain active within
        // one IB.  Re-emitting four immutable shader relocations for every UI
        // primitive bloats both the wire authority table and CP work.  The
        // kernel accepts this omission only for a bounded retained-state delta
        // following a complete shader segment in this same submission.
        self.load_direct(CP_LOAD_STATE6_GEOM, 8, 1, 5, &draw.uniforms)?;
        self.load_direct(CP_LOAD_STATE6_FRAG, 12, 1, 5, &draw.uniforms)?;

        let texture_changed = self.last_draw.is_none_or(|previous| {
            previous.texture != draw.texture || previous.linear_sampler != draw.linear_sampler
        });
        if texture_changed && let Some(texture) = draw.texture {
            let sampler = if draw.linear_sampler {
                SAMPLER_CLAMP_LINEAR
            } else {
                SAMPLER_CLAMP_NEAREST
            };
            let state = self.texture_state_backing(texture, sampler)?;
            self.load_indirect(
                CP_LOAD_STATE6_FRAG,
                4,
                0,
                1,
                ObjectRef::Generated(state),
                64,
                16,
            )?;
            self.load_indirect(
                CP_LOAD_STATE6_FRAG,
                4,
                1,
                1,
                ObjectRef::Generated(state),
                0,
                64,
            )?;
        }

        // Compatible draws are one pipelined render pass.  A WAIT_FOR_IDLE
        // here drains every earlier primitive before every following draw,
        // turning a retained UI frame into hundreds of serialized jobs.  The
        // pass begin/end paths retain the required cache and CCU barriers;
        // dynamic scissor, vertex, constant, and texture state is ordered by
        // the command stream itself.
        match draw.draw {
            DrawCall::NonIndexed { vertex_count, .. } => {
                self.packet7(CP_DRAW_INDX_OFFSET, &[0x84, 1, vertex_count])?;
            }
            DrawCall::Indexed(indexed) => {
                let initiator = match indexed.format {
                    IndexFormat::Uint16 => 0x404,
                    IndexFormat::Uint32 => 0x804,
                };
                self.push_word(
                    type7(CP_DRAW_INDX_OFFSET, 7).map_err(|_| CompileError::InvalidPm4)?,
                )?;
                self.extend_words(&[initiator, 1, indexed.index_count, indexed.first_index])?;
                self.address_words(
                    ObjectRef::External(indexed.index),
                    indexed.index_offset,
                    indexed.index_size,
                    Access::READ,
                )?;
                self.push_word(indexed.max_indices)?;
            }
        }
        Ok(())
    }

    fn emit_vs(&mut self, vs: VertexMeta, link: LinkMeta) -> Result<(), CompileError> {
        self.packet4(SP_VS_CNTL_0, &[vs.sp_vs_cntl_0])?;
        self.packet4(SP_VS_OUTPUT_CNTL, &[link.sp_vs_output_cntl])?;
        self.packet4(SP_VS_OUTPUT_REG, link.sp_vs_output_reg)?;
        self.packet4(SP_VS_VPC_DEST_REG, link.sp_vs_vpc_dest_reg)?;
        self.packet4(SP_VS_INSTR_SIZE, &[vs.sp_vs_instr_size])?;
        self.packet4(
            VPC_VARYING_LM_TRANSFER_CNTL_DISABLE,
            &link.lm_transfer_disable,
        )?;
        self.packet4(VPC_VS_CNTL, &[link.vpc_vs_cntl])?;
        self.packet4(VPC_VS_CLIP_CULL_CNTL, &[0x00ff_ff00])?;
        self.packet4(VPC_VS_CLIP_CULL_CNTL_V2, &[0x00ff_ff00])?;
        self.packet4(GRAS_CL_VS_CLIP_CULL_DISTANCE, &[0])?;
        self.packet4(VPC_PS_CNTL, &[link.vpc_ps_cntl])?;
        self.packet4(PC_VS_CNTL, &[link.pc_vs_cntl])?;
        self.packet4(VPC_VS_SIV_CNTL, &[0x0000_ffff])?;
        self.packet4(VPC_VS_SIV_CNTL_V2, &[0x0000_ffff])?;
        self.packet4(GRAS_SU_VS_SIV_CNTL, &[0])?;
        self.packet4(PC_PS_CNTL, &[0])
    }

    fn emit_fs(&mut self, fs: FragmentMeta) -> Result<(), CompileError> {
        self.packet4(SP_PS_CNTL_0, &[fs.sp_ps_cntl_0])?;
        self.packet4(
            SP_PS_INITIAL_TEX_LOAD_CNTL,
            &[&[fs.initial_tex_load_cntl], fs.initial_tex_load_cmd].concat(),
        )?;
        if !fs.initial_tex_load_cmd.is_empty() {
            // IR3 lowered the texture operation into sampler pre-dispatch.
            // A6xx still needs the parallel sampler/texture index mapping and
            // the number of active FS texture descriptors.  Both IDs are zero
            // for the canonical single-texture SGFX programs.
            self.packet4(SP_PS_INITIAL_TEX_INDEX_CMD, &[0])?;
            self.packet4(SP_PS_TSIZE, &[1])?;
        } else {
            self.packet4(SP_PS_TSIZE, &[0])?;
        }
        self.packet4(SP_REG_PROG_ID_0, &fs.sp_reg_prog_id)?;
        self.packet4(SP_PS_OUTPUT_CNTL, &[fs.sp_ps_output_cntl])?;
        self.packet4(SP_PS_OUTPUT_REG, fs.sp_ps_output_reg)?;
        self.packet4(SP_PS_OUTPUT_MASK, &[fs.sp_ps_output_mask])?;
        self.packet4(RB_PS_OUTPUT_CNTL, &[fs.rb_ps_output_cntl])?;
        self.packet4(RB_PS_OUTPUT_MASK, &[fs.rb_ps_output_mask])?;
        self.packet4(SP_PS_INSTR_SIZE, &[fs.sp_ps_instr_size])?;
        self.packet4(SP_PS_WAVE_CNTL, &[fs.sp_ps_wave_cntl])?;
        self.packet4(SP_LB_PARAM_LIMIT, &[7])?;
        self.packet4(GRAS_CL_INTERP_CNTL, &[fs.gras_cl_interp_cntl])?;
        self.packet4(RB_INTERP_CNTL, &[fs.rb_interp_cntl])?;
        self.packet4(RB_PS_INPUT_CNTL, &[fs.rb_ps_input_cntl])?;
        self.packet4(VPC_VARYING_INTERP_MODE, &[0; 8])?;
        self.packet4(VPC_VARYING_REPLACE_MODE, &[0; 8])?;
        self.packet4(RB_PS_SAMPLEFREQ_CNTL, &[0])?;
        self.packet4(GRAS_LRZ_PS_INPUT_CNTL, &[0])?;
        self.packet4(GRAS_LRZ_PS_SAMPLEFREQ_CNTL, &[0])
    }

    fn emit_program_config(
        &mut self,
        vs: VertexMeta,
        fs: FragmentMeta,
    ) -> Result<(), CompileError> {
        self.packet4(SP_VS_CONST_CONFIG, &[vs.sp_vs_const_config, 0, 0, 0])?;
        self.packet4(SP_PS_CONST_CONFIG, &[fs.sp_ps_const_config])?;
        self.packet4(SP_VS_CONFIG, &[vs.sp_vs_config])?;
        self.packet4(SP_HS_CONFIG, &[0])?;
        self.packet4(SP_DS_CONFIG, &[0])?;
        self.packet4(SP_GS_CONFIG, &[0])?;
        self.packet4(SP_PS_CONFIG, &[fs.sp_ps_config])?;
        self.packet4(SP_GFX_USIZE, &[0])
    }

    fn emit_depth_state(&mut self, depth: Option<DepthDrawState>) -> Result<(), CompileError> {
        self.packet4(RB_ALPHA_TEST_CNTL, &[0])?;
        self.packet4(RB_STENCIL_CNTL, &[0])?;
        self.packet4(GRAS_SU_STENCIL_CNTL, &[0])?;
        self.packet4(RB_STENCIL_REF_CNTL, &[0])?;
        self.packet4(RB_STENCIL_MASK, &[0, 0])?;
        self.packet4(RB_DEPTH_PLANE_CNTL, &[0])?;
        self.packet4(GRAS_SU_DEPTH_PLANE_CNTL, &[0])?;
        self.packet4(RB_DEPTH_BOUND_MIN, &[0, f32_bits(1.0)])?;
        self.packet4(RB_STENCIL_BUFFER_INFO, &[0])?;
        let Some(depth) = depth else {
            self.packet4(RB_DEPTH_CNTL, &[0])?;
            self.packet4(GRAS_SU_DEPTH_CNTL, &[0])?;
            // DEPTH_BUFFER_INFO, PITCH, ARRAY_PITCH, BASE_LO/HI and GMEM_BASE.
            self.packet4(RB_DEPTH_BUFFER_INFO, &[0; 6])?;
            self.packet4(GRAS_SU_DEPTH_BUFFER_INFO, &[0])?;
            return Ok(());
        };

        let compare = match depth.compare {
            CompareFunction::Never => 0,
            CompareFunction::Less => 1,
            CompareFunction::Equal => 2,
            CompareFunction::LessEqual => 3,
            CompareFunction::Greater => 4,
            CompareFunction::NotEqual => 5,
            CompareFunction::GreaterEqual => 6,
            CompareFunction::Always => 7,
        };
        let reads_depth = !matches!(
            depth.compare,
            CompareFunction::Never | CompareFunction::Always
        );
        let depth_control = 1
            | (u32::from(depth.write_enabled) << 1)
            | (compare << 2)
            | (u32::from(reads_depth) << 6);
        self.packet4(RB_DEPTH_CNTL, &[depth_control])?;
        self.packet4(GRAS_SU_DEPTH_CNTL, &[1])?;
        // TILE6_3 depth is uncompressed and has no UBWC flag plane. Clear the
        // complete flag-buffer state rather than inheriting firmware or an
        // earlier depth draw's compression state.
        self.packet4(RB_DEPTH_FLAG_BUFFER_BASE, &[0; 3])?;

        // Keep the six-register A6xx depth layout atomic, matching Freedreno:
        // INFO, PITCH, ARRAY_PITCH, BASE_LO/HI, and the zero sysmem GMEM base.
        self.push_word(type4(RB_DEPTH_BUFFER_INFO, 6).map_err(|_| CompileError::InvalidPm4)?)?;
        self.extend_words(&[
            DEPTH_FORMAT_32,
            depth.target.stride >> 6,
            u32::try_from(depth.target.plane_size >> 6).map_err(|_| CompileError::Overflow)?,
        ])?;
        self.address_words(
            ObjectRef::External(depth.target.object),
            depth.target.plane_offset,
            depth.target.plane_size,
            Access::READ | Access::WRITE,
        )?;
        self.push_word(0)?;
        self.packet4(GRAS_SU_DEPTH_BUFFER_INFO, &[DEPTH_FORMAT_32])
    }

    fn emit_single_sample_state(&mut self) -> Result<(), CompileError> {
        // MSAA_ONE is encoded as zero.  DEST_MSAA.MSAA_DISABLE is bit 2.
        self.packet4(TPL1_RAS_MSAA_CNTL, &[0, 4])?;
        self.packet4(GRAS_SC_RAS_MSAA_CNTL, &[0, 4, 0])?;
        self.packet4(RB_RAS_MSAA_CNTL, &[0, 4, 0])?;
        self.packet4(TPL1_MSAA_SAMPLE_POS_CNTL, &[0])?;
        self.packet4(RB_RESOLVE_GMEM_BUFFER_INFO, &[0])
    }

    fn emit_vs_program(
        &mut self,
        vs: VertexMeta,
        variant: adreno_a6xx_shader_pack::ShaderVariant,
    ) -> Result<(), CompileError> {
        self.emit_shader_program_layout(SP_VS_PROGRAM_COUNTER_OFFSET, variant)?;
        self.packet4(SP_VS_PVT_MEM_STACK_OFFSET, &[0])?;
        self.preload_shader(CP_LOAD_STATE6_GEOM, 8, vs.sp_vs_instr_size, variant)
    }

    fn emit_fs_program(
        &mut self,
        fs: FragmentMeta,
        variant: adreno_a6xx_shader_pack::ShaderVariant,
    ) -> Result<(), CompileError> {
        self.emit_shader_program_layout(SP_PS_PROGRAM_COUNTER_OFFSET, variant)?;
        self.packet4(SP_PS_PVT_MEM_STACK_OFFSET, &[0])?;
        self.preload_shader(CP_LOAD_STATE6_FRAG, 12, fs.sp_ps_instr_size, variant)
    }

    fn emit_shader_program_layout(
        &mut self,
        first_exec_register: u32,
        variant: adreno_a6xx_shader_pack::ShaderVariant,
    ) -> Result<(), CompileError> {
        // Freedreno programs FIRST_EXEC_OFFSET, OBJ_START, PVT_MEM_PARAM,
        // PVT_MEM_ADDR, and PVT_MEM_SIZE as one seven-register type-4 burst.
        // Keeping this atomic matters: splitting at OBJ_START exposes an
        // incomplete private-memory layout before CP_LOAD_STATE6 preloads the
        // shader. Every canonical shader has pvtmem_size=0, hence the four
        // trailing zero words after the two-word executable address.
        self.push_word(type4(first_exec_register, 7).map_err(|_| CompileError::InvalidPm4)?)?;
        self.push_word(0)?;
        self.address_words(
            ObjectRef::CanonicalShader(variant),
            0,
            SHADER_SIZE as u64,
            Access::READ,
        )?;
        self.extend_words(&[0; 4])
    }

    fn addressed_cache_event(&mut self, event: u32) -> Result<(), CompileError> {
        let sequence = match self.ccu_sequence {
            Some(sequence) => sequence,
            None => {
                let sequence = self.add_generated(
                    &[0; 4],
                    64,
                    Access::WRITE,
                    GeneratedObjectKind::CcuSequence,
                )?;
                self.ccu_sequence = Some(sequence);
                sequence
            }
        };
        self.event_sequence = self.event_sequence.wrapping_add(1).max(1);
        self.push_word(type7(opcode::EVENT_WRITE, 4).map_err(|_| CompileError::InvalidPm4)?)?;
        // A6xx CCU clean events are timestamp events.  Freedreno marks every
        // addressed PC_CCU_FLUSH_{COLOR,DEPTH}_TS packet with TIMESTAMP; the
        // CP then retires the event by writing its hardware timestamp to the
        // supplied address.  Omitting this bit leaves a depth flush pending
        // and the following kernel CACHE_FLUSH_TS faults on A618.
        self.push_word(event | EVENT_WRITE_TIMESTAMP)?;
        self.address_words(ObjectRef::Generated(sequence), 0, 4, Access::WRITE)?;
        self.push_word(self.event_sequence)
    }

    fn clean_color_cache(&mut self) -> Result<(), CompileError> {
        self.addressed_cache_event(EVENT_CCU_FLUSH_COLOR_TS)
    }

    fn invalidate_submission_caches(&mut self) -> Result<(), CompileError> {
        // Freedreno's A6xx cache contract is strict here: invalidating a CCU
        // that may still contain dirty color data does not work.  A command
        // buffer can contain clear -> draw, copy -> draw, or multiple draws,
        // so every color invalidation must first complete an addressed clean.
        self.clean_color_cache()?;
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CCU_INVALIDATE_COLOR])?;
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CACHE_INVALIDATE])?;
        Ok(())
    }

    fn submission_begin_2d(&mut self) -> Result<(), CompileError> {
        if self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        self.invalidate_submission_caches()?;
        self.wait_for_idle()?;
        self.packet4(RB_CCU_CNTL, &[0x0800_0000])?;
        self.packet4(RB_DBG_ECO_CNTL, &[0x0410_0000])?;
        self.packet7(opcode::SET_MARKER, &[12])
    }

    fn submission_begin_3d(&mut self) -> Result<(), CompileError> {
        self.invalidate_submission_caches()?;
        self.restore_3d_baseline()
    }

    fn restore_3d_baseline(&mut self) -> Result<(), CompileError> {
        // Start every self-contained draw from the upstream A6xx restore
        // baseline.  This backend has one synchronous context, so only state
        // reachable by this compact sysmem stream needs to be reset here.
        self.packet4(SP_UPDATE_CNTL, &[0x0000_00ff])?;
        self.wait_for_idle()?;
        self.packet4(RB_DBG_ECO_CNTL, &[0x0410_0000])?;
        self.packet4(GRAS_SC_CNTL, &[2])?;
        self.packet4(GRAS_LRZ_CNTL, &[0])?;
        self.packet4(RB_LRZ_CNTL, &[0])?;
        self.packet4(VFD_RENDER_MODE, &[0])?;
        self.packet4(VFD_MODE_CNTL, &[3])?;
        self.packet4(PC_MODE_CNTL, &[0x1f])?;
        self.packet4(PC_STEREO_RENDERING_CNTL, &[0])?;
        self.packet4(SP_MODE_CNTL, &[5])?;
        self.packet4(TPL1_MODE_CNTL, &[0xa2])?;
        self.packet4(RB_MODE_CNTL, &[0x10])?;
        self.packet4(GRAS_SC_BIN_CNTL, &[0x00c0_0000])?;
        self.packet4(RB_CNTL, &[0x00c0_0000])?;
        self.packet4(RB_BIN_CONTROL2, &[0])?;
        self.packet4(RB_WINDOW_OFFSET, &[0])?;
        self.packet4(RB_WINDOW_OFFSET2, &[0])?;
        self.packet4(SP_WINDOW_OFFSET, &[0])?;
        self.packet4(TPL1_WINDOW_OFFSET, &[0])?;
        self.packet4(GRAS_SC_SCREEN_SCISSOR_CNTL, &[0])?;
        // A6xx sysmem rendering has a single geometry pass.  VPC stream-out
        // must therefore remain enabled so the draw reaches rasterization;
        // the disable override is reserved for a GMEM draw pass that replays
        // visibility generated by an earlier binning pass.
        self.packet4(VPC_SO_OVERRIDE, &[0])?;
        self.packet7(opcode::SET_MARKER, &[1])?;
        // Do not inherit an IB2 skip policy from firmware or an earlier
        // render mode.  These are the exact A6xx sysmem-prep values used by
        // Freedreno before enabling direct rendering visibility.
        self.packet7(CP_SKIP_IB2_ENABLE_GLOBAL, &[0])?;
        self.packet7(CP_SKIP_IB2_ENABLE_LOCAL, &[1])?;
        self.packet7(CP_SET_VISIBILITY_OVERRIDE, &[1])?;
        self.packet4(RB_CCU_CNTL, &[0x0800_0000])
    }

    fn submission_end(&mut self) -> Result<(), CompileError> {
        // Retire both CCUs before returning to the trusted ring.  The final
        // general CACHE_FLUSH_TS is deliberately kernel-owned: issuing one
        // here and another for the kernel fence leaves the second event
        // permanently pending on CoachZ even though the IB and CCUs drained.
        // FD6_INVALIDATE_CCHE is an A7xx+ operation and does not belong here.
        self.addressed_cache_event(EVENT_CCU_FLUSH_COLOR_TS)?;
        self.addressed_cache_event(EVENT_CCU_FLUSH_DEPTH_TS)?;
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CCU_INVALIDATE_COLOR])?;
        self.packet7(opcode::EVENT_WRITE, &[EVENT_CCU_INVALIDATE_DEPTH])?;
        self.wait_for_idle()
    }

    fn emit_coordinates(
        &mut self,
        destination: PixelRect,
        scissor: Option<PixelRect>,
    ) -> Result<(), CompileError> {
        self.packet4(
            GRAS_A2D_DEST_TL,
            &[
                pack_xy(destination.x(), destination.y()),
                pack_xy(
                    destination.x() + destination.width() - 1,
                    destination.y() + destination.height() - 1,
                ),
            ],
        )?;
        let scissor = scissor.unwrap_or(destination);
        self.packet4(
            GRAS_A2D_SCISSOR_TL,
            &[
                pack_xy(scissor.x(), scissor.y()),
                pack_xy(
                    scissor.x() + scissor.width() - 1,
                    scissor.y() + scissor.height() - 1,
                ),
            ],
        )?;
        Ok(())
    }

    fn emit_a2d_common(
        &mut self,
        format: u32,
        ifmt: u32,
        solid: bool,
        scissor: bool,
    ) -> Result<(), CompileError> {
        let control = (format << 8)
            | BLIT_CHANNEL_MASK
            | if solid { BLIT_SOLID_COLOR } else { 0 }
            | if scissor { BLIT_SCISSOR } else { 0 }
            | (ifmt << 24);
        self.packet4(RB_A2D_BLT_CNTL, &[control])?;
        self.packet4(GRAS_A2D_BLT_CNTL, &[control])?;
        self.packet4(SP_A2D_OUTPUT_INFO, &[(format << 3) | (0xf << 12)])?;
        self.packet4(RB_A2D_PIXEL_CNTL, &[0])
    }

    fn emit_destination(
        &mut self,
        target: Surface,
        format: u32,
        color_swap: u32,
    ) -> Result<(), CompileError> {
        self.packet4(
            RB_A2D_DEST_BUFFER_INFO,
            &[format | (target.tile_mode << 8) | color_swap],
        )?;
        self.address_register(
            RB_A2D_DEST_BUFFER_BASE,
            ObjectRef::External(target.object),
            target.plane_offset,
            target.plane_size,
            Access::WRITE,
        )?;
        self.packet4(RB_A2D_DEST_BUFFER_PITCH, &[target.stride >> 6])
    }

    fn emit_blit(&mut self) -> Result<(), CompileError> {
        self.wait_for_idle()?;
        self.packet7(opcode::BLIT, &[3])?;
        self.wait_for_idle()
    }

    pub(crate) fn clear(
        &mut self,
        target: Surface,
        area: PixelRect,
        color: Color,
    ) -> Result<(), CompileError> {
        self.submission_begin_2d()?;
        self.wait_for_idle()?;
        self.emit_coordinates(area, None)?;
        let [red, green, blue, alpha] = color.components();
        self.packet4(
            RB_A2D_CLEAR_COLOR_DW0,
            &[unorm8(red), unorm8(green), unorm8(blue), unorm8(alpha)],
        )?;
        self.emit_a2d_common(FORMAT_8_8_8_8_UNORM, 0, true, true)?;
        self.emit_destination(target, FORMAT_8_8_8_8_UNORM, A2D_BGRA_COLOR_SWAP)?;
        self.emit_blit()?;
        self.submission_end()
    }

    pub(crate) fn clear_depth(
        &mut self,
        target: Surface,
        area: PixelRect,
        depth: f32,
    ) -> Result<(), CompileError> {
        if !depth.is_finite() || !(0.0..=1.0).contains(&depth) {
            return Err(CompileError::InvalidResource);
        }
        self.submission_begin_2d()?;
        self.wait_for_idle()?;
        self.emit_coordinates(area, None)?;
        self.packet4(RB_A2D_CLEAR_COLOR_DW0, &[depth.to_bits(), 0, 0, 0])?;
        self.emit_a2d_common(FORMAT_32_FLOAT, A2D_IFMT_FLOAT32, true, true)?;
        self.emit_destination(target, FORMAT_32_FLOAT, 0)?;
        self.emit_blit()?;
        self.submission_end()
    }

    pub(crate) fn copy(
        &mut self,
        source: Surface,
        source_rect: PixelRect,
        destination: Surface,
        destination_rect: PixelRect,
    ) -> Result<(), CompileError> {
        self.submission_begin_2d()?;
        self.wait_for_idle()?;
        self.packet4(
            GRAS_A2D_SRC_XMIN,
            &[
                source_rect.x() << 8,
                (source_rect.x() + source_rect.width() - 1) << 8,
                source_rect.y() << 8,
                (source_rect.y() + source_rect.height() - 1) << 8,
            ],
        )?;
        self.emit_coordinates(destination_rect, None)?;
        self.emit_a2d_common(FORMAT_8_8_8_8_UNORM, 0, false, true)?;
        self.packet4(
            TPL1_A2D_SRC_TEXTURE_INFO,
            &[FORMAT_8_8_8_8_UNORM | A2D_BGRA_COLOR_SWAP | SOURCE_TEXTURE_REQUIRED],
        )?;
        self.packet4(
            TPL1_A2D_SRC_TEXTURE_SIZE,
            &[source.width | (source.height << 15)],
        )?;
        self.address_register(
            TPL1_A2D_SRC_TEXTURE_BASE,
            ObjectRef::External(source.object),
            source.plane_offset,
            source.plane_size,
            Access::READ,
        )?;
        self.packet4(TPL1_A2D_SRC_TEXTURE_PITCH, &[(source.stride >> 6) << 9])?;
        self.emit_destination(destination, FORMAT_8_8_8_8_UNORM, A2D_BGRA_COLOR_SWAP)?;
        self.emit_blit()?;
        self.submission_end()
    }

    pub(crate) fn upload_buffer(
        &mut self,
        destination: ObjectId,
        destination_offset: u64,
        destination_required_size: u64,
        bytes: &[u8],
    ) -> Result<(), CompileError> {
        if bytes.is_empty() || bytes.len() & 3 != 0 || destination_offset & 3 != 0 {
            return Err(CompileError::UnsupportedFeature);
        }
        let generated = self.add_generated(bytes, 64, Access::READ, GeneratedObjectKind::Upload)?;
        let upload_size = u64::try_from(bytes.len()).map_err(|_| CompileError::Overflow)?;
        if upload_size > destination_required_size {
            return Err(CompileError::OutOfBounds);
        }
        let total_dwords = u32::try_from(bytes.len() / 4).map_err(|_| CompileError::Overflow)?;
        let mut copied_dwords = 0_u32;
        while copied_dwords < total_dwords {
            let chunk_dwords = (total_dwords - copied_dwords).min(CP_MEMCPY_MAX_DWORDS);
            let chunk_offset = u64::from(copied_dwords)
                .checked_mul(4)
                .ok_or(CompileError::Overflow)?;
            let chunk_size = u64::from(chunk_dwords)
                .checked_mul(4)
                .ok_or(CompileError::Overflow)?;
            let chunk_destination = destination_offset
                .checked_add(chunk_offset)
                .ok_or(CompileError::Overflow)?;

            self.push_word(type7(CP_MEMCPY, 5).map_err(|_| CompileError::InvalidPm4)?)?;
            self.push_word(chunk_dwords)?;
            self.address_words(
                ObjectRef::Generated(generated),
                chunk_offset,
                chunk_size,
                Access::READ,
            )?;
            self.address_words(
                ObjectRef::External(destination),
                chunk_destination,
                chunk_size,
                Access::WRITE,
            )?;
            // CP memory writes execute asynchronously with respect to both the
            // next chunk and following graphics work. WAIT_FOR_IDLE does not
            // order CP_MEMCPY writes.
            self.packet7(opcode::WAIT_MEM_WRITES, &[])?;
            copied_dwords = copied_dwords
                .checked_add(chunk_dwords)
                .ok_or(CompileError::Overflow)?;
        }
        Ok(())
    }

    fn add_generated(
        &mut self,
        bytes: &[u8],
        alignment: u64,
        access: Access,
        kind: GeneratedObjectKind,
    ) -> Result<GeneratedObjectId, CompileError> {
        let raw =
            u32::try_from(self.generated_objects.len()).map_err(|_| CompileError::Overflow)?;
        let id = GeneratedObjectId::new(raw);
        let mut contents = Vec::new();
        contents
            .try_reserve_exact(bytes.len())
            .map_err(|_| CompileError::OutOfMemory)?;
        contents.extend_from_slice(bytes);
        self.generated_objects
            .try_reserve(1)
            .map_err(|_| CompileError::OutOfMemory)?;
        self.generated_objects.push(GeneratedObject {
            id,
            alignment,
            bytes: contents,
            access,
            kind,
        });
        Ok(id)
    }

    pub(crate) fn finish(self) -> Result<RelocatablePm4, CompileError> {
        if self.draw_batch_active {
            return Err(CompileError::InvalidState);
        }
        let mut previous_end = None;
        for fixup in &self.fixups {
            let end = fixup
                .word_offset
                .checked_add(fixup.encoding.word_count())
                .ok_or(CompileError::Overflow)? as usize;
            let start = fixup.word_offset as usize;
            if previous_end.is_some_and(|offset| offset > start) {
                return Err(CompileError::InvalidPm4);
            }
            if end > self.words.len()
                || !fixup.encoding.placeholder_is_valid(&self.words[start..end])
            {
                return Err(CompileError::InvalidPm4);
            }
            previous_end = Some(end);
        }
        Ok(RelocatablePm4 {
            words: self.words,
            fixups: self.fixups,
            accesses: self.accesses,
            generated_objects: self.generated_objects,
        })
    }
}

fn pack_xy(x: u32, y: u32) -> u32 {
    x | (y << 16)
}

fn f32_bits(value: f32) -> u32 {
    value.to_bits()
}

/// Build the A6xx clip-space to framebuffer transform for SGFX.
///
/// SGFX follows the same contract as its VirGL and wgpu backends: clip-space
/// `+Y` is the top of an upper-left-origin render target, while clip-space Z
/// spans `[-W, +W]`.  A6xx framebuffer coordinates grow downwards, so Y must
/// use a negative scale.  Z uses the OpenGL-style half-scale/half-offset that
/// pairs with `GRAS_CL_CNTL.ZERO_GB_SCALE_Z = 0`.
fn viewport_transform(area: PixelRect) -> [u32; 6] {
    let x_scale = area.width() as f32 * 0.5;
    let y_scale = area.height() as f32 * 0.5;
    [
        f32_bits(area.x() as f32 + x_scale),
        f32_bits(x_scale),
        f32_bits(area.y() as f32 + y_scale),
        f32_bits(-y_scale),
        f32_bits(0.5),
        f32_bits(0.5),
    ]
}

fn guardband_axis(offset: f32, scale: f32) -> u32 {
    const MAX_GUARDBAND: u32 = 0x1ff;

    let scale = scale.abs();
    let minimum = (-32_768.0 - offset) / scale;
    let maximum = (32_767.0 - offset) / scale;
    let adjustment = (-minimum).min(maximum);
    if adjustment < 1.0 || !adjustment.is_finite() {
        return MAX_GUARDBAND;
    }

    // Freedreno converts the positive adjustment to a 3.6 floating-point
    // value, rounding down.  IEEE-754's exponent and top six mantissa bits
    // are the same encoding after removing the implicit leading one.
    let bits = adjustment.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    if !(0..=7).contains(&exponent) {
        return MAX_GUARDBAND;
    }
    ((exponent as u32) << 6) | ((bits >> 17) & 0x3f)
}

fn guardband_clip_adj(area: PixelRect) -> u32 {
    let x_scale = area.width() as f32 * 0.5;
    let y_scale = area.height() as f32 * 0.5;
    let x_offset = area.x() as f32 + x_scale;
    let y_offset = area.y() as f32 + y_scale;
    guardband_axis(x_offset, x_scale) | (guardband_axis(y_offset, y_scale) << 10)
}

fn unorm8(value: f32) -> u32 {
    let value = value.clamp(0.0, 1.0);
    (value * 255.0 + 0.5) as u32
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use sgfx_core::ir::PixelRect;

    use super::{Emitter, guardband_axis, guardband_clip_adj, viewport_transform};
    use crate::model::{
        Access, AddressEncoding, CompileError, ObjectId, ObjectRef, SymbolicAddress,
    };

    #[test]
    fn finish_rejects_overlapping_64_bit_fixups() {
        let emitter = Emitter {
            words: vec![0, 0, 0],
            fixups: vec![
                SymbolicAddress {
                    word_offset: 0,
                    object: ObjectRef::External(ObjectId::new(1)),
                    object_offset: 0,
                    required_size: 8,
                    access: Access::READ,
                    encoding: AddressEncoding::GpuVa64,
                },
                SymbolicAddress {
                    word_offset: 1,
                    object: ObjectRef::External(ObjectId::new(2)),
                    object_offset: 0,
                    required_size: 8,
                    access: Access::READ,
                    encoding: AddressEncoding::GpuVa64,
                },
            ],
            accesses: vec![],
            generated_objects: vec![],
            ccu_sequence: None,
            event_sequence: 0,
            draw_batch_active: false,
            last_draw: None,
            max_words: 3,
        };

        assert_eq!(emitter.finish(), Err(CompileError::InvalidPm4));
    }

    #[test]
    fn guardband_matches_freedreno_3_6_encoding() {
        assert_eq!(guardband_axis(8.0, 8.0), 0x1ff);
        assert_eq!(guardband_axis(1080.0, 1080.0), 0x135);
        assert_eq!(guardband_axis(720.0, 720.0), 0x159);

        let panel = PixelRect::new(0, 0, 2160, 1440).unwrap();
        assert_eq!(guardband_clip_adj(panel), 0x0005_6535);
    }

    #[test]
    fn viewport_uses_the_sgfx_upper_left_and_negative_one_to_one_depth_contract() {
        let area = PixelRect::new(32, 48, 640, 480).unwrap();
        let words = viewport_transform(area);
        let values = words.map(f32::from_bits);
        assert_eq!(values, [352.0, 320.0, 288.0, -240.0, 0.5, 0.5]);

        let map = |coordinate: f32, offset: f32, scale: f32| offset + coordinate * scale;
        assert_eq!(map(-1.0, values[0], values[1]), 32.0);
        assert_eq!(map(1.0, values[0], values[1]), 672.0);
        assert_eq!(map(1.0, values[2], values[3]), 48.0);
        assert_eq!(map(-1.0, values[2], values[3]), 528.0);
        assert_eq!(map(-1.0, values[4], values[5]), 0.0);
        assert_eq!(map(1.0, values[4], values[5]), 1.0);
    }
}
