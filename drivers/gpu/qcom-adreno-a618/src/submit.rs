// SPDX-License-Identifier: GPL-2.0-only

//! Hostile-input validation and relocation for the A618 submit dialect.

use alloc::vec::Vec;

use adreno_a6xx_pm4::{Header, Packets, opcode};
use adreno_a6xx_shader_pack::{
    PipelineVariant, ShaderMeta, ShaderVariant, link_meta, pipeline_state_meta, shader_meta,
};
use adreno_a6xx_submit_wire::{
    ACCESS_READ, ACCESS_WRITE, AddressEncoding, DecodedSubmit, Relocation, RelocationSource,
};

const CP_MEMCPY: u8 = 0x75;
const CP_SET_VISIBILITY_OVERRIDE: u8 = 0x64;
const CP_REG_WRITE: u8 = 0x6d;

const EVENT_CCU_INVALIDATE_COLOR: u32 = 0x19;
const EVENT_CCU_FLUSH_COLOR_TS: u32 = 0x1d;
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
const GRAS_CL_VIEWPORT_XOFFSET: u32 = 0x8010;
const GRAS_SU_CNTL: u32 = 0x8090;
const GRAS_SC_CNTL: u32 = 0x80a0;
const GRAS_SC_SCREEN_SCISSOR_TL: u32 = 0x80b0;
const GRAS_SC_VIEWPORT_SCISSOR_TL: u32 = 0x80d0;
const GRAS_SC_WINDOW_SCISSOR_TL: u32 = 0x80f0;
const GRAS_SC_BIN_CNTL: u32 = 0x80a1;
const GRAS_LRZ_CNTL: u32 = 0x8100;
const RB_CNTL: u32 = 0x8800;
const RB_RENDER_CNTL: u32 = 0x8801;
const RB_PS_OUTPUT_CNTL: u32 = 0x880b;
const RB_PS_MRT_CNTL: u32 = 0x880c;
const RB_PS_OUTPUT_MASK: u32 = 0x880d;
const RB_MRT_CONTROL: u32 = 0x8820;
const RB_MRT_BUF_INFO: u32 = 0x8822;
const RB_MRT_PITCH: u32 = 0x8823;
const RB_MRT_BASE: u32 = 0x8825;
const RB_BLEND_CNTL: u32 = 0x8865;
const RB_MODE_CNTL: u32 = 0x8811;
const RB_WINDOW_OFFSET: u32 = 0x8890;
const RB_LRZ_CNTL: u32 = 0x8898;
const RB_BIN_CONTROL2: u32 = 0x88d3;
const RB_WINDOW_OFFSET2: u32 = 0x88d4;
const VPC_VARYING_LM_TRANSFER_CNTL_DISABLE: u32 = 0x9212;
const VPC_VS_CNTL: u32 = 0x9301;
const VPC_PS_CNTL: u32 = 0x9304;
const VPC_SO_OVERRIDE: u32 = 0x9306;
const PC_MODE_CNTL: u32 = 0x9804;
const PC_DGEN_RAST_CNTL: u32 = 0x9981;
const PC_VS_CNTL: u32 = 0x9b01;
const VFD_CNTL_0: u32 = 0xa000;
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
const SP_VS_BASE: u32 = 0xa81c;
const SP_VS_CONFIG: u32 = 0xa823;
const SP_VS_INSTR_SIZE: u32 = 0xa824;
const SP_PS_CNTL_0: u32 = 0xa980;
const SP_PS_BASE: u32 = 0xa983;
const SP_BLEND_CNTL: u32 = 0xa989;
const SP_PS_OUTPUT_MASK: u32 = 0xa98b;
const SP_PS_OUTPUT_CNTL: u32 = 0xa98c;
const SP_PS_MRT_CNTL: u32 = 0xa98d;
const SP_PS_OUTPUT_REG: u32 = 0xa98e;
const SP_PS_MRT_REG: u32 = 0xa996;
const SP_PS_INITIAL_TEX_LOAD_CNTL: u32 = 0xa99e;
const SP_PS_CONFIG: u32 = 0xab04;
const SP_PS_INSTR_SIZE: u32 = 0xab05;
const SP_MODE_CNTL: u32 = 0xab00;
const SP_REG_PROG_ID_0: u32 = 0xb983;
const TPL1_MODE_CNTL: u32 = 0xb309;
const SP_UPDATE_CNTL: u32 = 0xbb08;

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
                VFD_DEST_CNTL => payload == meta.vfd_dest_cntl,
                _ => false,
            },
            ShaderMeta::Fragment(meta) => match register {
                SP_PS_CNTL_0 => payload == [meta.sp_ps_cntl_0],
                SP_PS_INSTR_SIZE => payload == [meta.sp_ps_instr_size],
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
            GRAS_LRZ_CNTL | RB_LRZ_CNTL | RB_BIN_CONTROL2 | RB_WINDOW_OFFSET | RB_WINDOW_OFFSET2
            | VPC_SO_OVERRIDE | VFD_RENDER_MODE,
            1,
        ) => exact(payload, &[0]),
        (VFD_MODE_CNTL, 1) => exact(payload, &[3]),
        (PC_MODE_CNTL, 1) => exact(payload, &[0x1f]),
        (SP_MODE_CNTL, 1) => exact(payload, &[5]),
        (TPL1_MODE_CNTL, 1) => exact(payload, &[0xa2]),
        (RB_MODE_CNTL, 1) => exact(payload, &[0x10]),
        (SP_UPDATE_CNTL, 1) => exact(payload, &[0x000f_ffff]),
        (RB_MRT_CONTROL, 2) if matches!(payload, [0x780, 0x0001_0001] | [0x783, 0x0701_0706]) => {
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
        (RB_BLEND_CNTL, 1) if matches!(payload[0], 0x0001_0100 | 0x0001_0101) => Ok(()),
        (SP_BLEND_CNTL, 1) if matches!(payload[0], 0x100 | 0x101) => Ok(()),
        (SP_PS_MRT_CNTL | RB_PS_MRT_CNTL, 1) => exact(payload, &[1]),
        (SP_PS_MRT_REG, 1) => exact(payload, &[FORMAT_8_8_8_8_UNORM]),
        (GRAS_CL_VIEWPORT_XOFFSET, 6) => Ok(()),
        (
            GRAS_SC_SCREEN_SCISSOR_TL | GRAS_SC_VIEWPORT_SCISSOR_TL | GRAS_SC_WINDOW_SCISSOR_TL,
            2,
        ) => Ok(()),
        (GRAS_SU_CNTL, 1) if payload[0] & !0x2017 == 0 && payload[0] & 0x2010 == 0x2010 => Ok(()),
        (PC_DGEN_RAST_CNTL, 1) => exact(payload, &[3]),
        (VFD_CNTL_0, 1)
            if payload[0] & 0xffff_0000 == 0 && payload[0] & 0xff == payload[0] >> 8 =>
        {
            Ok(())
        }
        (VFD_INDEX_OFFSET, 2) => exact(&payload[1..], &[0]),
        (VFD_VERTEX_BUFFER_BASE, 2) => {
            address_field(addresses, packet_word + 1, ACCESS_READ, false, None)
        }
        (VFD_VERTEX_BUFFER_SIZE, 2)
            if payload[0] != 0 && matches!(payload[1], 16 | 24 | 28 | 40) =>
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
            | SP_PS_CNTL_0
            | SP_PS_INSTR_SIZE
            | SP_PS_INITIAL_TEX_LOAD_CNTL
            | SP_REG_PROG_ID_0
            | SP_PS_OUTPUT_CNTL
            | SP_PS_OUTPUT_REG
            | SP_PS_OUTPUT_MASK
            | RB_PS_OUTPUT_CNTL
            | RB_PS_OUTPUT_MASK,
            _,
        ) if any_shader_payload(register, payload) => Ok(()),
        (SP_VS_OUTPUT_CNTL | VPC_VS_CNTL | VPC_PS_CNTL | PC_VS_CNTL, 1) => Ok(()),
        (SP_VS_OUTPUT_REG | SP_VS_VPC_DEST_REG, count) if count <= 2 => Ok(()),
        (VPC_VARYING_LM_TRANSFER_CNTL_DISABLE, 4) => Ok(()),
        (SP_VS_CONFIG, 1) => exact(payload, &[0x100]),
        (SP_PS_CONFIG, 1) if matches!(payload[0], 0x100 | 0x10101) => Ok(()),
        (SP_VS_BASE, 2) => {
            canonical_address_field(addresses, packet_word + 1, ShaderVariant::VsStride16Pos2)
        }
        (SP_PS_BASE, 2) => {
            canonical_address_field(addresses, packet_word + 1, ShaderVariant::FsSolid)
        }
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
        (opcode::WAIT_FOR_IDLE, 0) => Ok(()),
        (opcode::EVENT_WRITE, 1)
            if matches!(
                payload[0],
                EVENT_CCU_INVALIDATE_COLOR | EVENT_CCU_FLUSH_COLOR_TS | EVENT_CACHE_INVALIDATE
            ) =>
        {
            Ok(())
        }
        (opcode::SET_MARKER, 1) if matches!(payload[0], 1 | 12) => Ok(()),
        (CP_SET_VISIBILITY_OVERRIDE, 1) => exact(payload, &[1]),
        (CP_REG_WRITE, 3) => exact(payload, &[2, RB_RENDER_CNTL, 0x10]),
        (opcode::BLIT, 1) => exact(payload, &[3]),
        (CP_MEMCPY, 5) if payload[0] != 0 => {
            let size = u64::from(payload[0])
                .checked_mul(4)
                .ok_or("qcom-adreno-a618: PM4 memcpy size overflows")?;
            address_field(addresses, packet_word + 2, ACCESS_READ, false, Some(size))?;
            address_field(addresses, packet_word + 4, ACCESS_WRITE, false, Some(size))
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
        (CP_LOAD_STATE6_FRAG, 19)
            if payload[0] == ((1 << 14) | (4 << 18) | (1 << 22))
                && payload[1..3] == [0, 0]
                && payload[3] == 0x4c00_6880
                && payload[4] & 0xc000_0000 == 0
                && payload[4] & 0x7fff != 0
                && (payload[4] >> 15) & 0x7fff != 0
                && payload[5] & 0x70 == 0
                && payload[5] >> 29 == 1
                && payload[6] & !0x007f_ffff == 0
                && payload[8] == (1 << 17)
                && payload[9..] == [0; 10] =>
        {
            address_field_encoded(
                addresses,
                packet_word + 8,
                ACCESS_READ,
                false,
                None,
                AddressEncoding::GpuVa49TexDescriptor,
            )
        }
        (CP_LOAD_STATE6_FRAG, 7)
            if payload[0] == ((4 << 18) | (1 << 22))
                && payload[1..3] == [0, 0]
                && matches!(payload[3], 0x920 | 0x92a)
                && payload[4..] == [0; 3] =>
        {
            Ok(())
        }
        (CP_DRAW_INDX_OFFSET, 3)
            if payload[0] == 0x84
                && payload[1] == 1
                && payload[2] != 0
                && payload[2].is_multiple_of(3) =>
        {
            Ok(())
        }
        _ => Err("qcom-adreno-a618: PM4 opcode is not allowlisted"),
    }
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
    let address = addresses
        .iter_mut()
        .find(|address| address.word_offset == word_offset)
        .ok_or("qcom-adreno-a618: A2D address lacks relocation metadata")?;
    if address.a2d.replace(expectation).is_some() {
        return Err("qcom-adreno-a618: A2D address is reused by multiple blits");
    }
    Ok(())
}

