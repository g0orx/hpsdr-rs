/*
    The wire encoding shared by hpsdr-rs (the server) and hpsdr-remote
    (the thin client): TCI's binary message header, and hpsdr-rs's remote
    extensions to TCI -- a ready-made spectrum row and IMA-ADPCM audio --
    for running over slow links. The protocol itself is described in
    docs/remote-protocol.md; this crate is just its encoding, with no
    dependencies so it builds for desktop, Android and WebAssembly.
*/

pub mod adpcm;
pub mod bands;
pub mod spectrum;
pub mod state;
pub mod tci;
pub mod tx;

/// This protocol's version, sent in `hpsdr_hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Binary stream types: TCI's own (0-3) and hpsdr-rs's extensions.
pub const TYPE_IQ: u32 = 0;
pub const TYPE_RX_AUDIO: u32 = 1;
pub const TYPE_TX_AUDIO: u32 = 2;
pub const TYPE_TX_CHRONO: u32 = 3;
pub const TYPE_SPECTRUM: u32 = 100;
pub const TYPE_ADPCM_AUDIO: u32 = 101;

/// The header's `format` field: TCI's float32, and the extensions' own.
pub const FORMAT_FLOAT32: u32 = 3;
pub const FORMAT_SPECTRUM_U8: u32 = 0x100;
pub const FORMAT_ADPCM: u32 = 0x101;

/// The header's `codec` field.
pub const CODEC_NONE: u32 = 0;
pub const CODEC_IMA_ADPCM: u32 = 1;

pub const HEADER_LEN: usize = 64;

/// TCI's 64-byte little-endian binary message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub receiver: u32,
    pub sample_rate: u32,
    pub format: u32,
    pub codec: u32,
    pub crc: u32,
    pub length: u32,
    pub msg_type: u32,
    pub channels: u32,
}

impl Header {
    pub fn encode(&self, out: &mut Vec<u8>) {
        for v in [
            self.receiver,
            self.sample_rate,
            self.format,
            self.codec,
            self.crc,
            self.length,
            self.msg_type,
            self.channels,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0u8; 32]);
    }

    /// The header and the payload after it, or None if `data` is shorter
    /// than a header.
    pub fn decode(data: &[u8]) -> Option<(Header, &[u8])> {
        if data.len() < HEADER_LEN {
            return None;
        }
        let word = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
        let header = Header {
            receiver: word(0),
            sample_rate: word(1),
            format: word(2),
            codec: word(3),
            crc: word(4),
            length: word(5),
            msg_type: word(6),
            channels: word(7),
        };
        Some((header, &data[HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = Header {
            receiver: 0,
            sample_rate: 24_000,
            format: FORMAT_ADPCM,
            codec: CODEC_IMA_ADPCM,
            crc: 0,
            length: 480,
            msg_type: TYPE_ADPCM_AUDIO,
            channels: 1,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        buf.extend_from_slice(&[1, 2, 3]);
        assert_eq!(buf.len(), HEADER_LEN + 3);
        let (back, payload) = Header::decode(&buf).unwrap();
        assert_eq!(back, h);
        assert_eq!(payload, &[1, 2, 3]);
        assert!(Header::decode(&buf[..63]).is_none());
    }
}
