// SPDX-License-Identifier: GPL-2.0-only

//! Hostile-input validation and relocation for the A618 submit dialect.

use alloc::vec::Vec;

use adreno_a6xx_pm4::{Header, Packet, Packets, opcode};
use adreno_a6xx_shader_pack::{
    PipelineVariant, SAMPLER_CLAMP_LINEAR, SAMPLER_CLAMP_NEAREST, ShaderMeta, ShaderVariant,
    link_meta, pipeline_state_meta, shader_meta,
};
use adreno_a6xx_submit_wire::{
    ACCESS_READ, ACCESS_WRITE, AddressEncoding, DecodedSubmit, Relocation, RelocationSource,
};

const CP_MEMCPY: u8 = 0x75;
const CP_MEM_WRITE: u8 = 0x3d;
// Must match the bounded chunks emitted by sgfx-codegen-adreno-a6xx. A single
// 252-dword transfer has stopped the CoachZ A618 command parser; 128 dwords
// also keeps production texture uploads inside the opaque submit budget.
const CP_MEMCPY_MAX_DWORDS: u32 = 128;
const CP_SET_VISIBILITY_OVERRIDE: u8 = 0x64;
const CP_REG_WRITE: u8 = 0x6d;
const CP_SKIP_IB2_ENABLE_GLOBAL: u8 = 0x1d;
const CP_SKIP_IB2_ENABLE_LOCAL: u8 = 0x23;

const EVENT_CCU_INVALIDATE_DEPTH: u32 = 0x18;
const EVENT_CCU_FLUSH_COLOR_TS: u32 = 0x1d;
const EVENT_CCU_FLUSH_DEPTH_TS: u32 = 0x1c;
const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
const EVENT_CACHE_INVALIDATE: u32 = 0x31;

const FORMAT_8_8_8_8_UNORM: u32 = 0x30;
const A2D_COLOR_SWAP_WXYZ: u32 = 1 << 10;
const MRT_COLOR_SWAP_WXYZ: u32 = 1 << 13;
const RB_CCU_CNTL: u32 = 0x8e07;
const RB_DBG_ECO_CNTL: u32 = 0x8e04;
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
const SP_A2D_OUTPUT_INFO: u32 = 0xacc0;
const TPL1_A2D_SRC_TEXTURE_INFO: u32 = 0xb4c0;
const TPL1_A2D_SRC_TEXTURE_SIZE: u32 = 0xb4c1;
const TPL1_A2D_SRC_TEXTURE_BASE: u32 = 0xb4c2;
const TPL1_A2D_SRC_TEXTURE_PITCH: u32 = 0xb4c4;
const CP_DRAW_INDX_OFFSET: u8 = 0x38;
const CP_LOAD_STATE6_GEOM: u8 = 0x32;
const CP_LOAD_STATE6_FRAG: u8 = 0x34;
const GRAS_CL_CNTL: u32 = 0x8000;
const GRAS_CL_VS_CLIP_CULL_DISTANCE: u32 = 0x8001;
const GRAS_CL_ARRAY_SIZE: u32 = 0x8004;
const GRAS_CL_INTERP_CNTL: u32 = 0x8005;
const GRAS_CL_GUARDBAND_CLIP_ADJ: u32 = 0x8006;
const GRAS_CL_VIEWPORT_XOFFSET: u32 = 0x8010;
const GRAS_SU_CNTL: u32 = 0x8090;
const GRAS_SU_POINT_MINMAX: u32 = 0x8091;
const GRAS_SU_POINT_SIZE: u32 = 0x8092;
const GRAS_SU_DEPTH_PLANE_CNTL: u32 = 0x8094;
const GRAS_SU_POLY_OFFSET_SCALE: u32 = 0x8095;
const GRAS_SU_DEPTH_BUFFER_INFO: u32 = 0x8098;
const GRAS_SU_VS_SIV_CNTL: u32 = 0x809b;
const GRAS_SC_CNTL: u32 = 0x80a0;
const GRAS_SC_RAS_MSAA_CNTL: u32 = 0x80a2;
const GRAS_SC_SCREEN_SCISSOR_CNTL: u32 = 0x80af;
const GRAS_SC_SCREEN_SCISSOR_TL: u32 = 0x80b0;
const GRAS_SC_VIEWPORT_SCISSOR_TL: u32 = 0x80d0;
const GRAS_SC_WINDOW_SCISSOR_TL: u32 = 0x80f0;
const GRAS_SC_BIN_CNTL: u32 = 0x80a1;
const GRAS_LRZ_CNTL: u32 = 0x8100;
const GRAS_LRZ_PS_INPUT_CNTL: u32 = 0x8101;
const GRAS_LRZ_MRT_BUFFER_INFO_0: u32 = 0x8102;
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
const RB_PS_SAMPLEFREQ_CNTL: u32 = 0x8810;
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
const RB_WINDOW_OFFSET: u32 = 0x8890;
const RB_LRZ_CNTL: u32 = 0x8898;
const RB_BIN_CONTROL2: u32 = 0x88d3;
const RB_WINDOW_OFFSET2: u32 = 0x88d4;
const RB_RESOLVE_GMEM_BUFFER_INFO: u32 = 0x88d5;
const RB_COLOR_FLAG_BUFFER_ADDR: u32 = 0x8903;
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
const SP_VS_CONFIG: u32 = 0xa823;
const SP_VS_INSTR_SIZE: u32 = 0xa824;
const SP_VS_PVT_MEM_STACK_OFFSET: u32 = 0xa825;
const SP_HS_CONFIG: u32 = 0xa83b;
const SP_DS_CONFIG: u32 = 0xa863;
const SP_GS_CONFIG: u32 = 0xa894;
const SP_PS_CNTL_0: u32 = 0xa980;
const SP_PS_PROGRAM_COUNTER_OFFSET: u32 = 0xa982;
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
const SP_PS_PVT_MEM_STACK_OFFSET: u32 = 0xa9a9;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AddressField {
    word_offset: u32,
    access: u32,
    requires_complete_linear_image: bool,
    required_size: Option<u64>,
    encoding: AddressEncoding,
    source: AddressSource,
    a2d: Option<A2dExpectation>,
    image: Option<ImageExpectation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressSource {
    Attachment,
    CanonicalShader(ShaderVariant),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct A2dExpectation {
    row_pitch: u32,
    required_width: u32,
    required_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImageExpectation {
    row_pitch: u32,
    width: u32,
    height: u32,
    exact_extent: bool,
    pitch_align: Option<u32>,
    array_pitch: Option<ArrayPitchExpectation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ArrayPitchExpectation {
    bytes: u64,
    alignment: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinearImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) row_pitch: u32,
    pub(crate) visible_size: u64,
}

/// Resolved context-local resource authority used only by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedResource {
    pub(crate) attachment_token: u64,
    pub(crate) gpu_va: u64,
    pub(crate) allocation_size: u64,
    pub(crate) allowed_access: u32,
    pub(crate) linear_image: Option<LinearImage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectedPacketKind {
    Type4,
    Type7,
}

impl RejectedPacketKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Type4 => "type4",
            Self::Type7 => "type7",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RejectedPacket {
    pub(crate) kind: RejectedPacketKind,
    pub(crate) word_offset: u32,
    pub(crate) selector: u32,
    pub(crate) payload_len: u32,
    pub(crate) first_value: Option<u32>,
}

fn exact(values: &[u32], expected: &[u32]) -> Result<(), &'static str> {
    if values == expected {
        Ok(())
    } else {
        Err("qcom-adreno-a618: unsafe PM4 register value")
    }
}

fn address_field(
    fields: &mut Vec<AddressField>,
    word_offset: u32,
    access: u32,
    requires_complete_linear_image: bool,
    required_size: Option<u64>,
) -> Result<(), &'static str> {
    address_field_encoded(
        fields,
        word_offset,
        access,
        requires_complete_linear_image,
        required_size,
        AddressEncoding::GpuVa64,
    )
}

fn address_field_encoded(
    fields: &mut Vec<AddressField>,
    word_offset: u32,
    access: u32,
    requires_complete_linear_image: bool,
    required_size: Option<u64>,
    encoding: AddressEncoding,
) -> Result<(), &'static str> {
    fields
        .try_reserve(1)
        .map_err(|_| "qcom-adreno-a618: PM4 validation allocation failed")?;
    fields.push(AddressField {
        word_offset,
        access,
        requires_complete_linear_image,
        required_size,
        encoding,
        source: AddressSource::Attachment,
        a2d: None,
        image: None,
    });
    Ok(())
}

fn canonical_address_field(
    fields: &mut Vec<AddressField>,
    word_offset: u32,
    variant: ShaderVariant,
) -> Result<(), &'static str> {
    fields
        .try_reserve(1)
        .map_err(|_| "qcom-adreno-a618: PM4 validation allocation failed")?;
    fields.push(AddressField {
        word_offset,
        access: ACCESS_READ,
        requires_complete_linear_image: false,
        required_size: Some(adreno_a6xx_shader_pack::SHADER_SIZE as u64),
        encoding: AddressEncoding::GpuVa64,
        source: AddressSource::CanonicalShader(variant),
        a2d: None,
        image: None,
    });
    Ok(())
}

fn any_shader_payload(register: u32, payload: &[u32]) -> bool {
    ShaderVariant::ALL
        .into_iter()
        .any(|variant| match shader_meta(variant) {
            ShaderMeta::Vertex(meta) => match register {
                SP_VS_CNTL_0 => payload == [meta.sp_vs_cntl_0],
                SP_VS_INSTR_SIZE => payload == [meta.sp_vs_instr_size],
                SP_VS_CONST_CONFIG => payload == [meta.sp_vs_const_config, 0, 0, 0],
                SP_VS_CONFIG => payload == [meta.sp_vs_config],
                VFD_CNTL_1 => payload == meta.vfd_cntl_1_6,
                VFD_DEST_CNTL => payload == meta.vfd_dest_cntl,
                _ => false,
            },
            ShaderMeta::Fragment(meta) => match register {
                SP_PS_CNTL_0 => payload == [meta.sp_ps_cntl_0],
                SP_PS_INSTR_SIZE => payload == [meta.sp_ps_instr_size],
                SP_PS_CONST_CONFIG => payload == [meta.sp_ps_const_config],
                SP_PS_CONFIG => payload == [meta.sp_ps_config],
                SP_PS_WAVE_CNTL => payload == [meta.sp_ps_wave_cntl],
                GRAS_CL_INTERP_CNTL => payload == [meta.gras_cl_interp_cntl],
                RB_INTERP_CNTL => payload == [meta.rb_interp_cntl],
                RB_PS_INPUT_CNTL => payload == [meta.rb_ps_input_cntl],
                SP_PS_INITIAL_TEX_LOAD_CNTL => {
                    payload.first() == Some(&meta.initial_tex_load_cntl)
                        && payload.get(1..) == Some(meta.initial_tex_load_cmd)
                }
                SP_REG_PROG_ID_0 => payload == meta.sp_reg_prog_id,
                SP_PS_OUTPUT_CNTL => payload == [meta.sp_ps_output_cntl],
                SP_PS_OUTPUT_REG => payload == meta.sp_ps_output_reg,
                SP_PS_OUTPUT_MASK => payload == [meta.sp_ps_output_mask],
                RB_PS_OUTPUT_CNTL => payload == [meta.rb_ps_output_cntl],
                RB_PS_OUTPUT_MASK => payload == [meta.rb_ps_output_mask],
                _ => false,
            },
        })
}