fn validate_a2d_sequences(
    words: &[u32],
    addresses: &mut [AddressField],
) -> Result<(), &'static str> {
    let mut state = A2dState::default();
    for packet in Packets::new(words) {
        let packet = packet.map_err(|_| "qcom-adreno-a618: malformed PM4 packet stream")?;
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

fn segment_reg<'a>(
    words: &'a [u32],
    start: u32,
    end: u32,
    wanted: u32,
) -> Option<(u32, &'a [u32])> {
    let mut found = None;
    for packet in Packets::new(words).filter_map(Result::ok) {
        if packet.word_offset >= start
            && packet.word_offset < end
            && matches!(packet.header, Header::Type4 { register, .. } if register == wanted)
        {
            if found.is_some() {
                return None;
            }
            found = Some((packet.word_offset, packet.payload));
        }
    }
    found
}

fn segment_matches_pipeline(words: &[u32], start: u32, end: u32, variant: PipelineVariant) -> bool {
    let link = link_meta(variant);
    let fixed = pipeline_state_meta(variant);
    let ShaderMeta::Vertex(vs) = shader_meta(link.vs) else {
        return false;
    };
    let ShaderMeta::Fragment(fs) = shader_meta(link.fs) else {
        return false;
    };
    let reg = |wanted| segment_reg(words, start, end, wanted).map(|(_, p)| p);
    let load_count = |opcode, block, state_type| {
        packets_in_segment(words, start, end)
            .filter(|p| {
                matches!(p.header, Header::Type7 { opcode: actual, .. } if actual == opcode)
                    && p.payload.first().is_some_and(|word| {
                        (word >> 18) & 0xf == block && (word >> 14) & 3 == state_type
                    })
            })
            .count()
    };
    let type7_count = |wanted_opcode, wanted_payload: &[u32]| {
        packets_in_segment(words, start, end)
            .filter(|packet| {
                matches!(packet.header, Header::Type7 { opcode, .. } if opcode == wanted_opcode)
                    && packet.payload == wanted_payload
            })
            .count()
    };
    let last_marker = packets_in_segment(words, start, end)
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
        && type7_count(CP_SET_VISIBILITY_OVERRIDE, &[1]) == 1
        && type7_count(CP_REG_WRITE, &[2, RB_RENDER_CNTL, 0x10]) == 1
        && reg(SP_VS_CNTL_0) == Some(&[vs.sp_vs_cntl_0])
        && reg(VFD_DEST_CNTL) == Some(vs.vfd_dest_cntl)
        && reg(SP_VS_OUTPUT_CNTL) == Some(&[link.sp_vs_output_cntl])
        && reg(SP_VS_OUTPUT_REG) == Some(link.sp_vs_output_reg)
        && reg(SP_VS_VPC_DEST_REG) == Some(link.sp_vs_vpc_dest_reg)
        && reg(VPC_VS_CNTL) == Some(&[link.vpc_vs_cntl])
        && reg(VPC_PS_CNTL) == Some(&[link.vpc_ps_cntl])
        && reg(PC_VS_CNTL) == Some(&[link.pc_vs_cntl])
        && reg(VPC_VARYING_LM_TRANSFER_CNTL_DISABLE) == Some(&link.lm_transfer_disable)
        && reg(SP_PS_CNTL_0) == Some(&[fs.sp_ps_cntl_0])
        && reg(SP_PS_OUTPUT_REG) == Some(fs.sp_ps_output_reg)
        && reg(SP_VS_INSTR_SIZE) == Some(&[vs.sp_vs_instr_size])
        && reg(SP_VS_CONFIG) == Some(&[0x100])
        && reg(SP_PS_INSTR_SIZE) == Some(&[fs.sp_ps_instr_size])
        && reg(SP_PS_CONFIG)
            == Some(&[if fixed.sampler_dword0.is_some() {
                0x10101
            } else {
                0x100
            }])
        && reg(SP_REG_PROG_ID_0) == Some(&fs.sp_reg_prog_id)
        && reg(SP_PS_OUTPUT_CNTL) == Some(&[fs.sp_ps_output_cntl])
        && reg(SP_PS_OUTPUT_MASK) == Some(&[fs.sp_ps_output_mask])
        && reg(RB_PS_OUTPUT_CNTL) == Some(&[fs.rb_ps_output_cntl])
        && reg(RB_PS_OUTPUT_MASK) == Some(&[fs.rb_ps_output_mask])
        && reg(SP_PS_INITIAL_TEX_LOAD_CNTL).is_some_and(|p| {
            p.first() == Some(&fs.initial_tex_load_cntl)
                && p.get(1..) == Some(fs.initial_tex_load_cmd)
        })
        && reg(VFD_VERTEX_BUFFER_SIZE).is_some_and(|p| p.len() == 2 && p[1] == fixed.stride)
        && reg(VFD_FETCH_INSTR) == Some(fixed.vfd_fetch)
        && reg(VFD_CNTL_0)
            == Some(&[
                (fixed.vfd_fetch.len() as u32 / 2) | ((fixed.vfd_fetch.len() as u32 / 2) << 8)
            ])
        && reg(RB_MRT_CONTROL)
            == Some(if fixed.source_over {
                &[0x783, 0x0701_0706]
            } else {
                &[0x780, 0x0001_0001]
            })
        && reg(RB_BLEND_CNTL)
            == Some(&[if fixed.source_over {
                0x0001_0101
            } else {
                0x0001_0100
            }])
        && reg(SP_BLEND_CNTL) == Some(&[if fixed.source_over { 0x101 } else { 0x100 }])
        && reg(GRAS_SU_CNTL).is_some_and(|p| {
            p.len() == 1
                && if fixed.stride == 28 {
                    p[0] & !0x2017 == 0 && p[0] & 0x2010 == 0x2010
                } else {
                    p[0] == if fixed.stride == 40 { 0x2012 } else { 0x2010 }
                }
        })
        && load_count(CP_LOAD_STATE6_GEOM, 8, 1) == 1
        && load_count(CP_LOAD_STATE6_FRAG, 12, 1) == 1
        && load_count(CP_LOAD_STATE6_FRAG, 4, 1) == usize::from(fixed.sampler_dword0.is_some())
        && load_count(CP_LOAD_STATE6_FRAG, 4, 0) == usize::from(fixed.sampler_dword0.is_some())
        && match fixed.sampler_dword0 {
            Some(expected) => packets_in_segment(words, start, end).any(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: CP_LOAD_STATE6_FRAG,
                        ..
                    }
                ) && p.payload.len() == 7
                    && p.payload[0] == ((4 << 18) | (1 << 22))
                    && p.payload[3] == expected
            }),
            None => !packets_in_segment(words, start, end).any(|p| {
                matches!(
                    p.header,
                    Header::Type7 {
                        opcode: CP_LOAD_STATE6_FRAG,
                        ..
                    }
                ) && p.payload.len() == 7
                    && p.payload[0] & (3 << 14) == 0
                    && p.payload[0] >> 18 & 0xf == 4
            }),
        }
}

