// SPDX-License-Identifier: GPL-2.0-only

//! Venus HFI 4xx packet definitions used by the SC7180 decoder.
//!
//! Packet layouts and numeric values follow Linux `hfi_cmds.h`,
//! `hfi_msgs.h`, and `hfi_helper.h`. HFI packets are native-endian arrays of
//! 32-bit words on little-endian SC7180.

use alloc::{vec, vec::Vec};

pub(crate) const HFI_CMD_SYS_INIT: u32 = 0x0001_0001;
pub(crate) const HFI_CMD_SYS_SET_PROPERTY: u32 = 0x0001_0005;
pub(crate) const HFI_CMD_SYS_SESSION_INIT: u32 = 0x0001_0007;
pub(crate) const HFI_CMD_SYS_SESSION_END: u32 = 0x0001_0008;
pub(crate) const HFI_CMD_SESSION_SET_PROPERTY: u32 = 0x0001_1001;
pub(crate) const HFI_CMD_SESSION_SET_BUFFERS: u32 = 0x0001_1002;
pub(crate) const HFI_CMD_SESSION_LOAD_RESOURCES: u32 = 0x0021_1001;
pub(crate) const HFI_CMD_SESSION_START: u32 = 0x0021_1002;
pub(crate) const HFI_CMD_SESSION_STOP: u32 = 0x0021_1003;
pub(crate) const HFI_CMD_SESSION_EMPTY_BUFFER: u32 = 0x0021_1004;
pub(crate) const HFI_CMD_SESSION_FILL_BUFFER: u32 = 0x0021_1005;
pub(crate) const HFI_CMD_SESSION_GET_PROPERTY: u32 = 0x0021_1009;
pub(crate) const HFI_CMD_SESSION_RELEASE_BUFFERS: u32 = 0x0021_100b;
pub(crate) const HFI_CMD_SESSION_RELEASE_RESOURCES: u32 = 0x0021_100c;
pub(crate) const HFI_CMD_SESSION_CONTINUE: u32 = 0x0021_100d;

pub(crate) const HFI_MSG_SYS_INIT: u32 = 0x0002_0001;
pub(crate) const HFI_MSG_SYS_SESSION_INIT: u32 = 0x0002_0006;
pub(crate) const HFI_MSG_SYS_SESSION_END: u32 = 0x0002_0007;
pub(crate) const HFI_MSG_EVENT_NOTIFY: u32 = 0x0002_1001;
pub(crate) const HFI_MSG_SESSION_LOAD_RESOURCES: u32 = 0x0022_1001;
pub(crate) const HFI_MSG_SESSION_START: u32 = 0x0022_1002;
pub(crate) const HFI_MSG_SESSION_STOP: u32 = 0x0022_1003;
pub(crate) const HFI_MSG_SESSION_EMPTY_BUFFER: u32 = 0x0022_1007;
pub(crate) const HFI_MSG_SESSION_FILL_BUFFER: u32 = 0x0022_1008;
pub(crate) const HFI_MSG_SESSION_PROPERTY_INFO: u32 = 0x0022_1009;
pub(crate) const HFI_MSG_SESSION_RELEASE_RESOURCES: u32 = 0x0022_100a;
pub(crate) const HFI_MSG_SESSION_RELEASE_BUFFERS: u32 = 0x0022_100c;

pub(crate) const HFI_EVENT_SYS_ERROR: u32 = 0x1;
pub(crate) const HFI_EVENT_SESSION_ERROR: u32 = 0x2;
pub(crate) const HFI_EVENT_DATA_SEQUENCE_CHANGED_SUFFICIENT: u32 = 0x0100_0001;
pub(crate) const HFI_EVENT_DATA_SEQUENCE_CHANGED_INSUFFICIENT: u32 = 0x0100_0002;
pub(crate) const HFI_EVENT_SESSION_SEQUENCE_CHANGED: u32 = 0x0100_0003;

pub(crate) const HFI_ERR_NONE: u32 = 0;
pub(crate) const HFI_ERR_SESSION_EMPTY_BUFFER_DONE_OUTPUT_PENDING: u32 = 0x0100_1001;

