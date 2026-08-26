//! Pure Adreno A6xx PM4 packet grammar.
//!
//! This crate intentionally contains packet syntax only.  It does not decide
//! which packets or registers are safe for an untrusted submission; that
//! policy belongs to the kernel driver.

#![no_std]

/// PM4 type-4 packet tag.
pub const TYPE4_TAG: u32 = 0x4 << 28;
/// PM4 type-7 packet tag.
pub const TYPE7_TAG: u32 = 0x7 << 28;
/// Largest type-4 payload representable by the header.
pub const TYPE4_MAX_COUNT: u16 = 0x7e;
/// Largest type-7 payload representable by the header.
pub const TYPE7_MAX_COUNT: u16 = 0x3fff;
/// Largest type-4 register index representable by the header.
pub const TYPE4_MAX_REGISTER: u32 = 0x3ffff;

/// A decoded A6xx PM4 packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
    /// Consecutive register writes.
    Type4 {
        /// First register index.
        register: u32,
        /// Number of following payload dwords.
        count: u16,
    },
    /// Opcode packet.
    Type7 {
        /// Seven-bit opcode.
        opcode: u8,
        /// Number of following payload dwords.
        count: u16,
    },
}

/// Packet-header construction or decoding failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The requested field cannot be represented by the packet header.
    FieldOutOfRange,
    /// The dword does not carry a supported A6xx packet type.
    UnsupportedPacketType,
    /// One of the odd-parity header checks failed.
    InvalidParity,
    /// A packet payload extends past the supplied command stream.
    TruncatedPacket,
}

const fn odd_parity_bit(value: u32) -> u32 {
    (value.count_ones() ^ 1) & 1
}

/// Encode a type-4 header.
///
/// The count and register fields include the odd-parity bits required by the
/// A6xx command processor.
pub const fn type4(register: u32, count: u16) -> Result<u32, Error> {
    if register == 0 || register > TYPE4_MAX_REGISTER || count == 0 || count > TYPE4_MAX_COUNT {
        return Err(Error::FieldOutOfRange);
    }
    let count = count as u32;
    Ok(TYPE4_TAG
        | count
        | (odd_parity_bit(count) << 7)
        | (register << 8)
        | (odd_parity_bit(register) << 27))
}

/// Encode a type-7 header.
///
/// The opcode and count fields include the odd-parity bits required by the
/// A6xx command processor.
pub const fn type7(opcode: u8, count: u16) -> Result<u32, Error> {
    if opcode > 0x7f || count > TYPE7_MAX_COUNT {
        return Err(Error::FieldOutOfRange);
    }
    let opcode = opcode as u32;
    let count = count as u32;
    Ok(TYPE7_TAG
        | count
        | (odd_parity_bit(count) << 15)
        | (opcode << 16)
        | (odd_parity_bit(opcode) << 23))
}

/// Decode and parity-check one packet header.
pub const fn decode_header(word: u32) -> Result<Header, Error> {
    match word >> 28 {
        4 => {
            let count = word & 0x7f;
            let count_parity = (word >> 7) & 1;
            let register = (word >> 8) & TYPE4_MAX_REGISTER;
            let register_parity = (word >> 27) & 1;
            if word & (1 << 26) != 0 || register == 0 || count == 0 || count > 0x7e {
                return Err(Error::FieldOutOfRange);
            }
            if count_parity != odd_parity_bit(count) || register_parity != odd_parity_bit(register)
            {
                return Err(Error::InvalidParity);
            }
            Ok(Header::Type4 {
                register,
                count: count as u16,
            })
        }
        7 => {
            let count = word & 0x3fff;
            let count_parity = (word >> 15) & 1;
            let opcode = (word >> 16) & 0x7f;
            let opcode_parity = (word >> 23) & 1;
            if word & 0x0f00_0000 != 0 {
                return Err(Error::FieldOutOfRange);
            }
            if count_parity != odd_parity_bit(count) || opcode_parity != odd_parity_bit(opcode) {
                return Err(Error::InvalidParity);
            }
            Ok(Header::Type7 {
                opcode: opcode as u8,
                count: count as u16,
            })
        }
        _ => Err(Error::UnsupportedPacketType),
    }
}