fn validate_type4(
    register: u32,
    payload: &[u32],
    packet_word: u32,
    addresses: &mut Vec<AddressField>,
) -> Result<(), &'static str> {
    match (register, payload.len()) {
        (RB_CCU_CNTL, 1) => exact(payload, &[0x0800_0000]),
        (RB_DBG_ECO_CNTL, 1) => exact(payload, &[0x0410_0000]),
        (RB_A2D_BLT_CNTL, 1) | (GRAS_A2D_BLT_CNTL, 1) => {
            let allowed = (FORMAT_8_8_8_8_UNORM << 8) | (0xf << 20) | (1 << 7) | (1 << 16);
            if payload[0] & !allowed == 0
                && payload[0] & (FORMAT_8_8_8_8_UNORM << 8) == FORMAT_8_8_8_8_UNORM << 8
                && payload[0] & (0xf << 20) == 0xf << 20
                && payload[0] & (1 << 16) != 0
            {
                Ok(())
            } else {
                Err("qcom-adreno-a618: unsafe A2D control value")
            }
        }
        (RB_A2D_PIXEL_CNTL, 1) => exact(payload, &[0]),
        (RB_A2D_DEST_BUFFER_INFO, 1) => {
            exact(payload, &[FORMAT_8_8_8_8_UNORM | A2D_COLOR_SWAP_WXYZ])
        }
        (RB_A2D_DEST_BUFFER_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_WRITE, true, None)
        }
        (RB_A2D_DEST_BUFFER_PITCH, 1) if payload[0] != 0 && payload[0] & !0x3fff == 0 => Ok(()),
        (RB_A2D_CLEAR_COLOR_DW0, 4) if payload.iter().all(|word| *word <= 0xff) => Ok(()),
        (GRAS_A2D_SRC_XMIN, 4) if payload.iter().all(|value| value & 0xff == 0) => Ok(()),
        (GRAS_A2D_DEST_TL, 2) | (GRAS_A2D_SCISSOR_TL, 2) => Ok(()),
        (SP_A2D_OUTPUT_INFO, 1) => exact(payload, &[(FORMAT_8_8_8_8_UNORM << 3) | (0xf << 12)]),
        (TPL1_A2D_SRC_TEXTURE_INFO, 1) => exact(
            payload,
            &[FORMAT_8_8_8_8_UNORM | A2D_COLOR_SWAP_WXYZ | (1 << 20) | (1 << 22)],
        ),
        (TPL1_A2D_SRC_TEXTURE_SIZE, 1) if payload[0] != 0 && payload[0] & 0xc000_0000 == 0 => {
            Ok(())
        }
        (TPL1_A2D_SRC_TEXTURE_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_READ, true, None)
        }
        (TPL1_A2D_SRC_TEXTURE_PITCH, 1)
            if payload[0] != 0 && payload[0] & 0x1ff == 0 && payload[0] >> 9 <= 0x3fff =>
        {
            Ok(())
        }
        (GRAS_SC_CNTL, 1) => exact(payload, &[2]),
        (GRAS_SC_BIN_CNTL | RB_CNTL, 1) => exact(payload, &[0x00c0_0000]),
        (
            GRAS_LRZ_CNTL
            | RB_LRZ_CNTL
            | RB_BIN_CONTROL2
            | RB_WINDOW_OFFSET
            | RB_WINDOW_OFFSET2
            | RB_RESOLVE_GMEM_BUFFER_INFO
            | SP_WINDOW_OFFSET
            | TPL1_WINDOW_OFFSET
            | GRAS_SC_SCREEN_SCISSOR_CNTL
            | VFD_RENDER_MODE
            | GRAS_CL_ARRAY_SIZE
            | GRAS_CL_VS_CLIP_CULL_DISTANCE
            | GRAS_SU_DEPTH_BUFFER_INFO
            | GRAS_SU_VS_SIV_CNTL
            | GRAS_SU_DEPTH_CNTL
            | GRAS_SU_STENCIL_CNTL
            | GRAS_LRZ_PS_INPUT_CNTL
            | GRAS_LRZ_PS_SAMPLEFREQ_CNTL
            | RB_PS_SAMPLEFREQ_CNTL
            | RB_DITHER_CNTL
            | RB_SRGB_CNTL
            | RB_MRT_BASE_GMEM
            | RB_ALPHA_TEST_CNTL
            | RB_DEPTH_PLANE_CNTL
            | RB_DEPTH_CNTL
            | RB_STENCIL_CNTL
            | RB_STENCIL_BUFFER_INFO
            | RB_STENCIL_REF_CNTL
            | PC_PS_CNTL
            | PC_CNTL
            | PC_STEREO_RENDERING_CNTL
            | SP_SRGB_CNTL
            | SP_VS_PVT_MEM_STACK_OFFSET
            | SP_PS_PVT_MEM_STACK_OFFSET
            | SP_GFX_USIZE,
            1,
        ) => exact(payload, &[0]),
        (VFD_MODE_CNTL, 1) => exact(payload, &[3]),
        (VPC_SO_OVERRIDE, 1) => exact(payload, &[0]),
        (PC_MODE_CNTL, 1) => exact(payload, &[0x1f]),
        (SP_MODE_CNTL, 1) => exact(payload, &[5]),
        (TPL1_MODE_CNTL, 1) => exact(payload, &[0xa2]),
        (RB_MODE_CNTL, 1) => exact(payload, &[0x10]),
        (SP_UPDATE_CNTL, 1) => exact(payload, &[0x0000_00ff]),
        (SP_HS_CONFIG | SP_DS_CONFIG | SP_GS_CONFIG, 1) => exact(payload, &[0]),
        (SP_VS_PROGRAM_COUNTER_OFFSET, 7) if payload[0] == 0 && payload[3..] == [0; 4] => {
            canonical_address_field(addresses, packet_word + 2, ShaderVariant::VsStride16Pos2)
        }
        (SP_PS_PROGRAM_COUNTER_OFFSET, 7) if payload[0] == 0 && payload[3..] == [0; 4] => {
            canonical_address_field(addresses, packet_word + 2, ShaderVariant::FsSolid)
        }
        (SP_LB_PARAM_LIMIT, 1) => exact(payload, &[7]),
        (GRAS_CL_CNTL, 1) => exact(payload, &[0x80]),
        (GRAS_CL_GUARDBAND_CLIP_ADJ, 1) if payload[0] & !(0x1ff | (0x1ff << 10)) == 0 => Ok(()),
        (GRAS_SU_POINT_MINMAX, 1) => exact(payload, &[0x0010_0010]),
        (GRAS_SU_POINT_SIZE, 1) => exact(payload, &[0x10]),
        (GRAS_SU_POLY_OFFSET_SCALE, 3) => exact(payload, &[0; 3]),
        (GRAS_SU_DEPTH_PLANE_CNTL, 1) => exact(payload, &[0]),
        (GRAS_LRZ_MRT_BUFFER_INFO_0, 1) => exact(payload, &[FORMAT_8_8_8_8_UNORM]),
        (TPL1_RAS_MSAA_CNTL, 2) => exact(payload, &[0, 4]),
        (GRAS_SC_RAS_MSAA_CNTL | RB_RAS_MSAA_CNTL, 3) => exact(payload, &[0, 4, 0]),
        (TPL1_MSAA_SAMPLE_POS_CNTL, 1) => exact(payload, &[0]),
        (RB_DEPTH_BUFFER_INFO, 6) => exact(payload, &[0; 6]),
        (RB_DEPTH_BOUND_MIN, 2) => exact(payload, &[0, 1.0_f32.to_bits()]),
        (RB_STENCIL_MASK, 2) => exact(payload, &[0, 0]),
        (RB_COLOR_FLAG_BUFFER_ADDR, 3) => exact(payload, &[0; 3]),
        (VPC_VARYING_INTERP_MODE | VPC_VARYING_REPLACE_MODE, 8) => exact(payload, &[0; 8]),
        (RB_MRT_CONTROL, 2) if matches!(payload, [0x7e0, 0x0001_0001] | [0x7e3, 0x0701_0706]) => {
            Ok(())
        }
        (RB_MRT_BUF_INFO, 1) => exact(payload, &[FORMAT_8_8_8_8_UNORM | MRT_COLOR_SWAP_WXYZ]),
        (RB_MRT_PITCH, 2)
            if payload[0] != 0
                && payload[0] <= 0xffff
                && payload[1] != 0
                && payload[1] <= 0x1fff_ffff =>
        {
            Ok(())
        }
        (RB_MRT_BASE, 2) => address_field(addresses, packet_word + 1, ACCESS_WRITE, false, None),
        (RB_BLEND_CNTL, 1) if matches!(payload[0], 0xffff_0000 | 0xffff_0001) => Ok(()),
        (SP_BLEND_CNTL, 1) if matches!(payload[0], 0 | 1) => Ok(()),
        (SP_PS_MRT_CNTL | RB_PS_MRT_CNTL, 1) => exact(payload, &[1]),
        (SP_PS_MRT_REG, 1) => exact(payload, &[FORMAT_8_8_8_8_UNORM]),
        (GRAS_CL_VIEWPORT_XOFFSET, 6) => Ok(()),
        (
            GRAS_SC_SCREEN_SCISSOR_TL | GRAS_SC_VIEWPORT_SCISSOR_TL | GRAS_SC_WINDOW_SCISSOR_TL,
            2,
        ) => Ok(()),
        (GRAS_SU_CNTL, 1) if payload[0] & !0x2017 == 0 && payload[0] & 0x2010 == 0x2010 => Ok(()),
        (VPC_RAST_CNTL | PC_DGEN_RAST_CNTL, 1) => exact(payload, &[3]),
        (VFD_CNTL_0, 1)
            if payload[0] & 0xffff_0000 == 0 && payload[0] & 0xff == payload[0] >> 8 =>
        {
            Ok(())
        }
        (VFD_CNTL_1, 6) if any_shader_payload(register, payload) => Ok(()),
        (VFD_INDEX_OFFSET, 2) => exact(&payload[1..], &[0]),
        (VFD_VERTEX_BUFFER_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_READ, false, None)
        }
        (VFD_VERTEX_BUFFER_SIZE, 2)
            if payload[0] != 0 && matches!(payload[1], 16 | 24 | 28 | 32 | 40) =>
        {
            Ok(())
        }
        (VFD_FETCH_INSTR, count) if count >= 2 && count <= 6 && count.is_multiple_of(2) => {
            if payload
                .chunks_exact(2)
                .all(|pair| pair[1] == 1 && pair[0] & 0x4000_0000 != 0)
            {
                Ok(())
            } else {
                Err("qcom-adreno-a618: unsafe VFD fetch instruction")
            }
        }
        (VFD_DEST_CNTL, _) if any_shader_payload(register, payload) => Ok(()),
        (
            SP_VS_CNTL_0
            | SP_VS_INSTR_SIZE
            | SP_VS_CONST_CONFIG
            | SP_VS_CONFIG
            | SP_PS_CNTL_0
            | SP_PS_INSTR_SIZE
            | SP_PS_CONST_CONFIG
            | SP_PS_CONFIG
            | SP_PS_WAVE_CNTL
            | GRAS_CL_INTERP_CNTL
            | RB_INTERP_CNTL
            | RB_PS_INPUT_CNTL
            | SP_PS_INITIAL_TEX_LOAD_CNTL
            | SP_REG_PROG_ID_0
            | SP_PS_OUTPUT_CNTL
            | SP_PS_OUTPUT_REG
            | SP_PS_OUTPUT_MASK
            | RB_PS_OUTPUT_CNTL
            | RB_PS_OUTPUT_MASK,
            _,
        ) if any_shader_payload(register, payload) => Ok(()),
        (SP_PS_INITIAL_TEX_INDEX_CMD, 1) => exact(payload, &[0]),
        (SP_PS_TSIZE, 1) if matches!(payload[0], 0 | 1) => Ok(()),
        (SP_PS_SAMPLER_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_READ, false, Some(16))
        }
        (SP_PS_TEXMEMOBJ_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_READ, false, Some(64))
        }
        (SP_VS_OUTPUT_CNTL | VPC_VS_CNTL | VPC_PS_CNTL | PC_VS_CNTL, 1) => Ok(()),
        (SP_VS_OUTPUT_REG | SP_VS_VPC_DEST_REG, count) if count <= 2 => Ok(()),
        (VPC_VARYING_LM_TRANSFER_CNTL_DISABLE, 4) => Ok(()),
        (VPC_VS_CLIP_CULL_CNTL | VPC_VS_CLIP_CULL_CNTL_V2, 1) => exact(payload, &[0x00ff_ff00]),
        (VPC_VS_SIV_CNTL | VPC_VS_SIV_CNTL_V2, 1) => exact(payload, &[0x0000_ffff]),
        _ => Err("qcom-adreno-a618: PM4 register write is not allowlisted"),
    }
}

fn validate_type7(
    opcode_value: u8,
    payload: &[u32],
    packet_word: u32,
    addresses: &mut Vec<AddressField>,
) -> Result<(), &'static str> {
    match (opcode_value, payload.len()) {
        (opcode::WAIT_MEM_WRITES, 0) => Ok(()),
        (opcode::WAIT_FOR_IDLE, 0) => Ok(()),
        (opcode::EVENT_WRITE, 1)
            if matches!(
                payload[0],
                EVENT_CCU_INVALIDATE_COLOR | EVENT_CCU_INVALIDATE_DEPTH | EVENT_CACHE_INVALIDATE
            ) =>
        {
            Ok(())
        }
        (opcode::EVENT_WRITE, 4)
            if [EVENT_CCU_FLUSH_COLOR_TS, EVENT_CCU_FLUSH_DEPTH_TS].contains(&payload[0])
                && payload[1..3] == [0, 0]
                && payload[3] != 0 =>
        {
            address_field(addresses, packet_word + 2, ACCESS_WRITE, false, Some(4))
        }
        (opcode::SET_MARKER, 1) if matches!(payload[0], 1 | 12) => Ok(()),
        (CP_SKIP_IB2_ENABLE_GLOBAL, 1) => exact(payload, &[0]),
        (CP_SKIP_IB2_ENABLE_LOCAL, 1) => exact(payload, &[1]),
        (CP_SET_VISIBILITY_OVERRIDE, 1) => exact(payload, &[1]),
        (CP_REG_WRITE, 3) => exact(payload, &[2, RB_RENDER_CNTL, 0x10]),
        (opcode::BLIT, 1) => exact(payload, &[3]),
        (CP_MEMCPY, 5) if matches!(payload[0], 1..=CP_MEMCPY_MAX_DWORDS) => {
            let size = u64::from(payload[0])
                .checked_mul(4)
                .ok_or("qcom-adreno-a618: PM4 memcpy size overflows")?;
            address_field(addresses, packet_word + 2, ACCESS_READ, false, Some(size))?;
            address_field(addresses, packet_word + 4, ACCESS_WRITE, false, Some(size))
        }
        (CP_MEM_WRITE, 22)
            if payload[0..2] == [0, 0]
                && payload[2] == 0x4c00_6880
                && payload[3] & 0xc000_0000 == 0
                && payload[3] & 0x7fff != 0
                && (payload[3] >> 15) & 0x7fff != 0
                && payload[4] & 0x7f == 0
                && payload[4] >> 29 == 1
                && payload[5] & !0x007f_ffff == 0
                && payload[7] == (1 << 17)
                && payload[8..18] == [0; 10]
                && matches!(&payload[18..], [0x920, 0x40, 0, 0] | [0x92a, 0x40, 0x20, 0]) =>
        {
            address_field(addresses, packet_word + 1, ACCESS_WRITE, false, Some(80))?;
            address_field_encoded(
                addresses,
                packet_word + 7,
                ACCESS_READ,
                false,
                None,
                AddressEncoding::GpuVa49TexDescriptor,
            )
        }
        (CP_LOAD_STATE6_GEOM, 3)
            if payload[0] == ((2 << 16) | (8 << 18) | (1 << 22)) && payload[1..3] == [0, 0] =>
        {
            address_field(
                addresses,
                packet_word + 2,
                ACCESS_READ,
                false,
                Some(adreno_a6xx_shader_pack::SHADER_SIZE as u64),
            )
        }
        (CP_LOAD_STATE6_FRAG, 3)
            if payload[0] == ((2 << 16) | (12 << 18) | (1 << 22)) && payload[1..3] == [0, 0] =>
        {
            address_field(
                addresses,
                packet_word + 2,
                ACCESS_READ,
                false,
                Some(adreno_a6xx_shader_pack::SHADER_SIZE as u64),
            )
        }
        (CP_LOAD_STATE6_GEOM, 23)
            if payload[0] == ((1 << 14) | (8 << 18) | (5 << 22)) && payload[1..3] == [0, 0] =>
        {
            Ok(())
        }
        (CP_LOAD_STATE6_FRAG, 23)
            if payload[0] == ((1 << 14) | (12 << 18) | (5 << 22)) && payload[1..3] == [0, 0] =>
        {
            Ok(())
        }
        (CP_LOAD_STATE6_FRAG, 3)
            if payload[0] == ((2 << 16) | (4 << 18) | (1 << 22)) && payload[1..3] == [0, 0] =>
        {
            address_field(addresses, packet_word + 2, ACCESS_READ, false, Some(16))
        }
        (CP_LOAD_STATE6_FRAG, 3)
            if payload[0] == ((1 << 14) | (2 << 16) | (4 << 18) | (1 << 22))
                && payload[1..3] == [0, 0] =>
        {
            address_field(addresses, packet_word + 2, ACCESS_READ, false, Some(64))
        }
        (CP_DRAW_INDX_OFFSET, 3)
            if payload[0] == 0x84
                && payload[1] == 1
                && payload[2] != 0
                && payload[2].is_multiple_of(3) =>
        {
            Ok(())
        }
        (CP_DRAW_INDX_OFFSET, 7) => {
            let index_size = match payload[0] {
                // TRIANGLE_LIST | DMA | INDEX4_SIZE_16/32_BIT.
                0x404 => 2_u64,
                0x804 => 4_u64,
                _ => return Err("qcom-adreno-a618: unsafe indexed draw initiator"),
            };
            let end_index = payload[3]
                .checked_add(payload[2])
                .ok_or("qcom-adreno-a618: indexed draw range overflows")?;
            if payload[1] != 1
                || payload[2] == 0
                || !payload[2].is_multiple_of(3)
                || payload[6] == 0
                || end_index > payload[6]
                || payload[4..6] != [0, 0]
            {
                return Err("qcom-adreno-a618: unsafe indexed draw range");
            }
            let required_size = u64::from(payload[6])
                .checked_mul(index_size)
                .ok_or("qcom-adreno-a618: index buffer size overflows")?;
            address_field(
                addresses,
                packet_word + 5,
                ACCESS_READ,
                false,
                Some(required_size),
            )
        }
        _ => Err("qcom-adreno-a618: PM4 opcode is not allowlisted"),
    }
}

/// Require every asynchronous CP memory copy to be ordered before the next
/// packet can consume its destination.  Userspace currently uses CP_MEMCPY for
/// per-frame vertex uploads, so accepting an unpaired copy would permit a
/// deterministic GPU data race even though each packet is individually safe.
fn validate_memcpy_barriers(packets: &[Packet<'_>]) -> Result<(), &'static str> {
    let mut require_wait = false;
    for packet in packets.iter().copied() {
        if require_wait {
            match packet.header {
                Header::Type7 {
                    opcode: opcode_value,
                    count,
                } if opcode_value == opcode::WAIT_MEM_WRITES
                    && count == 0
                    && packet.payload.is_empty() =>
                {
                    require_wait = false;
                    continue;
                }
                _ => {
                    return Err(
                        "qcom-adreno-a618: CP memory write must be followed by CP_WAIT_MEM_WRITES",
                    );
                }
            }
        }
        require_wait = matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_MEMCPY | CP_MEM_WRITE,
                ..
            }
        );
    }
    if require_wait {
        return Err("qcom-adreno-a618: CP memory write must be followed by CP_WAIT_MEM_WRITES");
    }
    Ok(())
}