pub(crate) const HFI_BUFFERFLAG_ENDOFFRAME: u32 = 0x10;

pub(crate) const HFI_BUFFER_INPUT: u32 = 0x1;
pub(crate) const HFI_BUFFER_OUTPUT: u32 = 0x2;
pub(crate) const HFI_BUFFER_OUTPUT2: u32 = 0x3;
pub(crate) const HFI_BUFFER_INTERNAL_PERSIST: u32 = 0x4;
pub(crate) const HFI_BUFFER_INTERNAL_PERSIST_1: u32 = 0x5;
pub(crate) const HFI_BUFFER_INTERNAL_SCRATCH: u32 = 0x6;
pub(crate) const HFI_BUFFER_INTERNAL_SCRATCH_1: u32 = 0x7;
pub(crate) const HFI_BUFFER_INTERNAL_SCRATCH_2: u32 = 0x8;

pub(crate) const HFI_PROPERTY_SYS_DEBUG_CONFIG: u32 = 0x1;
pub(crate) const HFI_PROPERTY_SYS_CODEC_POWER_PLANE_CTRL: u32 = 0x5;
pub(crate) const HFI_PROPERTY_PARAM_FRAME_SIZE: u32 = 0x1001;
pub(crate) const HFI_PROPERTY_PARAM_UNCOMPRESSED_FORMAT_SELECT: u32 = 0x1003;
pub(crate) const HFI_PROPERTY_PARAM_WORK_MODE: u32 = 0x1015;
pub(crate) const HFI_PROPERTY_PARAM_BUFFER_COUNT_ACTUAL: u32 = 0x0020_1001;
pub(crate) const HFI_PROPERTY_PARAM_BUFFER_SIZE_ACTUAL: u32 = 0x0020_100c;
pub(crate) const HFI_PROPERTY_CONFIG_BUFFER_REQUIREMENTS: u32 = 0x0020_2001;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_MULTI_STREAM: u32 = 0x0100_3001;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_PIXEL_BITDEPTH: u32 = 0x0100_3007;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_PIC_STRUCT: u32 = 0x0100_3009;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_COLOUR_SPACE: u32 = 0x0100_300a;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_OUTPUT_ORDER: u32 = 0x0120_3005;
pub(crate) const HFI_PROPERTY_PARAM_VDEC_DPB_COUNTS: u32 = 0x0120_300e;
pub(crate) const HFI_PROPERTY_PARAM_PROFILE_LEVEL_CURRENT: u32 = 0x1005;
pub(crate) const HFI_PROPERTY_CONFIG_VDEC_ENTROPY: u32 = 0x0120_4004;
pub(crate) const HFI_INDEX_EXTRADATA_INPUT_CROP: u32 = 0x0700_000e;

pub(crate) const HFI_COLOR_FORMAT_NV12: u32 = 0x2;
pub(crate) const HFI_COLOR_FORMAT_NV12_UBWC: u32 = 0x8002;
pub(crate) const HFI_VIDEO_CODEC_H264: u32 = 0x2;
pub(crate) const VIDC_SESSION_TYPE_DEC: u32 = 0x2;
pub(crate) const VIDC_WORK_MODE_1: u32 = 1;
pub(crate) const VIDC_WORK_MODE_2: u32 = 2;
pub(crate) const HFI_OUTPUT_ORDER_DECODE: u32 = 0x0100_0002;

const HFI_DEBUG_MSG_ERROR: u32 = 0x08;
const HFI_DEBUG_MSG_FATAL: u32 = 0x10;
const HFI_DEBUG_MODE_QUEUE: u32 = 0x1;
const HFI_VIDEO_ARCH_OX: u32 = 0x1;

fn finish(mut words: Vec<u32>) -> Vec<u32> {
    words[0] = (words.len() * core::mem::size_of::<u32>()) as u32;
    words
}

pub(crate) fn sys_init() -> Vec<u32> {
    finish(vec![0, HFI_CMD_SYS_INIT, HFI_VIDEO_ARCH_OX])
}