/// One completely bounded packet borrowed from a command stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    /// Decoded packet header.
    pub header: Header,
    /// Packet payload immediately following the header.
    pub payload: &'a [u32],
    /// Header dword offset in the full command stream.
    pub word_offset: u32,
}

/// Iterator that completely consumes a PM4 dword stream.
pub struct Packets<'a> {
    words: &'a [u32],
    offset: usize,
    failed: bool,
}

impl<'a> Packets<'a> {
    /// Construct an iterator over `words`.
    pub const fn new(words: &'a [u32]) -> Self {
        Self {
            words,
            offset: 0,
            failed: false,
        }
    }
}

impl<'a> Iterator for Packets<'a> {
    type Item = Result<Packet<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset == self.words.len() {
            return None;
        }
        let start = self.offset;
        let header = match decode_header(self.words[start]) {
            Ok(header) => header,
            Err(error) => {
                self.failed = true;
                return Some(Err(error));
            }
        };
        let count = match header {
            Header::Type4 { count, .. } | Header::Type7 { count, .. } => usize::from(count),
        };
        let end = match start
            .checked_add(1)
            .and_then(|value| value.checked_add(count))
        {
            Some(end) if end <= self.words.len() => end,
            _ => {
                self.failed = true;
                return Some(Err(Error::TruncatedPacket));
            }
        };
        self.offset = end;
        Some(Ok(Packet {
            header,
            payload: &self.words[start + 1..end],
            word_offset: start as u32,
        }))
    }
}

/// Common A6xx type-7 opcodes used by the trusted kernel stream builder.
pub mod opcode {
    /// Wait until preceding asynchronous command-processor memory writes complete.
    pub const WAIT_MEM_WRITES: u8 = 0x12;
    /// Wait until preceding work is idle.
    pub const WAIT_FOR_IDLE: u8 = 0x26;
    /// A6xx blit operation.
    pub const BLIT: u8 = 0x2c;
    /// Emit a GPU event.
    pub const EVENT_WRITE: u8 = 0x46;
    /// Initialize the micro-engine.
    pub const ME_INIT: u8 = 0x48;
    /// Set a command-stream marker.
    pub const SET_MARKER: u8 = 0x65;
}

#[cfg(test)]
mod tests {
    use super::{Error, Header, Packets, decode_header, type4, type7};

    #[test]
    fn headers_round_trip_with_parity() {
        let register = type4(0x12345, 3).unwrap();
        assert_eq!(
            decode_header(register).unwrap(),
            Header::Type4 {
                register: 0x12345,
                count: 3,
            }
        );
        let opcode = type7(0x46, 4).unwrap();
        assert_eq!(
            decode_header(opcode).unwrap(),
            Header::Type7 {
                opcode: 0x46,
                count: 4,
            }
        );
    }

    #[test]
    fn corrupt_parity_is_rejected() {
        let header = type7(0x26, 0).unwrap();
        assert_eq!(decode_header(header ^ (1 << 23)), Err(Error::InvalidParity));
    }

    #[test]
    fn iterator_rejects_truncation() {
        let words = [type4(7, 2).unwrap(), 1];
        assert_eq!(
            Packets::new(&words).next(),
            Some(Err(Error::TruncatedPacket))
        );
    }

    #[test]
    fn reserved_header_bits_and_out_of_range_fields_are_rejected() {
        assert_eq!(type4(0, 1), Err(Error::FieldOutOfRange));
        assert_eq!(type4(1, 0x7f), Err(Error::FieldOutOfRange));
        assert_eq!(type4(0x4_0000, 1), Err(Error::FieldOutOfRange));
        assert_eq!(type7(1, 0x4000), Err(Error::FieldOutOfRange));

        let type4_reserved = type4(1, 1).unwrap() | (1 << 26);
        assert_eq!(decode_header(type4_reserved), Err(Error::FieldOutOfRange));
        let type7_reserved = type7(1, 1).unwrap() | (1 << 24);
        assert_eq!(decode_header(type7_reserved), Err(Error::FieldOutOfRange));
    }
}