/// A6xx cannot invalidate a CCU that may still contain dirty data.  Accept
/// only the two sequences emitted by this dialect: color clean/invalidate at
/// submission start, or the complete sysmem color+depth retirement chain.
fn validate_ccu_transitions(packets: &[Packet<'_>]) -> Result<(), &'static str> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Transition {
        Idle,
        ColorClean,
        ColorAndDepthClean,
        DepthInvalidatePending,
    }

    let mut transition = Transition::Idle;
    for packet in packets.iter().copied() {
        let addressed = |event| {
            matches!(
                packet.header,
                Header::Type7 {
                    opcode: opcode::EVENT_WRITE,
                    count: 4,
                }
            ) && packet.payload.first() == Some(&event)
        };
        let plain = |event| {
            matches!(
                packet.header,
                Header::Type7 {
                    opcode: opcode::EVENT_WRITE,
                    count: 1,
                }
            ) && packet.payload == [event]
        };
        let color_clean = addressed(EVENT_CCU_FLUSH_COLOR_TS);
        let depth_clean = addressed(EVENT_CCU_FLUSH_DEPTH_TS);
        let color_invalidate = plain(EVENT_CCU_INVALIDATE_COLOR);
        let depth_invalidate = plain(EVENT_CCU_INVALIDATE_DEPTH);
        let is_ccu_transition = color_clean || depth_clean || color_invalidate || depth_invalidate;

        transition = match (
            transition,
            color_clean,
            depth_clean,
            color_invalidate,
            depth_invalidate,
        ) {
            (Transition::Idle, true, false, false, false) => Transition::ColorClean,
            (Transition::ColorClean, false, true, false, false) => Transition::ColorAndDepthClean,
            (Transition::ColorClean, false, false, true, false) => Transition::Idle,
            (Transition::ColorAndDepthClean, false, false, true, false) => {
                Transition::DepthInvalidatePending
            }
            (Transition::DepthInvalidatePending, false, false, false, true) => Transition::Idle,
            (Transition::Idle, false, false, false, false) => Transition::Idle,
            _ if is_ccu_transition => {
                return Err("qcom-adreno-a618: invalid CCU clean/invalidate ordering");
            }
            _ => {
                return Err("qcom-adreno-a618: CCU clean/invalidate sequence is not contiguous");
            }
        };
    }
    if transition != Transition::Idle {
        return Err("qcom-adreno-a618: CCU clean is missing its matching invalidation");
    }
    Ok(())
}

/// A618 can consume the complete IB while a misplaced WFI remains parked in
/// front of dirty color-cache retirement. A2D blits remain self-contained,
/// while consecutive 3D draws may share one render-pass retirement sequence.
/// The canonical 3D validator independently requires a complete safe state
/// segment before every draw.
fn validate_render_retirement(packets: &[Packet<'_>]) -> Result<(), &'static str> {
    let is_addressed = |packet: &Packet<'_>, event| {
        matches!(
            packet.header,
            Header::Type7 {
                opcode: opcode::EVENT_WRITE,
                count: 4,
            }
        ) && packet.payload.first() == Some(&event)
    };
    let is_plain_event = |packet: &Packet<'_>, event| {
        matches!(
            packet.header,
            Header::Type7 {
                opcode: opcode::EVENT_WRITE,
                count: 1,
            }
        ) && packet.payload == [event]
    };
    let is_wfi = |packet: &Packet<'_>| {
        matches!(
            packet.header,
            Header::Type7 {
                opcode: opcode::WAIT_FOR_IDLE,
                count: 0,
            }
        )
    };
    let is_sysmem_retirement = |retirement: &[Packet<'_>]| {
        retirement.len() == 5
            && is_addressed(&retirement[0], EVENT_CCU_FLUSH_COLOR_TS)
            && is_addressed(&retirement[1], EVENT_CCU_FLUSH_DEPTH_TS)
            && is_plain_event(&retirement[2], EVENT_CCU_INVALIDATE_COLOR)
            && is_plain_event(&retirement[3], EVENT_CCU_INVALIDATE_DEPTH)
            && is_wfi(&retirement[4])
    };

    let mut pending_draw = false;
    let mut index = 0usize;
    while index < packets.len() {
        let packet = &packets[index];
        if matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_DRAW_INDX_OFFSET,
                ..
            }
        ) {
            pending_draw = true;
            index += 1;
            continue;
        }

        if matches!(
            packet.header,
            Header::Type7 {
                opcode: opcode::BLIT,
                ..
            }
        ) {
            if pending_draw {
                return Err(
                    "qcom-adreno-a618: rendering operation lacks the complete A6xx sysmem epilogue",
                );
            }
            if !packets.get(index + 1).is_some_and(is_wfi) {
                return Err("qcom-adreno-a618: A2D blit is not idle before retirement");
            }
            let Some(retirement) = packets.get(index + 2..index + 7) else {
                return Err("qcom-adreno-a618: rendering operation lacks sysmem retirement");
            };
            if !is_sysmem_retirement(retirement) {
                return Err(
                    "qcom-adreno-a618: rendering operation lacks the complete A6xx sysmem epilogue",
                );
            }
            index += 7;
            continue;
        }

        if pending_draw
            && packets
                .get(index..index.saturating_add(5))
                .is_some_and(is_sysmem_retirement)
        {
            pending_draw = false;
            index += 5;
            continue;
        }

        index += 1;
    }
    if pending_draw {
        return Err("qcom-adreno-a618: rendering operation lacks sysmem retirement");
    }
    Ok(())
}

#[derive(Default)]
struct A2dState {
    render_mode: Option<u32>,
    solid: Option<bool>,
    gras_control: Option<u32>,
    rb_control: Option<u32>,
    pixel_control: bool,
    output_info: bool,
    destination_info: bool,
    destination_word: Option<u32>,
    destination_pitch: Option<u32>,
    destination_tl: Option<u32>,
    destination_br: Option<u32>,
    scissor_tl: Option<u32>,
    scissor_br: Option<u32>,
    source_info: bool,
    source_word: Option<u32>,
    source_pitch: Option<u32>,
    source_size: Option<u32>,
    source_xmin: Option<u32>,
    source_ymin: Option<u32>,
    source_xmax: Option<u32>,
    source_ymax: Option<u32>,
    clear_color: bool,
}

fn set_a2d_expectation(
    addresses: &mut [AddressField],
    word_offset: u32,
    expectation: A2dExpectation,
) -> Result<(), &'static str> {
    let address = address_by_word_mut(addresses, word_offset)
        .ok_or("qcom-adreno-a618: A2D address lacks relocation metadata")?;
    if address.a2d.replace(expectation).is_some() {
        return Err("qcom-adreno-a618: A2D address is reused by multiple blits");
    }
    Ok(())
}

fn validate_a2d_sequences(
    packets: &[Packet<'_>],
    addresses: &mut [AddressField],
) -> Result<(), &'static str> {
    let mut state = A2dState::default();
    for packet in packets.iter().copied() {
        match packet.header {
            Header::Type4 { register, .. } => match register {
                RB_A2D_BLT_CNTL => {
                    state.rb_control = Some(packet.payload[0]);
                    state.solid = Some(packet.payload[0] & (1 << 7) != 0);
                }
                GRAS_A2D_BLT_CNTL => state.gras_control = Some(packet.payload[0]),
                RB_A2D_PIXEL_CNTL => state.pixel_control = true,
                SP_A2D_OUTPUT_INFO => state.output_info = true,
                RB_A2D_DEST_BUFFER_INFO => state.destination_info = true,
                RB_A2D_CLEAR_COLOR_DW0 => state.clear_color = true,
                RB_A2D_DEST_BUFFER_BASE => state.destination_word = Some(packet.word_offset + 1),
                RB_A2D_DEST_BUFFER_PITCH => {
                    state.destination_pitch = packet.payload[0].checked_mul(64)
                }
                GRAS_A2D_DEST_TL => {
                    let tl = packet.payload[0];
                    let br = packet.payload[1];
                    if (tl & 0xffff) > (br & 0xffff) || (tl >> 16) > (br >> 16) {
                        return Err("qcom-adreno-a618: inverted A2D destination rectangle");
                    }
                    state.destination_tl = Some(tl);
                    state.destination_br = Some(br);
                }
                GRAS_A2D_SCISSOR_TL => {
                    let tl = packet.payload[0];
                    let br = packet.payload[1];
                    if (tl & 0xffff) > (br & 0xffff) || (tl >> 16) > (br >> 16) {
                        return Err("qcom-adreno-a618: inverted A2D scissor rectangle");
                    }
                    state.scissor_tl = Some(tl);
                    state.scissor_br = Some(br);
                }
                TPL1_A2D_SRC_TEXTURE_INFO => state.source_info = true,
                TPL1_A2D_SRC_TEXTURE_BASE => state.source_word = Some(packet.word_offset + 1),
                TPL1_A2D_SRC_TEXTURE_PITCH => {
                    state.source_pitch = (packet.payload[0] >> 9).checked_mul(64)
                }
                TPL1_A2D_SRC_TEXTURE_SIZE => state.source_size = Some(packet.payload[0]),
                GRAS_A2D_SRC_XMIN => {
                    if packet.payload[0] > packet.payload[1]
                        || packet.payload[2] > packet.payload[3]
                    {
                        return Err("qcom-adreno-a618: inverted A2D source rectangle");
                    }
                    state.source_xmin = Some(packet.payload[0] >> 8);
                    state.source_ymin = Some(packet.payload[2] >> 8);
                    state.source_xmax = Some((packet.payload[1] >> 8).saturating_add(1));
                    state.source_ymax = Some((packet.payload[3] >> 8).saturating_add(1));
                }
                _ => {}
            },
            Header::Type7 {
                opcode: opcode::SET_MARKER,
                ..
            } => state.render_mode = packet.payload.first().copied(),
            Header::Type7 {
                opcode: opcode::BLIT,
                ..
            } => {
                if state.render_mode != Some(12)
                    || state.rb_control.is_none()
                    || state.gras_control != state.rb_control
                    || state.solid.is_none()
                    || !state.pixel_control
                    || !state.output_info
                    || !state.destination_info
                {
                    return Err("qcom-adreno-a618: incomplete A2D control state");
                }
                let destination_tl = state
                    .destination_tl
                    .ok_or("qcom-adreno-a618: A2D destination rectangle is missing")?;
                let destination_br = state
                    .destination_br
                    .ok_or("qcom-adreno-a618: A2D destination rectangle is missing")?;
                let scissor_tl = state
                    .scissor_tl
                    .ok_or("qcom-adreno-a618: A2D scissor is missing")?;
                let scissor_br = state
                    .scissor_br
                    .ok_or("qcom-adreno-a618: A2D scissor is missing")?;
                if (scissor_tl & 0xffff) < (destination_tl & 0xffff)
                    || (scissor_tl >> 16) < (destination_tl >> 16)
                    || (scissor_br & 0xffff) > (destination_br & 0xffff)
                    || (scissor_br >> 16) > (destination_br >> 16)
                {
                    return Err("qcom-adreno-a618: A2D scissor exceeds destination");
                }
                set_a2d_expectation(
                    addresses,
                    state
                        .destination_word
                        .ok_or("qcom-adreno-a618: A2D destination base is missing")?,
                    A2dExpectation {
                        row_pitch: state
                            .destination_pitch
                            .ok_or("qcom-adreno-a618: A2D destination pitch is missing")?,
                        required_width: (destination_br & 0xffff).saturating_add(1),
                        required_height: (destination_br >> 16).saturating_add(1),
                    },
                )?;
                if state.solid == Some(false) {
                    if !state.source_info {
                        return Err("qcom-adreno-a618: A2D source info is missing");
                    }
                    let size = state
                        .source_size
                        .ok_or("qcom-adreno-a618: A2D source size is missing")?;
                    let width = size & 0x7fff;
                    let height = size >> 15;
                    let required_width = state
                        .source_xmax
                        .ok_or("qcom-adreno-a618: A2D source rectangle is missing")?;
                    let required_height = state
                        .source_ymax
                        .ok_or("qcom-adreno-a618: A2D source rectangle is missing")?;
                    let source_xmin = state
                        .source_xmin
                        .ok_or("qcom-adreno-a618: A2D source rectangle is missing")?;
                    let source_ymin = state
                        .source_ymin
                        .ok_or("qcom-adreno-a618: A2D source rectangle is missing")?;
                    if width == 0
                        || height == 0
                        || source_xmin >= required_width
                        || source_ymin >= required_height
                        || required_width > width
                        || required_height > height
                    {
                        return Err("qcom-adreno-a618: A2D source rectangle exceeds texture");
                    }
                    set_a2d_expectation(
                        addresses,
                        state
                            .source_word
                            .ok_or("qcom-adreno-a618: A2D source base is missing")?,
                        A2dExpectation {
                            row_pitch: state
                                .source_pitch
                                .ok_or("qcom-adreno-a618: A2D source pitch is missing")?,
                            required_width: width,
                            required_height: height,
                        },
                    )?;
                } else if !state.clear_color {
                    return Err("qcom-adreno-a618: A2D solid blit clear color is missing");
                }
                state = A2dState::default();
            }
            _ => {}
        }
    }
    if addresses
        .iter()
        .any(|address| address.requires_complete_linear_image && address.a2d.is_none())
    {
        return Err("qcom-adreno-a618: A2D address is not consumed by a complete blit");
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct SegmentRegister<'a> {
    register: u32,
    word_offset: u32,
    payload: &'a [u32],
}

/// One parsed draw-state segment with a sorted Type4 register index.
///
/// The validator compares every segment against each canonical shader/link
/// variant. Re-scanning the entire PM4 segment for every register in every
/// candidate made validation dominate short UI submissions. The index only
/// accelerates lookup: duplicate register packets still resolve to `None`, so
/// the accepted language is unchanged.
struct SegmentIndex<'packets, 'registers, 'words> {
    packets: &'packets [Packet<'words>],
    registers: &'registers [SegmentRegister<'words>],
}

impl<'packets, 'registers, 'words> SegmentIndex<'packets, 'registers, 'words> {
    fn new(
        packets: &'packets [Packet<'words>],
        register_scratch: &'registers mut Vec<SegmentRegister<'words>>,
    ) -> Self {
        register_scratch.clear();
        register_scratch.extend(packets.iter().filter_map(|packet| {
            let Header::Type4 { register, .. } = packet.header else {
                return None;
            };
            Some(SegmentRegister {
                register,
                word_offset: packet.word_offset,
                payload: packet.payload,
            })
        }));
        register_scratch.sort_unstable_by_key(|entry| entry.register);
        Self {
            packets,
            registers: register_scratch.as_slice(),
        }
    }

    fn reg(&self, wanted: u32) -> Option<(u32, &'words [u32])> {
        let index = self
            .registers
            .partition_point(|entry| entry.register < wanted);
        let entry = self.registers.get(index)?;
        if entry.register != wanted
            || self
                .registers
                .get(index + 1)
                .is_some_and(|next| next.register == wanted)
        {
            return None;
        }
        Some((entry.word_offset, entry.payload))
    }
}

/// Find the upstream A6xx shader-program layout burst and return the packet
/// offset plus the address field within it. FIRST_EXEC_OFFSET, OBJ_START,
/// PVT_MEM_PARAM, PVT_MEM_ADDR, and PVT_MEM_SIZE must be programmed together;
/// accepting the older split form exposes a partially updated SP layout.
fn segment_shader_program_layout(
    segment: &SegmentIndex<'_, '_, '_>,
    first_exec_register: u32,
) -> Option<(u32, u32)> {
    let (word_offset, payload) = segment.reg(first_exec_register)?;
    if payload.len() != 7 || payload[0] != 0 || payload[3..] != [0; 4] {
        return None;
    }
    Some((word_offset, word_offset + 2))
}

