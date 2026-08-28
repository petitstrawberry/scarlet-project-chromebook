// SPDX-License-Identifier: GPL-2.0-only

//! Pure legacy A618 HFI v1 layouts, kept separate for byte-golden testing.

extern crate alloc;

use alloc::{vec, vec::Vec};

pub(crate) const TABLE_HEADER_WORDS: usize = 6;
pub(crate) const QUEUE_HEADER_WORDS: usize = 12;
pub(crate) const COMMAND_HEADER_WORD: usize = TABLE_HEADER_WORDS;
pub(crate) const RESPONSE_HEADER_WORD: usize = TABLE_HEADER_WORDS + QUEUE_HEADER_WORDS;
pub(crate) const QUEUE_WORDS: usize = 0x400;

const Q_STATUS: usize = 0;
const Q_IOVA: usize = 1;
const Q_TYPE: usize = 2;
const Q_SIZE: usize = 3;
const Q_RX_WATERMARK: usize = 6;
const Q_TX_WATERMARK: usize = 7;
const Q_RX_REQUEST: usize = 8;

const HFI_F2H_MSG_ERROR: u8 = 100;
const HFI_F2H_MSG_ACK: u8 = 126;
const HFI_MSG_CMD: u32 = 0;
const HFI_MSG_ACK: u32 = 1;
const HFI_MSG_ACK_V1: u32 = 2;

pub(crate) const MAX_GPU_LEVELS: usize = 16;
pub(crate) const MAX_GMU_LEVELS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HfiPerfLevel {
    pub(crate) vote: u32,
    pub(crate) frequency_khz: u32,
}

fn levels_are_valid(levels: &[HfiPerfLevel], maximum: usize) -> bool {
    !levels.is_empty()
        && levels.len() <= maximum
        && levels[0].frequency_khz == 0
        && levels
            .windows(2)
            .all(|pair| pair[0].frequency_khz < pair[1].frequency_khz)
}

fn initialize_queue(words: &mut [u32], header: usize, iova: u32, id: u32) {
    words[header + Q_STATUS] = 1;
    words[header + Q_IOVA] = iova;
    words[header + Q_TYPE] = (10 << 8) | id;
    words[header + Q_SIZE] = QUEUE_WORDS as u32;
    words[header + Q_RX_WATERMARK] = 1;
    words[header + Q_TX_WATERMARK] = 1;
    words[header + Q_RX_REQUEST] = 1;
}

pub(crate) fn initialize_legacy_table(
    words: &mut [u32],
    hfi_iova: u32,
) -> Result<(), &'static str> {
    if words.len() < 0x3000 / 4 {
        return Err("qcom-adreno-a618: HFI allocation is too small");
    }
    words.fill(0);
    words[0] = 0;
    words[1] = ((TABLE_HEADER_WORDS + QUEUE_HEADER_WORDS * 2) * 4) as u32;
    words[2] = TABLE_HEADER_WORDS as u32;
    words[3] = QUEUE_HEADER_WORDS as u32;
    words[4] = 2;
    words[5] = 2;
    initialize_queue(words, COMMAND_HEADER_WORD, hfi_iova + 0x1000, 0);
    initialize_queue(words, RESPONSE_HEADER_WORD, hfi_iova + 0x2000, 4);
    Ok(())
}

pub(crate) fn performance_table(
    gx_levels: &[HfiPerfLevel],
    cx_levels: &[HfiPerfLevel],
) -> Result<Vec<u32>, &'static str> {
    if !levels_are_valid(gx_levels, MAX_GPU_LEVELS) || !levels_are_valid(cx_levels, MAX_GMU_LEVELS)
    {
        return Err("qcom-adreno-a618: HFI performance table level count is invalid");
    }
    let mut message = vec![0; 3 + MAX_GPU_LEVELS * 2 + MAX_GMU_LEVELS * 2];
    message[1] = gx_levels.len() as u32;
    message[2] = cx_levels.len() as u32;
    for (index, level) in gx_levels.iter().enumerate() {
        let offset = 3 + index * 2;
        message[offset] = level.vote;
        message[offset + 1] = level.frequency_khz;
    }
    let cx = 3 + MAX_GPU_LEVELS * 2;
    for (index, level) in cx_levels.iter().enumerate() {
        let offset = cx + index * 2;
        message[offset] = level.vote;
        message[offset + 1] = level.frequency_khz;
    }
    Ok(message)
}

pub(crate) fn bandwidth_table() -> Vec<u32> {
    let mut message = vec![0; 160];
    message[1] = 1;
    message[2] = 1;
    message[3] = 3;
    message[4] = 1;
    message[5] = 1;
    message[6] = 0x5007c;
    message[12] = 0x4000_0000;
    message[18] = 0x6000_0001;
    message[24] = 0x50000;
    message[25] = 0x5003c;
    message[26] = 0x5000c;
    message[32] = 0x4000_0000;
    message[33] = 0x4000_0000;
    message[34] = 0x4000_0000;
    message
}