pub(crate) fn sys_debug_errors_only() -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SYS_SET_PROPERTY,
        1,
        HFI_PROPERTY_SYS_DEBUG_CONFIG,
        HFI_DEBUG_MSG_ERROR | HFI_DEBUG_MSG_FATAL,
        HFI_DEBUG_MODE_QUEUE,
    ])
}

pub(crate) fn sys_disable_power_collapse() -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SYS_SET_PROPERTY,
        1,
        HFI_PROPERTY_SYS_CODEC_POWER_PLANE_CTRL,
        0,
    ])
}

pub(crate) fn session_init(session: u32) -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SYS_SESSION_INIT,
        session,
        VIDC_SESSION_TYPE_DEC,
        HFI_VIDEO_CODEC_H264,
    ])
}

pub(crate) fn session_command(command: u32, session: u32) -> Vec<u32> {
    finish(vec![0, command, session])
}

fn session_property(session: u32, property: u32, data: &[u32]) -> Vec<u32> {
    let mut words = Vec::with_capacity(5 + data.len());
    words.extend_from_slice(&[0, HFI_CMD_SESSION_SET_PROPERTY, session, 1, property]);
    words.extend_from_slice(data);
    finish(words)
}

pub(crate) fn set_frame_size(session: u32, buffer_type: u32, width: u32, height: u32) -> Vec<u32> {
    session_property(
        session,
        HFI_PROPERTY_PARAM_FRAME_SIZE,
        &[buffer_type, width, height],
    )
}

pub(crate) fn set_raw_format(session: u32, buffer_type: u32, format: u32) -> Vec<u32> {
    session_property(
        session,
        HFI_PROPERTY_PARAM_UNCOMPRESSED_FORMAT_SELECT,
        &[buffer_type, format],
    )
}

pub(crate) fn set_multistream(session: u32, buffer_type: u32, enable: bool) -> Vec<u32> {
    // HFI 4xx inherits the compact 3xx two-word multi-stream payload.
    session_property(
        session,
        HFI_PROPERTY_PARAM_VDEC_MULTI_STREAM,
        &[buffer_type, u32::from(enable)],
    )
}

pub(crate) fn set_work_mode(session: u32, mode: u32) -> Vec<u32> {
    session_property(session, HFI_PROPERTY_PARAM_WORK_MODE, &[mode])
}

pub(crate) fn set_decode_order(session: u32) -> Vec<u32> {
    session_property(
        session,
        HFI_PROPERTY_PARAM_VDEC_OUTPUT_ORDER,
        &[HFI_OUTPUT_ORDER_DECODE],
    )
}

pub(crate) fn set_buffer_count(session: u32, buffer_type: u32, count: u32) -> Vec<u32> {
    // HFI 4xx appends count_min_host after the common payload.
    session_property(
        session,
        HFI_PROPERTY_PARAM_BUFFER_COUNT_ACTUAL,
        &[buffer_type, count, count],
    )
}

pub(crate) fn set_buffer_size(session: u32, buffer_type: u32, size: u32) -> Vec<u32> {
    session_property(
        session,
        HFI_PROPERTY_PARAM_BUFFER_SIZE_ACTUAL,
        &[buffer_type, size],
    )
}

pub(crate) fn get_buffer_requirements(session: u32) -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SESSION_GET_PROPERTY,
        session,
        1,
        HFI_PROPERTY_CONFIG_BUFFER_REQUIREMENTS,
    ])
}

pub(crate) fn set_internal_buffer(
    session: u32,
    buffer_type: u32,
    size: u32,
    address: u32,
) -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SESSION_SET_BUFFERS,
        session,
        buffer_type,
        size,
        0,
        size,
        1,
        address,
    ])
}

pub(crate) fn release_internal_buffer(
    session: u32,
    buffer_type: u32,
    size: u32,
    address: u32,
) -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SESSION_RELEASE_BUFFERS,
        session,
        buffer_type,
        size,
        0,
        1,
        1,
        address,
    ])
}