fn segment_matches_pipeline(segment: &SegmentIndex<'_, '_, '_>, variant: PipelineVariant) -> bool {
    let packets = segment.packets;
    let link = link_meta(variant);
    let fixed = pipeline_state_meta(variant);
    let ShaderMeta::Vertex(vs) = shader_meta(link.vs) else {
        return false;
    };
    let ShaderMeta::Fragment(fs) = shader_meta(link.fs) else {
        return false;
    };
    let reg = |wanted| segment.reg(wanted).map(|(_, payload)| payload);
    let load_count = |opcode, block, state_type| {
        packets
            .iter()
            .copied()
            .filter(|p| {
                matches!(p.header, Header::Type7 { opcode: actual, .. } if actual == opcode)
                    && p.payload.first().is_some_and(|word| {
                        (word >> 18) & 0xf == block && (word >> 14) & 3 == state_type
                    })
            })
            .count()
    };
    let type7_count = |wanted_opcode, wanted_payload: &[u32]| {
        packets
            .iter()
            .copied()
            .filter(|packet| {
                matches!(packet.header, Header::Type7 { opcode, .. } if opcode == wanted_opcode)
                    && packet.payload == wanted_payload
            })
            .count()
    };
    let last_marker = packets
        .iter()
        .copied()
        .filter_map(|packet| {
            matches!(
                packet.header,
                Header::Type7 {
                    opcode: opcode::SET_MARKER,
                    ..
                }
            )
            .then(|| packet.payload.first().copied())
            .flatten()
        })
        .last();
    type7_count(opcode::SET_MARKER, &[1]) == 1
        // A clear/copy before the first draw legitimately leaves an earlier
        // BLIT2D marker in this segment.  What controls the draw is the final
        // marker, which must select direct rendering.
        && last_marker == Some(1)
        && type7_count(CP_SKIP_IB2_ENABLE_GLOBAL, &[0]) == 1
        && type7_count(CP_SKIP_IB2_ENABLE_LOCAL, &[1]) == 1
        && type7_count(CP_SET_VISIBILITY_OVERRIDE, &[1]) == 1
        && type7_count(CP_REG_WRITE, &[2, RB_RENDER_CNTL, 0x10]) == 1
        && reg(SP_UPDATE_CNTL) == Some(&[0x0000_00ff])
        && reg(GRAS_SC_CNTL) == Some(&[2])
        && reg(GRAS_SC_BIN_CNTL) == Some(&[0x00c0_0000])
        && reg(RB_CNTL) == Some(&[0x00c0_0000])
        && reg(RB_MODE_CNTL) == Some(&[0x10])
        && reg(RB_BIN_CONTROL2) == Some(&[0])
        && reg(RB_WINDOW_OFFSET) == Some(&[0])
        && reg(RB_WINDOW_OFFSET2) == Some(&[0])
        && reg(SP_WINDOW_OFFSET) == Some(&[0])
        && reg(TPL1_WINDOW_OFFSET) == Some(&[0])
        && reg(GRAS_SC_SCREEN_SCISSOR_CNTL) == Some(&[0])
        && reg(VPC_SO_OVERRIDE) == Some(&[0])
        && reg(PC_STEREO_RENDERING_CNTL) == Some(&[0])
        && reg(SP_VS_CNTL_0) == Some(&[vs.sp_vs_cntl_0])
        && reg(SP_VS_CONST_CONFIG) == Some(&[vs.sp_vs_const_config, 0, 0, 0])
        && reg(SP_PS_CONST_CONFIG) == Some(&[fs.sp_ps_const_config])
        && reg(SP_HS_CONFIG) == Some(&[0])
        && reg(SP_DS_CONFIG) == Some(&[0])
        && reg(SP_GS_CONFIG) == Some(&[0])
        && reg(SP_GFX_USIZE) == Some(&[0])
        && reg(VFD_CNTL_1) == Some(&vs.vfd_cntl_1_6)
        && reg(VFD_DEST_CNTL) == Some(vs.vfd_dest_cntl)
        && reg(SP_VS_OUTPUT_CNTL) == Some(&[link.sp_vs_output_cntl])
        && reg(SP_VS_OUTPUT_REG) == Some(link.sp_vs_output_reg)
        && reg(SP_VS_VPC_DEST_REG) == Some(link.sp_vs_vpc_dest_reg)
        && reg(VPC_VS_CNTL) == Some(&[link.vpc_vs_cntl])
        && reg(VPC_VS_CLIP_CULL_CNTL) == Some(&[0x00ff_ff00])
        && reg(VPC_VS_CLIP_CULL_CNTL_V2) == Some(&[0x00ff_ff00])
        && reg(GRAS_CL_VS_CLIP_CULL_DISTANCE) == Some(&[0])
        && reg(VPC_VS_SIV_CNTL) == Some(&[0x0000_ffff])
        && reg(VPC_VS_SIV_CNTL_V2) == Some(&[0x0000_ffff])
        && reg(GRAS_SU_VS_SIV_CNTL) == Some(&[0])
        && reg(PC_PS_CNTL) == Some(&[0])
        && reg(VPC_PS_CNTL) == Some(&[link.vpc_ps_cntl])
        && reg(PC_VS_CNTL) == Some(&[link.pc_vs_cntl])
        && reg(VPC_VARYING_LM_TRANSFER_CNTL_DISABLE) == Some(&link.lm_transfer_disable)
        && reg(SP_PS_CNTL_0) == Some(&[fs.sp_ps_cntl_0])
        && reg(SP_PS_WAVE_CNTL) == Some(&[fs.sp_ps_wave_cntl])
        && reg(SP_LB_PARAM_LIMIT) == Some(&[7])
        && reg(GRAS_CL_INTERP_CNTL) == Some(&[fs.gras_cl_interp_cntl])
        && reg(RB_INTERP_CNTL) == Some(&[fs.rb_interp_cntl])
        && reg(RB_PS_INPUT_CNTL) == Some(&[fs.rb_ps_input_cntl])
        && reg(RB_PS_SAMPLEFREQ_CNTL) == Some(&[0])
        && reg(GRAS_LRZ_PS_INPUT_CNTL) == Some(&[0])
        && reg(GRAS_LRZ_PS_SAMPLEFREQ_CNTL) == Some(&[0])
        && reg(VPC_VARYING_INTERP_MODE) == Some(&[0; 8])
        && reg(VPC_VARYING_REPLACE_MODE) == Some(&[0; 8])
        && reg(SP_PS_OUTPUT_REG) == Some(fs.sp_ps_output_reg)
        && reg(SP_VS_INSTR_SIZE) == Some(&[vs.sp_vs_instr_size])
        && reg(SP_VS_CONFIG) == Some(&[vs.sp_vs_config])
        && reg(SP_PS_INSTR_SIZE) == Some(&[fs.sp_ps_instr_size])
        && reg(SP_PS_CONFIG) == Some(&[fs.sp_ps_config])
        && segment_shader_program_layout(segment, SP_VS_PROGRAM_COUNTER_OFFSET).is_some()
        && reg(SP_VS_PVT_MEM_STACK_OFFSET) == Some(&[0])
        && segment_shader_program_layout(segment, SP_PS_PROGRAM_COUNTER_OFFSET).is_some()
        && reg(SP_PS_PVT_MEM_STACK_OFFSET) == Some(&[0])
        && reg(SP_REG_PROG_ID_0) == Some(&fs.sp_reg_prog_id)
        && reg(SP_PS_OUTPUT_CNTL) == Some(&[fs.sp_ps_output_cntl])
        && reg(SP_PS_OUTPUT_MASK) == Some(&[fs.sp_ps_output_mask])
        && reg(RB_PS_OUTPUT_CNTL) == Some(&[fs.rb_ps_output_cntl])
        && reg(RB_PS_OUTPUT_MASK) == Some(&[fs.rb_ps_output_mask])
        && reg(SP_PS_INITIAL_TEX_LOAD_CNTL).is_some_and(|p| {
            p.first() == Some(&fs.initial_tex_load_cntl)
                && p.get(1..) == Some(fs.initial_tex_load_cmd)
        })
        && reg(SP_PS_TSIZE) == Some(&[u32::from(fixed.uses_sampler)])
        && if fixed.uses_sampler {
                reg(SP_PS_INITIAL_TEX_INDEX_CMD) == Some(&[0])
                    && reg(SP_PS_SAMPLER_BASE).is_some()
                    && reg(SP_PS_TEXMEMOBJ_BASE).is_some()
            } else {
                reg(SP_PS_INITIAL_TEX_INDEX_CMD).is_none()
                    && reg(SP_PS_SAMPLER_BASE).is_none()
                    && reg(SP_PS_TEXMEMOBJ_BASE).is_none()
            }
        && reg(VFD_VERTEX_BUFFER_SIZE).is_some_and(|p| p.len() == 2 && p[1] == fixed.stride)
        && reg(VFD_FETCH_INSTR) == Some(fixed.vfd_fetch)
        && reg(VFD_CNTL_0)
            == Some(&[
                (fixed.vfd_fetch.len() as u32 / 2) | ((fixed.vfd_fetch.len() as u32 / 2) << 8)
            ])
        && reg(RB_MRT_BUF_INFO) == Some(&[FORMAT_8_8_8_8_UNORM | MRT_COLOR_SWAP_WXYZ])
        && reg(RB_MRT_BASE_GMEM) == Some(&[0])
        && reg(RB_COLOR_FLAG_BUFFER_ADDR) == Some(&[0, 0, 0])
        && reg(GRAS_LRZ_MRT_BUFFER_INFO_0) == Some(&[FORMAT_8_8_8_8_UNORM])
        && reg(RB_SRGB_CNTL) == Some(&[0])
        && reg(SP_SRGB_CNTL) == Some(&[0])
        && reg(GRAS_CL_ARRAY_SIZE) == Some(&[0])
        && reg(RB_MRT_CONTROL)
            == Some(if fixed.source_over {
                &[0x7e3, 0x0701_0706]
            } else {
                &[0x7e0, 0x0001_0001]
            })
        && reg(RB_BLEND_CNTL)
            == Some(&[if fixed.source_over {
                0xffff_0001
            } else {
                0xffff_0000
            }])
        && reg(SP_BLEND_CNTL) == Some(&[u32::from(fixed.source_over)])
        && reg(RB_DITHER_CNTL) == Some(&[0])
        && reg(RB_ALPHA_TEST_CNTL) == Some(&[0])
        && reg(RB_STENCIL_CNTL) == Some(&[0])
        && reg(GRAS_SU_STENCIL_CNTL) == Some(&[0])
        && reg(RB_STENCIL_REF_CNTL) == Some(&[0])
        && reg(RB_STENCIL_MASK) == Some(&[0, 0])
        && reg(RB_DEPTH_CNTL) == Some(&[0])
        && reg(GRAS_SU_DEPTH_CNTL) == Some(&[0])
        && reg(RB_DEPTH_PLANE_CNTL) == Some(&[0])
        && reg(GRAS_SU_DEPTH_PLANE_CNTL) == Some(&[0])
        && reg(RB_DEPTH_BOUND_MIN) == Some(&[0, 1.0_f32.to_bits()])
        && reg(RB_DEPTH_BUFFER_INFO) == Some(&[0, 0, 0, 0, 0, 0])
        && reg(GRAS_SU_DEPTH_BUFFER_INFO) == Some(&[0])
        && reg(RB_STENCIL_BUFFER_INFO) == Some(&[0])
        && reg(TPL1_RAS_MSAA_CNTL) == Some(&[0, 4])
        && reg(GRAS_SC_RAS_MSAA_CNTL) == Some(&[0, 4, 0])
        && reg(RB_RAS_MSAA_CNTL) == Some(&[0, 4, 0])
        && reg(TPL1_MSAA_SAMPLE_POS_CNTL) == Some(&[0])
        && reg(RB_RESOLVE_GMEM_BUFFER_INFO) == Some(&[0])
        && reg(GRAS_CL_CNTL) == Some(&[0x80])
        && reg(GRAS_CL_GUARDBAND_CLIP_ADJ)
            .is_some_and(|p| p.len() == 1 && p[0] & !(0x1ff | (0x1ff << 10)) == 0)
        && reg(GRAS_SU_CNTL).is_some_and(|p| {
            p.len() == 1
                && if fixed.stride == 28 {
                    p[0] & !0x2017 == 0 && p[0] & 0x2010 == 0x2010
                } else {
                    p[0] == if fixed.stride == 40 { 0x2012 } else { 0x2010 }
                }
        })
        && reg(GRAS_SU_POINT_MINMAX) == Some(&[0x0010_0010])
        && reg(GRAS_SU_POINT_SIZE) == Some(&[0x10])
        && reg(GRAS_SU_POLY_OFFSET_SCALE) == Some(&[0, 0, 0])
        && reg(PC_CNTL) == Some(&[0])
        && reg(VPC_RAST_CNTL) == Some(&[3])
        && reg(PC_DGEN_RAST_CNTL) == Some(&[3])
        && load_count(CP_LOAD_STATE6_GEOM, 8, 1) == 1
        && load_count(CP_LOAD_STATE6_FRAG, 12, 1) == 1
        && load_count(CP_LOAD_STATE6_GEOM, 8, 0) == 1
        && load_count(CP_LOAD_STATE6_FRAG, 12, 0) == 1
        && load_count(CP_LOAD_STATE6_FRAG, 4, 1) == usize::from(fixed.uses_sampler)
        && load_count(CP_LOAD_STATE6_FRAG, 4, 0) == usize::from(fixed.uses_sampler)
        && if fixed.uses_sampler {
            packets.iter().copied().any(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: CP_MEM_WRITE,
                        ..
                    }
                ) && p.payload.len() == 22
                    && (p.payload[18..] == SAMPLER_CLAMP_NEAREST
                        || p.payload[18..] == SAMPLER_CLAMP_LINEAR)
            })
        } else {
            !packets.iter().copied().any(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: CP_MEM_WRITE,
                        ..
                    }
                ) && p.payload.len() == 22
                    && p.payload[2] == 0x4c00_6880
            })
        }
}

/// Reject impossible pipeline candidates using state that the full matcher
/// already requires.  A production UI draw used to run the packet-scanning
/// matcher for all thirteen canonical variants even though stride, vertex
/// fetch layout, sampler use, and blend mode narrow that set to at most three.
/// This is only a prefilter: every surviving candidate still passes the exact
/// canonical-state matcher below, so the accepted PM4 language is unchanged.
fn segment_may_match_pipeline(
    segment: &SegmentIndex<'_, '_, '_>,
    variant: PipelineVariant,
) -> bool {
    let fixed = pipeline_state_meta(variant);
    let reg = |wanted| segment.reg(wanted).map(|(_, payload)| payload);
    reg(VFD_VERTEX_BUFFER_SIZE)
        .is_some_and(|payload| payload.len() == 2 && payload[1] == fixed.stride)
        && reg(VFD_FETCH_INSTR) == Some(fixed.vfd_fetch)
        && reg(SP_PS_TSIZE) == Some(&[u32::from(fixed.uses_sampler)])
        && reg(RB_MRT_CONTROL)
            == Some(if fixed.source_over {
                &[0x7e3, 0x0701_0706]
            } else {
                &[0x7e0, 0x0001_0001]
            })
}

fn address_by_word_mut(
    addresses: &mut [AddressField],
    word_offset: u32,
) -> Option<&mut AddressField> {
    let index = addresses
        .binary_search_by_key(&word_offset, |address| address.word_offset)
        .ok()?;
    addresses.get_mut(index)
}

fn set_address_source(
    addresses: &mut [AddressField],
    word: u32,
    source: AddressSource,
) -> Result<(), &'static str> {
    let address = address_by_word_mut(addresses, word)
        .ok_or("qcom-adreno-a618: 3D address lacks relocation metadata")?;
    address.source = source;
    Ok(())
}

fn set_image_expectation(
    addresses: &mut [AddressField],
    word: u32,
    expectation: ImageExpectation,
) -> Result<(), &'static str> {
    let address = address_by_word_mut(addresses, word)
        .ok_or("qcom-adreno-a618: 3D image address lacks relocation metadata")?;
    if address.image.replace(expectation).is_some() {
        return Err("qcom-adreno-a618: 3D image address is reused");
    }
    Ok(())
}