pub(crate) fn ack_matches(
    response: &[u32],
    command_id: u8,
    sequence: u32,
) -> Result<bool, &'static str> {
    let header = *response
        .first()
        .ok_or("qcom-adreno-a618: empty HFI response")?;
    let response_id = (header & 0xff) as u8;
    if response_id == HFI_F2H_MSG_ERROR {
        return Ok(false);
    }
    let response_type = (header >> 16) & 0xf;
    if response_id != HFI_F2H_MSG_ACK || !matches!(response_type, HFI_MSG_ACK | HFI_MSG_ACK_V1) {
        return Ok(false);
    }
    if response.len() < 3 {
        return Err("qcom-adreno-a618: short HFI acknowledgement");
    }
    let returned = response[1];
    if (returned & 0xff) as u8 != command_id
        || (returned >> 16) & 0xf != HFI_MSG_CMD
        || (returned >> 20) & 0xfff != sequence
    {
        return Ok(false);
    }
    if response[2] != 0 {
        return Err("qcom-adreno-a618: GMU rejected HFI message");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn legacy_queue_table_matches_linux_120_byte_contract() {
        let mut words = std::vec![0xdead_beef; 0x1000];
        initialize_legacy_table(&mut words, 0x6000_9000).unwrap();
        assert_eq!(&words[..6], &[0, 120, 6, 12, 2, 2]);
        assert_eq!(
            &words[COMMAND_HEADER_WORD..COMMAND_HEADER_WORD + 12],
            &[1, 0x6000_a000, 0x0a00, 0x400, 0, 0, 1, 1, 1, 0, 0, 0]
        );
        assert_eq!(
            &words[RESPONSE_HEADER_WORD..RESPONSE_HEADER_WORD + 12],
            &[1, 0x6000_b000, 0x0a04, 0x400, 0, 0, 1, 1, 1, 0, 0, 0]
        );
        assert_eq!(words[TABLE_HEADER_WORDS + QUEUE_HEADER_WORDS * 2], 0);
    }

    #[test]
    fn coachz_perf_v1_table_has_canonical_43_dwords() {
        let gx_levels = [
            HfiPerfLevel {
                vote: 0x11,
                frequency_khz: 0,
            },
            HfiPerfLevel {
                vote: 0x22,
                frequency_khz: 180_000,
            },
            HfiPerfLevel {
                vote: 0x55,
                frequency_khz: 800_000,
            },
        ];
        let cx_levels = [
            HfiPerfLevel {
                vote: 0x33,
                frequency_khz: 0,
            },
            HfiPerfLevel {
                vote: 0x44,
                frequency_khz: 200_000,
            },
        ];
        let table = performance_table(&gx_levels, &cx_levels).unwrap();
        assert_eq!(table.len(), 43);
        assert_eq!(
            &table[..9],
            &[0, 3, 2, 0x11, 0, 0x22, 180_000, 0x55, 800_000]
        );
        assert_eq!(&table[35..39], &[0x33, 0, 0x44, 200_000]);
        assert!(table[9..35].iter().all(|word| *word == 0));
        assert!(table[39..].iter().all(|word| *word == 0));
    }

    #[test]
    fn perf_v1_rejects_counts_that_firmware_cannot_represent() {
        let level = HfiPerfLevel {
            vote: 0,
            frequency_khz: 0,
        };
        assert!(performance_table(&[], &[level]).is_err());
        assert!(performance_table(&[level], &[]).is_err());
        assert!(performance_table(&[level; 17], &[level]).is_err());
        assert!(performance_table(&[level], &[level; 5]).is_err());
        assert!(
            performance_table(
                &[
                    HfiPerfLevel {
                        vote: 1,
                        frequency_khz: 180_000,
                    },
                    HfiPerfLevel {
                        vote: 2,
                        frequency_khz: 800_000,
                    },
                ],
                &[level],
            )
            .is_err()
        );
    }

    #[test]
    fn a618_bandwidth_table_matches_canonical_struct_offsets() {
        let table = bandwidth_table();
        assert_eq!(table.len(), 160);
        let nonzero: std::vec::Vec<_> = table
            .iter()
            .enumerate()
            .filter_map(|(index, value)| (*value != 0).then_some((index, *value)))
            .collect();
        assert_eq!(
            nonzero,
            std::vec![
                (1, 1),
                (2, 1),
                (3, 3),
                (4, 1),
                (5, 1),
                (6, 0x5007c),
                (12, 0x4000_0000),
                (18, 0x6000_0001),
                (24, 0x50000),
                (25, 0x5003c),
                (26, 0x5000c),
                (32, 0x4000_0000),
                (33, 0x4000_0000),
                (34, 0x4000_0000),
            ]
        );
    }

    #[test]
    fn only_matching_ack_or_ack_v1_can_complete_a_command() {
        let sequence = 7;
        let returned = (sequence << 20) | 4;
        assert_eq!(
            ack_matches(&[(1 << 16) | (3 << 8) | 126, returned, 0], 4, sequence),
            Ok(true)
        );
        assert_eq!(
            ack_matches(&[(2 << 16) | (3 << 8) | 126, returned, 0], 4, sequence),
            Ok(true)
        );
        assert_eq!(
            ack_matches(&[(3 << 16) | (3 << 8) | 126, returned, 0], 4, sequence),
            Ok(false)
        );
        assert_eq!(
            ack_matches(&[(1 << 16) | (3 << 8) | 42, returned, 0], 4, sequence),
            Ok(false)
        );
        assert_eq!(
            ack_matches(&[(1 << 16) | (3 << 8) | 126, returned, 1], 4, sequence),
            Err("qcom-adreno-a618: GMU rejected HFI message")
        );
    }
}
