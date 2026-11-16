// SPDX-License-Identifier: GPL-2.0-only
//! WCN3990 SNOC pipe topology from Linux ath10k/snoc.c.
use crate::qmi;
use alloc::vec::Vec;
// The QMI structure omits ce_pipe_config's host-side reserved word.
pub const TARGET_PIPES: [[u32; 5]; 12] = [
    [0, 2, 32, 2048, 0],
    [1, 1, 32, 2048, 0],
    [2, 1, 64, 2048, 0],
    [3, 2, 32, 2048, 0],
    [4, 2, 256, 256, 8],
    [5, 2, 1024, 64, 8],
    [6, 3, 32, 16384, 0],
    [7, 4, 0, 0, 8],
    [8, 1, 32, 2048, 0],
    [9, 1, 32, 2048, 0],
    [10, 1, 32, 2048, 0],
    [11, 1, 32, 2048, 0],
];
pub const SERVICE_PIPES: [[u32; 3]; 18] = [
    [0x104, 2, 3],
    [0x104, 1, 2],
    [0x102, 2, 3],
    [0x102, 1, 2],
    [0x101, 2, 3],
    [0x101, 1, 2],
    [0x103, 2, 3],
    [0x103, 1, 2],
    [0x100, 2, 3],
    [0x100, 1, 2],
    [1, 2, 0],
    [1, 1, 2],
    [0x300, 2, 4],
    [0x300, 1, 1],
    [0xfe00, 2, 5],
    [0x301, 1, 9],
    [0x302, 1, 10],
    [0x600, 1, 11],
];
pub const SHADOW_REGISTERS: [(u16, u16); 12] = [
    (0, 0x3c),
    (3, 0x3c),
    (4, 0x3c),
    (5, 0x3c),
    (7, 0x3c),
    (1, 0x40),
    (2, 0x40),
    (7, 0x40),
    (8, 0x40),
    (9, 0x40),
    (10, 0x40),
    (11, 0x40),
];
pub fn configuration() -> Vec<u8> {
    let mut body = Vec::new();
    let mut pipes = Vec::new();
    pipes.push(TARGET_PIPES.len() as u8);
    for row in TARGET_PIPES {
        for word in row {
            pipes.extend_from_slice(&word.to_le_bytes());
        }
    }
    qmi::tlv(&mut body, 0x11, &pipes).unwrap();
    let mut services = Vec::new();
    services.push(SERVICE_PIPES.len() as u8);
    for row in SERVICE_PIPES {
        for word in row {
            services.extend_from_slice(&word.to_le_bytes());
        }
    }
    qmi::tlv(&mut body, 0x12, &services).unwrap();
    let mut shadow = Vec::new();
    shadow.push(SHADOW_REGISTERS.len() as u8);
    for (ce, offset) in SHADOW_REGISTERS {
        shadow.extend_from_slice(&ce.to_le_bytes());
        shadow.extend_from_slice(&offset.to_le_bytes());
    }
    qmi::tlv(&mut body, 0x13, &shadow).unwrap();
    body
}