fn validate_3d_sequences(
    packets: &[Packet<'_>],
    addresses: &mut [AddressField],
) -> Result<(), &'static str> {
    let mut segment_start = 0usize;
    let mut segment_registers = Vec::new();
    for (packet_index, packet) in packets.iter().copied().enumerate() {
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_DRAW_INDX_OFFSET,
                ..
            }
        ) {
            continue;
        }
        let segment = SegmentIndex::new(
            &packets[segment_start..packet_index],
            &mut segment_registers,
        );
        let mut matched = None;
        for candidate in PipelineVariant::ALL {
            if segment_may_match_pipeline(&segment, candidate)
                && segment_matches_pipeline(&segment, candidate)
            {
                if matched.is_some() {
                    return Err("qcom-adreno-a618: ambiguous canonical 3D pipeline state");
                }
                matched = Some(candidate);
            }
        }
        let link =
            link_meta(matched.ok_or("qcom-adreno-a618: incomplete canonical 3D pipeline state")?);
        let (_, vs_address) = segment_shader_program_layout(&segment, SP_VS_PROGRAM_COUNTER_OFFSET)
            .ok_or("qcom-adreno-a618: vertex shader program layout is missing")?;
        let (_, fs_address) = segment_shader_program_layout(&segment, SP_PS_PROGRAM_COUNTER_OFFSET)
            .ok_or("qcom-adreno-a618: fragment shader program layout is missing")?;
        set_address_source(
            addresses,
            vs_address,
            AddressSource::CanonicalShader(link.vs),
        )?;
        set_address_source(
            addresses,
            fs_address,
            AddressSource::CanonicalShader(link.fs),
        )?;
        let shader_preload_word = |wanted_opcode: u8, wanted_block: u32| {
            segment
                .packets
                .iter()
                .copied()
                .find(|candidate| {
                    matches!(
                        candidate.header,
                        Header::Type7 { opcode, .. } if opcode == wanted_opcode
                    ) && candidate.payload.len() == 3
                        && (candidate.payload[0] >> 14) & 3 == 0
                        && (candidate.payload[0] >> 16) & 3 == 2
                        && (candidate.payload[0] >> 18) & 0xf == wanted_block
                })
                .map(|candidate| candidate.word_offset + 2)
        };
        set_address_source(
            addresses,
            shader_preload_word(CP_LOAD_STATE6_GEOM, 8)
                .ok_or("qcom-adreno-a618: vertex shader preload is missing")?,
            AddressSource::CanonicalShader(link.vs),
        )?;
        set_address_source(
            addresses,
            shader_preload_word(CP_LOAD_STATE6_FRAG, 12)
                .ok_or("qcom-adreno-a618: fragment shader preload is missing")?,
            AddressSource::CanonicalShader(link.fs),
        )?;

        let (pitch_packet, pitch) = segment
            .reg(RB_MRT_PITCH)
            .ok_or("qcom-adreno-a618: MRT layout is missing")?;
        let (base_packet, _) = segment
            .reg(RB_MRT_BASE)
            .ok_or("qcom-adreno-a618: MRT base is missing")?;
        let (_, clip_transform) = segment
            .reg(GRAS_CL_VIEWPORT_XOFFSET)
            .ok_or("qcom-adreno-a618: viewport transform is missing")?;
        let (_, screen) = segment
            .reg(GRAS_SC_SCREEN_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: render bounds are missing")?;
        let (_, viewport) = segment
            .reg(GRAS_SC_VIEWPORT_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: viewport scissor is missing")?;
        let (_, window) = segment
            .reg(GRAS_SC_WINDOW_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: window scissor is missing")?;
        let x = |word: u32| word & 0xffff;
        let y = |word: u32| word >> 16;
        if viewport[0] != 0
            || x(viewport[0]) > x(viewport[1])
            || y(viewport[0]) > y(viewport[1])
            || x(screen[0]) > x(screen[1])
            || y(screen[0]) > y(screen[1])
            || x(screen[0]) < x(viewport[0])
            || y(screen[0]) < y(viewport[0])
            || x(screen[1]) > x(viewport[1])
            || y(screen[1]) > y(viewport[1])
            || x(window[0]) > x(window[1])
            || y(window[0]) > y(window[1])
            || x(window[0]) < x(screen[0])
            || y(window[0]) < y(screen[0])
            || x(window[1]) > x(screen[1])
            || y(window[1]) > y(screen[1])
        {
            return Err("qcom-adreno-a618: unsafe 3D scissor state");
        }
        // SGFX clip-space coordinates are defined against the complete
        // attachment.  The render area is damage and may only narrow the
        // screen/window scissors; it must never remap the viewport.
        let width = (x(viewport[1]) - x(viewport[0]) + 1) as f32;
        let height = (y(viewport[1]) - y(viewport[0]) + 1) as f32;
        let x_scale = width * 0.5;
        let y_scale = height * 0.5;
        let expected_clip_transform = [
            (x(viewport[0]) as f32 + x_scale).to_bits(),
            x_scale.to_bits(),
            (y(viewport[0]) as f32 + y_scale).to_bits(),
            (-y_scale).to_bits(),
            0.5_f32.to_bits(),
            0.5_f32.to_bits(),
        ];
        if clip_transform != expected_clip_transform {
            return Err("qcom-adreno-a618: unsafe 3D viewport transform");
        }
        let _ = pitch_packet;
        let row_pitch = pitch[0]
            .checked_mul(64)
            .ok_or("qcom-adreno-a618: MRT pitch overflows")?;
        set_image_expectation(
            addresses,
            base_packet + 1,
            ImageExpectation {
                row_pitch,
                width: (viewport[1] & 0xffff).saturating_add(1),
                height: (viewport[1] >> 16).saturating_add(1),
                exact_extent: true,
                pitch_align: None,
                array_pitch: Some(ArrayPitchExpectation {
                    bytes: u64::from(pitch[1]) << 6,
                    alignment: 64,
                }),
            },
        )?;
        let (_, vertex_layout) = segment
            .reg(VFD_VERTEX_BUFFER_SIZE)
            .ok_or("qcom-adreno-a618: VFD buffer layout is missing")?;
        let (_, vertex_offset) = segment
            .reg(VFD_INDEX_OFFSET)
            .ok_or("qcom-adreno-a618: VFD vertex offset is missing")?;
        if packet.payload.len() == 3 {
            let required_vertex_bytes = packet.payload[2]
                .checked_add(vertex_offset[0])
                .and_then(|count| count.checked_mul(vertex_layout[1]))
                .ok_or("qcom-adreno-a618: VFD vertex span overflows")?;
            if vertex_layout[0] != required_vertex_bytes {
                return Err("qcom-adreno-a618: VFD size does not match draw span");
            }
        } else {
            let minimum_vertex_bytes = vertex_offset[0]
                .checked_add(1)
                .and_then(|count| count.checked_mul(vertex_layout[1]))
                .ok_or("qcom-adreno-a618: indexed VFD base vertex overflows")?;
            if minimum_vertex_bytes > vertex_layout[0] {
                return Err("qcom-adreno-a618: indexed VFD base vertex is out of bounds");
            }
        }
        let (vertex_base, _) = segment
            .reg(VFD_VERTEX_BUFFER_BASE)
            .ok_or("qcom-adreno-a618: VFD buffer base is missing")?;
        let vertex_address = address_by_word_mut(addresses, vertex_base + 1)
            .ok_or("qcom-adreno-a618: VFD address lacks relocation")?;
        vertex_address.required_size = Some(u64::from(vertex_layout[0]));

        if let Some((descriptor_packet, descriptor)) =
            segment.packets.iter().copied().find_map(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: CP_LOAD_STATE6_FRAG,
                        ..
                    }
                )
                .then_some(packet)
                .filter(|packet| {
                    packet.payload.len() == 19
                        && packet.payload[0] == ((1 << 14) | (4 << 18) | (1 << 22))
                })
                .map(|packet| (packet.word_offset, packet.payload))
            })
        {
            let width = descriptor[4] & 0x7fff;
            let height = (descriptor[4] >> 15) & 0x7fff;
            let row_pitch = (descriptor[5] >> 7) & 0x3f_ffff;
            set_image_expectation(
                addresses,
                descriptor_packet + 8,
                ImageExpectation {
                    row_pitch,
                    width,
                    height,
                    exact_extent: true,
                    pitch_align: Some(descriptor[5] & 0xf),
                    array_pitch: Some(ArrayPitchExpectation {
                        bytes: u64::from(descriptor[6] & 0x7f_ffff) << 12,
                        alignment: 4096,
                    }),
                },
            )?;
        }
        if let Some((descriptor_packet, descriptor)) =
            segment.packets.iter().copied().find_map(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: CP_MEM_WRITE,
                        ..
                    }
                )
                .then_some(packet)
                .filter(|packet| packet.payload.len() == 22 && packet.payload[2] == 0x4c00_6880)
                .map(|packet| (packet.word_offset, packet.payload))
            })
        {
            let width = descriptor[3] & 0x7fff;
            let height = (descriptor[3] >> 15) & 0x7fff;
            let row_pitch = (descriptor[4] >> 7) & 0x3f_ffff;
            set_image_expectation(
                addresses,
                descriptor_packet + 7,
                ImageExpectation {
                    row_pitch,
                    width,
                    height,
                    exact_extent: true,
                    pitch_align: Some(descriptor[4] & 0xf),
                    array_pitch: Some(ArrayPitchExpectation {
                        bytes: u64::from(descriptor[5] & 0x7f_ffff) << 12,
                        alignment: 4096,
                    }),
                },
            )?;
        }
        segment_start = packet_index + 1;
    }
    Ok(())
}

fn copy_pm4(decoded: DecodedSubmit<'_>) -> Result<Vec<u32>, &'static str> {
    let mut words = Vec::new();
    words
        .try_reserve_exact(decoded.pm4_len())
        .map_err(|_| "qcom-adreno-a618: PM4 copy allocation failed")?;
    for index in 0..decoded.pm4_len() {
        words.push(
            decoded
                .pm4_word(index)
                .ok_or("qcom-adreno-a618: PM4 table changed during decode")?,
        );
    }
    Ok(words)
}

/// Return the first individually rejected PM4 packet for a diagnostic log.
///
/// Sequence-level and relocation-level failures intentionally return `None`;
/// this helper never weakens or replaces the authoritative validation pass.
pub(crate) fn diagnose_rejected_packet(bytes: &[u8]) -> Option<RejectedPacket> {
    let decoded = adreno_a6xx_submit_wire::decode(bytes).ok()?;
    let words = copy_pm4(decoded).ok()?;
    let mut addresses = Vec::new();
    for packet in Packets::new(&words) {
        let packet = packet.ok()?;
        let (kind, selector, result) = match packet.header {
            Header::Type4 { register, .. } => (
                RejectedPacketKind::Type4,
                register,
                validate_type4(register, packet.payload, packet.word_offset, &mut addresses),
            ),
            Header::Type7 { opcode, .. } => (
                RejectedPacketKind::Type7,
                u32::from(opcode),
                validate_type7(opcode, packet.payload, packet.word_offset, &mut addresses),
            ),
        };
        if result.is_err() {
            return Some(RejectedPacket {
                kind,
                word_offset: packet.word_offset,
                selector,
                payload_len: packet.payload.len() as u32,
                first_value: packet.payload.first().copied(),
            });
        }
    }
    None
}

fn validate_pm4(
    decoded: DecodedSubmit<'_>,
    relocations: &[Relocation],
) -> Result<(Vec<u32>, Vec<AddressField>), &'static str> {
    let words = copy_pm4(decoded)?;
    let packets = Packets::new(&words)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "qcom-adreno-a618: malformed PM4 packet stream")?;

    let mut addresses = Vec::new();
    addresses
        .try_reserve_exact(decoded.relocation_len())
        .map_err(|_| "qcom-adreno-a618: PM4 address validation allocation failed")?;
    for packet in packets.iter().copied() {
        match packet.header {
            Header::Type4 { register, .. } => {
                validate_type4(register, packet.payload, packet.word_offset, &mut addresses)?
            }
            Header::Type7 { opcode, .. } => {
                validate_type7(opcode, packet.payload, packet.word_offset, &mut addresses)?
            }
        }
    }
    if addresses.len() != relocations.len() {
        return Err("qcom-adreno-a618: every GPU address must have one relocation");
    }
    if !addresses
        .windows(2)
        .all(|pair| pair[0].word_offset < pair[1].word_offset)
    {
        return Err("qcom-adreno-a618: GPU address fields are not strictly ordered");
    }
    validate_memcpy_barriers(&packets)?;
    validate_ccu_transitions(&packets)?;
    validate_render_retirement(&packets)?;
    validate_a2d_sequences(&packets, &mut addresses)?;
    validate_3d_sequences(&packets, &mut addresses)?;
    for (address, relocation) in addresses.iter().zip(relocations.iter().copied()) {
        let source_matches = match (address.source, relocation.source) {
            (AddressSource::Attachment, RelocationSource::Attachment(_)) => true,
            (
                AddressSource::CanonicalShader(expected),
                RelocationSource::CanonicalShader(actual),
            ) => expected == actual,
            _ => false,
        };
        if relocation.pm4_word_offset != address.word_offset
            || relocation.access != address.access
            || relocation.encoding != address.encoding
            || !source_matches
        {
            return Err("qcom-adreno-a618: relocation does not match its PM4 address field");
        }
    }
    Ok((words, addresses))
}

fn relocate_one(
    words: &mut [u32],
    relocation: Relocation,
    resource: ResolvedResource,
) -> Result<(), &'static str> {
    let end = resource
        .gpu_va
        .checked_add(resource.allocation_size)
        .ok_or("qcom-adreno-a618: resource GPU VA overflows")?;
    let address = resource
        .gpu_va
        .checked_add(relocation.resource_offset)
        .ok_or("qcom-adreno-a618: relocated GPU VA overflows")?;
    let required_end = address
        .checked_add(relocation.required_size)
        .ok_or("qcom-adreno-a618: relocated range overflows")?;
    if required_end > end {
        return Err("qcom-adreno-a618: relocation exceeds attached allocation");
    }
    let index = relocation.pm4_word_offset as usize;
    match relocation.encoding {
        AddressEncoding::GpuVa64 => {
            let destination = words
                .get_mut(index..index + 2)
                .ok_or("qcom-adreno-a618: relocation PM4 offset is invalid")?;
            destination.copy_from_slice(&[address as u32, (address >> 32) as u32]);
        }
        AddressEncoding::GpuVa49TexDescriptor => {
            if address & 0x1f != 0 || address >= (1_u64 << 49) {
                return Err("qcom-adreno-a618: texture descriptor address is invalid");
            }
            let destination = words
                .get_mut(index..index + 2)
                .ok_or("qcom-adreno-a618: relocation PM4 offset is invalid")?;
            destination[0] = (destination[0] & 0x1f) | (address as u32 & !0x1f);
            destination[1] = (destination[1] & !0x1ffff) | ((address >> 32) as u32 & 0x1ffff);
        }
    }
    Ok(())
}

