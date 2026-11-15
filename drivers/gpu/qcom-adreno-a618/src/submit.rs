// SPDX-License-Identifier: GPL-2.0-only

//! Hostile-input validation and relocation for the A618 submit dialect.

use alloc::vec::Vec;
#[path="submit_private.rs"]
mod submit_private;
#[path="submit_storage.rs"]
mod submit_storage;

use adreno_a6xx_pm4::{Header, Packet, Packets, opcode};
use adreno_a6xx_shader_pack::{
    PipelineVariant, ShaderMeta, ShaderVariant, link_meta, pipeline_state_meta, shader_meta,
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
const EVENT_WRITE_TIMESTAMP: u32 = 1 << 30;
const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
const EVENT_CACHE_INVALIDATE: u32 = 0x31;

const FORMAT_8_8_8_8_UNORM: u32 = 0x30;
const FORMAT_32_FLOAT: u32 = 0x4a;
const FORMAT_16_16_16_16_FLOAT: u32 = 0x62;
const FORMAT_32_32_32_32_FLOAT: u32 = 0x82;
const DEPTH_FORMAT_32: u32 = 4;
const A2D_IFMT_FLOAT32: u32 = 4;
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
const CP_LOAD_STATE6: u8 = 0x36;
const TEX_DESCRIPTOR_BGRA_IDENTITY: u32 = 0x4c00_6880;
const TEX_DESCRIPTOR_ALPHA_MASK: u32 = 0x4c00_76d0;
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

fn is_nv12_descriptor(word: u32) -> bool {
    word & !0x0005_0000 == 0x2380_6880
}

fn supported_texture_descriptor(word: u32) -> bool {
    if is_nv12_descriptor(word) {
        return true;
    }
    matches!(
        word & !(0x000f_0000 | (1 << 2)),
        TEX_DESCRIPTOR_BGRA_IDENTITY | TEX_DESCRIPTOR_ALPHA_MASK | 0x0c00_6883
    )
}

fn supported_sampler(words: &[u32], last_mip: u32) -> bool {
    if words.len() != 4 {
        return false;
    }
    let min_filter = (words[0] >> 3) & 3;
    let mag_filter = (words[0] >> 1) & 3;
    let wrap_s = (words[0] >> 5) & 7;
    let wrap_t = (words[0] >> 8) & 7;
    let aniso = (words[0] >> 14) & 7;
    let filter_valid = |filter| filter == 0 || filter == if aniso == 0 { 1 } else { 2 };
    let min_lod = words[1] >> 20;
    let max_lod = (words[1] >> 8) & 0xfff;
    // No LOD bias, reserved flags, border-table index or indirect address.
    // Border sampling is safe only with the explicit fast transparent color.
    words[0] & !0x0001_ffff == 0
        && (words[0] >> 11) & 7 == 1
        && wrap_s <= 3
        && wrap_t <= 3
        && aniso <= 4
        && filter_valid(min_filter)
        && filter_valid(mag_filter)
        && words[1] & 0xff == if max_lod == 0 && aniso == 0 { 0x40 } else { 0 }
        && min_lod <= max_lod
        && max_lod <= last_mip * 256
        && words[3] == 0
        && words[2]
            == (u32::from(wrap_s == 3 || wrap_t == 3) << 4) | (u32::from(min_filter != 0) << 5)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AddressField {
    private_memory: bool,
    word_offset: u32,
    access: u32,
    requires_complete_linear_image: bool,
    required_size: Option<u64>,
    size_is_minimum: bool,
    encoding: AddressEncoding,
    source: AddressSource,
    a2d: Option<A2dExpectation>,
    image: Option<ImageExpectation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressSource {
    Attachment,
    CanonicalShader(ShaderStage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShaderStage {
    Vertex,
    Fragment,
    Compute,
}

fn address_source_matches(source: AddressSource, relocation: RelocationSource) -> bool {
    match (source, relocation) {
        (AddressSource::Attachment, RelocationSource::Attachment(_)) => true,
        (AddressSource::CanonicalShader(_), RelocationSource::ShaderAttachment(_)) => true,
        (
            AddressSource::CanonicalShader(ShaderStage::Vertex),
            RelocationSource::CanonicalShader(actual),
        ) => actual.is_vertex(),
        (
            AddressSource::CanonicalShader(ShaderStage::Fragment),
            RelocationSource::CanonicalShader(actual),
        ) => !actual.is_vertex(),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct A2dExpectation {
    format: LinearImageFormat,
    modifier: ImageModifier,
    row_pitch: u32,
    required_width: u32,
    required_height: u32,
    // Only linear R32 upload sources may use explicitly bounded buffer storage.
    depth_staging: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImageExpectation {
    mip_levels: u32,
    format: LinearImageFormat,
    modifier: ImageModifier,
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
pub(crate) enum LinearImageFormat {
    Nv12 { uv_offset: u64, uv_size: u64 },
    Nv12Y,
    Nv12Uv,
    Bgra8Unorm,
    Depth32Float,
    Rgba16Float,
    Rgba32Float,
}

impl LinearImageFormat {
    fn texel_bytes(self) -> u32 {
        match self {
            Self::Nv12Y => 1,
            Self::Nv12Uv => 2,
            Self::Rgba16Float => 8,
            Self::Rgba32Float => 16,
            _ => 4,
        }
    }
}

// A6xx format encodings from Mesa a6xx_enums.xml. Float color is linear
// and uses native RGBA ordering (WZYX swap zero), never the BGRA swap.
fn native_color_format(value: u32) -> Option<LinearImageFormat> {
    match value {
        FORMAT_8_8_8_8_UNORM => Some(LinearImageFormat::Bgra8Unorm),
        FORMAT_16_16_16_16_FLOAT => Some(LinearImageFormat::Rgba16Float),
        FORMAT_32_32_32_32_FLOAT => Some(LinearImageFormat::Rgba32Float),
        _ => None,
    }
}

fn native_color_target(value: u32) -> Option<(LinearImageFormat, ImageModifier)> {
    if let Some(modifier) = color_target_modifier(value) {
        return Some((LinearImageFormat::Bgra8Unorm, modifier));
    }
    match value {
        FORMAT_16_16_16_16_FLOAT | FORMAT_32_32_32_32_FLOAT => {
            Some((native_color_format(value)?, ImageModifier::Linear))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageModifier {
    Linear,
    Tile6_3Depth,
    Tile6_3Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinearImage {
    pub(crate) texture: Option<adreno_a6xx_layout::LinearTextureLayout>,
    pub(crate) format: LinearImageFormat,
    pub(crate) modifier: ImageModifier,
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

fn a2d_control_format(value: u32) -> Option<LinearImageFormat> {
    let common = (0xf << 20) | (1 << 16);
    let color = (FORMAT_8_8_8_8_UNORM << 8) | common;
    let depth = (A2D_IFMT_FLOAT32 << 24) | (FORMAT_32_FLOAT << 8) | common | (1 << 7);
    let rgba16 = (3 << 24) | (FORMAT_16_16_16_16_FLOAT << 8) | common | (1 << 7);
    let rgba32 = (4 << 24) | (FORMAT_32_32_32_32_FLOAT << 8) | common | (1 << 7);
    if !matches!(value & 7, 0 | 2 | 4 | 5) {
        return None;
    }
    match value & !7 {
        value if value == color || value == color | (1 << 7) => Some(LinearImageFormat::Bgra8Unorm),
        value if value | (1 << 7) == depth => Some(LinearImageFormat::Depth32Float),
        value if value | (1 << 7) == rgba16 => Some(LinearImageFormat::Rgba16Float),
        value if value | (1 << 7) == rgba32 => Some(LinearImageFormat::Rgba32Float),
        _ => None,
    }
}

fn color_target_modifier(value: u32) -> Option<ImageModifier> {
    if value == FORMAT_8_8_8_8_UNORM | MRT_COLOR_SWAP_WXYZ
        || value == FORMAT_8_8_8_8_UNORM | (3 << 13)
    {
        Some(ImageModifier::Linear)
    } else if value == FORMAT_8_8_8_8_UNORM | (3 << 8) {
        Some(ImageModifier::Tile6_3Color)
    } else {
        None
    }
}

fn a2d_destination_format(value: u32) -> Option<(LinearImageFormat, ImageModifier)> {
    match value {
        FORMAT_16_16_16_16_FLOAT | FORMAT_32_32_32_32_FLOAT => {
            Some((native_color_format(value)?, ImageModifier::Linear))
        }
        value if value == FORMAT_8_8_8_8_UNORM | A2D_COLOR_SWAP_WXYZ
            || value == FORMAT_8_8_8_8_UNORM | (3 << 10) =>
        {
            Some((LinearImageFormat::Bgra8Unorm, ImageModifier::Linear))
        }
        value if value == FORMAT_8_8_8_8_UNORM | (3 << 8) => {
            Some((LinearImageFormat::Bgra8Unorm, ImageModifier::Tile6_3Color))
        }
        value if value == FORMAT_32_FLOAT | (adreno_a6xx_layout::TILE_MODE_3 << 8) => {
            Some((LinearImageFormat::Depth32Float, ImageModifier::Tile6_3Depth))
        }
        _ => None,
    }
}

fn a2d_output_format(value: u32) -> Option<LinearImageFormat> {
    match value {
        value if value == (FORMAT_16_16_16_16_FLOAT << 3) | (0xf << 12) | 1 => {
            Some(LinearImageFormat::Rgba16Float)
        }
        value if value == (FORMAT_32_32_32_32_FLOAT << 3) | (0xf << 12) => {
            Some(LinearImageFormat::Rgba32Float)
        }
        value if value == (FORMAT_8_8_8_8_UNORM << 3) | (0xf << 12) => {
            Some(LinearImageFormat::Bgra8Unorm)
        }
        value if value == (FORMAT_32_FLOAT << 3) | (0xf << 12) => {
            Some(LinearImageFormat::Depth32Float)
        }
        _ => None,
    }
}

fn a2d_source_format(value: u32) -> Option<(LinearImageFormat, ImageModifier)> {
    let required = (1 << 20) | (1 << 22);
    match value & !(1 << 16) {
        v if v == FORMAT_8_8_8_8_UNORM | A2D_COLOR_SWAP_WXYZ | required => {
            Some((LinearImageFormat::Bgra8Unorm, ImageModifier::Linear))
        }
        v if v == FORMAT_16_16_16_16_FLOAT | required => {
            Some((LinearImageFormat::Rgba16Float, ImageModifier::Linear))
        }
        v if v == FORMAT_32_32_32_32_FLOAT | required => {
            Some((LinearImageFormat::Rgba32Float, ImageModifier::Linear))
        }
        v if v == FORMAT_32_FLOAT | required => {
            Some((LinearImageFormat::Depth32Float, ImageModifier::Linear))
        }
        v if v == FORMAT_32_FLOAT | (3 << 8) | required => {
            Some((LinearImageFormat::Depth32Float, ImageModifier::Tile6_3Depth))
        }
        _ => None,
    }
}

fn safe_depth_control(value: u32) -> bool {
    if value == 0 {
        return true;
    }
    let compare = (value >> 2) & 7;
    let expected_read = !matches!(compare, 0 | 7);
    value & !0x5f == 0 && value & 1 != 0 && (value & (1 << 6) != 0) == expected_read
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
        private_memory: false,
        word_offset,
        access,
        requires_complete_linear_image,
        required_size,
        size_is_minimum: false,
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
    stage: ShaderStage,
) -> Result<(), &'static str> {
    fields
        .try_reserve(1)
        .map_err(|_| "qcom-adreno-a618: PM4 validation allocation failed")?;
    fields.push(AddressField {
        private_memory: false,
        word_offset,
        access: ACCESS_READ,
        requires_complete_linear_image: false,
        required_size: None,
        size_is_minimum: false,
        encoding: AddressEncoding::GpuVa64,
        source: AddressSource::CanonicalShader(stage),
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

// Native-program state is limited to named, nonprivileged A6xx fields from
// Mesa a6xx.xml/adreno_pm4.xml and fd6_compute.cc. It is NEVER used without a
// ShaderAttachment relocation and the backend's private execution domain.
const CP_EXEC_CS: u8 = 0x33;
const SP_CS_CNTL_0: u32 = 0xa9b0;
const SP_CS_PROGRAM_COUNTER_OFFSET: u32 = 0xa9b3;
const SP_CS_CONFIG: u32 = 0xa9bb;
const SP_CS_INSTR_SIZE: u32 = 0xa9bc;
const SP_CS_CONST_CONFIG: u32 = 0xb987;
const SP_CS_NDRANGE_0: u32 = 0xb990;

fn native_scalar(register: u32, value: u32) -> bool {
    match register {
        SP_VS_CNTL_0 => value & !0x003f_ffff == 0,
        SP_PS_CNTL_0 => value & !0x97ff_ffff == 0,
        SP_CS_CNTL_0 => value & !0x80ff_ffff == 0,
        0xa9b1 => value & !0x0400_007f == 0 && (value >> 5) & 3 <= 2,
        SP_VS_CONFIG | SP_PS_CONFIG | SP_CS_CONFIG => value & !0x1fff_ff0f == 0,
        SP_VS_INSTR_SIZE | SP_PS_INSTR_SIZE | SP_CS_INSTR_SIZE => (1..=1023).contains(&value),
        SP_VS_CONST_CONFIG | SP_PS_CONST_CONFIG | SP_CS_CONST_CONFIG => {
            value & !0x1ff == 0 && value & 0x100 != 0
        }
        SP_PS_WAVE_CNTL => value & !0xfff == 0,
        SP_UPDATE_CNTL => value & !0x000f_ffff == 0,
        SP_CS_NDRANGE_0 => {
            let x = ((value >> 2) & 1023) + 1;
            let y = ((value >> 12) & 1023) + 1;
            let z = ((value >> 22) & 1023) + 1;
            value & 3 != 0 && x * y * z <= 1024
        }
        0xb991..=0xb997 | 0xb999..=0xb99b => true, // sizes, offsets, reg IDs; no addresses
        0xb998 => value & !0x7ff == 0,
        0xa822 | 0xa9ba => value <= 32,
        SP_GFX_USIZE => value <= 127,
        SP_PS_TSIZE => value <= 16,
        GRAS_CL_CNTL => matches!(value, 0x80 | 0xc0),
        // Mesa a6xx.xml PC_PS_CNTL has only PRIMITIVEIDEN bit 0. This
        // non-address scalar is admitted only through the native shader path;
        // canonical/default program state below still requires zero.
        PC_PS_CNTL => value <= 1,
        GRAS_SU_DEPTH_PLANE_CNTL | RB_DEPTH_PLANE_CNTL => value <= 1,
        0xaa00 => value <= 127,
        0xa9bd | 0xa9be => value == 0, // no unbound private-memory/hysteresis state
        GRAS_CL_INTERP_CNTL
        | RB_INTERP_CNTL
        | RB_PS_INPUT_CNTL
        | SP_PS_INITIAL_TEX_LOAD_CNTL
        | SP_REG_PROG_ID_0
        | 0xb984..=0xb986
        | SP_PS_OUTPUT_CNTL
        | SP_PS_OUTPUT_MASK
        | RB_PS_OUTPUT_CNTL
        | RB_PS_OUTPUT_MASK
        | SP_VS_OUTPUT_CNTL
        | VPC_VS_CNTL
        | VPC_PS_CNTL
        | PC_VS_CNTL => true,
        _ => false,
    }
}

fn validate_native_type4(
    register: u32,
    payload: &[u32],
    word: u32,
    addresses: &mut Vec<AddressField>,
) -> Option<Result<(), &'static str>> {
    if let Some(result) = submit_private::validate_type4(register, payload, word, addresses) { return Some(result); }

    let count = payload.len() as u32;
    // Mesa GRAS_SU_POLY_OFFSET_{SCALE,OFFSET,OFFSET_CLAMP} are float data.
    // Admit only the named native raster state, never adjacent registers.
    if register == GRAS_SU_POLY_OFFSET_SCALE {
        return Some(
            if payload.len() == 3 && payload.iter().all(|v| f32::from_bits(*v).is_finite()) {
                Ok(())
            } else {
                Err("a618: native depth bias must contain three finite values")
            },
        );
    }
    if register == GRAS_SU_CNTL {
        return Some(
            if payload.len() == 1
                && payload[0] & !0x2817 == 0
                && payload[0] & 0x2010 == 0x2010
            {
                Ok(())
            } else {
                Err("a618: unsupported native raster control")
            },
        );
    }
    if matches!(register, RB_SRGB_CNTL | SP_SRGB_CNTL) && count == 1 {
        return Some(if payload[0] & !0xff == 0 {
            Ok(())
        } else {
            Err("a618: invalid MRT sRGB mask")
        });
    }
    // Eight named MRT slots; address registers are handled individually.
    // Do not widen this to a generic register-range allowlist.
    if (RB_MRT_CONTROL..RB_MRT_CONTROL + 8 * 8).contains(&register) {
        return Some(match ((register - RB_MRT_CONTROL) % 8, payload.len()) {
            (0, 1) if payload[0] == 0 => Ok(()),
            (0, 2) if valid_native_blend_control(payload) || payload == [0, 0] => Ok(()),
            (2, 1) if native_color_target(payload[0]).is_some() => Ok(()),
            (3, 2)
                if (1..=0xffff).contains(&payload[0])
                    && (1..=0x1fff_ffff).contains(&payload[1]) =>
            {
                Ok(())
            }
            (5, 2) => address_field(addresses, word + 1, ACCESS_WRITE, false, None),
            (7, 1) if payload[0] == 0 => Ok(()),
            _ => Err("a618: invalid native MRT register state"),
        });
    }
    if register >= SP_PS_MRT_REG && register + count <= SP_PS_MRT_REG + 8 {
        return Some(
            if payload.iter().all(|v| native_color_format(*v).is_some()) {
                Ok(())
            } else {
                Err("a618: unsupported native MRT format")
            },
        );
    }
    if matches!(register, SP_PS_MRT_CNTL | RB_PS_MRT_CNTL) && count == 1 {
        return Some(if payload[0] <= 8 {
            Ok(())
        } else {
            Err("a618: native MRT count exceeds eight")
        });
    }
    if register == GRAS_LRZ_MRT_BUFFER_INFO_0 && count == 1 {
        return Some(if native_color_format(payload[0]).is_some() {
            Ok(())
        } else {
            Err("a618: unsupported native LRZ color format")
        });
    }
    if register == RB_BLEND_CNTL && count == 1 && payload[0] & !0xffff_01ff == 0 {
        return Some(Ok(()));
    }
    if register == SP_BLEND_CNTL && count == 1 && payload[0] & !0x1ff == 0 {
        return Some(Ok(()));
    }
    if (RB_COLOR_FLAG_BUFFER_ADDR..RB_COLOR_FLAG_BUFFER_ADDR + 8 * 3).contains(&register)
        && (register - RB_COLOR_FLAG_BUFFER_ADDR) % 3 == 0
        && payload == [0; 3]
    {
        return Some(Ok(()));
    }
    if (VFD_VERTEX_BUFFER_BASE..VFD_VERTEX_BUFFER_BASE + 128).contains(&register) {
        let slot_offset = (register - VFD_VERTEX_BUFFER_BASE) % 4;
        return Some(match (slot_offset, payload.len()) {
            (0, 2) => address_field(addresses, word + 1, ACCESS_READ, false, None),
            (0, 4) if payload[2] != 0 && payload[3] <= 4095 => address_field(
                addresses,
                word + 1,
                ACCESS_READ,
                false,
                Some(u64::from(payload[2])),
            ),
            (2, 2) if payload[0] != 0 && payload[1] <= 4095 => Ok(()),
            _ => Err("a618: invalid native VFD buffer slot"),
        });
    }
    if register == VFD_INDEX_OFFSET && payload.len() == 2 {
        return Some(Ok(()));
    }
    if register == VFD_CNTL_0
        && payload.len() == 1
        && payload[0] & !0xffff == 0
        && payload[0] & 0xff <= 32
        && payload[0] >> 8 <= 32
    {
        return Some(Ok(()));
    }
    // Explicit address-bearing compute registers, never a register interval.
    if register == SP_CS_PROGRAM_COUNTER_OFFSET && payload.len() == 7 {
        return Some(if payload[0] == 0 && payload[3..] == [0; 4] {
            canonical_address_field(addresses, word + 2, ShaderStage::Compute)
        } else {
            Err("a618: unbound compute private memory or program offset")
        });
    }
    let descriptor_size = match register {
        0xa8a0 | SP_PS_SAMPLER_BASE | 0xa9e2 => Some(16), // VS/FS/CS sampler base
        0xa8a8 | SP_PS_TEXMEMOBJ_BASE | 0xa9e6 => Some(64), // VS/FS/CS texture base
        0xab1a | 0xa9f2 => Some(64),                      // graphics/compute UAV descriptor base
        _ => None,
    };
    if let Some(size) = descriptor_size {
        return Some(if payload.len() == 2 {
            address_field(addresses, word + 1, ACCESS_READ, false, Some(size)).map(|()| {
                addresses.last_mut().unwrap().size_is_minimum = true;
            })
        } else {
            Err("a618: malformed compute descriptor address")
        });
    }
    if payload.len() == 1 && native_scalar(register, payload[0]) {
        return Some(Ok(()));
    }
    if register == SP_CS_NDRANGE_0
        && payload.len() == 7
        && payload
            .iter()
            .enumerate()
            .all(|(i, v)| native_scalar(register + i as u32, *v))
    {
        return Some(Ok(()));
    }
    if register == 0xb997 && payload.len() == 2 && native_scalar(0xb998, payload[1]) {
        return Some(Ok(()));
    }
    if register == 0xb999 && payload.len() == 3 {
        return Some(Ok(()));
    }
    if register == SP_VS_CONST_CONFIG
        && payload.len() == 4
        && payload[1..] == [0; 3]
        && (payload[0] == 0 || native_scalar(register, payload[0]))
    {
        return Some(Ok(()));
    }
    // Linkage/VFD arrays contain only register IDs, formats, counts and
    // offsets. Keep every burst within that named array's register bounds.
    if (register == VFD_CNTL_1 && count == 6)
        || (register == VPC_VARYING_INTERP_MODE && count == 8)
        || (register >= VFD_DEST_CNTL
            && register + count <= VFD_DEST_CNTL + 32
            && payload.iter().all(|v| v & !0xfff == 0))
        || (register >= SP_VS_OUTPUT_REG && register + count <= SP_VS_OUTPUT_REG + 16)
        || (register >= SP_VS_VPC_DEST_REG && register + count <= SP_VS_VPC_DEST_REG + 8)
        || (register >= SP_PS_OUTPUT_REG && register + count <= SP_PS_OUTPUT_REG + 8)
        || (register == SP_REG_PROG_ID_0 && count <= 4)
        || (register == VFD_FETCH_INSTR
            && count <= 64
            && count % 2 == 0
            && payload.chunks_exact(2).all(|p| p[0] & 0x4000_0000 != 0))
    {
        return Some(Ok(()));
    }
    if register == VFD_VERTEX_BUFFER_SIZE
        && payload.len() == 2
        && payload[0] != 0
        && (1..=2048).contains(&payload[1])
    {
        return Some(Ok(()));
    }
    None
}

// Structural wire decoding proves that offsets are strictly increasing and
// relocation word spans do not overlap. All callers use that validated order.
fn relocation_at(relocations: &[Relocation], word: u32) -> Option<&Relocation> {
    let index = relocations.partition_point(|relocation| relocation.pm4_word_offset < word);
    relocations.get(index).filter(|relocation| relocation.pm4_word_offset == word)
}

fn relocations_in_words(relocations: &[Relocation], start: u32, end: u32) -> &[Relocation] {
    let first = relocations.partition_point(|relocation| relocation.pm4_word_offset < start);
    let count = relocations[first..].partition_point(|relocation| relocation.pm4_word_offset < end);
    &relocations[first..first + count]
}

fn validate_native_type7(
    opcode_value: u8,
    payload: &[u32],
    word: u32,
    addresses: &mut Vec<AddressField>,
    relocations: &[Relocation],
) -> Option<Result<(), &'static str>> {
    if opcode_value == opcode::EVENT_WRITE
        && payload.len() == 4
        && payload[0] == (0x04 | EVENT_WRITE_TIMESTAMP)
        && payload[1..3] == [0, 0]
        && payload[3] != 0
    {
        // Native intra-submit UCHE clean, Mesa FD_CACHE_CLEAN. No IRQ bit;
        // only an authorized user sequence word, never the kernel fence.
        return Some(address_field(
            addresses,
            word + 2,
            ACCESS_WRITE,
            false,
            Some(4),
        ));
    }
    if opcode_value == opcode::SET_MARKER && payload == [8] {
        return Some(Ok(()));
    }
    if opcode_value == CP_DRAW_INDX_OFFSET {
        if !matches!(payload.len(), 3 | 7) {
            return Some(Err("a618: invalid native draw packet"));
        }
        // A6xx non-binning primitive types POINT_LIST through TRIANGLE_FAN.
        let primitive = payload[0] & 0x3f;
        if !matches!(primitive, 1..=6 | 9) || payload[1] == 0 || payload[2] == 0 {
            return Some(Err("a618: invalid native draw counts/topology"));
        }
        if payload.len() == 3 {
            return Some(if payload[0] & !0x3f == 0x80 {
                Ok(())
            } else {
                Err("a618: unsafe native nonindexed draw")
            });
        }
        let index_size = match payload[0] & !0x3f {
            0x400 => 2u64,
            0x800 => 4u64,
            _ => return Some(Err("a618: unsafe native indexed draw")),
        };
        if payload[4..6] != [0, 0]
            || payload[3]
                .checked_add(payload[2])
                .is_none_or(|n| n > payload[6])
        {
            return Some(Err("a618: native index span out of bounds"));
        }
        return Some(address_field(
            addresses,
            word + 5,
            ACCESS_READ,
            false,
            Some(u64::from(payload[6]) * index_size),
        ));
    }
    if opcode_value == CP_MEM_WRITE {
        if !(3..=4098).contains(&payload.len()) || payload[..2] != [0, 0] {
            return Some(Err("a618: invalid native table write"));
        }
        if let Err(error) = address_field(
            addresses,
            word + 1,
            ACCESS_WRITE,
            false,
            Some((payload.len() as u64 - 2) * 4),
        ) {
            return Some(Err(error));
        }
        for r in relocations_in_words(relocations, word + 3, word + 1 + payload.len() as u32) {
            if !matches!(r.source, RelocationSource::Attachment(_))
                || r.pm4_word_offset + 2 > word + 1 + payload.len() as u32
            {
                return Some(Err("a618: invalid table data relocation"));
            }
            if let Err(error) = address_field_encoded(
                addresses,
                r.pm4_word_offset,
                r.access,
                false,
                None,
                r.encoding,
            ) {
                return Some(Err(error));
            }
        }
        return Some(Ok(()));
    }
    if opcode_value == CP_EXEC_CS {
        return Some(
            if payload.len() == 4
                && payload[0] == 0
                && payload[1..].iter().all(|v| (1..=65535).contains(v))
            {
                Ok(())
            } else {
                Err("a618: invalid compute dispatch")
            },
        );
    }
    // A6xx graphics UAV tables use the shared UAV state block, not a stage's
    // shader block with ST6_UAV (that combination is compute-only). See Mesa
    // fd6_image.cc graphics/compute paths and adreno_pm4.xml LOAD_STATE6.
    if opcode_value == CP_LOAD_STATE6 {
        let control = payload.first().copied().unwrap_or(0);
        let units = control >> 22;
        if payload.len() != 3
            || (control >> 18) & 15 != 14
            || (control >> 14) & 3 != 0
            || (control >> 16) & 3 != 2
            || units == 0
            || (control & 0x3fff) + units > 128
            || payload[1..] != [0, 0]
        {
            return Some(Err("a618: invalid graphics UAV load state"));
        }
        return Some(address_field(
            addresses, word + 2, ACCESS_READ, false, Some(u64::from(units) * 64),
        ));
    }
    if !matches!(opcode_value, CP_LOAD_STATE6_GEOM | CP_LOAD_STATE6_FRAG) || payload.len() < 3 {
        return None;
    }
    let control = payload[0];
    let block = (control >> 18) & 15;
    let kind = (control >> 14) & 3;
    let source = (control >> 16) & 3;
    let units = control >> 22;
    let offset = control & 0x3fff;
    if matches!(
        (opcode_value, block),
        (CP_LOAD_STATE6_GEOM, 0) | (CP_LOAD_STATE6_FRAG, 4 | 5)
    ) && source == 2
        && matches!(kind, 0 | 1)
        && payload.len() == 3
        && units != 0
        && offset + units <= if kind == 0 { 32 } else { 16 }
    {
        let size = u64::from(units) * if kind == 0 { 16 } else { 64 };
        return Some(address_field(
            addresses,
            word + 2,
            ACCESS_READ,
            false,
            Some(size),
        ));
    }
    if !matches!(
        (opcode_value, block),
        (CP_LOAD_STATE6_GEOM, 8) | (CP_LOAD_STATE6_FRAG, 12 | 13)
    ) {
        return None;
    }
    if units == 0 || payload[1..3] != [0, 0] {
        return Some(Err("a618: invalid native load state"));
    }
    if kind == 3 && block != 13 {
        return Some(Err("a618: graphics UAV load must use the shared UAV block"));
    }
    if kind == 3 && source == 2 && offset + units <= 128 && payload.len() == 3 {
        return Some(address_field(
            addresses,
            word + 2,
            ACCESS_READ,
            false,
            Some(u64::from(units) * 64),
        ));
    }
    if kind == 0 && source == 2 && offset == 0 && payload.len() == 3 {
        let stage = match block {
            8 => ShaderStage::Vertex,
            12 => ShaderStage::Fragment,
            _ => ShaderStage::Compute,
        };
        return Some(
            canonical_address_field(addresses, word + 2, stage).map(|()| {
                // LOAD_STATE6 reads instructions in 128-byte units. The wire
                // format retains the complete canonical shader allocation,
                // which may also contain constants (e.g. FsNv12). Require the
                // fetch to fit within it; do not equate the two lengths.
                let address = addresses.last_mut().unwrap();
                address.required_size = Some(u64::from(units) * 128);
                address.size_is_minimum = true;
            }),
        );
    }
    if kind == 2 && source == 0 && offset + units <= 32 && payload.len() == 3 + units as usize * 2 {
        for (index, descriptor) in payload[3..].chunks_exact(2).enumerate() {
            let size = u64::from(descriptor[1] >> 17) * 16;
            if size == 0 {
                if descriptor != [0, 0] {
                    return Some(Err("a618: nonzero empty UBO descriptor"));
                }
                continue;
            }
            if let Err(error) = address_field_encoded(
                addresses,
                word + 4 + index as u32 * 2,
                ACCESS_READ,
                false,
                Some(size),
                AddressEncoding::GpuVa49UboDescriptor,
            ) {
                return Some(Err(error));
            }
        }
        return Some(Ok(()));
    }
    if kind == 1 && offset + units <= 256 {
        if source == 2 && payload.len() == 3 {
            return Some(address_field(
                addresses,
                word + 2,
                ACCESS_READ,
                false,
                Some(u64::from(units) * 16),
            ));
        }
        if source == 0 && payload.len() == 3 + units as usize * 4 {
            // Constants may include explicitly typed buffer pointers. Other
            // constant words are data, not PM4 commands. ISA memory accesses
            // are confined by the private domain, including indirect pointers.
            for r in relocations_in_words(relocations, word + 4, word + 1 + payload.len() as u32) {
                if !matches!(r.source, RelocationSource::Attachment(_))
                    || r.encoding != AddressEncoding::GpuVa64
                    || r.pm4_word_offset + 2 > word + 1 + payload.len() as u32
                    || (r.pm4_word_offset - word - 4) % 2 != 0
                {
                    return Some(Err("a618: invalid native constant pointer"));
                }
                if let Err(error) =
                    address_field(addresses, r.pm4_word_offset, r.access, false, None)
                {
                    return Some(Err(error));
                }
            }
            return Some(Ok(()));
        }
    }
    Some(Err("a618: unsupported native load state"))
}

fn validate_compute_sequences(
    packets: &[Packet<'_>],
    relocations: &[Relocation],
) -> Result<(), &'static str> {
    let mut registers = Vec::new();
    let mut start = 0;
    for (index, packet) in packets.iter().enumerate() {
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_EXEC_CS,
                ..
            }
        ) {
            continue;
        }
        let segment = SegmentIndex::new(&packets[start..index], &mut registers);
        let (_, base) = submit_private::program_layout(&segment, SP_CS_PROGRAM_COUNTER_OFFSET)
            .ok_or("a618: compute program must be initialized in this dispatch")?;
        let preload = segment_shader_preload_word(&segment, CP_LOAD_STATE6_FRAG, 13)
            .ok_or("a618: compute preload is missing")?;
        let base = relocation_at(relocations, base).ok_or("a618: compute program relocation missing")?;
        let preload = relocation_at(relocations, preload).ok_or("a618: compute preload relocation missing")?;
        if !matches!(base.source, RelocationSource::ShaderAttachment(_))
            || base.source != preload.source
            || base.resource_offset != preload.resource_offset
            || base.required_size != preload.required_size
        {
            return Err("a618: compute program/preload do not match");
        }
        for reg in [
            SP_CS_CNTL_0,
            0xa9b1,
            SP_CS_CONFIG,
            SP_CS_INSTR_SIZE,
            SP_CS_CONST_CONFIG,
            SP_CS_NDRANGE_0,
            0xb997,
            0xb998,
            SP_PS_WAVE_CNTL,
        ] {
            // SegmentIndex stores bursts by their first register. Accept the
            // documented paired ID/WGE packet as well as separate scalars.
            if segment.reg(reg).is_none()
                && !(reg == 0xb998 && segment.reg(0xb997).is_some_and(|(_, p)| p.len() == 2))
            {
                return Err("a618: incomplete compute state");
            }
        }
        let (_, size) = segment.reg(SP_CS_INSTR_SIZE).unwrap();
        if u64::from(size[0]) * 128 > base.required_size {
            return Err("a618: compute instruction size exceeds shader");
        }
        let (_, config) = segment.reg(SP_CS_CONFIG).unwrap();
        if config[0] & 0x100 == 0 {
            return Err("a618: compute shader is disabled");
        }
        let (_, nd) = segment.reg(SP_CS_NDRANGE_0).unwrap();
        for axis in 0..3 {
            let local = ((nd[0] >> (2 + axis * 10)) & 1023) + 1;
            let expected = packet.payload[axis + 1]
                .checked_mul(local)
                .ok_or("a618: compute range overflow")?;
            let (global, offset) = if nd.len() == 7 {
                (nd[1 + axis * 2], nd[2 + axis * 2])
            } else {
                let (_, global) = segment
                    .reg(SP_CS_NDRANGE_0 + 1 + axis as u32 * 2)
                    .ok_or("a618: compute global size missing")?;
                let (_, offset) = segment
                    .reg(SP_CS_NDRANGE_0 + 2 + axis as u32 * 2)
                    .ok_or("a618: compute global offset missing")?;
                (global[0], offset[0])
            };
            if global != expected || global.checked_add(offset).is_none() {
                return Err("a618: compute dispatch/ND range mismatch");
            }
        }
        start = index + 1;
    }
    Ok(())
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
        (RB_A2D_BLT_CNTL, 1) | (GRAS_A2D_BLT_CNTL, 1)
            if a2d_control_format(payload[0]).is_some() =>
        {
            Ok(())
        }
        (RB_A2D_PIXEL_CNTL, 1) => exact(payload, &[0]),
        (RB_A2D_DEST_BUFFER_INFO, 1) if a2d_destination_format(payload[0]).is_some() => Ok(()),
        (RB_A2D_DEST_BUFFER_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_WRITE, true, None)
        }
        (RB_A2D_DEST_BUFFER_PITCH, 1) if payload[0] != 0 && payload[0] & !0x3fff == 0 => Ok(()),
        // Data only. Encoding is checked against the selected format at BLIT.
        (RB_A2D_CLEAR_COLOR_DW0, 4) => Ok(()),
        (GRAS_A2D_SRC_XMIN, 4) if payload.iter().all(|value| value & 0xff == 0) => Ok(()),
        (GRAS_A2D_DEST_TL, 2) | (GRAS_A2D_SCISSOR_TL, 2) => Ok(()),
        (SP_A2D_OUTPUT_INFO, 1)
            if a2d_output_format(payload[0]).is_some()
                || payload[0] == (FORMAT_16_16_16_16_FLOAT << 3) | (0xf << 12) =>
        {
            Ok(())
        }
        (TPL1_A2D_SRC_TEXTURE_INFO, 1) if a2d_source_format(payload[0]).is_some() => Ok(()),
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
        (RB_SRGB_CNTL | SP_SRGB_CNTL, 1) if payload[0] <= 1 => Ok(()),
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
            | GRAS_SU_VS_SIV_CNTL
            | GRAS_SU_STENCIL_CNTL
            | GRAS_LRZ_PS_INPUT_CNTL
            | GRAS_LRZ_PS_SAMPLEFREQ_CNTL
            | RB_PS_SAMPLEFREQ_CNTL
            | RB_DITHER_CNTL
            | RB_MRT_BASE_GMEM
            | RB_ALPHA_TEST_CNTL
            | RB_DEPTH_PLANE_CNTL
            | RB_STENCIL_CNTL
            | RB_STENCIL_BUFFER_INFO
            | RB_STENCIL_REF_CNTL
            | PC_PS_CNTL
            | PC_CNTL
            | PC_STEREO_RENDERING_CNTL
            | SP_VS_PVT_MEM_STACK_OFFSET
            | SP_PS_PVT_MEM_STACK_OFFSET
            | SP_GFX_USIZE,
            1,
        ) => exact(payload, &[0]),
        (GRAS_SU_DEPTH_BUFFER_INFO, 1) if matches!(payload[0], 0 | DEPTH_FORMAT_32) => Ok(()),
        (GRAS_SU_DEPTH_CNTL, 1) if matches!(payload[0], 0 | 1) => Ok(()),
        (RB_DEPTH_CNTL, 1) if safe_depth_control(payload[0]) => Ok(()),
        (VFD_MODE_CNTL, 1) => exact(payload, &[3]),
        (VPC_SO_OVERRIDE, 1) => exact(payload, &[0]),
        (PC_MODE_CNTL, 1) => exact(payload, &[0x1f]),
        (SP_MODE_CNTL, 1) => exact(payload, &[5]),
        (TPL1_MODE_CNTL, 1) => exact(payload, &[0xa2]),
        (RB_MODE_CNTL, 1) => exact(payload, &[0x10]),
        (SP_UPDATE_CNTL, 1) => exact(payload, &[0x0000_00ff]),
        (SP_HS_CONFIG | SP_DS_CONFIG | SP_GS_CONFIG, 1) => exact(payload, &[0]),
        (SP_VS_PROGRAM_COUNTER_OFFSET, 7) if payload[0] == 0 && payload[3..] == [0; 4] => {
            canonical_address_field(addresses, packet_word + 2, ShaderStage::Vertex)
        }
        (SP_PS_PROGRAM_COUNTER_OFFSET, 7) if payload[0] == 0 && payload[3..] == [0; 4] => {
            canonical_address_field(addresses, packet_word + 2, ShaderStage::Fragment)
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
        (RB_DEPTH_BUFFER_INFO, 6) if payload == [0; 6] => Ok(()),
        (RB_DEPTH_BUFFER_INFO, 6)
            if payload[0] == DEPTH_FORMAT_32
                && payload[1] != 0
                && payload[1] <= 0x3fff
                && payload[2] != 0
                && payload[2] <= 0x0fff_ffff
                && payload[3..] == [0; 3] =>
        {
            address_field(
                addresses,
                packet_word + 4,
                ACCESS_READ | ACCESS_WRITE,
                false,
                None,
            )
        }
        (RB_DEPTH_BOUND_MIN, 2) => exact(payload, &[0, 1.0_f32.to_bits()]),
        (RB_STENCIL_MASK, 2) => exact(payload, &[0, 0]),
        (RB_COLOR_FLAG_BUFFER_ADDR, 3) => exact(payload, &[0; 3]),
        (RB_DEPTH_FLAG_BUFFER_BASE, 3) => exact(payload, &[0; 3]),
        (VPC_VARYING_INTERP_MODE | VPC_VARYING_REPLACE_MODE, 8) => exact(payload, &[0; 8]),
        (RB_MRT_CONTROL, 2) if valid_blend_control(payload) => Ok(()),
        (RB_MRT_BUF_INFO, 1) if color_target_modifier(payload[0]).is_some() => Ok(()),
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
            if [
                EVENT_CCU_FLUSH_COLOR_TS | EVENT_WRITE_TIMESTAMP,
                EVENT_CCU_FLUSH_DEPTH_TS | EVENT_WRITE_TIMESTAMP,
            ]
            .contains(&payload[0])
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
                && supported_texture_descriptor(payload[2])
                && payload[3] & 0xc000_0000 == 0
                && payload[3] & 0x7fff != 0
                && (payload[3] >> 15) & 0x7fff != 0
                && payload[4] & 0x7f == if payload[2] & 3 == 3 { 2 } else { 0 }
                && payload[4] >> 29 == 1
                && payload[5] & !0x007f_ffff == 0
                && payload[7] == (1 << 17)
                && (if is_nv12_descriptor(payload[2]) {
                    payload[8] == (((payload[4] >> 7) & 0x3f_ffff) << 8) && payload[9..18] == [0; 9]
                } else {
                    payload[8..18] == [0; 10]
                })
                && supported_sampler(
                    &payload[18..],
                    if is_nv12_descriptor(payload[2]) {
                        0
                    } else {
                        (payload[2] >> 16) & 15
                    },
                ) =>
        {
            address_field(addresses, packet_word + 1, ACCESS_WRITE, false, Some(80))?;
            address_field_encoded(
                addresses,
                packet_word + 7,
                ACCESS_READ,
                false,
                None,
                AddressEncoding::GpuVa49TexDescriptor,
            )?;
            if is_nv12_descriptor(payload[2]) {
                address_field(addresses, packet_word + 10, ACCESS_READ, false, None)?;
            }
            Ok(())
        }
        (CP_LOAD_STATE6_GEOM, 3)
            if payload[0] == ((2 << 16) | (8 << 18) | (1 << 22)) && payload[1..3] == [0, 0] =>
        {
            canonical_address_field(addresses, packet_word + 2, ShaderStage::Vertex)
        }
        (CP_LOAD_STATE6_FRAG, 3)
            if matches!(payload[0] >> 22, 1 | 2)
                && payload[0] & ((1 << 22) - 1) == ((2 << 16) | (12 << 18))
                && payload[1..3] == [0, 0] =>
        {
            canonical_address_field(addresses, packet_word + 2, ShaderStage::Fragment)
        }
        (CP_LOAD_STATE6_GEOM, 23)
            if payload[0] == ((1 << 14) | (8 << 18) | (5 << 22)) && payload[1..3] == [0, 0] =>
        {
            Ok(())
        }
        (CP_LOAD_STATE6_FRAG, 35)
            if payload[0] == ((1 << 14) | (12 << 18) | (8 << 22)) && payload[1..3] == [0, 0] =>
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
            if payload[0] & !0x3f == 0x80
                && canonical_topology_count(payload[0] & 0x3f, payload[2])
                && payload[1] == 1 =>
        {
            Ok(())
        }
        (CP_DRAW_INDX_OFFSET, 7) => {
            let index_size = match payload[0] & !0x3f {
                // Public primitive mode | DMA | INDEX4_SIZE_16/32_BIT.
                0x400 => 2_u64,
                0x800 => 4_u64,
                _ => return Err("qcom-adreno-a618: unsafe indexed draw initiator"),
            };
            let end_index = payload[3]
                .checked_add(payload[2])
                .ok_or("qcom-adreno-a618: indexed draw range overflows")?;
            if payload[1] != 1
                || !canonical_topology_count(payload[0] & 0x3f, payload[2])
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
fn validate_memcpy_barriers(packets: &[Packet<'_>], native: bool) -> Result<(), &'static str> {
    let mut require_wait = false;
    let mut can_batch = false;
    for packet in packets.iter().copied() {
        if require_wait {
            if native
                && can_batch
                && matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: CP_MEM_WRITE,
                        ..
                    }
                )
            {
                // Independent descriptor writes may batch, but no state load,
                // shader dispatch or other consumer may precede their wait.
                continue;
            }
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
        can_batch = matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_MEM_WRITE,
                ..
            }
        );
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
            ) && packet.payload.first() == Some(&(event | EVENT_WRITE_TIMESTAMP))
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
/// front of dirty color-cache retirement. Solid A2D clears may share one
/// retirement sequence; sampled blits still retire independently. Consecutive
/// 3D draws may share one render-pass retirement sequence.
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
        ) && packet.payload.first() == Some(&(event | EVENT_WRITE_TIMESTAMP))
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
    let mut pending_clear = false;
    let mut solid_blit = false;
    let mut index = 0usize;
    while index < packets.len() {
        let packet = &packets[index];
        if (pending_draw || pending_clear)
            && packets
                .get(index..index.saturating_add(5))
                .is_some_and(is_sysmem_retirement)
        {
            pending_draw = false;
            pending_clear = false;
            index += 5;
            continue;
        }
        if matches!(
            packet.header,
            Header::Type4 {
                register: RB_A2D_BLT_CNTL,
                ..
            }
        ) {
            solid_blit = packet.payload[0] & (1 << 7) != 0;
        }
        // An unretired clear can only be followed by another solid clear's
        // bounded A2D setup. Do not permit texture reads, 3D state, cache
        // invalidations or memory operations before the complete epilogue.
        if pending_clear
            && !matches!(
                packet.header,
                Header::Type4 {
                    register: RB_A2D_BLT_CNTL
                        | GRAS_A2D_BLT_CNTL
                        | RB_A2D_PIXEL_CNTL
                        | SP_A2D_OUTPUT_INFO
                        | RB_A2D_CLEAR_COLOR_DW0
                        | RB_A2D_DEST_BUFFER_INFO
                        | RB_A2D_DEST_BUFFER_BASE
                        | RB_A2D_DEST_BUFFER_PITCH
                        | GRAS_A2D_DEST_TL
                        | GRAS_A2D_SCISSOR_TL,
                    ..
                } | Header::Type7 {
                    opcode: opcode::WAIT_FOR_IDLE | opcode::BLIT,
                    ..
                }
            )
        {
            return Err("qcom-adreno-a618: clear batch lacks sysmem retirement");
        }
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
            if solid_blit {
                pending_clear = true;
                index += 2;
                continue;
            }
            if pending_clear {
                return Err("qcom-adreno-a618: sampled blit follows an unretired clear");
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

        index += 1;
    }
    if pending_draw || pending_clear {
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
    control_format: Option<LinearImageFormat>,
    pixel_control: bool,
    output_format: Option<LinearImageFormat>,
    destination_format: Option<(LinearImageFormat, ImageModifier)>,
    destination_word: Option<u32>,
    destination_pitch: Option<u32>,
    destination_tl: Option<u32>,
    destination_br: Option<u32>,
    scissor_tl: Option<u32>,
    scissor_br: Option<u32>,
    source_info: Option<u32>,
    source_word: Option<u32>,
    source_pitch: Option<u32>,
    source_size: Option<u32>,
    source_xmin: Option<u32>,
    source_ymin: Option<u32>,
    source_xmax: Option<u32>,
    source_ymax: Option<u32>,
    clear_color: Option<[u32; 4]>,
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
                    state.control_format = a2d_control_format(packet.payload[0]);
                }
                GRAS_A2D_BLT_CNTL => state.gras_control = Some(packet.payload[0]),
                RB_A2D_PIXEL_CNTL => state.pixel_control = true,
                SP_A2D_OUTPUT_INFO => state.output_format = a2d_output_format(packet.payload[0]),
                RB_A2D_DEST_BUFFER_INFO => {
                    state.destination_format = a2d_destination_format(packet.payload[0])
                }
                RB_A2D_CLEAR_COLOR_DW0 => state.clear_color = packet.payload.try_into().ok(),
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
                TPL1_A2D_SRC_TEXTURE_INFO => state.source_info = Some(packet.payload[0]),
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
                    || state.control_format.is_none()
                    || state.output_format != state.control_format
                    || state.destination_format.map(|(format, _)| format) != state.control_format
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
                        format: state
                            .destination_format
                            .ok_or("qcom-adreno-a618: A2D destination format is missing")?
                            .0,
                        modifier: state
                            .destination_format
                            .ok_or("qcom-adreno-a618: A2D destination format is missing")?
                            .1,
                        row_pitch: state
                            .destination_pitch
                            .ok_or("qcom-adreno-a618: A2D destination pitch is missing")?,
                        required_width: (destination_br & 0xffff).saturating_add(1),
                        required_height: (destination_br >> 16).saturating_add(1),
                        depth_staging: false,
                    },
                )?;
                if state.solid == Some(false) {
                    let source_info = state
                        .source_info
                        .ok_or("qcom-adreno-a618: A2D source info is missing")?;
                    let (format, modifier) = a2d_source_format(source_info)
                        .ok_or("a618: unsupported A2D source format")?;
                    if Some(format) != state.control_format {
                        return Err("a618: A2D source and destination formats differ");
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
                    if format == LinearImageFormat::Depth32Float
                        && (source_info & (1 << 16) != 0
                            || state.rb_control.unwrap() & 7 != 0
                            || required_width - source_xmin
                                != (destination_br & 0xffff) - (destination_tl & 0xffff) + 1
                            || required_height - source_ymin
                                != (destination_br >> 16) - (destination_tl >> 16) + 1)
                    {
                        return Err("a618: D32 transfer must be an unfiltered same-size copy");
                    }
                    set_a2d_expectation(
                        addresses,
                        state
                            .source_word
                            .ok_or("qcom-adreno-a618: A2D source base is missing")?,
                        A2dExpectation {
                            format,
                            modifier,
                            row_pitch: state
                                .source_pitch
                                .ok_or("qcom-adreno-a618: A2D source pitch is missing")?,
                            required_width: width,
                            required_height: height,
                            depth_staging: format == LinearImageFormat::Depth32Float
                                && modifier == ImageModifier::Linear,
                        },
                    )?;
                } else {
                    let color = state
                        .clear_color
                        .ok_or("qcom-adreno-a618: A2D solid blit clear color is missing")?;
                    let valid = match state.control_format {
                        Some(LinearImageFormat::Bgra8Unorm) => color.iter().all(|v| *v <= 0xff),
                        Some(LinearImageFormat::Depth32Float) => {
                            color[1..] == [0; 3]
                                && f32::from_bits(color[0]).is_finite()
                                && (0.0..=1.0).contains(&f32::from_bits(color[0]))
                        }
                        Some(LinearImageFormat::Rgba16Float) => {
                            color.iter().all(|v| *v <= 0xffff && v & 0x7c00 != 0x7c00)
                        }
                        Some(LinearImageFormat::Rgba32Float) => {
                            color.iter().all(|v| f32::from_bits(*v).is_finite())
                        }
                        _ => false,
                    };
                    if !valid {
                        return Err("a618: clear color does not match image format");
                    }
                }
                // CP_SET_MARKER persists across a batch of solid clears.
                // Bounds, addresses and format still need fresh authority
                // for every BLIT; only retain the current render mode.
                state = A2dState {
                    render_mode: state.render_mode,
                    ..A2dState::default()
                };
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

fn segment_shader_preload_word(
    segment: &SegmentIndex<'_, '_, '_>,
    wanted_opcode: u8,
    wanted_block: u32,
) -> Option<u32> {
    segment
        .packets
        .iter()
        .copied()
        .find(|candidate| {
            matches!(candidate.header, Header::Type7 { opcode, .. } if opcode == wanted_opcode)
                && candidate.payload.len() == 3
                && (candidate.payload[0] >> 14) & 3 == 0
                && (candidate.payload[0] >> 16) & 3 == 2
                && (candidate.payload[0] >> 18) & 0xf == wanted_block
        })
        .map(|candidate| candidate.word_offset + 2)
}

fn relocation_is_shader(
    relocations: &[Relocation],
    word_offset: u32,
    expected: ShaderVariant,
) -> bool {
    relocation_at(relocations, word_offset)
        .is_some_and(|relocation| relocation.source == RelocationSource::CanonicalShader(expected))
}

fn segment_shader_relocations_match(
    segment: &SegmentIndex<'_, '_, '_>,
    relocations: &[Relocation],
    variant: PipelineVariant,
) -> bool {
    let link = link_meta(variant);
    let Some((_, vs_program)) =
        segment_shader_program_layout(segment, SP_VS_PROGRAM_COUNTER_OFFSET)
    else {
        return false;
    };
    let Some((_, fs_program)) =
        segment_shader_program_layout(segment, SP_PS_PROGRAM_COUNTER_OFFSET)
    else {
        return false;
    };
    let Some(vs_preload) = segment_shader_preload_word(segment, CP_LOAD_STATE6_GEOM, 8) else {
        return false;
    };
    let Some(fs_preload) = segment_shader_preload_word(segment, CP_LOAD_STATE6_FRAG, 12) else {
        return false;
    };
    relocation_is_shader(relocations, vs_program, link.vs)
        && relocation_is_shader(relocations, vs_preload, link.vs)
        && relocation_is_shader(relocations, fs_program, link.fs)
        && relocation_is_shader(relocations, fs_preload, link.fs)
}

/// Validate the compact delta emitted after one complete canonical draw.
///
/// No shader-program register or preload is accepted here. Therefore the
/// exact immutable shader pair validated for the preceding draw remains
/// active, while only addresses, bounds, constants, texture bindings, and
/// their required synchronization may vary. Every packet still passes the
/// production allowlist and relocation-range validator before this check.
fn segment_matches_retained_pipeline(segment: &SegmentIndex<'_, '_, '_>) -> bool {
    segment.packets.iter().all(|packet| match packet.header {
        Header::Type4 { register, .. } => matches!(
            register,
            RB_MRT_PITCH
                | RB_MRT_BASE
                | GRAS_CL_VIEWPORT_XOFFSET
                | GRAS_SC_SCREEN_SCISSOR_TL
                | GRAS_SC_VIEWPORT_SCISSOR_TL
                | GRAS_SC_WINDOW_SCISSOR_TL
                | VFD_INDEX_OFFSET
                | VFD_VERTEX_BUFFER_BASE
                | VFD_VERTEX_BUFFER_SIZE
                | SP_PS_SAMPLER_BASE
                | SP_PS_TEXMEMOBJ_BASE
        ),
        Header::Type7 { opcode, .. } => match opcode {
            CP_LOAD_STATE6_GEOM => {
                packet.payload.len() == 23
                    && packet.payload[0] == ((1 << 14) | (8 << 18) | (5 << 22))
                    && packet.payload[1..3] == [0, 0]
            }
            CP_LOAD_STATE6_FRAG => {
                ((packet.payload.len() == 23
                    && packet.payload[0] == ((1 << 14) | (12 << 18) | (5 << 22))
                    || packet.payload.len() == 35
                        && packet.payload[0] == ((1 << 14) | (12 << 18) | (8 << 22)))
                    && packet.payload[1..3] == [0, 0])
                    || (packet.payload.len() == 3
                        && matches!(
                            packet.payload[0],
                            value if value == ((2 << 16) | (4 << 18) | (1 << 22))
                                || value
                                    == ((1 << 14) | (2 << 16) | (4 << 18) | (1 << 22))
                        )
                        && packet.payload[1..3] == [0, 0])
            }
            CP_MEM_WRITE => true,
            opcode::WAIT_FOR_IDLE | opcode::WAIT_MEM_WRITES => true,
            opcode::EVENT_WRITE => packet.payload == [EVENT_CACHE_INVALIDATE],
            _ => false,
        },
    })
}

fn segment_depth_state_is_canonical(segment: &SegmentIndex<'_, '_, '_>) -> bool {
    let reg = |wanted| segment.reg(wanted).map(|(_, payload)| payload);
    if reg(RB_DEPTH_PLANE_CNTL) != Some(&[0])
        || reg(GRAS_SU_DEPTH_PLANE_CNTL) != Some(&[0])
        || reg(RB_DEPTH_BOUND_MIN) != Some(&[0, 1.0_f32.to_bits()])
        || reg(RB_STENCIL_BUFFER_INFO) != Some(&[0])
    {
        return false;
    }
    let Some(info) = reg(RB_DEPTH_BUFFER_INFO) else {
        return false;
    };
    if info == [0; 6] {
        return reg(RB_DEPTH_CNTL) == Some(&[0])
            && reg(GRAS_SU_DEPTH_CNTL) == Some(&[0])
            && reg(GRAS_SU_DEPTH_BUFFER_INFO) == Some(&[0])
            && reg(RB_DEPTH_FLAG_BUFFER_BASE).is_none();
    }
    info.len() == 6
        && info[0] == DEPTH_FORMAT_32
        && info[1] != 0
        && info[1] <= 0x3fff
        && info[2] != 0
        && info[2] <= 0x0fff_ffff
        && info[3..] == [0; 3]
        && reg(RB_DEPTH_CNTL).is_some_and(|control| {
            control.len() == 1 && control[0] != 0 && safe_depth_control(control[0])
        })
        && reg(GRAS_SU_DEPTH_CNTL) == Some(&[1])
        && reg(GRAS_SU_DEPTH_BUFFER_INFO) == Some(&[DEPTH_FORMAT_32])
        && reg(RB_DEPTH_FLAG_BUFFER_BASE) == Some(&[0, 0, 0])
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
        && reg(SP_PS_OUTPUT_REG) == Some(fs.sp_ps_output_reg)
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
        && reg(RB_MRT_BUF_INFO).is_some_and(|p| p.len() == 1 && color_target_modifier(p[0]).is_some())
        && reg(RB_MRT_BASE_GMEM) == Some(&[0])
        && reg(RB_COLOR_FLAG_BUFFER_ADDR) == Some(&[0, 0, 0])
        && reg(GRAS_LRZ_MRT_BUFFER_INFO_0) == Some(&[FORMAT_8_8_8_8_UNORM])
        && reg(RB_SRGB_CNTL).is_some_and(|p| p.len() == 1 && p[0] <= 1)
        && reg(SP_SRGB_CNTL) == reg(RB_SRGB_CNTL)
        && reg(GRAS_CL_ARRAY_SIZE) == Some(&[0])
        && reg(RB_MRT_CONTROL).is_some_and(valid_blend_control)
        && reg(RB_MRT_CONTROL).is_some_and(|payload| {
            let enabled = u32::from(payload[0] & 3 == 3);
            reg(RB_BLEND_CNTL) == Some(&[0xffff_0000 | enabled])
                && reg(SP_BLEND_CNTL) == Some(&[enabled])
        })
        && reg(RB_DITHER_CNTL) == Some(&[0])
        && reg(RB_ALPHA_TEST_CNTL) == Some(&[0])
        && reg(RB_STENCIL_CNTL) == Some(&[0])
        && reg(GRAS_SU_STENCIL_CNTL) == Some(&[0])
        && reg(RB_STENCIL_REF_CNTL) == Some(&[0])
        && reg(RB_STENCIL_MASK) == Some(&[0, 0])
        && segment_depth_state_is_canonical(segment)
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
                } else if fixed.stride == 40 {
                    matches!(p[0], 0x2010 | 0x2012)
                } else {
                    p[0] == 0x2010
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
        && (!fixed.uses_sampler
            || (reg(SP_PS_SAMPLER_BASE).is_some() && reg(SP_PS_TEXMEMOBJ_BASE).is_some()))
}

fn valid_blend_control(payload: &[u32]) -> bool {
    let [control, blend] = *payload else {
        return false;
    };
    if matches!(control, 0x7e0 | 0xe0 | 0x1e0) {
        return blend == 0x0001_0001;
    }
    if !matches!(control, 0x7e3 | 0xe3 | 0x1e3) || blend & 0xe000_e000 != 0 {
        return false;
    }
    // Exclude reserved, constant-color and dual-source factors: their state
    // is not part of this dialect. All ordinary SGFX factors are bounded.
    [blend & 0xffff, blend >> 16].into_iter().all(|component| {
        matches!(component & 0x1f, 0 | 1 | 4..=11)
            && matches!((component >> 8) & 0x1f, 0 | 1 | 4..=11)
            && ((component >> 5) & 7) <= 4
    })
}

fn valid_native_blend_control(payload: &[u32]) -> bool {
    let [control, blend] = *payload else {
        return false;
    };
    // Per-component masks may differ between MRT slots. Preserve the same
    // bounded blend equations/factors and fixed COPY ROP as canonical draws.
    control & !0x7ff == 0 && valid_blend_control(&[control | (15 << 7), blend])
}

/// Reject impossible pipeline candidates using state that the full matcher
/// already requires.  Stride, vertex fetch layout, sampler use,
/// and the fragment program's initial texture-load command form a unique
/// identity for every supported canonical pipeline.  This keeps the expensive
/// packet-scanning matcher to one candidate per draw.  It remains only a
/// prefilter: every surviving candidate still passes the exact canonical-state
/// matcher below. Blend state is validated independently of shader identity.
fn segment_may_match_pipeline(
    segment: &SegmentIndex<'_, '_, '_>,
    variant: PipelineVariant,
) -> bool {
    let fixed = pipeline_state_meta(variant);
    let ShaderMeta::Fragment(fs) = shader_meta(link_meta(variant).fs) else {
        return false;
    };
    let reg = |wanted| segment.reg(wanted).map(|(_, payload)| payload);
    reg(VFD_VERTEX_BUFFER_SIZE)
        .is_some_and(|payload| payload.len() == 2 && payload[1] == fixed.stride)
        && reg(VFD_FETCH_INSTR) == Some(fixed.vfd_fetch)
        && reg(SP_PS_TSIZE) == Some(&[u32::from(fixed.uses_sampler)])
        && reg(SP_PS_OUTPUT_REG) == Some(fs.sp_ps_output_reg)
        && reg(SP_PS_INITIAL_TEX_LOAD_CNTL).is_some_and(|payload| {
            payload.first() == Some(&fs.initial_tex_load_cntl)
                && payload.get(1..) == Some(fs.initial_tex_load_cmd)
        })
        && reg(RB_MRT_CONTROL).is_some_and(valid_blend_control)
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

#[derive(Clone, Copy)]
struct ValidatedTargetState {
    screen: [u32; 2],
    viewport: [u32; 2],
}

fn validate_native_graphics_program(
    segment: &SegmentIndex<'_, '_, '_>,
    relocations: &[Relocation],
) -> Result<(), &'static str> {
    // Native program allocations may include rodata, but every fetch
    // must fit the authorized code allocation and use the same base
    // as the preload. Do not infer safety from compiler metadata.
    for (program, size_register, preload_opcode, block) in [
        (
            SP_VS_PROGRAM_COUNTER_OFFSET,
            SP_VS_INSTR_SIZE,
            CP_LOAD_STATE6_GEOM,
            8,
        ),
        (
            SP_PS_PROGRAM_COUNTER_OFFSET,
            SP_PS_INSTR_SIZE,
            CP_LOAD_STATE6_FRAG,
            12,
        ),
    ] {
        let (_, base_word) = submit_private::program_layout(&segment, program).unwrap();
        let base = relocation_at(relocations, base_word)
            .ok_or("a618: shader program relocation is missing")?;
        if matches!(base.source, RelocationSource::ShaderAttachment(_)) {
            let preload_word =
                segment_shader_preload_word(&segment, preload_opcode, block).unwrap();
            let preload = relocation_at(relocations, preload_word)
                .ok_or("a618: native shader preload is missing")?;
            let (_, size) = segment
                .reg(size_register)
                .ok_or("a618: native shader instruction size missing")?;
            if base.source != preload.source
                || base.resource_offset != preload.resource_offset
                || base.required_size != preload.required_size
                || size.len() != 1
                || u64::from(size[0]) * 128 > base.required_size
            {
                return Err("a618: native shader program/preload size mismatch");
            }
        }
    }
    Ok(())
}

fn validate_native_graphics_sequences(
    packets: &[Packet<'_>],
    relocations: &[Relocation],
) -> Result<(), &'static str> {
    let mut registers = Vec::new();
    let mut start = 0;
    let mut initialized = false;
    for (index, packet) in packets.iter().enumerate() {
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_DRAW_INDX_OFFSET,
                ..
            }
        ) {
            continue;
        }
        let segment = SegmentIndex::new(&packets[start..index], &mut registers);
        let fields = [
            submit_private::program_layout(&segment, SP_VS_PROGRAM_COUNTER_OFFSET).is_some(),
            submit_private::program_layout(&segment, SP_PS_PROGRAM_COUNTER_OFFSET).is_some(),
            segment_shader_preload_word(&segment, CP_LOAD_STATE6_GEOM, 8).is_some(),
            segment_shader_preload_word(&segment, CP_LOAD_STATE6_FRAG, 12).is_some(),
        ];
        if fields.iter().all(|v| *v) {
            validate_native_graphics_program(&segment, relocations)?;
            initialized = true;
        } else if !initialized
            || fields.iter().any(|v| *v)
            || segment.reg(SP_VS_INSTR_SIZE).is_some()
            || segment.reg(SP_PS_INSTR_SIZE).is_some()
        {
            return Err("a618: incomplete native graphics program transition");
        }
        start = index + 1;
    }
    Ok(())
}

/// Track only framebuffer state written in this submit. The private domain
/// bounds shader memory accesses, while typed image authority also prevents a
/// PM4 render target from inventing a format, pitch or subresource footprint.
fn validate_native_render_targets(
    packets: &[Packet<'_>],
    addresses: &mut [AddressField],
) -> Result<(), &'static str> {
    let mut info = [None; 8];
    let mut pitch = [None; 8];
    let mut base = [None; 8];
    let mut output = [None; 8];
    let mut rb_count = None;
    let mut sp_count = None;
    let mut viewport = None;
    let mut depth = None;
    let mut depth_control = 0;
    let mut poly_offset = false;
    let mut bias_initialized = false;
    for packet in packets {
        if let Header::Type4 { register, .. } = packet.header {
            let p = packet.payload;
            if (RB_MRT_CONTROL..RB_MRT_CONTROL + 64).contains(&register) {
                let slot = ((register - RB_MRT_CONTROL) / 8) as usize;
                match (register - RB_MRT_CONTROL) % 8 {
                    2 => info[slot] = Some(p[0]),
                    3 => pitch[slot] = Some([p[0], p[1]]),
                    5 => base[slot] = Some(packet.word_offset + 1),
                    _ => {}
                }
            } else if (SP_PS_MRT_REG..SP_PS_MRT_REG + 8).contains(&register) {
                for (index, value) in p.iter().enumerate() {
                    output[(register - SP_PS_MRT_REG) as usize + index] = Some(*value);
                }
            } else {
                match register {
                    RB_PS_MRT_CNTL => rb_count = Some(p[0]),
                    SP_PS_MRT_CNTL => sp_count = Some(p[0]),
                    GRAS_SC_VIEWPORT_SCISSOR_TL => viewport = Some([p[0], p[1]]),
                    RB_DEPTH_BUFFER_INFO => depth = Some((packet.word_offset + 4, p)),
                    RB_DEPTH_CNTL => depth_control = p[0],
                    GRAS_SU_CNTL => poly_offset = p[0] & (1 << 11) != 0,
                    GRAS_SU_POLY_OFFSET_SCALE => bias_initialized = true,
                    _ => {}
                }
            }
            continue;
        }
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_DRAW_INDX_OFFSET,
                ..
            }
        ) {
            continue;
        }
        let count = rb_count.ok_or("a618: native MRT count is not initialized")?;
        if poly_offset && !bias_initialized {
            return Err("a618: native depth bias is not initialized in this submission");
        }
        if sp_count != Some(count) {
            return Err("a618: native SP/RB MRT counts disagree");
        }
        if count == 0 && depth_control == 0 {
            continue;
        }
        let [origin, bounds] = viewport.ok_or("a618: native target extent is missing")?;
        if origin != 0 {
            return Err("a618: native attachment scissor must start at zero");
        }
        let width = (bounds & 0xffff) + 1;
        let height = (bounds >> 16) + 1;
        let mut authorize = |word, expectation| {
            let address = address_by_word_mut(addresses, word)
                .ok_or("a618: native target lacks a typed relocation")?;
            if address.image.is_some_and(|old| old != expectation) {
                return Err("a618: native target layout changed without a new relocation");
            }
            address.image = Some(expectation);
            Ok(())
        };
        for slot in 0..count as usize {
            let (format, modifier) = info[slot]
                .and_then(native_color_target)
                .ok_or("a618: native MRT format is missing")?;
            if output[slot].and_then(native_color_format) != Some(format) {
                return Err("a618: native SP/RB MRT formats disagree");
            }
            let [row, array] = pitch[slot].ok_or("a618: native MRT pitch is missing")?;
            authorize(
                base[slot].ok_or("a618: native MRT base is missing")?,
                ImageExpectation {
                    mip_levels: 1,
                    format,
                    modifier,
                    row_pitch: row * 64,
                    width,
                    height,
                    exact_extent: true,
                    pitch_align: None,
                    array_pitch: Some(ArrayPitchExpectation {
                        bytes: u64::from(array) * 64,
                        alignment: 64,
                    }),
                },
            )?;
        }
        if depth_control != 0 {
            let (word, info) = depth.ok_or("a618: native depth target is missing")?;
            if info[0] != DEPTH_FORMAT_32 {
                return Err("a618: native depth target is disabled");
            }
            authorize(
                word,
                ImageExpectation {
                    mip_levels: 1,
                    format: LinearImageFormat::Depth32Float,
                    modifier: ImageModifier::Tile6_3Depth,
                    row_pitch: info[1] * 64,
                    width,
                    height,
                    exact_extent: true,
                    pitch_align: None,
                    array_pitch: Some(ArrayPitchExpectation {
                        bytes: u64::from(info[2]) * 64,
                        alignment: adreno_a6xx_layout::DEPTH32_LAYER_ALIGNMENT,
                    }),
                },
            )?;
        }
    }
    Ok(())
}

fn validate_3d_sequences(
    packets: &[Packet<'_>],
    addresses: &mut [AddressField],
    relocations: &[Relocation],
    require_canonical_pipeline: bool,
) -> Result<(), &'static str> {
    let mut segment_start = 0usize;
    let mut segment_registers = Vec::new();
    let mut retained_pipeline = None;
    let mut shader_state_initialized = false;
    let mut target_state = None;
    let mut depth_state_initialized = false;
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
        let retained_delta = segment_matches_retained_pipeline(&segment);
        if require_canonical_pipeline {
            let mut matched = None;
            for candidate in PipelineVariant::ALL {
                if segment_may_match_pipeline(&segment, candidate)
                    && segment_matches_pipeline(&segment, candidate)
                    && segment_shader_relocations_match(&segment, relocations, candidate)
                {
                    if matched.is_some() {
                        return Err("qcom-adreno-a618: ambiguous canonical 3D pipeline state");
                    }
                    matched = Some(candidate);
                }
            }
            match matched {
                Some(variant) => retained_pipeline = Some(variant),
                None if retained_pipeline.is_some() && retained_delta => {}
                None => {
                    return Err("qcom-adreno-a618: incomplete canonical 3D pipeline state");
                }
            }
        }

        // Shader state may be inherited only inside this one decoded submit,
        // and only across the restricted retained-state language above. A
        // partial program/preload update is never accepted.
        let shader_fields = [
            segment_shader_program_layout(&segment, SP_VS_PROGRAM_COUNTER_OFFSET).is_some(),
            segment_shader_program_layout(&segment, SP_PS_PROGRAM_COUNTER_OFFSET).is_some(),
            segment_shader_preload_word(&segment, CP_LOAD_STATE6_GEOM, 8).is_some(),
            segment_shader_preload_word(&segment, CP_LOAD_STATE6_FRAG, 12).is_some(),
        ];
        if shader_fields.iter().all(|present| *present) {
            validate_native_graphics_program(&segment, relocations)?;
            shader_state_initialized = true;
        } else if !(shader_fields.iter().all(|present| !*present)
            && shader_state_initialized
            && retained_delta)
        {
            return Err("qcom-adreno-a618: incomplete shader state transition");
        }

        let x = |word: u32| word & 0xffff;
        let y = |word: u32| word >> 16;
        let pitch = segment.reg(RB_MRT_PITCH);
        let base = segment.reg(RB_MRT_BASE);
        let clip_transform = segment.reg(GRAS_CL_VIEWPORT_XOFFSET);
        let screen = segment.reg(GRAS_SC_SCREEN_SCISSOR_TL);
        let viewport = segment.reg(GRAS_SC_VIEWPORT_SCISSOR_TL);
        let target_fields = [
            pitch.is_some(),
            base.is_some(),
            clip_transform.is_some(),
            screen.is_some(),
            viewport.is_some(),
        ];
        let target = if target_fields.iter().all(|present| *present) {
            let (_, pitch) = pitch.ok_or("qcom-adreno-a618: MRT layout is missing")?;
            let (base_packet, _) = base.ok_or("qcom-adreno-a618: MRT base is missing")?;
            let (_, clip_transform) =
                clip_transform.ok_or("qcom-adreno-a618: viewport transform is missing")?;
            let (_, screen) = screen.ok_or("qcom-adreno-a618: render bounds are missing")?;
            let (_, viewport) = viewport.ok_or("qcom-adreno-a618: viewport scissor is missing")?;
            let screen: [u32; 2] = screen
                .try_into()
                .map_err(|_| "qcom-adreno-a618: invalid render bounds")?;
            let viewport: [u32; 2] = viewport
                .try_into()
                .map_err(|_| "qcom-adreno-a618: invalid viewport scissor")?;
            if viewport[0] != 0
                || x(viewport[0]) > x(viewport[1])
                || y(viewport[0]) > y(viewport[1])
                || x(screen[0]) > x(screen[1])
                || y(screen[0]) > y(screen[1])
                || x(screen[0]) < x(viewport[0])
                || y(screen[0]) < y(viewport[0])
                || x(screen[1]) > x(viewport[1])
                || y(screen[1]) > y(viewport[1])
            {
                return Err("qcom-adreno-a618: unsafe 3D scissor state");
            }
            // The attachment scissor continues to authorize the complete image.
            // Explicit SGFX viewports may map into a smaller region, invert Y,
            // or reverse depth, but must remain finite and inside that image.
            let width = (x(viewport[1]) + 1) as f32;
            let height = (y(viewport[1]) + 1) as f32;
            let transform: [u32; 6] = clip_transform
                .try_into()
                .map_err(|_| "qcom-adreno-a618: invalid viewport transform")?;
            let [xo, xs, yo, ys, zo, zs] = transform.map(f32::from_bits);
            if ![xo, xs, yo, ys, zo, zs].iter().all(|v| v.is_finite())
                || xs <= 0.0
                || xo - xs < 0.0
                || xo + xs > width
                || yo - ys.abs() < 0.0
                || yo + ys.abs() > height
                || zo - zs.abs() < 0.0
                || zo + zs.abs() > 1.0
            {
                return Err("qcom-adreno-a618: unsafe 3D viewport transform");
            }
            let row_pitch = pitch[0]
                .checked_mul(64)
                .ok_or("qcom-adreno-a618: MRT pitch overflows")?;
            set_image_expectation(
                addresses,
                base_packet + 1,
                ImageExpectation {
                    mip_levels: 1,
                    format: LinearImageFormat::Bgra8Unorm,
                    modifier: segment
                        .reg(RB_MRT_BUF_INFO)
                        .and_then(|(_, values)| values.first().copied())
                        .and_then(color_target_modifier)
                        .ok_or("qcom-adreno-a618: invalid color attachment modifier")?,
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
            let target = ValidatedTargetState { screen, viewport };
            target_state = Some(target);
            target
        } else if target_fields.iter().all(|present| !*present) && retained_delta {
            target_state.ok_or("qcom-adreno-a618: target state is not initialized")?
        } else {
            return Err("qcom-adreno-a618: incomplete target state transition");
        };

        if let Some((depth_packet, depth_info)) = segment.reg(RB_DEPTH_BUFFER_INFO) {
            if !segment_depth_state_is_canonical(&segment) {
                return Err("qcom-adreno-a618: incomplete canonical depth state");
            }
            depth_state_initialized = true;
            if depth_info != [0; 6] {
                let width = (target.viewport[1] & 0xffff).saturating_add(1);
                let height = (target.viewport[1] >> 16).saturating_add(1);
                let row_pitch = depth_info[1]
                    .checked_mul(64)
                    .ok_or("qcom-adreno-a618: depth pitch overflows")?;
                let array_pitch = u64::from(depth_info[2]) << 6;
                let Some(layout) = adreno_a6xx_layout::depth32_tile6_3_layout(width, height) else {
                    return Err("qcom-adreno-a618: depth target layout overflows");
                };
                if row_pitch != layout.row_pitch || array_pitch != layout.layer_size {
                    return Err("qcom-adreno-a618: depth array pitch does not match target");
                }
                set_image_expectation(
                    addresses,
                    depth_packet + 4,
                    ImageExpectation {
                        mip_levels: 1,
                        format: LinearImageFormat::Depth32Float,
                        modifier: ImageModifier::Tile6_3Depth,
                        row_pitch,
                        width,
                        height,
                        exact_extent: true,
                        pitch_align: None,
                        array_pitch: Some(ArrayPitchExpectation {
                            bytes: array_pitch,
                            alignment: adreno_a6xx_layout::DEPTH32_LAYER_ALIGNMENT,
                        }),
                    },
                )?;
            }
        } else if !(retained_delta && depth_state_initialized) {
            return Err("qcom-adreno-a618: depth state is not initialized");
        }

        let (_, window) = segment
            .reg(GRAS_SC_WINDOW_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: window scissor is missing")?;
        if x(window[0]) > x(window[1])
            || y(window[0]) > y(window[1])
            || x(window[0]) < x(target.screen[0])
            || y(window[0]) < y(target.screen[0])
            || x(window[1]) > x(target.screen[1])
            || y(window[1]) > y(target.screen[1])
            || x(window[1]) > x(target.viewport[1])
            || y(window[1]) > y(target.viewport[1])
        {
            return Err("qcom-adreno-a618: unsafe 3D scissor state");
        }
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
                    mip_levels: ((descriptor[2] >> 16) & 0xf) + 1,
                    format: LinearImageFormat::Bgra8Unorm,
                    modifier: if descriptor[2] & 3 == 3 {
                        ImageModifier::Tile6_3Color
                    } else {
                        ImageModifier::Linear
                    },
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
        segment_start = packet_index + 1;
    }
    Ok(())
}

/// Descriptor initialization can precede all render passes. Validate every
/// descriptor's image extent and require each SP/indirect lookup to refer to
/// a state range initialized earlier in this submit, not an arbitrary buffer.
fn validate_texture_initialization(
    decoded: DecodedSubmit<'_>,
    packets: &[Packet<'_>],
    addresses: &mut [AddressField],
    relocations: &[Relocation],
) -> Result<(), &'static str> {
    let location = |word: u32, field_offset: u64| -> Result<(u64, u64), &'static str> {
        let index = relocations.partition_point(|r| r.pm4_word_offset < word);
        let relocation = relocations
            .get(index)
            .filter(|r| r.pm4_word_offset == word)
            .ok_or("qcom-adreno-a618: texture state lacks relocation")?;
        let RelocationSource::Attachment(index) = relocation.source else {
            return Err("qcom-adreno-a618: texture state is not an attachment");
        };
        let resource = decoded
            .resource(index as usize)
            .ok_or("qcom-adreno-a618: texture state attachment is missing")?;
        let offset = resource
            .range_offset
            .checked_add(relocation.resource_offset)
            .and_then(|offset| offset.checked_sub(field_offset))
            .ok_or("qcom-adreno-a618: invalid texture state offset")?;
        Ok((resource.attachment_token, offset))
    };
    let mut initialized = alloc::collections::BTreeSet::new();
    for packet in packets {
        let lookup = match packet.header {
            Header::Type7 {
                opcode: CP_MEM_WRITE,
                ..
            } => {
                let descriptor = packet.payload;
                let nv12 = is_nv12_descriptor(descriptor[2]);
                let width = descriptor[3] & 0x7fff;
                let height = (descriptor[3] >> 15) & 0x7fff;
                set_image_expectation(
                    addresses,
                    packet.word_offset + 7,
                    ImageExpectation {
                        mip_levels: if nv12 {
                            1
                        } else {
                            ((descriptor[2] >> 16) & 0xf) + 1
                        },
                        format: if nv12 {
                            LinearImageFormat::Nv12Y
                        } else {
                            LinearImageFormat::Bgra8Unorm
                        },
                        modifier: if descriptor[2] & 3 == 3 {
                            ImageModifier::Tile6_3Color
                        } else {
                            ImageModifier::Linear
                        },
                        row_pitch: (descriptor[4] >> 7) & 0x3f_ffff,
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
                if nv12 {
                    if width % 2 != 0 || height % 2 != 0 {
                        return Err("qcom-adreno-a618: odd NV12 extent");
                    }
                    set_image_expectation(
                        addresses,
                        packet.word_offset + 10,
                        ImageExpectation {
                            mip_levels: 1,
                            format: LinearImageFormat::Nv12Uv,
                            modifier: ImageModifier::Linear,
                            row_pitch: descriptor[8] >> 8,
                            width: width / 2,
                            height: height / 2,
                            exact_extent: true,
                            pitch_align: None,
                            array_pitch: None,
                        },
                    )?;
                }
                initialized.insert(location(packet.word_offset + 1, 0)?);
                None
            }
            Header::Type4 {
                register: SP_PS_TEXMEMOBJ_BASE,
                ..
            } => Some((packet.word_offset + 1, 0)),
            Header::Type4 {
                register: SP_PS_SAMPLER_BASE,
                ..
            } => Some((packet.word_offset + 1, 64)),
            Header::Type7 {
                opcode: CP_LOAD_STATE6_FRAG,
                ..
            } if packet.payload.len() == 3 && (packet.payload[0] >> 18) & 0xf == 4 => {
                let sampler = (packet.payload[0] >> 14) & 3 == 0;
                Some((packet.word_offset + 2, if sampler { 64 } else { 0 }))
            }
            _ => None,
        };
        if let Some((word, offset)) = lookup {
            if !initialized.contains(&location(word, offset)?) {
                return Err("qcom-adreno-a618: texture state lookup precedes initialization");
            }
        }
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
    let relocations: Vec<_> = (0..decoded.relocation_len())
        .filter_map(|i| decoded.relocation(i))
        .collect();
    for packet in Packets::new(&words) {
        let packet = packet.ok()?;
        if decoded.requires_isolation() {
            let result = match packet.header {
                Header::Type4 { register, .. } => validate_native_type4(
                    register,
                    packet.payload,
                    packet.word_offset,
                    &mut addresses,
                ),
                Header::Type7 { opcode, .. } => validate_native_type7(
                    opcode,
                    packet.payload,
                    packet.word_offset,
                    &mut addresses,
                    &relocations,
                ),
            };
            if let Some(result) = result {
                if result.is_ok() {
                    continue;
                }
                let (kind, selector) = match packet.header {
                    Header::Type4 { register, .. } => (RejectedPacketKind::Type4, register),
                    Header::Type7 { opcode, .. } => (RejectedPacketKind::Type7, u32::from(opcode)),
                };
                return Some(RejectedPacket {
                    kind,
                    selector,
                    word_offset: packet.word_offset,
                    payload_len: packet.payload.len() as u32,
                    first_value: packet.payload.first().copied(),
                });
            }
        }
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
    native: bool,
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
                if native {
                    if let Some(result) = validate_native_type4(
                        register,
                        packet.payload,
                        packet.word_offset,
                        &mut addresses,
                    ) {
                        result?;
                        continue;
                    }
                }
                validate_type4(register, packet.payload, packet.word_offset, &mut addresses)?
            }
            Header::Type7 { opcode, .. } => {
                if native {
                    if let Some(result) = validate_native_type7(
                        opcode,
                        packet.payload,
                        packet.word_offset,
                        &mut addresses,
                        relocations,
                    ) {
                        result?;
                        continue;
                    }
                }
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
    validate_memcpy_barriers(&packets, native)?;
    validate_ccu_transitions(&packets)?;
    validate_render_retirement(&packets)?;
    validate_a2d_sequences(&packets, &mut addresses)?;
    if !native {
        validate_texture_initialization(decoded, &packets, &mut addresses, relocations)?;
    }
    if native { submit_private::validate_sequences(&packets)?; }
    validate_compute_sequences(&packets, relocations)?;
    if native {
        // Arbitrary shader address arithmetic cannot be proven from PM4.
        // Native fetch/store authority is the private SMMU page set, not a
        // fixed pipeline template. Every PM4 address is still typed below.
        validate_native_graphics_sequences(&packets, relocations)?;
        validate_native_render_targets(&packets, &mut addresses)?;
    } else {
        validate_3d_sequences(
            &packets,
            &mut addresses,
            relocations,
            cfg!(feature = "strict-command-validation"),
        )?;
    }
    for (address, relocation) in addresses.iter().zip(relocations.iter().copied()) {
        if relocation.pm4_word_offset != address.word_offset
            || relocation.access != address.access
            || relocation.encoding != address.encoding
            || !address_source_matches(address.source, relocation.source)
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
        AddressEncoding::GpuVa49UboDescriptor => {
            if address & 15 != 0 || address >= (1_u64 << 49) {
                return Err("a618: UBO descriptor address is invalid");
            }
            let destination = words
                .get_mut(index..index + 2)
                .ok_or("a618: invalid UBO relocation offset")?;
            destination[0] = address as u32;
            destination[1] = (destination[1] & !0x1ffff) | ((address >> 32) as u32 & 0x1ffff);
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

fn image_footprint(image: LinearImage) -> Option<u64> {
    if let Some(texture) = image.texture {
        return Some(texture.total_size);
    }
    match (image.format, image.modifier) {
        (
            LinearImageFormat::Bgra8Unorm
            | LinearImageFormat::Nv12Y
            | LinearImageFormat::Nv12Uv
            | LinearImageFormat::Rgba16Float
            | LinearImageFormat::Rgba32Float,
            ImageModifier::Linear,
        ) => u64::from(image.row_pitch).checked_mul(u64::from(image.height)),
        (LinearImageFormat::Depth32Float, ImageModifier::Tile6_3Depth)
        | (LinearImageFormat::Bgra8Unorm, ImageModifier::Tile6_3Color) => {
            let layout = adreno_a6xx_layout::depth32_tile6_3_layout(image.width, image.height)?;
            (layout.row_pitch == image.row_pitch).then_some(layout.layer_size)
        }
        _ => None,
    }
}

/// Resolve only complete canonical subresources; a byte slice cannot invent
/// dimensions or expose padding as a render target.
fn image_range(image: LinearImage, offset: u64, size: u64) -> Option<LinearImage> {
    if let LinearImageFormat::Nv12 { uv_offset, uv_size } = image.format {
        if offset == 0 && size == image.visible_size {
            return Some(image);
        }
        let y_size = u64::from(image.row_pitch) * u64::from(image.height);
        if offset == 0 && size == y_size {
            return Some(LinearImage {
                format: LinearImageFormat::Nv12Y,
                visible_size: size,
                ..image
            });
        }
        if offset == uv_offset && size == uv_size {
            return Some(LinearImage {
                format: LinearImageFormat::Nv12Uv,
                width: image.width / 2,
                height: image.height / 2,
                visible_size: size,
                ..image
            });
        }
        return None;
    }

    if offset == 0 && size == image.visible_size {
        return Some(image);
    }
    if image.format == LinearImageFormat::Depth32Float {
        let layer_size = image_footprint(image)?;
        if size == layer_size
            && offset % layer_size == 0
            && offset.checked_add(size)? <= image.visible_size
        {
            return Some(LinearImage {
                visible_size: size,
                ..image
            });
        }
        return None;
    }
    let texture = image.texture?;
    let layer = u32::try_from(offset / u64::from(texture.layer_stride)).ok()?;
    if layer < texture.array_layers {
        for level in 0..texture.mip_levels {
            let sub = texture.subresource(level, layer)?;
            if sub.offset == offset && sub.size == size {
                return Some(LinearImage {
                    texture: None,
                    width: sub.width,
                    height: sub.height,
                    row_pitch: sub.row_pitch,
                    visible_size: sub.size,
                    ..image
                });
            }
        }
    }
    None
}

/// Decode, authorize, validate, and relocate into a kernel-owned dword vector.
pub(crate) fn validate_and_relocate(
    bytes: &[u8],
    resolve: impl FnMut(u64) -> Option<ResolvedResource>,
    resolve_shader: impl FnMut(ShaderVariant) -> Option<ResolvedResource>,
) -> Result<Vec<u32>, &'static str> {
    let decoded = adreno_a6xx_submit_wire::decode(bytes)
        .map_err(|_| "qcom-adreno-a618: invalid A6xx submit wire")?;
    validate_and_relocate_decoded(decoded, resolve, resolve_shader)
}

/// Reuse structural validation of the same immutable, kernel-owned wire.
/// DecodedSubmit can only be constructed by the complete wire decoder; this
/// entry still performs every resource, PM4, shader and relocation check.
pub(crate) fn validate_and_relocate_decoded(
    decoded: DecodedSubmit<'_>,
    mut resolve: impl FnMut(u64) -> Option<ResolvedResource>,
    mut resolve_shader: impl FnMut(ShaderVariant) -> Option<ResolvedResource>,
) -> Result<Vec<u32>, &'static str> {
    let native = decoded.requires_isolation();
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
        resources.push((
            ResolvedResource {
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
                linear_image: resolved
                    .linear_image
                    .and_then(|image| image_range(image, wire.range_offset, wire.range_size)),
            },
            resolved.linear_image.is_some(),
        ));
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
    let (mut words, addresses) = validate_pm4(decoded, &relocations, native)?;
    for (index, relocation) in relocations.iter().copied().enumerate() {
        let (resource, attached_image) = match relocation.source {
            RelocationSource::Attachment(resource_index)
            | RelocationSource::ShaderAttachment(resource_index) => *resources
                .get(resource_index as usize)
                .ok_or("qcom-adreno-a618: relocation resource index is invalid")?,
            RelocationSource::CanonicalShader(variant) => (
                resolve_shader(variant)
                    .ok_or("qcom-adreno-a618: canonical shader is unavailable")?,
                false,
            ),
        };
        if relocation.access & !resource.allowed_access != 0 {
            return Err("qcom-adreno-a618: relocation exceeds declared resource access");
        }
        if matches!(relocation.source, RelocationSource::ShaderAttachment(_))
            && (resource.linear_image.is_some()
                || resource
                    .gpu_va
                    .checked_add(relocation.resource_offset)
                    .is_none_or(|address| address & 127 != 0))
        {
            return Err("a618: native shader requires aligned buffer storage");
        }
        if native {
            submit_storage::validate(&words, &relocation, &resource, attached_image)?;
        }
        let address = addresses[index];
        if address.private_memory { submit_private::validate_resolved(&relocation, &resource, attached_image)?; }
        if address.required_size.is_some_and(|required| {
            if address.size_is_minimum
                || matches!(relocation.source, RelocationSource::ShaderAttachment(_))
            {
                relocation.required_size < required
            } else {
                relocation.required_size != required
            }
        }) {
            return Err("qcom-adreno-a618: relocation size does not match PM4 operation");
        }
        if address.requires_complete_linear_image {
            let expectation = address
                .a2d
                .ok_or("qcom-adreno-a618: A2D relocation lacks semantic bounds")?;
            let staging = expectation.depth_staging;
            let image = if staging {
                // This is a read-only linear R32 staging source, never a render
                // target or arbitrary image reinterpretation. Retain the original
                // attachment kind even when a forged image subrange lost metadata.
                let size = u64::from(expectation.row_pitch)
                    .checked_mul(u64::from(expectation.required_height))
                    .ok_or("a618: D32 staging footprint overflows")?;
                if attached_image
                    || relocation.access != ACCESS_READ
                    || expectation.row_pitch & 63 != 0
                    || relocation.required_size != size
                    || resource
                        .gpu_va
                        .checked_add(relocation.resource_offset)
                        .is_none_or(|base| base & 63 != 0)
                {
                    return Err("a618: invalid D32 linear staging authority");
                }
                LinearImage {
                    texture: None,
                    format: LinearImageFormat::Depth32Float,
                    modifier: ImageModifier::Linear,
                    width: expectation.required_width,
                    height: expectation.required_height,
                    row_pitch: expectation.row_pitch,
                    visible_size: size,
                }
            } else {
                resource
                    .linear_image
                    .and_then(|image| {
                        image_range(image, relocation.resource_offset, relocation.required_size)
                    })
                    .ok_or("qcom-adreno-a618: A2D address must cover one complete linear image")?
            };
            let row_bytes = image
                .width
                .checked_mul(image.format.texel_bytes())
                .ok_or("qcom-adreno-a618: image row size overflows")?;
            let layout_size = if staging {
                image.visible_size
            } else {
                image_footprint(image).ok_or("qcom-adreno-a618: image layout is not supported")?
            };
            if image.width == 0
                || image.height == 0
                || image.row_pitch < row_bytes
                || layout_size != image.visible_size
                || relocation.required_size != image.visible_size
            {
                return Err("qcom-adreno-a618: A2D relocation does not match image layout");
            }
            let accessed_bytes = if image.modifier == ImageModifier::Linear {
                u64::from(expectation.required_height.saturating_sub(1))
                    .checked_mul(u64::from(expectation.row_pitch))
                    .and_then(|bytes| {
                        u64::from(expectation.required_width)
                            .checked_mul(u64::from(image.format.texel_bytes()))
                            .and_then(|row| bytes.checked_add(row))
                    })
                    .ok_or("qcom-adreno-a618: A2D byte range overflows")?
            } else {
                image.visible_size
            };
            if expectation.row_pitch != image.row_pitch
                || expectation.format != image.format
                || expectation.modifier != image.modifier
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
            let image = resource
                .linear_image
                .and_then(|image| {
                    image_range(image, relocation.resource_offset, relocation.required_size)
                })
                .ok_or("qcom-adreno-a618: 3D image address must cover one complete linear image")?;
            let layout_size = image_footprint(image)
                .ok_or("qcom-adreno-a618: 3D image layout is not supported")?;
            if image.width == 0
                || image.height == 0
                || image.row_pitch
                    < image
                        .width
                        .checked_mul(image.format.texel_bytes())
                        .ok_or("qcom-adreno-a618: 3D row size overflows")?
                || layout_size != image.visible_size
                || relocation.required_size != image.visible_size
                || expectation.mip_levels != image.texture.map_or(1, |t| t.mip_levels)
                || expectation.format != image.format
                || expectation.modifier != image.modifier
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
                || expectation.pitch_align.is_some_and(|align| align != if image.modifier == ImageModifier::Tile6_3Color { 2 } else { 0 })
                || expectation.array_pitch.is_some_and(|pitch| {
                    image.texture.map_or(image.visible_size, |t| u64::from(t.layer_stride))
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
        Access, Capabilities, CompileInput, DepthAttachment, ImageMeta, ImageModifier, ObjectId,
        ObjectRef, Operation, PipelineId, PipelineMeta, PlaneLayout, RelocatablePm4, RenderPass,
        ResourceKind, ResourceMeta, compile,
    };
    use sgfx_core::ir::{
        AddressMode, BlendState, BufferUsage, Color, CompareFunction, CullMode, DepthLoadOp,
        DepthState, DrawUniforms, Extent2D, FilterMode, FragmentProgram, FrontFace, IndexFormat,
        LoadOp, PixelRect, PrimitiveTopology, RasterState, RenderPipelineDesc, SamplerDesc,
        StoreOp, TextureFormat, TextureSampleMode, TextureUsage, Transform, VertexAttribute,
        VertexBufferLayout, VertexFormat,
    };

    use super::{
        AddressSource, CP_MEMCPY, CP_MEMCPY_MAX_DWORDS, ImageModifier as KernelImageModifier,
        LinearImage, LinearImageFormat, RB_A2D_DEST_BUFFER_BASE, RejectedPacketKind,
        ResolvedResource, ShaderStage, address_source_matches, diagnose_rejected_packet,
        relocate_one, validate_and_relocate, validate_type7,
    };

    #[test]
    fn float_mip_and_array_ranges_expose_only_canonical_subresources() {
        for format in [
            LinearImageFormat::Rgba16Float,
            LinearImageFormat::Rgba32Float,
        ] {
            let texture = adreno_a6xx_layout::LinearTextureLayout::new_with_texel_bytes(
                65,
                17,
                4,
                3,
                format.texel_bytes(),
            )
            .unwrap();
            let image = LinearImage {
                texture: Some(texture),
                format,
                modifier: KernelImageModifier::Linear,
                width: 65,
                height: 17,
                row_pitch: texture.row_pitch,
                visible_size: texture.total_size,
            };
            for layer in 0..3 {
                for level in 0..4 {
                    let sub = texture.subresource(level, layer).unwrap();
                    let view = super::image_range(image, sub.offset, sub.size).unwrap();
                    assert_eq!(view.format, format);
                    assert_eq!(
                        (view.width, view.height, view.row_pitch),
                        (sub.width, sub.height, sub.row_pitch)
                    );
                    assert_eq!(super::image_footprint(view), Some(sub.size));
                    assert!(sub.row_pitch >= sub.width * format.texel_bytes());
                    assert!(super::image_range(image, sub.offset + 1, sub.size).is_none());
                    assert!(super::image_range(image, sub.offset, sub.size - 1).is_none());
                }
            }
            assert!(super::image_range(image, texture.total_size, 64).is_none());
        }
    }

    #[test]
    fn buffer_copy_and_global_barrier_pass_real_kernel_validation() {
        let resources = [
            ResourceMeta {
                id: SOURCE,
                size: 4096,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::COPY_SRC,
                },
            },
            ResourceMeta {
                id: TARGET,
                size: 4096,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::COPY_DST,
                },
            },
        ];
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &[],
            operations: &[
                Operation::CopyBufferToBuffer {
                    source: SOURCE,
                    source_offset: 64,
                    destination: TARGET,
                    destination_offset: 128,
                    size: 1028,
                },
                Operation::ResourceBarrier,
            ],
        })
        .unwrap();
        let bytes = wire_from_codegen(&artifact);
        let resolve = |attachment_token| {
            Some(ResolvedResource {
                attachment_token,
                gpu_va: 0x1_0000_0000 + attachment_token * 0x1_0000,
                allocation_size: 4096,
                allowed_access: if attachment_token == token(ObjectRef::External(SOURCE)) {
                    ACCESS_READ
                } else {
                    ACCESS_WRITE
                },
                linear_image: None,
            })
        };
        let words = validate_no_shaders(&bytes, resolve).unwrap();
        let packets: std::vec::Vec<_> = Packets::new(&words).map(Result::unwrap).collect();
        let copies: std::vec::Vec<_> = packets
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: CP_MEMCPY,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(copies.len(), 3);
        let mut copied = 0u64;
        for (i, p) in copies {
            assert!(p.payload[0] <= CP_MEMCPY_MAX_DWORDS);
            let src = u64::from(p.payload[1]) | (u64::from(p.payload[2]) << 32);
            let dst = u64::from(p.payload[3]) | (u64::from(p.payload[4]) << 32);
            assert_eq!(
                src,
                resolve(token(ObjectRef::External(SOURCE))).unwrap().gpu_va + 64 + copied
            );
            assert_eq!(
                dst,
                resolve(token(ObjectRef::External(TARGET))).unwrap().gpu_va + 128 + copied
            );
            assert!(matches!(
                packets[i + 1].header,
                Header::Type7 {
                    opcode: opcode::WAIT_MEM_WRITES,
                    count: 0
                }
            ));
            copied += u64::from(p.payload[0]) * 4;
        }
        assert_eq!(copied, 1028);
        // A forged destination without write authority must still fail.
        assert!(
            validate_no_shaders(&bytes, |token| {
                let mut r = resolve(token)?;
                r.allowed_access = ACCESS_READ;
                Some(r)
            })
            .is_err()
        );
    }

    #[test]
    fn mip_and_cube_layer_views_are_bounded_by_canonical_storage() {
        let texture = adreno_a6xx_layout::LinearTextureLayout::new(64, 64, 7, 6).unwrap();
        let image = LinearImage {
            texture: Some(texture),
            format: LinearImageFormat::Bgra8Unorm,
            modifier: KernelImageModifier::Linear,
            width: 64,
            height: 64,
            row_pitch: texture.row_pitch,
            visible_size: texture.total_size,
        };
        for layer in 0..6 {
            for level in 0..7 {
                let sub = texture.subresource(level, layer).unwrap();
                let view = super::image_range(image, sub.offset, sub.size).unwrap();
                assert_eq!(
                    (view.width, view.height, view.row_pitch),
                    (sub.width, sub.height, sub.row_pitch)
                );
                assert!(super::image_range(image, sub.offset + 4, sub.size).is_none());
                assert!(super::image_range(image, sub.offset, sub.size + 4).is_none());
            }
        }
        assert!(super::image_range(image, texture.total_size, 64).is_none());
    }

    #[test]
    fn depth_array_views_retain_whole_tiled_layers_only() {
        let layout = adreno_a6xx_layout::depth32_tile6_3_layout(65, 17).unwrap();
        let image = LinearImage {
            texture: None,
            format: LinearImageFormat::Depth32Float,
            modifier: KernelImageModifier::Tile6_3Depth,
            width: 65,
            height: 17,
            row_pitch: layout.row_pitch,
            visible_size: layout.layer_size * 6,
        };
        for layer in 0..6 {
            let offset = layout.layer_size * layer;
            let view = super::image_range(image, offset, layout.layer_size).unwrap();
            assert_eq!(super::image_footprint(view), Some(view.visible_size));
            assert!(super::image_range(image, offset + 64, layout.layer_size).is_none());
            assert!(super::image_range(image, offset, layout.layer_size - 64).is_none());
        }
        assert!(super::image_range(image, image.visible_size, layout.layer_size).is_none());
    }

    #[test]
    fn gpu_filtered_blits_and_flips_pass_kernel_validation() {
        let resources = [
            image(SOURCE, TextureUsage::COPY_SRC),
            image(TARGET, TextureUsage::COPY_DST),
        ];
        for filter in [FilterMode::Nearest, FilterMode::Linear] {
            for flips in [[false, false], [true, false], [false, true], [true, true]] {
                let ops = [Operation::BlitTexture {
                    source: SOURCE,
                    destination: TARGET,
                    source_rect: PixelRect::new(0, 0, 16, 16).unwrap(),
                    destination_rect: PixelRect::new(0, 0, 8, 8).unwrap(),
                    filter,
                    flips,
                }];
                let artifact = compile(CompileInput {
                    capabilities: Capabilities::a618(512 * 1024, 4096),
                    resources: &resources,
                    pipelines: &[],
                    operations: &ops,
                })
                .unwrap();
                accept_codegen(&artifact).unwrap();
            }
        }
    }

    #[test]
    fn mip_views_of_every_cube_face_blit_through_real_relocation_validation() {
        let texture = adreno_a6xx_layout::LinearTextureLayout::new(16, 16, 5, 6).unwrap();
        for layer in 0..6 {
            for level in 0..5 {
                let sub = texture.subresource(level, layer).unwrap();
                let mut source = image(SOURCE, TextureUsage::COPY_SRC);
                source.size = texture.total_size;
                let ResourceKind::Image(ref mut meta) = source.kind else {
                    unreachable!()
                };
                meta.extent = Extent2D::new(sub.width, sub.height).unwrap();
                meta.planes[0] = PlaneLayout {
                    offset: sub.offset,
                    stride: sub.row_pitch,
                    size: sub.size,
                };
                let resources = [source, image(TARGET, TextureUsage::COPY_DST)];
                let ops = [Operation::BlitTexture {
                    source: SOURCE,
                    destination: TARGET,
                    source_rect: PixelRect::new(0, 0, sub.width, sub.height).unwrap(),
                    destination_rect: PixelRect::new(0, 0, 16, 16).unwrap(),
                    filter: FilterMode::Linear,
                    flips: [false; 2],
                }];
                let artifact = compile(CompileInput {
                    capabilities: Capabilities::a618(512 * 1024, 4096),
                    resources: &resources,
                    pipelines: &[],
                    operations: &ops,
                })
                .unwrap();
                accept_codegen_with_texture(
                    &artifact,
                    LinearImageFormat::Depth32Float,
                    Some(texture),
                )
                .unwrap();
                let mut invalid = artifact.clone();
                for access in &mut invalid.accesses {
                    if access.object == ObjectRef::External(SOURCE) {
                        access.offset += 4;
                    }
                }
                for fixup in &mut invalid.fixups {
                    if fixup.object == ObjectRef::External(SOURCE) {
                        fixup.object_offset += 4;
                    }
                }
                assert!(
                    accept_codegen_with_texture(
                        &invalid,
                        LinearImageFormat::Depth32Float,
                        Some(texture)
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn mip_chain_sampling_and_lod_clamps_reach_the_kernel() {
        let layout = adreno_a6xx_layout::LinearTextureLayout::new(16, 16, 5, 1).unwrap();
        let mut sampled = image(SOURCE, TextureUsage::SAMPLED);
        sampled.size = layout.total_size;
        let ResourceKind::Image(ref mut meta) = sampled.kind else {
            unreachable!()
        };
        meta.texture = Some(layout);
        meta.planes[0].size = layout.total_size;
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            sampled,
            ResourceMeta {
                id: BUFFER,
                size: 72,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
        ];
        let pipeline = PipelineMeta {
            id: PipelineId::new(0),
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
        let sampler = SamplerDesc::new(
            FilterMode::Linear,
            FilterMode::Linear,
            AddressMode::ClampToEdge,
            AddressMode::ClampToEdge,
        )
        .with_mip_filter(FilterMode::Linear, 1.5, 4.0)
        .unwrap();
        let ops = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Load,
                store: StoreOp::Store,
                depth: None,
            }),
            Operation::SetPipeline(pipeline.id),
            Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            },
            Operation::SetTexture(SOURCE),
            Operation::SetSampler(sampler),
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
            pipelines: &[pipeline],
            operations: &ops,
        })
        .unwrap();
        accept_codegen_with_texture(&artifact, LinearImageFormat::Depth32Float, Some(layout))
            .unwrap();
        let descriptor = Packets::new(&artifact.words)
            .map(Result::unwrap)
            .find(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: super::CP_MEM_WRITE,
                        ..
                    }
                ) && p.payload.len() == 22
            })
            .unwrap();
        assert_eq!((descriptor.payload[2] >> 16) & 15, 4);
        assert_eq!(descriptor.payload[18] & 1, 1);
        assert_eq!(descriptor.payload[19], 0x1800_0000 | 0x40000);
        let descriptor_word = descriptor.word_offset as usize + 3;
        let mut invalid = artifact.clone();
        invalid.words[descriptor_word] ^= 1 << 16;
        assert!(
            accept_codegen_with_texture(&invalid, LinearImageFormat::Depth32Float, Some(layout))
                .is_err()
        );
    }

    #[test]
    fn canonical_shader_relocations_cannot_cross_pipeline_stages() {
        use adreno_a6xx_shader_pack::ShaderVariant;

        assert!(address_source_matches(
            AddressSource::CanonicalShader(ShaderStage::Vertex),
            RelocationSource::CanonicalShader(ShaderVariant::VsStride16Pos2),
        ));
        assert!(address_source_matches(
            AddressSource::CanonicalShader(ShaderStage::Fragment),
            RelocationSource::CanonicalShader(ShaderVariant::FsSolid),
        ));
        assert!(!address_source_matches(
            AddressSource::CanonicalShader(ShaderStage::Vertex),
            RelocationSource::CanonicalShader(ShaderVariant::FsSolid),
        ));
        assert!(!address_source_matches(
            AddressSource::CanonicalShader(ShaderStage::Fragment),
            RelocationSource::CanonicalShader(ShaderVariant::VsStride16Pos2),
        ));
    }

    #[test]
    fn canonical_pipeline_prefilter_identity_is_unique() {
        use adreno_a6xx_shader_pack::{
            PipelineVariant, ShaderMeta, link_meta, pipeline_state_meta, shader_meta,
        };

        for variant in PipelineVariant::ALL {
            let fixed = pipeline_state_meta(variant);
            let ShaderMeta::Fragment(fragment) = shader_meta(link_meta(variant).fs) else {
                panic!("canonical pipeline has a non-fragment pixel stage");
            };
            let identity_matches = PipelineVariant::ALL
                .into_iter()
                .filter(|candidate| {
                    let candidate_fixed = pipeline_state_meta(*candidate);
                    let ShaderMeta::Fragment(candidate_fragment) =
                        shader_meta(link_meta(*candidate).fs)
                    else {
                        return false;
                    };
                    candidate_fixed == fixed
                        && candidate_fragment.sp_ps_output_reg == fragment.sp_ps_output_reg
                        && candidate_fragment.initial_tex_load_cntl
                            == fragment.initial_tex_load_cntl
                        && candidate_fragment.initial_tex_load_cmd == fragment.initial_tex_load_cmd
                })
                .count();
            assert_eq!(
                identity_matches, 1,
                "canonical pipeline identity for {variant:?} is ambiguous"
            );
        }
    }

    const TARGET: ObjectId = ObjectId::new(0);
    const SOURCE: ObjectId = ObjectId::new(1);
    const BUFFER: ObjectId = ObjectId::new(2);
    const ALPHA: ObjectId = ObjectId::new(3);
    const DEPTH: ObjectId = ObjectId::new(4);

    fn token(object: ObjectRef) -> u64 {
        match object {
            ObjectRef::External(id) => u64::from(id.raw()) + 1,
            ObjectRef::Generated(id) => 0x1000 + u64::from(id.raw()),
            ObjectRef::CanonicalShader(_) => panic!("canonical shaders never have tokens"),
            #[allow(unreachable_patterns)]
            _ => panic!("native shaders use the native test wire encoder"),
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
                        #[allow(unreachable_patterns)]
                        _ => panic!("native encoding uses the native test wire encoder"),
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
                conversion: None,
                texture: None,
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

    fn depth_image(id: ObjectId) -> ResourceMeta {
        ResourceMeta {
            id,
            size: 0x1000,
            kind: ResourceKind::Image(ImageMeta {
                conversion: None,
                texture: None,
                format: TextureFormat::Depth32Float,
                storage_format: TextureFormat::Depth32Float,
                extent: Extent2D::new(16, 16).unwrap(),
                usage: TextureUsage::RENDER_ATTACHMENT,
                modifier: ImageModifier::A6xxTile6_3Depth,
                planes: std::vec![PlaneLayout {
                    offset: 0,
                    stride: 256,
                    size: 0x1000,
                }],
            }),
        }
    }

    fn accept_codegen(artifact: &RelocatablePm4) -> Result<std::vec::Vec<u32>, &'static str> {
        accept_codegen_with_depth_format(artifact, LinearImageFormat::Depth32Float)
    }

    fn accept_codegen_with_depth_format(
        artifact: &RelocatablePm4,
        depth_format: LinearImageFormat,
    ) -> Result<std::vec::Vec<u32>, &'static str> {
        accept_codegen_with_texture(artifact, depth_format, None)
    }

    fn accept_codegen_with_texture(
        artifact: &RelocatablePm4,
        depth_format: LinearImageFormat,
        texture: Option<adreno_a6xx_layout::LinearTextureLayout>,
    ) -> Result<std::vec::Vec<u32>, &'static str> {
        let bytes = wire_from_codegen(artifact);
        validate_and_relocate(
            &bytes,
            |attachment_token| {
                let is_depth = attachment_token == token(ObjectRef::External(DEPTH));
                let is_image = attachment_token == token(ObjectRef::External(TARGET))
                    || attachment_token == token(ObjectRef::External(SOURCE))
                    || attachment_token == token(ObjectRef::External(ALPHA))
                    || is_depth;
                let texture =
                    texture.filter(|_| attachment_token == token(ObjectRef::External(SOURCE)));
                Some(ResolvedResource {
                    attachment_token,
                    gpu_va: 0x1_0000_0000 + attachment_token * 0x1_0000,
                    allocation_size: texture.map_or(0x1000, |t| t.total_size),
                    allowed_access: ACCESS_READ | ACCESS_WRITE,
                    linear_image: is_image.then_some(LinearImage {
                        texture,
                        format: if is_depth {
                            depth_format
                        } else {
                            LinearImageFormat::Bgra8Unorm
                        },
                        modifier: if is_depth && depth_format == LinearImageFormat::Depth32Float {
                            KernelImageModifier::Tile6_3Depth
                        } else {
                            KernelImageModifier::Linear
                        },
                        width: 16,
                        height: 16,
                        row_pitch: if is_depth { 256 } else { 64 },
                        visible_size: texture
                            .map_or(if is_depth { 0x1000 } else { 0x400 }, |t| t.total_size),
                    }),
                })
            },
            |variant| {
                Some(ResolvedResource {
                    attachment_token: 0,
                    gpu_va: 0x4_0000_0000 + variant.offset() as u64,
                    allocation_size: variant.size() as u64,
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
                    texture: None,
                    format: LinearImageFormat::Bgra8Unorm,
                    modifier: KernelImageModifier::Linear,
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
        // Use a valid addressed event so the validator reaches the ordering
        // check instead of rejecting the packet's missing timestamp bit.
        let pm4 = [
            type7(opcode::EVENT_WRITE, 4).unwrap(),
            super::EVENT_CCU_FLUSH_COLOR_TS | super::EVENT_WRITE_TIMESTAMP,
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
    fn canonical_depth32_clear_test_and_write_are_accepted_end_to_end() {
        const PIPELINE: PipelineId = PipelineId::new(17);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            depth_image(DEPTH),
            ResourceMeta {
                id: BUFFER,
                size: 120,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
                },
            },
        ];
        let pipeline = RenderPipelineDesc::new(
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
        .unwrap()
        .with_depth_stencil(DepthState::new(
            TextureFormat::Depth32Float,
            CompareFunction::Less,
            true,
        ))
        .unwrap();
        let pipelines = [PipelineMeta {
            id: PIPELINE,
            descriptor: pipeline,
        }];
        let operations = [
            Operation::BeginRenderPass(RenderPass {
                target: TARGET,
                area: PixelRect::new(0, 0, 16, 16).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.1, 0.2, 0.3, 1.0).unwrap()),
                store: StoreOp::Store,
                depth: Some(DepthAttachment {
                    target: DEPTH,
                    load: DepthLoadOp::Clear(1.0),
                    store: StoreOp::Store,
                }),
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
        accept_codegen(&artifact).unwrap();
        // Partial, transparent color clears and independent load operations
        // must keep the same validated depth/alpha semantics.
        for color_clear in [false, true] {
            for depth_clear in [false, true] {
                let mut operations = operations.clone();
                let Operation::BeginRenderPass(ref mut pass) = operations[0] else {
                    unreachable!()
                };
                pass.area = PixelRect::new(2, 3, 8, 7).unwrap();
                pass.load = if color_clear {
                    LoadOp::Clear(Color::rgba(0.2, 0.4, 0.6, 0.25).unwrap())
                } else {
                    LoadOp::Load
                };
                pass.depth.as_mut().unwrap().load = if depth_clear {
                    DepthLoadOp::Clear(0.5)
                } else {
                    DepthLoadOp::Load
                };
                let part = compile(CompileInput {
                    capabilities: Capabilities::a618(512 * 1024, 4096),
                    resources: &resources,
                    pipelines: &pipelines,
                    operations: &operations,
                })
                .unwrap();
                accept_codegen(&part).unwrap();
            }
        }
        let packets = Packets::new(&artifact.words)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap();
        let first_blit = packets
            .iter()
            .position(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: opcode::BLIT,
                        ..
                    }
                )
            })
            .unwrap();
        let mut missing_idle = packets.clone();
        missing_idle.remove(first_blit + 1);
        assert!(super::validate_render_retirement(&missing_idle).is_err());
        let second_blit = packets
            .iter()
            .rposition(|packet| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: opcode::BLIT,
                        ..
                    }
                )
            })
            .unwrap();
        let mut missing_retirement = packets.clone();
        missing_retirement.drain(second_blit + 2..second_blit + 7);
        assert!(super::validate_render_retirement(&missing_retirement).is_err());
        let mut early_invalidate = packets.clone();
        early_invalidate.insert(first_blit + 2, packets[second_blit + 4]);
        assert!(super::validate_render_retirement(&early_invalidate).is_err());
        assert_eq!(
            accept_codegen_with_depth_format(&artifact, LinearImageFormat::Bgra8Unorm),
            Err("qcom-adreno-a618: A2D state exceeds the authorized image layout")
        );

        let mut unsafe_depth_control = artifact.clone();
        let depth_control_word_offset = Packets::new(&unsafe_depth_control.words)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .find(|packet| {
                matches!(
                    packet.header,
                    Header::Type4 {
                        register: super::RB_DEPTH_CNTL,
                        ..
                    }
                ) && packet.payload != [0]
            })
            .unwrap()
            .word_offset as usize;
        unsafe_depth_control.words[depth_control_word_offset + 1] |= 1 << 7;
        assert!(accept_codegen(&unsafe_depth_control).is_err());
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

        // A same-stage canonical relocation is not enough in strict mode:
        // the shader must be the exact program identified by the fixed
        // pipeline state. This also pins the authority inherited by compact
        // follow-up draws.
        let mut wrong_vertex_program = artifact.clone();
        let vertex_shader = wrong_vertex_program
            .fixups
            .iter_mut()
            .find(|fixup| {
                matches!(
                    fixup.object,
                    ObjectRef::CanonicalShader(
                        adreno_a6xx_shader_pack::ShaderVariant::VsStride40Pos4Color4
                    )
                )
            })
            .unwrap();
        vertex_shader.object =
            ObjectRef::CanonicalShader(adreno_a6xx_shader_pack::ShaderVariant::VsStride16Pos2);
        assert_eq!(
            accept_codegen(&wrong_vertex_program).is_err(),
            cfg!(feature = "strict-command-validation")
        );

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
        let idle_before_retirement_packets = Packets::new(&idle_before_retirement)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            super::validate_render_retirement(&idle_before_retirement_packets),
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
        let two_draw_packets = Packets::new(&two_draw_artifact.words)
            .collect::<Result<std::vec::Vec<_>, _>>()
            .unwrap();
        let draws = two_draw_packets
            .iter()
            .enumerate()
            .filter_map(|(index, packet)| {
                matches!(
                    packet.header,
                    Header::Type7 {
                        opcode: super::CP_DRAW_INDX_OFFSET,
                        ..
                    }
                )
                .then_some(index)
            })
            .collect::<std::vec::Vec<_>>();
        assert_eq!(draws.len(), 2);
        assert!(
            two_draw_packets[draws[0] + 1..draws[1]]
                .iter()
                .all(|packet| {
                    !matches!(
                        packet.header,
                        Header::Type4 {
                            register: super::RB_MRT_PITCH
                                | super::RB_MRT_BASE
                                | super::GRAS_CL_VIEWPORT_XOFFSET
                                | super::GRAS_SC_SCREEN_SCISSOR_TL
                                | super::GRAS_SC_VIEWPORT_SCISSOR_TL,
                            ..
                        }
                    )
                })
        );

        // Retained target state is all-or-nothing. Injecting only a new pitch
        // must not let a compact segment combine attacker-controlled target
        // state with the previously authorized base and viewport.
        let second_draw_word = two_draw_packets[draws[1]].word_offset as usize;
        let mut partial_target = two_draw_artifact.clone();
        partial_target.words.splice(
            second_draw_word..second_draw_word,
            [type4(super::RB_MRT_PITCH, 2).unwrap(), 1, 16],
        );
        for fixup in &mut partial_target.fixups {
            if fixup.word_offset as usize >= second_draw_word {
                fixup.word_offset += 3;
            }
        }
        assert!(accept_codegen(&partial_target).is_err());

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
        assert_eq!(
            accept_codegen(&wrong_final_marker).is_err(),
            cfg!(feature = "strict-command-validation")
        );

        let mut hostile = artifact.clone();
        let linkage = hostile
            .words
            .iter_mut()
            .find(|word| **word == 0x00ff_0408)
            .unwrap();
        *linkage ^= 1;
        assert_eq!(
            accept_codegen(&hostile).is_err(),
            cfg!(feature = "strict-command-validation")
        );

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
        assert_eq!(
            accept_codegen(&duplicate).is_err(),
            cfg!(feature = "strict-command-validation")
        );

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

        let mut nonfinite_viewport = artifact.clone();
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
        nonfinite_viewport.words[viewport.word_offset as usize + 4] = f32::INFINITY.to_bits();
        assert_eq!(
            accept_codegen(&nonfinite_viewport),
            Err("qcom-adreno-a618: unsafe 3D viewport transform")
        );

        for values in [
            [4.5, 2.0, 6.5, -3.0, 0.5, 0.25],
            [4.5, 2.0, 6.5, 3.0, 0.5, -0.25],
            [4.5, 2.0, 6.5, 0.0, 0.5, 0.0],
        ] {
            let mut explicit = artifact.clone();
            let start = viewport.word_offset as usize + 1;
            explicit.words[start..start + 6].copy_from_slice(&values.map(f32::to_bits));
            assert!(accept_codegen(&explicit).is_ok());
        }
        for values in [
            [1.0, 2.0, 6.5, -3.0, 0.5, 0.25],
            [4.5, 2.0, 15.0, -3.0, 0.5, 0.25],
            [4.5, 2.0, 6.5, -3.0, 0.8, 0.25],
            [f32::NAN, 2.0, 6.5, -3.0, 0.5, 0.25],
        ] {
            let mut invalid = artifact.clone();
            let start = viewport.word_offset as usize + 1;
            invalid.words[start..start + 6].copy_from_slice(&values.map(f32::to_bits));
            assert_eq!(
                accept_codegen(&invalid),
                Err("qcom-adreno-a618: unsafe 3D viewport transform")
            );
        }

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
        assert_eq!(
            accept_codegen(&bad_vfd_count).is_err(),
            cfg!(feature = "strict-command-validation")
        );
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
    fn tiled_color_clear_draw_and_sample_preserve_alpha_and_validate_layout() {
        let tiled = adreno_a6xx_layout::depth32_tile6_3_layout(64, 17).unwrap();
        let resources = [
            ResourceMeta {
                id: TARGET,
                size: tiled.layer_size,
                kind: ResourceKind::Image(ImageMeta {
                    conversion: None,
                    texture: None,
                    format: TextureFormat::Bgra8Unorm,
                    storage_format: TextureFormat::Bgra8Unorm,
                    extent: Extent2D::new(64, 17).unwrap(),
                    usage: TextureUsage::RENDER_ATTACHMENT | TextureUsage::SAMPLED,
                    modifier: ImageModifier::A6xxTile6_3Color,
                    planes: std::vec![PlaneLayout {
                        offset: 0,
                        stride: tiled.row_pitch,
                        size: tiled.layer_size
                    }],
                }),
            },
            ResourceMeta {
                id: SOURCE,
                size: 256 * 17,
                kind: ResourceKind::Image(ImageMeta {
                    conversion: None,
                    texture: None,
                    format: TextureFormat::Bgra8Unorm,
                    storage_format: TextureFormat::Bgra8Unorm,
                    extent: Extent2D::new(64, 17).unwrap(),
                    usage: TextureUsage::RENDER_ATTACHMENT,
                    modifier: ImageModifier::Linear,
                    planes: std::vec![PlaneLayout {
                        offset: 0,
                        stride: 256,
                        size: 256 * 17
                    }],
                }),
            },
            ResourceMeta {
                id: BUFFER,
                size: 144,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX,
                },
            },
        ];
        let pipelines: std::vec::Vec<_> = [
            FragmentProgram::Solid,
            FragmentProgram::Texture(TextureSampleMode::Rgba),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, fragment)| PipelineMeta {
            id: PipelineId::new(i as u32),
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
                fragment,
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                RasterState::new(CullMode::None, FrontFace::CounterClockwise),
            )
            .unwrap(),
        })
        .collect();
        let mut operations = std::vec::Vec::new();
        for (i, target) in [TARGET, SOURCE].into_iter().enumerate() {
            operations.push(Operation::BeginRenderPass(RenderPass {
                target,
                area: PixelRect::new(0, 0, 64, 17).unwrap(),
                load: LoadOp::Clear(Color::rgba(0.1, 0.2, 0.3, 0.4).unwrap()),
                store: StoreOp::Store,
                depth: None,
            }));
            operations.push(Operation::SetPipeline(PipelineId::new(i as u32)));
            operations.push(Operation::SetVertexBuffer {
                buffer: BUFFER,
                offset: 0,
            });
            if i == 1 {
                operations.push(Operation::SetTexture(TARGET));
                operations.push(Operation::SetSampler(SamplerDesc::new(
                    FilterMode::Linear,
                    FilterMode::Linear,
                    AddressMode::ClampToEdge,
                    AddressMode::ClampToEdge,
                )));
            }
            operations.push(Operation::SetUniforms(DrawUniforms::new(
                Transform::identity(),
                Color::rgba(0.8, 0.4, 0.2, 0.5).unwrap(),
            )));
            operations.push(Operation::Draw {
                vertex_count: 6,
                first_vertex: 0,
            });
            operations.push(Operation::EndRenderPass);
        }
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();
        let validate = |artifact: &RelocatablePm4, target_modifier| {
            validate_and_relocate(
                &wire_from_codegen(artifact),
                |attachment_token| {
                    let image = if attachment_token == token(ObjectRef::External(TARGET)) {
                        Some(LinearImage {
                            texture: None,
                            format: LinearImageFormat::Bgra8Unorm,
                            modifier: target_modifier,
                            width: 64,
                            height: 17,
                            row_pitch: 256,
                            visible_size: tiled.layer_size,
                        })
                    } else if attachment_token == token(ObjectRef::External(SOURCE)) {
                        Some(LinearImage {
                            texture: None,
                            format: LinearImageFormat::Bgra8Unorm,
                            modifier: KernelImageModifier::Linear,
                            width: 64,
                            height: 17,
                            row_pitch: 256,
                            visible_size: 256 * 17,
                        })
                    } else {
                        None
                    };
                    Some(ResolvedResource {
                        attachment_token,
                        gpu_va: 0x1_0000_0000 + attachment_token * 0x1_0000,
                        allocation_size: image.map_or(4096, |i| i.visible_size),
                        allowed_access: ACCESS_READ | ACCESS_WRITE,
                        linear_image: image,
                    })
                },
                |variant| {
                    Some(ResolvedResource {
                        attachment_token: 0,
                        gpu_va: 0x4_0000_0000 + variant.offset() as u64,
                        allocation_size: variant.size() as u64,
                        allowed_access: ACCESS_READ,
                        linear_image: None,
                    })
                },
            )
        };
        assert!(validate(&artifact, KernelImageModifier::Tile6_3Color).is_ok());
        assert!(validate(&artifact, KernelImageModifier::Linear).is_err());
        assert!(Packets::new(&artifact.words).flatten().any(|p| matches!(
            p.header,
            Header::Type4 {
                register: super::RB_MRT_BUF_INFO,
                ..
            }
        ) && p.payload
            == [super::FORMAT_8_8_8_8_UNORM | (3 << 8)]));
        assert!(
            Packets::new(&artifact.words)
                .flatten()
                .filter(|p| matches!(
                    p.header,
                    Header::Type4 {
                        register: super::RB_MRT_CONTROL,
                        ..
                    }
                ))
                .all(|p| p.payload == [0x7e3, 0x0701_0706])
        );
        let descriptor = Packets::new(&artifact.words)
            .flatten()
            .find(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: super::CP_MEM_WRITE,
                        ..
                    }
                ) && p.payload.len() == 22
            })
            .unwrap();
        assert_eq!(descriptor.payload[2], 0x0c00_6883); // native channel order, TILE6_3
        assert_eq!(descriptor.payload[4] & 0x7f, 2); // 256-byte pitch alignment
        let index = descriptor.word_offset as usize;
        let mut bad = artifact.clone();
        bad.words[index + 5] &= !0xf;
        assert!(validate(&bad, KernelImageModifier::Tile6_3Color).is_err());
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
        // validated userspace IB at 532 dwords, including the indirect sampler
        // and texture-descriptor state backing required by A618.
        assert_eq!(artifact.words.len(), 532);
        assert!(accept_codegen(&artifact).is_ok());
    }

    #[test]
    fn nv12_planes_are_read_only_bounded_and_use_native_shaders() {
        const PIPELINE: PipelineId = PipelineId::new(11);
        let mut resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            image(SOURCE, TextureUsage::SAMPLED),
            image(ALPHA, TextureUsage::SAMPLED),
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
        use sgfx_core::ir::{ChromaLocation, YcbcrConversion, YcbcrMatrix, YcbcrRange};
        resources[1].size = 8192;
        let ResourceKind::Image(source) = &mut resources[1].kind else {
            unreachable!()
        };
        source.format = TextureFormat::Nv12;
        source.storage_format = TextureFormat::Nv12;
        source.conversion = Some(YcbcrConversion {
            matrix: YcbcrMatrix::Bt709,
            range: YcbcrRange::Limited,
            chroma_x: ChromaLocation::Cosited,
            chroma_y: ChromaLocation::Midpoint,
        });
        source.planes.push(PlaneLayout {
            offset: 4096,
            stride: 64,
            size: 512,
        });
        let vertices = [0_u8; 144];
        let operations = std::vec![
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
            capabilities: Capabilities::a618(512 * 1024, 64 * 1024),
            resources: &resources,
            pipelines: core::slice::from_ref(&pipeline),
            operations: &operations,
        })
        .unwrap();
        assert!(artifact.fixups.iter().any(|r| r.object
            == ObjectRef::CanonicalShader(adreno_a6xx_shader_pack::ShaderVariant::FsNv12)));
        let validate = |artifact: &RelocatablePm4, uv_size| {
            validate_and_relocate(
                &wire_from_codegen(artifact),
                |attachment_token| {
                    let source = attachment_token == token(ObjectRef::External(SOURCE));
                    let target = attachment_token == token(ObjectRef::External(TARGET));
                    Some(ResolvedResource {
                        attachment_token,
                        gpu_va: 0x1_0000_0000 + attachment_token * 0x10000,
                        allocation_size: if source { 8192 } else { 4096 },
                        allowed_access: if source {
                            ACCESS_READ
                        } else {
                            ACCESS_READ | ACCESS_WRITE
                        },
                        linear_image: (source || target).then_some(LinearImage {
                            texture: None,
                            format: if source {
                                LinearImageFormat::Nv12 {
                                    uv_offset: 4096,
                                    uv_size,
                                }
                            } else {
                                LinearImageFormat::Bgra8Unorm
                            },
                            modifier: KernelImageModifier::Linear,
                            width: 16,
                            height: 16,
                            row_pitch: 64,
                            visible_size: if source { 8192 } else { 1024 },
                        }),
                    })
                },
                |variant| {
                    Some(ResolvedResource {
                        attachment_token: 0,
                        gpu_va: 0x4_0000_0000 + variant.offset() as u64,
                        allocation_size: variant.size() as u64,
                        allowed_access: ACCESS_READ,
                        linear_image: None,
                    })
                },
            )
        };
        assert!(
            validate(&artifact, 512).is_ok(),
            "{:?}",
            validate(&artifact, 512)
        );
        assert!(validate(&artifact, 256).is_err());
        let descriptor = Packets::new(&artifact.words)
            .flatten()
            .find(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: super::CP_MEM_WRITE,
                        ..
                    }
                )
            })
            .unwrap();
        let mut bad = artifact.clone();
        bad.words[descriptor.word_offset as usize + 9] += 64 << 8;
        assert!(validate(&bad, 512).is_err());
    }

    #[test]
    fn alternating_texture_prologue_is_validated_before_state_lookup() {
        const PIPELINE: PipelineId = PipelineId::new(11);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            image(SOURCE, TextureUsage::SAMPLED),
            image(ALPHA, TextureUsage::SAMPLED),
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
        let mut operations = std::vec![
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
        operations.pop(); // EndRenderPass
        for index in 0..32 {
            operations.push(Operation::SetTexture(if index % 2 == 0 {
                ALPHA
            } else {
                SOURCE
            }));
            operations.push(Operation::Draw {
                vertex_count: 6,
                first_vertex: 0,
            });
        }
        operations.push(Operation::EndRenderPass);
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 64 * 1024),
            resources: &resources,
            pipelines: core::slice::from_ref(&pipeline),
            operations: &operations,
        })
        .unwrap();

        accept_codegen(&artifact).unwrap();
        // Production packs generated states into one upload arena: distinct
        // resource records may share a token but have different range offsets.
        let bytes = wire_from_codegen(&artifact);
        let decoded = adreno_a6xx_submit_wire::decode(&bytes).unwrap();
        let resources: std::vec::Vec<_> = (0..decoded.resource_len())
            .map(|i| {
                let mut resource = decoded.resource(i).unwrap();
                if resource.attachment_token >= 0x1000 {
                    resource.range_offset += (resource.attachment_token - 0x1000) * 256;
                    resource.attachment_token = 0x1000;
                }
                resource
            })
            .collect();
        let relocations: std::vec::Vec<_> = (0..decoded.relocation_len())
            .map(|i| decoded.relocation(i).unwrap())
            .collect();
        let submit = Submit {
            pm4: &artifact.words,
            resources: &resources,
            relocations: &relocations,
        };
        let mut shared_arena = std::vec![0; encoded_len(submit).unwrap()];
        encode(submit, &mut shared_arena).unwrap();
        let decoded_shared = adreno_a6xx_submit_wire::decode(&shared_arena).unwrap();
        super::validate_pm4(
            decoded_shared,
            &relocations,
            decoded_shared.requires_isolation(),
        )
        .unwrap();

        let packets: std::vec::Vec<_> = Packets::new(&artifact.words).map(Result::unwrap).collect();
        let descriptors: std::vec::Vec<_> = packets
            .iter()
            .filter(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: super::CP_MEM_WRITE,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(descriptors.len(), 2);
        let first_draw = packets
            .iter()
            .find(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: super::CP_DRAW_INDX_OFFSET,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            descriptors
                .iter()
                .all(|p| p.word_offset < first_draw.word_offset)
        );
        // Every descriptor in the prologue needs image validation, including
        // the second one which used to reside in the second draw's segment.
        let mut bad_layout = artifact.clone();
        bad_layout.words[descriptors[1].word_offset as usize + 4] += 1;
        assert!(accept_codegen(&bad_layout).is_err());
        // A bounds-valid read inside the same state allocation is still not
        // a descriptor initialized by the prologue.
        let texture_base = packets
            .iter()
            .find(|p| {
                matches!(
                    p.header,
                    Header::Type4 {
                        register: super::SP_PS_TEXMEMOBJ_BASE,
                        ..
                    }
                )
            })
            .unwrap();
        let mut bad_lookup = artifact.clone();
        let fixup = bad_lookup
            .fixups
            .iter_mut()
            .find(|f| f.word_offset == texture_base.word_offset + 1)
            .unwrap();
        fixup.object_offset += 16;
        assert!(accept_codegen(&bad_lookup).is_err());
    }

    #[test]
    fn sampler_rejects_indirect_border_tables_and_reserved_modes() {
        assert!(super::supported_sampler(&[0x961, 0x1806_0000, 0x10, 0], 8));
        for words in [
            [0x960, 0x40, 0, 0],             // border without fast transparent color
            [0x960, 0x40, 0x90, 0],          // indirect border table index
            [0x960, 0x40, 0x14, 0],          // opaque border color
            [0x928, 0x40, 0x20, 1],          // reserved tail
            [0x920 | (5 << 14), 0x40, 0, 0], // reserved anisotropy
            [0x938, 0x40, 0x20, 0],          // cubic filtering unsupported on A618
            [0x920 | (1 << 19), 0x40, 0, 0], // unsupported LOD bias
            [0x920, 0x41, 0, 0],             // unsupported clamp/compare flag
        ] {
            assert!(!super::supported_sampler(&words, 0), "{words:x?}");
        }
    }

    #[test]
    fn blend_state_rejects_reserved_fields_and_unbound_factors() {
        assert!(super::valid_blend_control(&[0x7e0, 0x0001_0001]));
        for value in [0x0101_0101, 0x0700_0700, 0x0701_0701] {
            assert!(super::valid_blend_control(&[0x7e3, value]));
        }
        for value in [
            0x0001_000c,
            0x0001_0014,
            0x0001_00a1,
            0x2001_0001,
            0x0001_2001,
        ] {
            assert!(!super::valid_blend_control(&[0x7e3, value]));
        }
        assert!(!super::valid_blend_control(&[0x7e7, 0x0001_0001]));
        assert!(!super::valid_blend_control(&[0x7e0, 0x0701_0701]));
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
        let mut cases = std::vec![
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
                CullMode::None
            ),
            (
                color4_uv.clone(),
                FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
            ),
            (
                color4_uv.clone(),
                FragmentProgram::TextureVertexColor(TextureSampleMode::Rgba),
                BlendState::new(
                    sgfx_core::ir::BlendComponent::new(
                        sgfx_core::ir::BlendFactor::One,
                        sgfx_core::ir::BlendFactor::OneMinusSourceAlpha,
                        sgfx_core::ir::BlendOp::Add
                    ),
                    sgfx_core::ir::BlendComponent::new(
                        sgfx_core::ir::BlendFactor::One,
                        sgfx_core::ir::BlendFactor::OneMinusSourceAlpha,
                        sgfx_core::ir::BlendOp::Add
                    ),
                ),
                CullMode::None
            ),
            (
                color4_uv,
                FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask),
                BlendState::SOURCE_OVER_STRAIGHT_ALPHA,
                CullMode::None
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
        use sgfx_core::ir::{BlendComponent, BlendFactor as F, BlendOp as O};
        let factors = [
            F::Zero,
            F::One,
            F::SourceAlpha,
            F::OneMinusSourceAlpha,
            F::DestinationAlpha,
            F::OneMinusDestinationAlpha,
            F::SourceColor,
            F::OneMinusSourceColor,
            F::DestinationColor,
            F::OneMinusDestinationColor,
        ];
        for op in [O::Add, O::Subtract, O::ReverseSubtract, O::Min, O::Max] {
            for src in factors {
                for dst in factors {
                    cases.push((
                        pos4(40),
                        FragmentProgram::Solid,
                        BlendState::new(
                            BlendComponent::new(src, dst, op),
                            BlendComponent::new(dst, src, op),
                        ),
                        CullMode::None,
                    ));
                }
            }
        }
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
                            | FragmentProgram::TextureVertexColor(TextureSampleMode::AlphaMask)
                    ) {
                        ALPHA
                    } else {
                        SOURCE
                    },
                ));
                // Sampler filtering is dynamic SGFX state rather than part of
                // the shader/vertex pipeline identity. Exercise linear mode
                // for every sampled layout, including the stride-16 UI path.
                let filter = FilterMode::Linear;
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
                let modes = [
                    AddressMode::ClampToEdge,
                    AddressMode::Repeat,
                    AddressMode::MirrorRepeat,
                    AddressMode::ClampToBorder,
                ];
                for u in modes {
                    for v in modes {
                        for min in [FilterMode::Nearest, FilterMode::Linear] {
                            for mag in [FilterMode::Nearest, FilterMode::Linear] {
                                for aniso in 1..=16 {
                                    let sampler = SamplerDesc::new(min, mag, u, v)
                                        .with_anisotropy(aniso)
                                        .unwrap();
                                    let mut sampling = operations.clone();
                                    for operation in &mut sampling {
                                        if let Operation::SetSampler(value) = operation {
                                            *value = sampler;
                                        }
                                    }
                                    let artifact = compile(CompileInput {
                                        capabilities: Capabilities::a618(512 * 1024, 4096),
                                        resources: &resources,
                                        pipelines: core::slice::from_ref(&pipeline),
                                        operations: &sampling,
                                    })
                                    .unwrap();
                                    assert!(accept_codegen(&artifact).is_ok(), "{sampler:?}");
                                }
                            }
                        }
                    }
                }
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
                assert_eq!(
                    accept_codegen(&hostile).is_err(),
                    cfg!(feature = "strict-command-validation")
                );

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
    #[test]
    fn canonical_public_topologies_preserve_authority_and_reject_invalid_initiators() {
        for format in [TextureFormat::Bgra8Unorm,TextureFormat::Rg8Unorm] {
        for topology in [PrimitiveTopology::TriangleList,PrimitiveTopology::TriangleStrip,
            PrimitiveTopology::TriangleFan,PrimitiveTopology::LineList,
            PrimitiveTopology::LineStrip,PrimitiveTopology::PointList] {
        for indexed in [false,true] {

        const PIPELINE: PipelineId = PipelineId::new(7);
        let resources = [
            image(TARGET, TextureUsage::RENDER_ATTACHMENT),
            ResourceMeta {
                id: BUFFER,
                size: 240,
                kind: ResourceKind::Buffer {
                    usage: BufferUsage::VERTEX | BufferUsage::COPY_DST,
                },
            },
        ];
        let pipelines = [PipelineMeta {
            id: PIPELINE,
            descriptor: RenderPipelineDesc::new(
                format,
                topology,
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
                vertex_count: 6,
                first_vertex: 0,
            },
            Operation::EndRenderPass,
        ];
        let mut resources = resources.to_vec();
        if let ResourceKind::Image(image)=&mut resources[0].kind {image.format=format;}
        let mut operations = operations.to_vec();
        if indexed {
            let id = ObjectId::new(123);
            resources.push(ResourceMeta {id,size:12,kind:ResourceKind::Buffer {usage:BufferUsage::INDEX}});
            operations.insert(4,Operation::SetIndexBuffer {buffer:id,offset:0,format:sgfx_core::ir::IndexFormat::Uint16});
            operations[5]=Operation::DrawIndexed {index_count:6,first_index:0,base_vertex:0};
        }
        let artifact = compile(CompileInput {
            capabilities: Capabilities::a618(512 * 1024, 4096),
            resources: &resources,
            pipelines: &pipelines,
            operations: &operations,
        })
        .unwrap();

            assert!(accept_codegen(&artifact).is_ok(),"{topology:?} indexed={indexed}");
            let packet = adreno_a6xx_pm4::Packets::new(&artifact.words).map(Result::unwrap)
                .find(|p| matches!(p.header,Header::Type7 {opcode,..} if opcode==super::CP_DRAW_INDX_OFFSET)).unwrap();
            let offset = packet.word_offset as usize + 1;
            for mode in [0,1,7,8,10,63] {
                let mut invalid = artifact.clone();
                invalid.words[offset]=(invalid.words[offset]&!63)|mode;
                assert!(accept_codegen(&invalid).is_err(),"reserved mode={mode}");
            }
            let mut invalid=artifact.clone();invalid.words[offset]|=1<<20;
            assert!(accept_codegen(&invalid).is_err());
            let mut invalid=artifact.clone();invalid.words[offset+2]=0;
            assert!(accept_codegen(&invalid).is_err(),"zero-count {topology:?} indexed={indexed} offset={offset} payload={:?}",&invalid.words[offset..offset+3]);
        }}}
    }

}

// Canonical programs use the same six non-binning public primitive modes as
// native pipelines. Count rules do not weaken vertex/index address bounds.
fn canonical_topology_count(mode:u32,count:u32)->bool {
    match mode {4=>count>0 && count%3==0,5|6=>count>=3,2=>count>0 && count%2==0,3=>count>=2,9=>count>0,_=>false}
}