fn set_address_source(
    addresses: &mut [AddressField],
    word: u32,
    source: AddressSource,
) -> Result<(), &'static str> {
    let address = addresses
        .iter_mut()
        .find(|a| a.word_offset == word)
        .ok_or("qcom-adreno-a618: 3D address lacks relocation metadata")?;
    address.source = source;
    Ok(())
}

fn set_image_expectation(
    addresses: &mut [AddressField],
    word: u32,
    expectation: ImageExpectation,
) -> Result<(), &'static str> {
    let address = addresses
        .iter_mut()
        .find(|a| a.word_offset == word)
        .ok_or("qcom-adreno-a618: 3D image address lacks relocation metadata")?;
    if address.image.replace(expectation).is_some() {
        return Err("qcom-adreno-a618: 3D image address is reused");
    }
    Ok(())
}

fn validate_3d_sequences(
    words: &[u32],
    addresses: &mut [AddressField],
) -> Result<(), &'static str> {
    let mut start = 0;
    let packets: Vec<_> = Packets::new(words)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "qcom-adreno-a618: malformed 3D packet stream")?;
    for packet in packets {
        if !matches!(
            packet.header,
            Header::Type7 {
                opcode: CP_DRAW_INDX_OFFSET,
                ..
            }
        ) {
            continue;
        }
        let end = packet.word_offset;
        let mut matched = None;
        for candidate in PipelineVariant::ALL {
            if segment_matches_pipeline(words, start, end, candidate) {
                if matched.is_some() {
                    return Err("qcom-adreno-a618: ambiguous canonical 3D pipeline state");
                }
                matched = Some(candidate);
            }
        }
        let link =
            link_meta(matched.ok_or("qcom-adreno-a618: incomplete canonical 3D pipeline state")?);
        let (vs_packet, _) = segment_reg(words, start, end, SP_VS_BASE)
            .ok_or("qcom-adreno-a618: vertex shader base is missing")?;
        let (fs_packet, _) = segment_reg(words, start, end, SP_PS_BASE)
            .ok_or("qcom-adreno-a618: fragment shader base is missing")?;
        set_address_source(
            addresses,
            vs_packet + 1,
            AddressSource::CanonicalShader(link.vs),
        )?;
        set_address_source(
            addresses,
            fs_packet + 1,
            AddressSource::CanonicalShader(link.fs),
        )?;

        let (pitch_packet, pitch) = segment_reg(words, start, end, RB_MRT_PITCH)
            .ok_or("qcom-adreno-a618: MRT layout is missing")?;
        let (base_packet, _) = segment_reg(words, start, end, RB_MRT_BASE)
            .ok_or("qcom-adreno-a618: MRT base is missing")?;
        let (_, screen) = segment_reg(words, start, end, GRAS_SC_SCREEN_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: render bounds are missing")?;
        let (_, viewport) = segment_reg(words, start, end, GRAS_SC_VIEWPORT_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: viewport scissor is missing")?;
        let (_, window) = segment_reg(words, start, end, GRAS_SC_WINDOW_SCISSOR_TL)
            .ok_or("qcom-adreno-a618: window scissor is missing")?;
        let x = |word: u32| word & 0xffff;
        let y = |word: u32| word >> 16;
        if viewport != screen
            || x(screen[0]) > x(screen[1])
            || y(screen[0]) > y(screen[1])
            || x(window[0]) > x(window[1])
            || y(window[0]) > y(window[1])
            || x(window[0]) < x(screen[0])
            || y(window[0]) < y(screen[0])
            || x(window[1]) > x(screen[1])
            || y(window[1]) > y(screen[1])
        {
            return Err("qcom-adreno-a618: unsafe 3D scissor state");
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
                width: (screen[1] & 0xffff).saturating_add(1),
                height: (screen[1] >> 16).saturating_add(1),
                exact_extent: false,
                pitch_align: None,
                array_pitch: Some(ArrayPitchExpectation {
                    bytes: u64::from(pitch[1]) << 6,
                    alignment: 64,
                }),
            },
        )?;
        let (_, vertex_layout) = segment_reg(words, start, end, VFD_VERTEX_BUFFER_SIZE)
            .ok_or("qcom-adreno-a618: VFD buffer layout is missing")?;
        let (_, vertex_offset) = segment_reg(words, start, end, VFD_INDEX_OFFSET)
            .ok_or("qcom-adreno-a618: VFD vertex offset is missing")?;
        let required_vertex_bytes = packet.payload[2]
            .checked_add(vertex_offset[0])
            .and_then(|count| count.checked_mul(vertex_layout[1]))
            .ok_or("qcom-adreno-a618: VFD vertex span overflows")?;
        if vertex_layout[0] != required_vertex_bytes {
            return Err("qcom-adreno-a618: VFD size does not match draw span");
        }
        let (vertex_base, _) = segment_reg(words, start, end, VFD_VERTEX_BUFFER_BASE)
            .ok_or("qcom-adreno-a618: VFD buffer base is missing")?;
        let vertex_address = addresses
            .iter_mut()
            .find(|a| a.word_offset == vertex_base + 1)
            .ok_or("qcom-adreno-a618: VFD address lacks relocation")?;
        vertex_address.required_size = Some(u64::from(vertex_layout[0]));

        if let Some((descriptor_packet, descriptor)) = packets_in_segment(words, start, end)
            .find_map(|packet| {
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
        start = packet.word_offset + 4;
    }
    Ok(())
}

fn packets_in_segment<'a>(
    words: &'a [u32],
    start: u32,
    end: u32,
) -> impl Iterator<Item = adreno_a6xx_pm4::Packet<'a>> {
    Packets::new(words)
        .filter_map(Result::ok)
        .filter(move |p| p.word_offset >= start && p.word_offset < end)
}