/// Decode, authorize, validate, and relocate into a kernel-owned dword vector.
pub(crate) fn validate_and_relocate(
    bytes: &[u8],
    mut resolve: impl FnMut(u64) -> Option<ResolvedResource>,
    mut resolve_shader: impl FnMut(ShaderVariant) -> Option<ResolvedResource>,
) -> Result<Vec<u32>, &'static str> {
    let decoded = adreno_a6xx_submit_wire::decode(bytes)
        .map_err(|_| "qcom-adreno-a618: invalid A6xx submit wire")?;
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(decoded.resource_len())
        .map_err(|_| "qcom-adreno-a618: resource validation allocation failed")?;
    for index in 0..decoded.resource_len() {
        let wire = decoded
            .resource(index)
            .ok_or("qcom-adreno-a618: resource table is incomplete")?;
        let resolved = resolve(wire.attachment_token)
            .ok_or("qcom-adreno-a618: resource is not attached to this context")?;
        let range_end = wire
            .range_offset
            .checked_add(wire.range_size)
            .ok_or("qcom-adreno-a618: resource range overflows")?;
        if wire.access & !resolved.allowed_access != 0 {
            return Err("qcom-adreno-a618: submit exceeds attached resource access authority");
        }
        if range_end > resolved.allocation_size {
            return Err("qcom-adreno-a618: resource range exceeds attached allocation");
        }
        resources.push(ResolvedResource {
            attachment_token: resolved.attachment_token,
            gpu_va: resolved
                .gpu_va
                .checked_add(wire.range_offset)
                .ok_or("qcom-adreno-a618: resource range GPU VA overflows")?,
            allocation_size: wire.range_size,
            // Preserve the wire-declared authority after checking it is a subset of
            // the attached object's actual authority. Relocations must be a subset
            // of both layers.
            allowed_access: wire.access,
            linear_image: resolved.linear_image.and_then(|image| {
                (wire.range_offset == 0 && wire.range_size == image.visible_size).then_some(image)
            }),
        });
    }
    let mut relocations = Vec::new();
    relocations
        .try_reserve_exact(decoded.relocation_len())
        .map_err(|_| "qcom-adreno-a618: relocation validation allocation failed")?;
    for index in 0..decoded.relocation_len() {
        relocations.push(
            decoded
                .relocation(index)
                .ok_or("qcom-adreno-a618: relocation table is incomplete")?,
        );
    }
    let (mut words, addresses) = validate_pm4(decoded, &relocations)?;
    for (index, relocation) in relocations.iter().copied().enumerate() {
        let resource = match relocation.source {
            RelocationSource::Attachment(resource_index) => *resources
                .get(resource_index as usize)
                .ok_or("qcom-adreno-a618: relocation resource index is invalid")?,
            RelocationSource::CanonicalShader(variant) => resolve_shader(variant)
                .ok_or("qcom-adreno-a618: canonical shader is unavailable")?,
        };
        if relocation.access & !resource.allowed_access != 0 {
            return Err("qcom-adreno-a618: relocation exceeds declared resource access");
        }
        let address = addresses[index];
        if address
            .required_size
            .is_some_and(|required| relocation.required_size != required)
        {
            return Err("qcom-adreno-a618: relocation size does not match PM4 operation");
        }
        if address.requires_complete_linear_image {
            let image = resource.linear_image.ok_or(
                "qcom-adreno-a618: A2D address must cover one complete linear BGRA8 image",
            )?;
            let row_bytes = image
                .width
                .checked_mul(4)
                .ok_or("qcom-adreno-a618: image row size overflows")?;
            let layout_size = u64::from(image.row_pitch)
                .checked_mul(u64::from(image.height))
                .ok_or("qcom-adreno-a618: image layout size overflows")?;
            if image.width == 0
                || image.height == 0
                || image.row_pitch < row_bytes
                || layout_size != image.visible_size
                || relocation.resource_offset != 0
                || relocation.required_size != image.visible_size
            {
                return Err("qcom-adreno-a618: A2D relocation does not match image layout");
            }
            let expectation = address
                .a2d
                .ok_or("qcom-adreno-a618: A2D relocation lacks semantic bounds")?;
            let accessed_bytes = u64::from(expectation.required_height.saturating_sub(1))
                .checked_mul(u64::from(expectation.row_pitch))
                .and_then(|bytes| {
                    u64::from(expectation.required_width)
                        .checked_mul(4)
                        .and_then(|row| bytes.checked_add(row))
                })
                .ok_or("qcom-adreno-a618: A2D byte range overflows")?;
            if expectation.row_pitch != image.row_pitch
                || expectation.required_width == 0
                || expectation.required_height == 0
                || expectation.required_width > image.width
                || expectation.required_height > image.height
                || accessed_bytes > relocation.required_size
            {
                return Err("qcom-adreno-a618: A2D state exceeds the authorized image layout");
            }
        }
        if let Some(expectation) = address.image {
            let image = resource.linear_image.ok_or(
                "qcom-adreno-a618: 3D image address must cover one complete linear BGRA8 image",
            )?;
            let layout_size = u64::from(image.row_pitch)
                .checked_mul(u64::from(image.height))
                .ok_or("qcom-adreno-a618: 3D image layout size overflows")?;
            if image.width == 0
                || image.height == 0
                || image.row_pitch
                    < image
                        .width
                        .checked_mul(4)
                        .ok_or("qcom-adreno-a618: 3D row size overflows")?
                || layout_size != image.visible_size
                || relocation.resource_offset != 0
                || relocation.required_size != image.visible_size
                || expectation.row_pitch != image.row_pitch
                || expectation.width == 0
                || expectation.height == 0
                || expectation.width > image.width
                || expectation.height > image.height
                || (expectation.exact_extent
                    && (expectation.width != image.width || expectation.height != image.height))
                // SGFX imports an explicit linear BGRA8 layout.  Pinned
                // Freedreno assigns that layout the minimum 64-byte pitch
                // alignment (descriptor encoding zero), independent of any
                // larger power-of-two factor in the actual row pitch.
                || expectation.pitch_align.is_some_and(|align| align != 0)
                || expectation.array_pitch.is_some_and(|pitch| {
                    image
                        .visible_size
                        .checked_add(pitch.alignment - 1)
                        .map(|size| size & !(pitch.alignment - 1))
                        != Some(pitch.bytes)
                })
            {
                return Err(
                    "qcom-adreno-a618: 3D descriptor does not match attached image metadata",
                );
            }
        }
        relocate_one(&mut words, relocation, resource)?;
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use adreno_a6xx_pm4::{Header, Packets, opcode, type4, type7};
    use adreno_a6xx_submit_wire::{
        ACCESS_READ, ACCESS_WRITE, AddressEncoding, Relocation, RelocationSource, Resource, Submit,
        encode, encoded_len,
    };
    use sgfx_codegen_adreno_a6xx::{
        Access, Capabilities, CompileInput, ImageMeta, ImageModifier, ObjectId, ObjectRef,
        Operation, PipelineId, PipelineMeta, PlaneLayout, RelocatablePm4, RenderPass, ResourceKind,
        ResourceMeta, compile,
    };
    use sgfx_core::ir::{
        AddressMode, BlendState, BufferUsage, Color, CullMode, DrawUniforms, Extent2D, FilterMode,
        FragmentProgram, FrontFace, IndexFormat, LoadOp, PixelRect, PrimitiveTopology, RasterState,
        RenderPipelineDesc, SamplerDesc, StoreOp, TextureFormat, TextureSampleMode, TextureUsage,
        Transform, VertexAttribute, VertexBufferLayout, VertexFormat,
    };

    use super::{
        CP_MEMCPY, CP_MEMCPY_MAX_DWORDS, LinearImage, RB_A2D_DEST_BUFFER_BASE, RejectedPacketKind,
        ResolvedResource, diagnose_rejected_packet, relocate_one, validate_and_relocate,
        validate_type7,
    };

    const TARGET: ObjectId = ObjectId::new(0);
    const SOURCE: ObjectId = ObjectId::new(1);
    const BUFFER: ObjectId = ObjectId::new(2);
    const ALPHA: ObjectId = ObjectId::new(3);

    fn token(object: ObjectRef) -> u64 {
        match object {
            ObjectRef::External(id) => u64::from(id.raw()) + 1,
            ObjectRef::Generated(id) => 0x1000 + u64::from(id.raw()),
            ObjectRef::CanonicalShader(_) => panic!("canonical shaders never have tokens"),
        }
    }

    fn access_bits(access: Access) -> u32 {
        u32::from(access.bits())
    }

    fn wire_from_codegen(artifact: &RelocatablePm4) -> std::vec::Vec<u8> {
        let resources: std::vec::Vec<_> = artifact
            .accesses
            .iter()
            .map(|access| Resource {
                attachment_token: token(access.object),
                range_offset: access.offset,
                range_size: access.size,
                access: access_bits(access.access),
            })
            .collect();
        let relocations: std::vec::Vec<_> = artifact
            .fixups
            .iter()
            .map(|fixup| {
                if let ObjectRef::CanonicalShader(variant) = fixup.object {
                    return Relocation {
                        pm4_word_offset: fixup.word_offset,
                        source: RelocationSource::CanonicalShader(variant),
                        resource_offset: 0,
                        required_size: fixup.required_size,
                        access: access_bits(fixup.access),
                        encoding: AddressEncoding::GpuVa64,
                    };
                }
                let (resource_index, access) = artifact
                    .accesses
                    .iter()
                    .enumerate()
                    .find(|(_, access)| {
                        let end = access.offset.checked_add(access.size).unwrap();
                        access.object == fixup.object
                            && access.access.contains(fixup.access)
                            && access.offset <= fixup.object_offset
                            && fixup
                                .object_offset
                                .checked_add(fixup.required_size)
                                .is_some_and(|fixup_end| fixup_end <= end)
                    })
                    .unwrap();
                Relocation {
                    pm4_word_offset: fixup.word_offset,
                    source: RelocationSource::Attachment(resource_index as u32),
                    resource_offset: fixup.object_offset - access.offset,
                    required_size: fixup.required_size,
                    access: access_bits(fixup.access),
                    encoding: match fixup.encoding {
                        sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa64 => {
                            AddressEncoding::GpuVa64
                        }
                        sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49TexDescriptor => {
                            AddressEncoding::GpuVa49TexDescriptor
                        }
                    },
                }
            })
            .collect();
        let submit = Submit {
            pm4: &artifact.words,
            resources: &resources,
            relocations: &relocations,
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        bytes
    }

    fn image(id: ObjectId, usage: TextureUsage) -> ResourceMeta {
        ResourceMeta {
            id,
            size: 0x400,
            kind: ResourceKind::Image(ImageMeta {
                format: TextureFormat::Bgra8Unorm,
                storage_format: TextureFormat::Bgra8Unorm,
                extent: Extent2D::new(16, 16).unwrap(),
                usage,
                modifier: ImageModifier::Linear,
                planes: std::vec![PlaneLayout {
                    offset: 0,
                    stride: 64,
                    size: 0x400,
                }],
            }),
        }
    }

    fn accept_codegen(artifact: &RelocatablePm4) -> Result<std::vec::Vec<u32>, &'static str> {
        let bytes = wire_from_codegen(artifact);
        validate_and_relocate(
            &bytes,
            |attachment_token| {
                let is_image = attachment_token == token(ObjectRef::External(TARGET))
                    || attachment_token == token(ObjectRef::External(SOURCE))
                    || attachment_token == token(ObjectRef::External(ALPHA));
                Some(ResolvedResource {
                    attachment_token,
                    gpu_va: 0x1_0000_0000 + attachment_token * 0x1_0000,
                    allocation_size: 0x1000,
                    allowed_access: ACCESS_READ | ACCESS_WRITE,
                    linear_image: is_image.then_some(LinearImage {
                        width: 16,
                        height: 16,
                        row_pitch: 64,
                        visible_size: 0x400,
                    }),
                })
            },
            |variant| {
                Some(ResolvedResource {
                    attachment_token: 0,
                    gpu_va: 0x4_0000_0000 + variant.offset() as u64,
                    allocation_size: adreno_a6xx_shader_pack::SHADER_SIZE as u64,
                    allowed_access: ACCESS_READ,
                    linear_image: None,
                })
            },
        )
    }

    fn payload(pm4: &[u32], relocation: Relocation) -> std::vec::Vec<u8> {
        let resources = [Resource {
            attachment_token: 0x55,
            range_offset: 0,
            range_size: 0x1000,
            access: ACCESS_WRITE,
        }];
        let relocations = [relocation];
        let submit = Submit {
            pm4,
            resources: &resources,
            relocations: &relocations,
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        bytes
    }

    fn validate_no_shaders(
        bytes: &[u8],
        resolve: impl FnMut(u64) -> Option<ResolvedResource>,
    ) -> Result<std::vec::Vec<u32>, &'static str> {
        validate_and_relocate(bytes, resolve, |_| None)
    }

    #[test]
    fn incomplete_a2d_state_cannot_inherit_registers_from_an_older_submit() {
        let pm4 = [type4(RB_A2D_DEST_BUFFER_BASE, 2).unwrap(), 0, 0];
        let bytes = payload(
            &pm4,
            Relocation {
                pm4_word_offset: 1,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 0x1000,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        );
        assert!(
            validate_no_shaders(&bytes, |token| Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000,
                allocation_size: 0x1000,
                allowed_access: ACCESS_WRITE,
                linear_image: Some(LinearImage {
                    width: 16,
                    height: 16,
                    row_pitch: 64,
                    visible_size: 0x400,
                }),
            }))
            .is_err()
        );
    }

    #[test]
    fn nested_indirect_buffer_is_rejected() {
        let pm4 = [type7(0x3f, 3).unwrap(), 0, 0, 1];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert!(validate_no_shaders(&bytes, |_| None).is_err());
    }

    #[test]
    fn arbitrary_register_write_is_rejected() {
        let pm4 = [type4(0x800, 1).unwrap(), 1];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert!(validate_no_shaders(&bytes, |_| None).is_err());
        let rejection = diagnose_rejected_packet(&bytes).unwrap();
        assert_eq!(rejection.kind, RejectedPacketKind::Type4);
        assert_eq!(rejection.word_offset, 0);
        assert_eq!(rejection.selector, 0x800);
        assert_eq!(rejection.payload_len, 1);
        assert_eq!(rejection.first_value, Some(1));
    }

    #[test]
    fn stale_userspace_sp_update_value_is_identified_without_allowing_it() {
        let pm4 = [type4(super::SP_UPDATE_CNTL, 1).unwrap(), 0x000f_ffff];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();

        assert_eq!(
            validate_no_shaders(&bytes, |_| None),
            Err("qcom-adreno-a618: unsafe PM4 register value")
        );
        let rejection = diagnose_rejected_packet(&bytes).unwrap();
        assert_eq!(rejection.kind, RejectedPacketKind::Type4);
        assert_eq!(rejection.selector, super::SP_UPDATE_CNTL);
        assert_eq!(rejection.first_value, Some(0x000f_ffff));
    }

    #[test]
    fn event_packet_cannot_smuggle_a_fence_address() {
        let pm4 = [type7(opcode::EVENT_WRITE, 3).unwrap(), 0x1d, 0, 0];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert!(validate_no_shaders(&bytes, |_| None).is_err());
    }

    #[test]
    fn addressed_ccu_flush_without_an_address_is_rejected() {
        // A6xx CCU_FLUSH_COLOR_TS is a four-dword CP_EVENT_WRITE operation.
        // The old userspace emitter sent only this selector, which is framed
        // PM4 but illegal SQE input.
        let pm4 = [type7(opcode::EVENT_WRITE, 1).unwrap(), 0x1d];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert!(validate_no_shaders(&bytes, |_| None).is_err());
    }

    #[test]
    fn userspace_cannot_duplicate_the_kernel_cache_fence() {
        let pm4 = [type7(opcode::EVENT_WRITE, 4).unwrap(), 0x4000_0004, 0, 0, 1];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert_eq!(
            validate_no_shaders(&bytes, |_| None),
            Err("qcom-adreno-a618: PM4 opcode is not allowlisted")
        );
    }

    #[test]
    fn color_invalidate_without_an_addressed_clean_is_rejected() {
        let pm4 = [
            type7(opcode::EVENT_WRITE, 1).unwrap(),
            super::EVENT_CCU_INVALIDATE_COLOR,
        ];
        let submit = Submit {
            pm4: &pm4,
            resources: &[],
            relocations: &[],
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert_eq!(
            validate_no_shaders(&bytes, |_| None),
            Err("qcom-adreno-a618: invalid CCU clean/invalidate ordering")
        );
    }

    #[test]
    fn addressed_ccu_clean_sequence_must_be_contiguous() {
        let pm4 = [
            type7(opcode::EVENT_WRITE, 4).unwrap(),
            super::EVENT_CCU_FLUSH_COLOR_TS,
            0,
            0,
            1,
            type7(opcode::EVENT_WRITE, 1).unwrap(),
            super::EVENT_CACHE_INVALIDATE,
            type7(opcode::EVENT_WRITE, 1).unwrap(),
            super::EVENT_CCU_INVALIDATE_COLOR,
        ];
        let bytes = payload(
            &pm4,
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 4,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        );
        assert_eq!(
            validate_no_shaders(&bytes, |token| Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000,
                allocation_size: 0x1000,
                allowed_access: ACCESS_WRITE,
                linear_image: None,
            })),
            Err("qcom-adreno-a618: CCU clean/invalidate sequence is not contiguous")
        );
    }

    #[test]
    fn memcpy_requires_two_authorized_kernel_relocations() {
        let pm4 = [
            type7(0x75, 5).unwrap(),
            4,
            0,
            0,
            0,
            0,
            type7(opcode::WAIT_MEM_WRITES, 0).unwrap(),
        ];
        let resources = [
            Resource {
                attachment_token: 1,
                range_offset: 0,
                range_size: 16,
                access: ACCESS_READ,
            },
            Resource {
                attachment_token: 2,
                range_offset: 32,
                range_size: 16,
                access: ACCESS_WRITE,
            },
        ];
        let relocations = [
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 16,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
            Relocation {
                pm4_word_offset: 4,
                source: RelocationSource::Attachment(1),
                resource_offset: 0,
                required_size: 16,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        ];
        let submit = Submit {
            pm4: &pm4,
            resources: &resources,
            relocations: &relocations,
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        let words = validate_no_shaders(&bytes, |token| match token {
            1 => Some(ResolvedResource {
                attachment_token: 1,
                gpu_va: 0x2_0000_0000,
                allocation_size: 0x1000,
                allowed_access: ACCESS_READ,
                linear_image: None,
            }),
            2 => Some(ResolvedResource {
                attachment_token: 2,
                gpu_va: 0x3_0000_0000,
                allocation_size: 0x1000,
                allowed_access: ACCESS_WRITE,
                linear_image: None,
            }),
            _ => None,
        })
        .unwrap();
        assert_eq!(&words[2..4], &[0, 2]);
        assert_eq!(&words[4..6], &[32, 3]);
    }

    #[test]
    fn memcpy_size_is_bounded_to_the_production_chunk() {
        let mut addresses = std::vec::Vec::new();
        assert!(
            validate_type7(
                CP_MEMCPY,
                &[CP_MEMCPY_MAX_DWORDS, 0, 0, 0, 0],
                0,
                &mut addresses,
            )
            .is_ok()
        );
        assert_eq!(addresses.len(), 2);

        addresses.clear();
        assert_eq!(
            validate_type7(
                CP_MEMCPY,
                &[CP_MEMCPY_MAX_DWORDS + 1, 0, 0, 0, 0],
                0,
                &mut addresses,
            ),
            Err("qcom-adreno-a618: PM4 opcode is not allowlisted")
        );
        assert!(addresses.is_empty());
    }

    #[test]
    fn codegen_memcpy_without_wait_mem_writes_is_rejected() {
        let buffers = [ResourceMeta {
            id: BUFFER,
            size: 64,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::COPY_DST,
            },
        }];
        let data = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let operations = [Operation::WriteBuffer {
            destination: BUFFER,
            offset: 16,
            data: &data,
        }];
        let mut artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &buffers,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap();
        let wait_word = artifact
            .words
            .iter()
            .position(|word| {
                adreno_a6xx_pm4::decode_header(*word)
                    == Ok(Header::Type7 {
                        opcode: opcode::WAIT_MEM_WRITES,
                        count: 0,
                    })
            })
            .unwrap();
        artifact.words.remove(wait_word);

        assert!(accept_codegen(&artifact).is_err());
    }

    #[test]
    fn forged_memcpy_count_with_tiny_relocations_is_rejected() {
        let pm4 = [type7(0x75, 5).unwrap(), 0x1000, 0, 0, 0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 0x10_000,
            access: ACCESS_READ | ACCESS_WRITE,
        }];
        let relocations = [
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 4,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
            Relocation {
                pm4_word_offset: 4,
                source: RelocationSource::Attachment(0),
                resource_offset: 0x8000,
                required_size: 4,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        ];
        let submit = Submit {
            pm4: &pm4,
            resources: &resources,
            relocations: &relocations,
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut bytes).unwrap();
        assert!(
            validate_no_shaders(&bytes, |token| Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000,
                allocation_size: 0x10_000,
                allowed_access: ACCESS_READ | ACCESS_WRITE,
                linear_image: None,
            }))
            .is_err()
        );
    }

    #[test]
    fn relocation_access_must_be_contained_by_wire_resource_access() {
        let pm4 = [type7(0x75, 5).unwrap(), 1, 0, 0, 0, 0];
        let resources = [Resource {
            attachment_token: 1,
            range_offset: 0,
            range_size: 8,
            access: ACCESS_READ,
        }];
        let relocations = [
            Relocation {
                pm4_word_offset: 2,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 4,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa64,
            },
            Relocation {
                pm4_word_offset: 4,
                source: RelocationSource::Attachment(0),
                resource_offset: 4,
                required_size: 4,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        ];
        let submit = Submit {
            pm4: &pm4,
            resources: &resources,
            relocations: &relocations,
        };
        let mut bytes = std::vec![0; encoded_len(submit).unwrap()];
        // The shared canonical encoder rejects this before it can become a
        // wire blob; validate_and_relocate independently repeats the same
        // containment check for hostile decoders/ABI revisions.
        assert!(encode(submit, &mut bytes).is_err());
    }

    #[test]
    fn reachable_codegen_clear_copy_and_upload_are_accepted_end_to_end() {
        let images = [
            image(
                TARGET,
                TextureUsage::RENDER_ATTACHMENT | TextureUsage::COPY_DST | TextureUsage::COPY_SRC,
            ),
            image(SOURCE, TextureUsage::COPY_SRC),
        ];
        let clear_operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.25, 0.5, 0.75, 1.0).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::EndRenderPass,
        ];
        let clear = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &images,
            pipelines: &[],
            operations: &clear_operations,
        })
        .unwrap();
        assert!(accept_codegen(&clear).is_ok());

        let copy_operations = [Operation::CopyTextureToTexture {
            source: SOURCE,
            source_rect: PixelRect::new(1, 2, 8, 6).unwrap(),
            destination: TARGET,
            destination_rect: PixelRect::new(3, 4, 8, 6).unwrap(),
        }];
        let copy = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &images,
            pipelines: &[],
            operations: &copy_operations,
        })
        .unwrap();
        assert!(accept_codegen(&copy).is_ok());

        let buffers = [ResourceMeta {
            id: BUFFER,
            size: 64,
            kind: ResourceKind::Buffer {
                usage: BufferUsage::COPY_DST,
            },
        }];
        let data = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let upload_operations = [Operation::WriteBuffer {
            destination: BUFFER,
            offset: 16,
            data: &data,
        }];
        let upload = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &buffers,
            pipelines: &[],
            operations: &upload_operations,
        })
        .unwrap();
        assert!(accept_codegen(&upload).is_ok());
    }

    #[test]
    fn reachable_canonical_vertex_color_draw_is_accepted_and_link_mutation_rejected() {
        const PIPELINE: PipelineId = PipelineId::new(7);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            ResourceMeta {
                id: BUFFER,
                size: 120,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
                },
            },
        ];
        let pipelines = [PipelineMeta {
            id: PIPELINE,
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    40,
                    std::vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                    ],
                )
                .unwrap(),
                FragmentProgram::VertexColor,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::Back, FrontFace::CounterClockwise),
            )
            .unwrap(),
        }];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(PIPELINE),
            Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();
        assert!(accept_codegen(&artifact).is_ok());

        // Keep the crucial A618 sysmem retirement order pinned independently
        // from relocation validation.  The old stream was valid packetized
        // PM4 but parked WFI in front of the CCU clean and never wrote the
        // trusted fence.
        let (retirement_start, retirement_wfi) = {
            let packets = Packets::new(&artifact.words)
                .collect::<Result<std::vec::Vec<_>, _>>()
                .unwrap();
            let draw = packets
                .iter()
                .position(|packet| {
                    matches!(
                        packet.header,
                        Header::Type7 {
                            opcode: super::CP_DRAW_INDX_OFFSET,
                            ..
                        }
                    )
                })
                .unwrap();
            (
                packets[draw + 1].word_offset as usize,
                packets[draw + 5].word_offset as usize,
            )
        };
        let mut idle_before_retirement = artifact.words.clone();
        let clean_and_invalidate =
            idle_before_retirement[retirement_start..retirement_wfi].to_vec();
        let wfi = idle_before_retirement[retirement_wfi];
        idle_before_retirement.splice(
            retirement_start..retirement_wfi + 1,
            core::iter::once(wfi).chain(clean_and_invalidate),
        );
        assert_eq!(
            super::validate_render_retirement(&idle_before_retirement),
            Err("qcom-adreno-a618: rendering operation lacks sysmem retirement")
        );

        // SWS writes its reused quad buffer and consumes it as vertex data in
        // one submit.  Keep the exact CP-write -> cache-invalidate -> draw
        // transition accepted as a unit, not merely as isolated operations.
        let upload_bytes = [0_u8; 120];
        let mut upload_then_draw = operations.to_vec();
        upload_then_draw.insert(
            0,
            Operation::WriteBuffer {
                destination: BUFFER,
                offset: 0,
                data: &upload_bytes,
            },
        );
        let upload_draw_artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &upload_then_draw,
        })
        .unwrap();
        assert!(accept_codegen(&upload_draw_artifact).is_ok());
        let upload_draw_packets = Packets::new(&upload_draw_artifact.words)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap();
        let packet_index = |wanted_opcode: u8, wanted_event: Option<u32>| {
            upload_draw_packets
                .iter()
                .position(|packet| {
                    matches!(
                        packet.header,
                        Header::Type7 { opcode, .. } if opcode == wanted_opcode
                    ) && wanted_event.is_none_or(|event| packet.payload.first() == Some(&event))
                })
                .unwrap()
        };
        let memcpy = packet_index(super::CP_MEMCPY, None);
        let wait = packet_index(opcode::WAIT_MEM_WRITES, None);
        let invalidate = packet_index(opcode::EVENT_WRITE, Some(super::EVENT_CACHE_INVALIDATE));
        let draw = packet_index(super::CP_DRAW_INDX_OFFSET, None);
        assert_eq!(wait, memcpy + 1);
        assert!(wait < invalidate && invalidate < draw);

        // A real frame normally clears the target before its first 3D draw.
        // The A2D marker from that clear must not make the following canonical
        // direct-render segment ambiguous.
        let mut clear_then_draw = operations.clone();
        let Operation::BeginRenderPass(pass) = &mut clear_then_draw[0] else {
            unreachable!()
        };
        pass.load = LoadOp::Clear(Color::rgba(0.1, 0.2, 0.3, 1.0).unwrap());
        let clear_artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &clear_then_draw,
        })
        .unwrap();
        assert!(accept_codegen(&clear_artifact).is_ok());

        let mut clear_then_two_draws = clear_then_draw.to_vec();
        clear_then_two_draws.insert(
            clear_then_two_draws.len() - 1,
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
        );
        let two_draw_artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &clear_then_two_draws,
        })
        .unwrap();
        assert!(accept_codegen(&two_draw_artifact).is_ok());

        let mut wrong_final_marker = clear_artifact.clone();
        let draw_word = Packets::new(&wrong_final_marker.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: super::CP_DRAW_INDX_OFFSET,
                        ..
                    }
                )
            })
            .unwrap()
            .word_offset as usize;
        wrong_final_marker.words.splice(
            draw_word..draw_word,
            [type7(opcode::SET_MARKER, 1).unwrap(), 12],
        );
        for fixup in &mut wrong_final_marker.fixups {
            if fixup.word_offset as usize >= draw_word {
                fixup.word_offset += 2;
            }
        }
        assert!(accept_codegen(&wrong_final_marker).is_err());

        let mut hostile = artifact.clone();
        let linkage = hostile
            .words
            .iter_mut()
            .find(|word| **word == 0x00ff_0408)
            .unwrap();
        *linkage ^= 1;
        assert!(accept_codegen(&hostile).is_err());

        let mut duplicate = artifact.clone();
        let insertion = duplicate
            .words
            .iter()
            .position(|word| *word == 0x00ff_0408)
            .unwrap()
            - 1;
        duplicate.words.splice(
            insertion..insertion,
            [type4(super::VPC_VS_CNTL, 1).unwrap(), 0x00ff_0408],
        );
        for fixup in &mut duplicate.fixups {
            if fixup.word_offset as usize >= insertion {
                fixup.word_offset += 2;
            }
        }
        assert!(accept_codegen(&duplicate).is_err());

        let mut bad_pitch = artifact.clone();
        let pitch = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: super::RB_MRT_PITCH,
                        ..
                    }
                )
            })
            .unwrap();
        bad_pitch.words[pitch.word_offset as usize + 1] = u32::MAX;
        assert!(accept_codegen(&bad_pitch).is_err());

        let mut upside_down_viewport = artifact.clone();
        let viewport = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: super::GRAS_CL_VIEWPORT_XOFFSET,
                        ..
                    }
                )
            })
            .unwrap();
        upside_down_viewport.words[viewport.word_offset as usize + 4] ^= 1 << 31;
        assert_eq!(
            accept_codegen(&upside_down_viewport),
            Err("qcom-adreno-a618: unsafe 3D viewport transform")
        );

        let mut bad_vfd_count = artifact.clone();
        let vfd = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: super::VFD_CNTL_0,
                        ..
                    }
                )
            })
            .unwrap();
        bad_vfd_count.words[vfd.word_offset as usize + 1] = 0x0303;
        assert!(accept_codegen(&bad_vfd_count).is_err());
    }

    #[test]
    fn partial_render_area_keeps_a_full_attachment_viewport_at_the_kernel_boundary() {
        const PIPELINE: PipelineId = PipelineId::new(8);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            ResourceMeta {
                id: BUFFER,
                size: 120,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
        ];
        let pipelines = [PipelineMeta {
            id: PIPELINE,
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    40,
                    std::vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                    ],
                )
                .unwrap(),
                FragmentProgram::VertexColor,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::Back, FrontFace::CounterClockwise),
            )
            .unwrap(),
        }];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(4, 5, 6, 7).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(PIPELINE),
            Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::SetScissor(None),
            Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();

        assert!(accept_codegen(&artifact).is_ok());
        let packets = Packets::new(&artifact.words)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap();
        let register = |wanted| {
            packets.iter().find_map(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register,
                        ..
                    } if register == wanted
                )
                .then_some(packet.payload)
            })
        };
        assert_eq!(
            register(super::GRAS_SC_SCREEN_SCISSOR_TL),
            Some(&[0x0005_0004, 0x000b_0009][..])
        );
        assert_eq!(
            register(super::GRAS_SC_VIEWPORT_SCISSOR_TL),
            Some(&[0, 0x000f_000f][..])
        );
    }

    #[test]
    fn indexed_draw_reaches_the_kernel_with_exact_buffer_bounds() {
        const PIPELINE: PipelineId = PipelineId::new(17);
        const INDEX: ObjectId = ObjectId::new(5);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            ResourceMeta {
                id: BUFFER,
                size: 128,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
            ResourceMeta {
                id: INDEX,
                size: 16,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::INDEX,
                },
            },
        ];
        let pipelines = [PipelineMeta {
            id: PIPELINE,
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    32,
                    std::vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                    ],
                )
                .unwrap(),
                FragmentProgram::VertexColor,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        }];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(PIPELINE),
            Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            },
            Operation::SetIndexBuffer {
                buffer: INDEX,
                offset: 0,
                format: IndexFormat::Uint16,
            },
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::DrawIndexed {
                index_count: 6,
                first_index: 2,
                base_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();
        assert!(accept_codegen(&artifact).is_ok());

        let draw = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: super::CP_DRAW_INDX_OFFSET,
                        count: 7,
                    }
                )
            })
            .unwrap();
        assert_eq!(draw.payload, [0x404, 1, 6, 2, 0, 0, 8]);
        let draw_word = draw.word_offset as usize;

        let mut short_max = artifact.clone();
        short_max.words[draw_word + 7] = 7;
        assert!(accept_codegen(&short_max).is_err());

        let mut short_relocation = artifact.clone();
        short_relocation
            .fixups
            .iter_mut()
            .find(|fixup| fixup.word_offset == draw.word_offset + 5)
            .unwrap()
            .required_size -= 2;
        assert!(accept_codegen(&short_relocation).is_err());

        let index_offset_packet = Packets::new(&artifact.words)
            .filter_map(Result::ok)
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: super::VFD_INDEX_OFFSET,
                        ..
                    }
                )
            })
            .unwrap();
        let mut negative_base = artifact.clone();
        negative_base.words[index_offset_packet.word_offset as usize + 1] = u32::MAX;
        assert!(accept_codegen(&negative_base).is_err());
    }

    #[test]
    fn sws_first_cursor_frame_leaves_the_general_cache_fence_to_the_kernel() {
        const PIPELINE: PipelineId = PipelineId::new(11);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            image(SOURCE, TextureUsage::SAMPLED),
            ResourceMeta {
                id: BUFFER,
                size: 144,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
                },
            },
        ];
        let pipeline = PipelineMeta {
            id: PIPELINE,
            descriptor: RenderPipelineDesc::new(
                TextureFormat::Bgra8Unorm,
                PrimitiveTopology::TriangleList,
                VertexBufferLayout::new(
                    24,
                    std::vec![
                        VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                        VertexAttribute::new(1, VertexFormat::Float32x2, 16),
                    ],
                )
                .unwrap(),
                FragmentProgram::Texture(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        };
        let vertices = [0_u8; 144];
        let operations = [
            Operation::WriteBuffer {
                destination: BUFFER,
                offset: 0,
                data: &vertices,
            },
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.05, 0.1, 0.2, 1.0).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            },
            Operation::SetPipeline(PIPELINE),
            Operation::SetTexture(SOURCE),
            Operation::SetSampler(SamplerDesc::new(
                FilterMode::Linear,
                FilterMode::Linear,
                AddressMode::ClampToEdge,
                AddressMode::ClampToEdge,
            )),
            Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )),
            Operation::SetScissor(None),
            Operation::Draw {
                vertex_count: 6,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: core::slice::from_ref(&pipeline),
            operations: &operations,
        })
        .unwrap();

        // This is the exact command shape reported by the first SWS cursor
        // frame on CoachZ: quad upload, target clear, one sampled draw, and
        // CCU retirement. The trusted kernel ring appends the only general
        // cache-clean event. Atomic VS/FS program-layout bursts keep the
        // validated userspace IB at 533 dwords, including the indirect sampler
        // and texture-descriptor state backing required by A618.
        assert_eq!(artifact.words.len(), 533);
        assert!(accept_codegen(&artifact).is_ok());
    }

    #[test]
    fn full_ui_and_compat_pipeline_matrix_reaches_kernel_validator() {
        let mut alpha = image(ALPHA, TextureUsage::SAMPLED);
        let ResourceKind::Image(alpha_meta) = &mut alpha.kind else {
            unreachable!()
        };
        alpha_meta.format = TextureFormat::R8Unorm;
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            image(SOURCE, TextureUsage::SAMPLED),
            ResourceMeta {
                id: BUFFER,
                size: 120,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
            alpha,
        ];
        let layout = |stride, attributes| VertexBufferLayout::new(stride, attributes).unwrap();
        let pos2 = layout(
            16,
            std::vec![VertexAttribute::new(0, VertexFormat::Float32x2, 0)],
        );
        let pos2_uv = layout(
            16,
            std::vec![
                VertexAttribute::new(0, VertexFormat::Float32x2, 0),
                VertexAttribute::new(1, VertexFormat::Float32x2, 8),
            ],
        );
        let pos4 = |stride| {
            layout(
                stride,
                std::vec![VertexAttribute::new(0, VertexFormat::Float32x4, 0)],
            )
        };
        let pos4_uv = layout(
            24,
            std::vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x2, 16),
            ],
        );
        let color3 = layout(
            28,
            std::vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x3, 16),
            ],
        );
        let color4 = layout(
            40,
            std::vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x4, 16),
            ],
        );
        let color4_uv = layout(
            40,
            std::vec![
                VertexAttribute::new(0, VertexFormat::Float32x4, 0),
                VertexAttribute::new(1, VertexFormat::Float32x4, 16),
                VertexAttribute::new(2, VertexFormat::Float32x2, 32),
            ],
        );
        let cases = std::vec![
            (
                pos2,
                FragmentProgram::Solid,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                pos2_uv.clone(),
                FragmentProgram::Texture(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                pos2_uv,
                FragmentProgram::Texture(TextureSampleMode::AlphaMask),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                pos4(40),
                FragmentProgram::Solid,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::Back
            ),
            (
                color4,
                FragmentProgram::VertexColor,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::Back
            ),
            (
                color4_uv,
                FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::Back
            ),
            (
                pos4(24),
                FragmentProgram::Solid,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                pos4_uv.clone(),
                FragmentProgram::Texture(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                pos4_uv,
                FragmentProgram::Texture(TextureSampleMode::RgbIgnoreAlpha),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                color3,
                FragmentProgram::VertexColor,
                BlendState::REPLACE,
                CullMode::Front
            ),
        ];
        for (index, (vertex_layout, fragment, blend, cull)) in cases.into_iter().enumerate() {
            let pipeline_id = PipelineId::new(index as u32);
            let textured = matches!(
                fragment,
                FragmentProgram::Texture(_) | FragmentProgram::TextureVertexColor(_)
            );
            let stride = vertex_layout.stride();
            let pipeline = PipelineMeta {
                id: pipeline_id,
                descriptor: RenderPipelineDesc::new(
                    TextureFormat::Bgra8Unorm,
                    PrimitiveTopology::TriangleList,
                    vertex_layout,
                    fragment,
                    blend,
                    RasterState::new(
                        cull,
                        if stride == 28 {
                            FrontFace::Clockwise
                        } else {
                            FrontFace::CounterClockwise
                        },
                    ),
                )
                .unwrap(),
            };
            let mut operations = std::vec![
                Operation::BeginRenderPass(RenderPass {
                    target: TARGET,
                    area: PixelRect::new(0, 0, 16, 16).unwrap(),
                    load: LoadOp::Clear(Color::rgba(0.05, 0.1, 0.2, 1.0).unwrap()),
                    store: StoreOp::Store,
                    depth: None,
                }),
                Operation::SetPipeline(pipeline_id),
                Operation::SetVertexBuffer {
                    buffer: BUFFER,
                    offset: 0
                },
            ];
            if textured {
                operations.push(Operation::SetTexture(
                    if matches!(
                        fragment,
                        FragmentProgram::Texture(TextureSampleMode::AlphaMask)
                    ) {
                        ALPHA
                    } else {
                        SOURCE
                    },
                ));
                // Sampler filtering is dynamic SGFX state rather than part of
                // the shader/vertex pipeline identity. Exercise linear mode
                // for every sampled layout, including the stride-16 UI path.
                let filter = if matches!(index, 1 | 2 | 5 | 8) {
                    FilterMode::Linear
                } else {
                    FilterMode::Nearest
                };
                operations.push(Operation::SetSampler(SamplerDesc::new(
                    filter,
                    filter,
                    AddressMode::ClampToEdge,
                    AddressMode::ClampToEdge,
                )));
            }
            operations.push(Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(1.0, 1.0, 1.0, 1.0).unwrap(),
            )));
            operations.push(Operation::Draw {
                vertex_count: 3,
                first_vertex: 0,
            });
            operations.push(Operation::EndRenderPass);
            let artifact = compile(CompileInput {
                capabilities: Capabilities::a618(512 * 1024, 4096),
                resources: &resources,
                pipelines: core::slice::from_ref(&pipeline),
                operations: &operations,
            })
            .unwrap_or_else(|error| panic!("pipeline case {index} failed compile: {error:?}"));
            assert!(
                accept_codegen(&artifact).is_ok(),
                "pipeline case {index} rejected"
            );
            if index == 1 {
                let sampler_load = Packets::new(&artifact.words)
                    .filter_map(Result::ok)
                    .find(|packet| {
                        matches!(
                            packet.header,
                            Header::Type7 {
                                opcode: super::CP_LOAD_STATE6_FRAG,
                                ..
                            }
                        ) && packet.payload.len() == 3
                            && packet.payload[0] == ((2 << 16) | (4 << 18) | (1 << 22))
                    })
                    .unwrap();
                let begin = sampler_load.word_offset as usize;
                let end = begin + 1 + sampler_load.payload.len();
                let duplicate_packet = artifact.words[begin..end].to_vec();
                let mut hostile = artifact.clone();
                hostile.words.splice(end..end, duplicate_packet);
                let mut duplicate_fixup = hostile
                    .fixups
                    .iter()
                    .find(|fixup| fixup.word_offset == sampler_load.word_offset + 2)
                    .cloned()
                    .unwrap();
                for fixup in &mut hostile.fixups {
                    if fixup.word_offset as usize >= end {
                        fixup.word_offset += 4;
                    }
                }
                duplicate_fixup.word_offset = u32::try_from(end + 2).unwrap();
                hostile.fixups.push(duplicate_fixup);
                hostile.fixups.sort_by_key(|fixup| fixup.word_offset);
                assert!(accept_codegen(&hostile).is_err());

                // The former emitter padded one four-dword ST6_SHADER sampler
                // unit to sixteen dwords. It is packet-framed PM4, but not a
                // legal CP_LOAD_STATE6 payload for a sampler and must never be
                // blessed by the kernel validator again.
                let mut padded_sampler = artifact.clone();
                let mut old_packet = std::vec![
                    type7(super::CP_LOAD_STATE6_FRAG, 19).unwrap(),
                    (4 << 18) | (1 << 22),
                    0,
                    0,
                    0x92a,
                ];
                old_packet.extend_from_slice(&[0; 15]);
                padded_sampler.words.splice(begin..end, old_packet);
                padded_sampler
                    .fixups
                    .retain(|fixup| !(begin..end).contains(&(fixup.word_offset as usize)));
                for fixup in &mut padded_sampler.fixups {
                    if fixup.word_offset as usize >= end {
                        fixup.word_offset += 16;
                    }
                }
                assert!(accept_codegen(&padded_sampler).is_err());

                let descriptor = Packets::new(&artifact.words)
                    .filter_map(Result::ok)
                    .find(|packet| {
                        matches!(
                            packet.header,
                            Header::Type7 {
                                opcode: super::CP_MEM_WRITE,
                                ..
                            }
                        ) && packet.payload.len() == 22
                            && packet.payload[2] == 0x4c00_6880
                    })
                    .unwrap();
                let mut missing_mip_disable = artifact.clone();
                missing_mip_disable.words[descriptor.word_offset as usize + 20] = 0;
                assert!(accept_codegen(&missing_mip_disable).is_err());

                let mut bad_extent = artifact.clone();
                bad_extent.words[descriptor.word_offset as usize + 4] += 1;
                assert!(accept_codegen(&bad_extent).is_err());

                let mut bad_type = artifact.clone();
                bad_type.words[descriptor.word_offset as usize + 5] |= 2 << 29;
                assert!(accept_codegen(&bad_type).is_err());

                let mut bad_pitch_align = artifact.clone();
                bad_pitch_align.words[descriptor.word_offset as usize + 5] |= 1;
                assert!(accept_codegen(&bad_pitch_align).is_err());

                let mut bad_depth = artifact.clone();
                bad_depth.words[descriptor.word_offset as usize + 8] = 2 << 17;
                assert!(accept_codegen(&bad_depth).is_err());

                let mut bad_range = artifact.clone();
                let texture_fixup = bad_range
                    .fixups
                    .iter_mut()
                    .find(|fixup| {
                        fixup.encoding
                            == sgfx_codegen_adreno_a6xx::AddressEncoding::GpuVa49TexDescriptor
                    })
                    .unwrap();
                texture_fixup.required_size -= 64;
                assert!(accept_codegen(&bad_range).is_err());

                let packet_for = |register| {
                    Packets::new(&artifact.words)
                        .filter_map(Result::ok)
                        .find(|packet| {
                            matches!(
                                packet.header,
                                Header::Type4 {
                                    register: actual,
                                    ..
                                } if actual == register
                            )
                        })
                        .unwrap()
                };

                // These were all accepted or emitted by earlier bring-up
                // revisions.  Keep the kernel boundary pinned to Mesa's A618
                // encodings so a userspace regression cannot revive them.
                let mut stream_out_disabled = artifact.clone();
                let stream_out = packet_for(super::VPC_SO_OVERRIDE);
                stream_out_disabled.words[stream_out.word_offset as usize + 1] = 1;
                assert!(accept_codegen(&stream_out_disabled).is_err());

                let packet7_for = |wanted| {
                    Packets::new(&artifact.words)
                        .filter_map(Result::ok)
                        .find(|packet| {
                            matches!(
                                packet.header,
                                Header::Type7 { opcode, .. } if opcode == wanted
                            )
                        })
                        .unwrap()
                };
                let mut global_ib2_skip = artifact.clone();
                let global = packet7_for(super::CP_SKIP_IB2_ENABLE_GLOBAL);
                global_ib2_skip.words[global.word_offset as usize + 1] = 1;
                assert!(accept_codegen(&global_ib2_skip).is_err());

                let mut local_ib2_skip_disabled = artifact.clone();
                let local = packet7_for(super::CP_SKIP_IB2_ENABLE_LOCAL);
                local_ib2_skip_disabled.words[local.word_offset as usize + 1] = 0;
                assert!(accept_codegen(&local_ib2_skip_disabled).is_err());

                let mut old_ps_config = artifact.clone();
                let ps_config = packet_for(super::SP_PS_CONFIG);
                old_ps_config.words[ps_config.word_offset as usize + 1] = 0x0001_0101;
                assert!(accept_codegen(&old_ps_config).is_err());

                let mut missing_constlen = artifact.clone();
                let ps_const = packet_for(super::SP_PS_CONST_CONFIG);
                missing_constlen.words[ps_const.word_offset as usize + 1] = 0;
                assert!(accept_codegen(&missing_constlen).is_err());

                let preload = Packets::new(&artifact.words)
                    .filter_map(Result::ok)
                    .find(|packet| {
                        matches!(
                            packet.header,
                            Header::Type7 {
                                opcode: super::CP_LOAD_STATE6_FRAG,
                                ..
                            }
                        ) && packet.payload.len() == 3
                            && (packet.payload[0] >> 16) & 3 == 2
                            && (packet.payload[0] >> 18) & 0xf == 12
                    })
                    .unwrap();
                let mut direct_shader_load = artifact.clone();
                direct_shader_load.words[preload.word_offset as usize + 1] &= !(3 << 16);
                assert!(accept_codegen(&direct_shader_load).is_err());

                let mut flat_interpolation = artifact.clone();
                let interpolation = packet_for(super::VPC_VARYING_INTERP_MODE);
                flat_interpolation.words[interpolation.word_offset as usize + 1] = 1;
                assert!(accept_codegen(&flat_interpolation).is_err());
            }
        }
    }

    #[test]
    fn forged_a2d_pitch_and_coordinates_are_rejected() {
        let images = [image(TARGET, TextureUsage::RENDER_ATTACHMENT)];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::EndRenderPass,
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &images,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap();

        let mut huge_pitch = artifact.clone();
        let pitch_header = type4(super::RB_A2D_DEST_BUFFER_PITCH, 1).unwrap();
        let pitch = huge_pitch
            .words
            .iter()
            .position(|word| *word == pitch_header)
            .unwrap();
        huge_pitch.words[pitch + 1] = 0x3fff;
        assert!(accept_codegen(&huge_pitch).is_err());

        let mut huge_coords = artifact;
        let destination_header = type4(super::GRAS_A2D_DEST_TL, 2).unwrap();
        let destination = huge_coords
            .words
            .iter()
            .position(|word| *word == destination_header)
            .unwrap();
        huge_coords.words[destination + 2] = 0x7fff_7fff;
        assert!(accept_codegen(&huge_coords).is_err());

        let mut stale_state_reuse = clear_artifact();
        stale_state_reuse
            .words
            .push(type7(opcode::BLIT, 1).unwrap());
        stale_state_reuse.words.push(3);
        assert!(accept_codegen(&stale_state_reuse).is_err());
    }

    fn clear_artifact() -> RelocatablePm4 {
        let images = [image(TARGET, TextureUsage::RENDER_ATTACHMENT)];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.0, 0.0, 0.0, 1.0).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::EndRenderPass,
        ];
        compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &images,
            pipelines: &[],
            operations: &operations,
        })
        .unwrap()
    }

    #[test]
    fn resource_range_cannot_exceed_attached_allocation() {
        let pm4 = [type4(RB_A2D_DEST_BUFFER_BASE, 2).unwrap(), 0, 0];
        let bytes = payload(
            &pm4,
            Relocation {
                pm4_word_offset: 1,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 4,
                access: ACCESS_WRITE,
                encoding: AddressEncoding::GpuVa64,
            },
        );
        assert!(
            validate_no_shaders(&bytes, |token| Some(ResolvedResource {
                attachment_token: token,
                gpu_va: 0x1_0000_0000,
                allocation_size: 0x200,
                allowed_access: ACCESS_WRITE,
                linear_image: None,
            }))
            .is_err()
        );
    }

    #[test]
    fn texture_descriptor_relocation_preserves_depth_bits() {
        let mut words = [0, 1 << 17];
        let resource = ResolvedResource {
            attachment_token: 1,
            gpu_va: 0x1234_5678_9a0,
            allocation_size: 0x1000,
            allowed_access: ACCESS_READ,
            linear_image: None,
        };
        relocate_one(
            &mut words,
            Relocation {
                pm4_word_offset: 0,
                source: RelocationSource::Attachment(0),
                resource_offset: 0,
                required_size: 64,
                access: ACCESS_READ,
                encoding: AddressEncoding::GpuVa49TexDescriptor,
            },
            resource,
        )
        .unwrap();
        assert_eq!(words[0], resource.gpu_va as u32);
        assert_eq!(words[1] & 0x1ffff, (resource.gpu_va >> 32) as u32);
        assert_eq!(words[1] >> 17, 1);
    }
}