pub(crate) fn empty_buffer(
    session: u32,
    timestamp: u64,
    input_tag: u32,
    address: u32,
    allocation_len: u32,
    filled_len: u32,
) -> Vec<u32> {
    finish(vec![
        0,
        HFI_CMD_SESSION_EMPTY_BUFFER,
        session,
        (timestamp >> 32) as u32,
        timestamp as u32,
        HFI_BUFFERFLAG_ENDOFFRAME,
        0,
        0,
        0,
        allocation_len,
        filled_len,
        input_tag,
        address,
        0,
        0,
    ])
}

pub(crate) fn fill_buffer(
    session: u32,
    buffer_type: u32,
    output_tag: u32,
    address: u32,
    allocation_len: u32,
) -> Vec<u32> {
    let stream_id = if buffer_type == HFI_BUFFER_OUTPUT2 {
        1
    } else {
        0
    };
    finish(vec![
        0,
        HFI_CMD_SESSION_FILL_BUFFER,
        session,
        stream_id,
        0,
        allocation_len,
        0,
        output_tag,
        address,
        0,
        0,
    ])
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BufferRequirement {
    pub(crate) buffer_type: u32,
    pub(crate) size: u32,
    pub(crate) count_min: u32,
    pub(crate) count_actual: u32,
    pub(crate) contiguous: u32,
    pub(crate) alignment: u32,
}

pub(crate) fn parse_buffer_requirements(
    words: &[u32],
) -> Result<Vec<BufferRequirement>, &'static str> {
    if words.len() < 5
        || words[1] != HFI_MSG_SESSION_PROPERTY_INFO
        || words[4] != HFI_PROPERTY_CONFIG_BUFFER_REQUIREMENTS
    {
        return Err("qcom-venus-sc7180: malformed HFI buffer-requirements message");
    }
    let data = &words[5..];
    if data.is_empty() || data.len() % 8 != 0 {
        return Err("qcom-venus-sc7180: invalid HFI buffer-requirements payload");
    }
    let mut requirements = Vec::with_capacity(data.len() / 8);
    for raw in data.chunks_exact(8) {
        requirements.push(BufferRequirement {
            buffer_type: raw[0],
            size: raw[1],
            // HFI 4xx swaps the common hold_count/count_min meanings.
            count_min: raw[3],
            count_actual: raw[5],
            contiguous: raw[6],
            alignment: raw[7],
        });
    }
    Ok(requirements)
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SequenceInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) crop_left: u32,
    pub(crate) crop_top: u32,
    pub(crate) crop_width: u32,
    pub(crate) crop_height: u32,
    pub(crate) minimum_dpb_count: u32,
}