fn validate_pm4(decoded: DecodedSubmit<'_>) -> Result<(Vec<u32>, Vec<AddressField>), &'static str> {
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

    let mut addresses = Vec::new();
    for packet in Packets::new(&words) {
        let packet = packet.map_err(|_| "qcom-adreno-a618: malformed PM4 packet stream")?;
        match packet.header {
            Header::Type4 { register, .. } => {
                validate_type4(register, packet.payload, packet.word_offset, &mut addresses)?
            }
            Header::Type7 { opcode, .. } => {
                validate_type7(opcode, packet.payload, packet.word_offset, &mut addresses)?
            }
        }
    }
    if addresses.len() != decoded.relocation_len() {
        return Err("qcom-adreno-a618: every GPU address must have one relocation");
    }
    validate_a2d_sequences(&words, &mut addresses)?;
    validate_3d_sequences(&words, &mut addresses)?;
    for (index, address) in addresses.iter().enumerate() {
        let relocation = decoded
            .relocation(index)
            .ok_or("qcom-adreno-a618: relocation table is incomplete")?;
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
    let (mut words, addresses) = validate_pm4(decoded)?;
    for index in 0..decoded.relocation_len() {
        let relocation = decoded
            .relocation(index)
            .ok_or("qcom-adreno-a618: relocation table is incomplete")?;
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
                || expectation.pitch_align.is_some_and(|align| {
                    align != image.row_pitch.trailing_zeros().saturating_sub(6).min(15)
                })
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
        FragmentProgram, FrontFace, LoadOp, PixelRect, PrimitiveTopology, RasterState,
        RenderPipelineDesc, SamplerDesc, StoreOp, TextureFormat, TextureSampleMode, TextureUsage,
        Transform, VertexAttribute, VertexBufferLayout, VertexFormat,
    };

    use super::{
        LinearImage, RB_A2D_DEST_BUFFER_BASE, ResolvedResource, relocate_one, validate_and_relocate,
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
    fn memcpy_requires_two_authorized_kernel_relocations() {
        let pm4 = [type7(0x75, 5).unwrap(), 4, 0, 0, 0, 0];
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
                let filter = if stride == 24 {
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
                let sampler = Packets::new(&artifact.words)
                    .filter_map(Result::ok)
                    .find(|packet| {
                        matches!(
                            packet.header,
                            Header::Type7 {
                                opcode: super::CP_LOAD_STATE6_FRAG,
                                ..
                            }
                        ) && packet.payload.len() == 7
                            && packet.payload[0] == ((4 << 18) | (1 << 22))
                    })
                    .unwrap();
                let begin = sampler.word_offset as usize;
                let end = begin + 1 + sampler.payload.len();
                let duplicate_packet = artifact.words[begin..end].to_vec();
                let mut hostile = artifact.clone();
                hostile.words.splice(end..end, duplicate_packet);
                for fixup in &mut hostile.fixups {
                    if fixup.word_offset as usize >= end {
                        fixup.word_offset += 8;
                    }
                }
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
                    sampler.payload[3],
                ];
                old_packet.extend_from_slice(&[0; 15]);
                padded_sampler.words.splice(begin..end, old_packet);
                for fixup in &mut padded_sampler.fixups {
                    if fixup.word_offset as usize >= end {
                        fixup.word_offset += 12;
                    }
                }
                assert!(accept_codegen(&padded_sampler).is_err());

                let descriptor = Packets::new(&artifact.words)
                    .filter_map(Result::ok)
                    .find(|packet| {
                        matches!(
                            packet.header,
                            Header::Type7 {
                                opcode: super::CP_LOAD_STATE6_FRAG,
                                ..
                            }
                        ) && packet.payload.len() == 19
                            && packet.payload[0] == ((1 << 14) | (4 << 18) | (1 << 22))
                    })
                    .unwrap();
                let mut bad_extent = artifact.clone();
                bad_extent.words[descriptor.word_offset as usize + 5] += 1;
                assert!(accept_codegen(&bad_extent).is_err());

                let mut bad_type = artifact.clone();
                bad_type.words[descriptor.word_offset as usize + 6] |= 2 << 29;
                assert!(accept_codegen(&bad_type).is_err());

                let mut bad_depth = artifact.clone();
                bad_depth.words[descriptor.word_offset as usize + 9] = 2 << 17;
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
