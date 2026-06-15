//! XRSP packet framing — the one layer that's been confirmed byte-for-byte from
//! `libmagicislandnative.so::XrspPacketHeaderInit` / `…GetPayloadNumBytes`.
//!
//! Wire format (little-endian, 8-byte header, packets padded to a 4-byte
//! boundary):
//!
//! | off | type | meaning                                                    |
//! |-----|------|------------------------------------------------------------|
//! | 0   | u16  | word0: version + flags (see [`Word0`])                      |
//! | 2   | u16  | length: `total_bytes = (value + 1) * 4`                     |
//! | 4   | u16  | topic id                                                    |
//! | 6   | u16  | reserved (0)                                                |
//!
//! `payload_len = total_bytes - 8 - padding`, `padding <= 255`.
//!
//! NOTE: the exact bit packing inside `word0` (version field + the two 6-bit
//! sub-fields) is not yet fully pinned down — only the `SIZED`/`INTERNAL` flags
//! are confirmed. See `docs/xrsp-protocol.md` ("Open questions").

/// Header length in bytes.
pub const HEADER_LEN: usize = 8;
/// Packets are sized in 4-byte words; the length field counts words minus one.
pub const WORD: usize = 4;

/// Confirmed flag bits inside `word0`.
pub mod word0 {
    /// Packet carries a payload/length beyond the bare 8-byte header.
    pub const SIZED: u16 = 0x0008;
    /// "Internal" packet-version variant.
    pub const INTERNAL: u16 = 0x0010;
}

/// A parsed XRSP packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    /// Raw flags/version word (offset 0).
    pub word0: u16,
    /// Total packet size in bytes, header included (decoded from offset 2).
    pub total_len: usize,
    /// Topic id (offset 4).
    pub topic: u16,
}

impl PacketHeader {
    /// Parse the 8-byte header. Returns `None` only if `buf` is too short.
    pub fn parse(buf: &[u8]) -> Option<PacketHeader> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let word0 = u16::from_le_bytes([buf[0], buf[1]]);
        let len_words_m1 = u16::from_le_bytes([buf[2], buf[3]]) as usize;
        let topic = u16::from_le_bytes([buf[4], buf[5]]);
        Some(PacketHeader { word0, total_len: (len_words_m1 + 1) * WORD, topic })
    }

    /// Encode this header's length+topic into 8 bytes. `word0` is written as-is.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let len_words_m1 = ((self.total_len / WORD).saturating_sub(1)) as u16;
        let mut b = [0u8; HEADER_LEN];
        b[0..2].copy_from_slice(&self.word0.to_le_bytes());
        b[2..4].copy_from_slice(&len_words_m1.to_le_bytes());
        b[4..6].copy_from_slice(&self.topic.to_le_bytes());
        b
    }

    pub fn is_sized(&self) -> bool {
        self.word0 & word0::SIZED != 0
    }

    pub fn is_internal(&self) -> bool {
        self.word0 & word0::INTERNAL != 0
    }

    /// Payload length given the trailing pad-byte count (`0..=255`).
    pub fn payload_len(&self, padding: u8) -> usize {
        self.total_len.saturating_sub(HEADER_LEN).saturating_sub(padding as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_and_topic() {
        // total = 16 bytes -> length field = 16/4 - 1 = 3; topic = 42; flags set.
        let w0 = word0::SIZED | word0::INTERNAL;
        let buf = [w0 as u8, (w0 >> 8) as u8, 3, 0, 42, 0, 0, 0];
        let h = PacketHeader::parse(&buf).unwrap();
        assert_eq!(h.total_len, 16);
        assert_eq!(h.topic, 42);
        assert!(h.is_sized() && h.is_internal());
        assert_eq!(h.payload_len(0), 8);
        assert_eq!(h.payload_len(2), 6);
    }

    #[test]
    fn roundtrips_header_bytes() {
        let h = PacketHeader { word0: word0::SIZED, total_len: 256, topic: 7 };
        let again = PacketHeader::parse(&h.to_bytes()).unwrap();
        assert_eq!(h, again);
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(PacketHeader::parse(&[0, 0, 0]).is_none());
    }
}