pub(crate) fn parse_sequence_changed(words: &[u32]) -> Result<SequenceInfo, &'static str> {
    if words.len() < 6
        || words[1] != HFI_MSG_EVENT_NOTIFY
        || words[3] != HFI_EVENT_SESSION_SEQUENCE_CHANGED
    {
        return Err("qcom-venus-sc7180: malformed HFI sequence-change event");
    }
    match words[4] {
        HFI_EVENT_DATA_SEQUENCE_CHANGED_SUFFICIENT
        | HFI_EVENT_DATA_SEQUENCE_CHANGED_INSUFFICIENT => {}
        _ => return Err("qcom-venus-sc7180: invalid HFI sequence-change reason"),
    }
    let mut info = SequenceInfo::default();
    let mut remaining = words[5] as usize;
    let mut cursor = 6;
    while remaining != 0 {
        let property = *words
            .get(cursor)
            .ok_or("qcom-venus-sc7180: truncated HFI sequence property")?;
        cursor += 1;
        let data_words = match property {
            HFI_PROPERTY_PARAM_FRAME_SIZE => {
                let data = words
                    .get(cursor..cursor + 3)
                    .ok_or("qcom-venus-sc7180: truncated HFI frame-size property")?;
                info.width = data[1];
                info.height = data[2];
                3
            }
            HFI_PROPERTY_PARAM_PROFILE_LEVEL_CURRENT => 2,
            HFI_PROPERTY_PARAM_VDEC_PIXEL_BITDEPTH => 2,
            HFI_PROPERTY_PARAM_VDEC_PIC_STRUCT
            | HFI_PROPERTY_PARAM_VDEC_COLOUR_SPACE
            | HFI_PROPERTY_CONFIG_VDEC_ENTROPY => 1,
            HFI_PROPERTY_CONFIG_BUFFER_REQUIREMENTS => {
                let data = words
                    .get(cursor..cursor + 8)
                    .ok_or("qcom-venus-sc7180: truncated HFI sequence buffer requirement")?;
                info.minimum_dpb_count = data[3];
                8
            }
            HFI_INDEX_EXTRADATA_INPUT_CROP => {
                let data = words
                    .get(cursor..cursor + 7)
                    .ok_or("qcom-venus-sc7180: truncated HFI crop property")?;
                info.crop_left = data[3];
                info.crop_top = data[4];
                info.crop_width = data[5];
                info.crop_height = data[6];
                7
            }
            HFI_PROPERTY_PARAM_VDEC_DPB_COUNTS => {
                let data = words
                    .get(cursor..cursor + 5)
                    .ok_or("qcom-venus-sc7180: truncated HFI DPB-count property")?;
                info.minimum_dpb_count = data[4];
                5
            }
            _ => {
                // HFI sequence properties do not carry individual payload
                // lengths. Stop safely at the first unknown property rather
                // than interpreting its payload as another property. The
                // dimensions already parsed remain valid, and reconfigure
                // queries buffer requirements separately.
                break;
            }
        };
        cursor = cursor
            .checked_add(data_words)
            .ok_or("qcom-venus-sc7180: HFI sequence property overflow")?;
        remaining -= 1;
    }
    if info.width == 0 || info.height == 0 {
        return Err("qcom-venus-sc7180: sequence event omitted frame dimensions");
    }
    if info.crop_width == 0 || info.crop_height == 0 {
        info.crop_width = info.width;
        info.crop_height = info.height;
    }
    Ok(info)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FillDone {
    pub(crate) session: u32,
    pub(crate) stream_id: u32,
    pub(crate) error: u32,
    pub(crate) flags: u32,
    pub(crate) filled_len: u32,
    pub(crate) offset: u32,
    pub(crate) output_tag: u32,
    pub(crate) packet_buffer: u32,
}

pub(crate) fn parse_fill_done(words: &[u32]) -> Result<FillDone, &'static str> {
    if words.len() < 25 || words[1] != HFI_MSG_SESSION_FILL_BUFFER {
        return Err("qcom-venus-sc7180: malformed HFI fill-buffer-done message");
    }
    Ok(FillDone {
        session: words[2],
        stream_id: words[3],
        error: words[5],
        flags: words[8],
        filled_len: words[13],
        offset: words[14],
        output_tag: words[21],
        packet_buffer: words[23],
    })
}

pub(crate) fn packet_type(words: &[u32]) -> Option<u32> {
    words.get(1).copied()
}

pub(crate) fn session_id(words: &[u32]) -> Option<u32> {
    words.get(2).copied()
}

pub(crate) fn response_error(words: &[u32]) -> Option<u32> {
    match packet_type(words)? {
        HFI_MSG_SYS_INIT => words.get(2).copied(),
        HFI_MSG_SYS_SESSION_INIT
        | HFI_MSG_SYS_SESSION_END
        | HFI_MSG_SESSION_LOAD_RESOURCES
        | HFI_MSG_SESSION_START
        | HFI_MSG_SESSION_STOP
        | HFI_MSG_SESSION_EMPTY_BUFFER
        | HFI_MSG_SESSION_RELEASE_RESOURCES
        | HFI_MSG_SESSION_RELEASE_BUFFERS => words.get(3).copied(),
        HFI_MSG_SESSION_FILL_BUFFER => words.get(5).copied(),
        _ => None,
    }
}

pub(crate) fn is_fatal_event(words: &[u32], session: u32) -> bool {
    packet_type(words) == Some(HFI_MSG_EVENT_NOTIFY)
        && session_id(words).is_some_and(|value| value == 0 || value == session)
        && words
            .get(3)
            .is_some_and(|event| *event == HFI_EVENT_SYS_ERROR || *event == HFI_EVENT_SESSION_ERROR)
}
