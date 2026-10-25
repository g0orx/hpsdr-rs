// BBFRAME handling -- ETSI EN 302 307-1 V1.4.1 clauses 5.1 and 5.2: BB
// descrambling, the BBHEADER (with its CRC-8), and recovering the MPEG
// transport stream packets from the data fields.
//
// In a transport stream, each 188-byte packet's sync byte (0x47) is
// replaced by the CRC-8 of the previous packet's other 187 bytes, and
// packets run on across BBFRAMEs; SYNCD says where in a data field the
// first packet to start in it begins.

pub const HEADER_BITS: usize = 80;
pub const TS_PACKET: usize = 188;

/// CRC-8 of clause 5.1.4: g(x) = x^8+x^7+x^6+x^4+x^2+1, register cleared
/// first, MSB first.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0xD5 } else { crc << 1 };
        }
    }
    crc
}

/// Clause 5.2.2: XOR with the 1 + x^14 + x^15 PRBS, loaded with
/// 100101010000000 at the start of every BBFRAME. Its own inverse.
pub fn descramble(bits: &mut [u8]) {
    let mut reg: u16 = 0b100_1010_1000_0000; // stage 1 is the MSB of 15
    for b in bits.iter_mut() {
        let out = (((reg >> 1) ^ reg) & 1) as u8; // stages 14 and 15
        *b ^= out;
        reg = (reg >> 1) | ((out as u16) << 14);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BbHeader {
    pub matype1: u8,
    pub matype2: u8,
    /// User packet length, bits (188*8 for a transport stream).
    pub upl: u16,
    /// Data field length, bits.
    pub dfl: u16,
    pub sync: u8,
    /// Bits from the start of the data field to the first packet that
    /// starts in it; 0xFFFF if none does.
    pub syncd: u16,
}

impl BbHeader {
    pub fn is_transport_stream(&self) -> bool {
        self.matype1 >> 6 == 0b11
    }
}

pub fn pack_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b) << (8 - c.len())).collect()
}

/// Parses a descrambled BBFRAME's header; None if its CRC-8 is wrong.
pub fn parse_header(bbframe_bits: &[u8]) -> Option<BbHeader> {
    let h = pack_bytes(&bbframe_bits[..HEADER_BITS]);
    if crc8(&h[..9]) != h[9] {
        return None;
    }
    Some(BbHeader {
        matype1: h[0],
        matype2: h[1],
        upl: u16::from_be_bytes([h[2], h[3]]),
        dfl: u16::from_be_bytes([h[4], h[5]]),
        sync: h[6],
        syncd: u16::from_be_bytes([h[7], h[8]]),
    })
}

/// Reassembles transport stream packets from successive BBFRAMEs.
#[derive(Default)]
pub struct TsExtractor {
    /// A packet in progress, from its CRC byte on.
    pending: Vec<u8>,
    /// CRC-8 of the last packet's 187 bytes after its sync, to check
    /// against the next packet's first byte.
    last_crc: Option<u8>,
    pub packets: u64,
    pub crc_errors: u64,
}

impl TsExtractor {
    /// Feeds one descrambled BBFRAME (whose header parsed as `h`);
    /// complete packets, with their 0x47 sync bytes back, go to `out`.
    /// A packet whose CRC doesn't match gets its transport_error_indicator
    /// set, as a decoder expects.
    pub fn push(&mut self, h: &BbHeader, bbframe_bits: &[u8], out: &mut Vec<[u8; TS_PACKET]>) {
        if !h.is_transport_stream() || h.upl as usize != TS_PACKET * 8 {
            return;
        }
        let dfl = (h.dfl as usize).min(bbframe_bits.len() - HEADER_BITS);
        let data = pack_bytes(&bbframe_bits[HEADER_BITS..HEADER_BITS + dfl - dfl % 8]);
        let start = if h.syncd == 0xFFFF { data.len() } else { (h.syncd as usize / 8).min(data.len()) };
        if h.syncd == 0xFFFF {
            self.extend_pending(&data, out);
            return;
        }
        // Bytes before SYNCD finish the packet in progress; if they don't
        // make it exactly 188, something was lost -- drop it.
        if !self.pending.is_empty() && self.pending.len() + start == TS_PACKET {
            self.pending.extend_from_slice(&data[..start]);
            self.emit(h.sync, out);
        } else {
            self.pending.clear();
            self.last_crc = None;
        }
        self.extend_pending(&data[start..], out);
    }

    fn extend_pending(&mut self, mut bytes: &[u8], out: &mut Vec<[u8; TS_PACKET]>) {
        while !bytes.is_empty() {
            let take = (TS_PACKET - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() == TS_PACKET {
                self.emit(0x47, out);
            }
        }
    }

    fn emit(&mut self, sync: u8, out: &mut Vec<[u8; TS_PACKET]>) {
        let mut packet = [0u8; TS_PACKET];
        packet.copy_from_slice(&self.pending);
        self.pending.clear();
        let crc_ok = self.last_crc.is_none_or(|c| c == packet[0]);
        self.last_crc = Some(crc8(&packet[1..]));
        packet[0] = if sync == 0 { 0x47 } else { sync };
        if !crc_ok {
            self.crc_errors += 1;
            packet[1] |= 0x80;
        }
        self.packets += 1;
        out.push(packet);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrambler_starts_like_figure_5() {
        // Figure 5 shows the PRBS output starting 0000 0011.
        let mut bits = vec![0u8; 8];
        descramble(&mut bits);
        assert_eq!(bits, [0, 0, 0, 0, 0, 0, 1, 1]);
    }

    #[test]
    fn crc8_detects_a_flipped_bit() {
        let data = b"123456789";
        let c = crc8(data);
        let mut with_crc = data.to_vec();
        with_crc.push(c);
        assert_eq!(crc8(&with_crc), 0);
        let mut bad = data.to_vec();
        bad[3] ^= 4;
        assert_ne!(crc8(&bad), c);
    }

    /// Builds BBFRAMEs the way a modulator does (clause 5.1) from a run of
    /// TS packets, and checks the extractor gets them all back.
    #[test]
    fn packets_survive_being_split_across_frames() {
        let kbch = 51_648; // QPSK 4/5 normal
        let packets: Vec<[u8; TS_PACKET]> = (0..200u32)
            .map(|n| {
                let mut p = [0u8; TS_PACKET];
                p[0] = 0x47;
                for (i, b) in p.iter_mut().enumerate().skip(1) {
                    *b = (n as usize * 7 + i) as u8;
                }
                p
            })
            .collect();
        // The UP stream: each sync byte replaced by the previous packet's CRC.
        let mut stream = Vec::new();
        let mut prev_crc = 0u8;
        for p in &packets {
            stream.push(prev_crc);
            stream.extend_from_slice(&p[1..]);
            prev_crc = crc8(&p[1..]);
        }
        let dfl_bytes = (kbch - HEADER_BITS) / 8;
        let mut ex = TsExtractor::default();
        let mut out = Vec::new();
        for (f, field) in stream.chunks(dfl_bytes).enumerate() {
            if field.len() < dfl_bytes {
                break;
            }
            let offset = f * dfl_bytes;
            let syncd = ((TS_PACKET - offset % TS_PACKET) % TS_PACKET * 8) as u16;
            let mut h = vec![0xF0, 0x00];
            h.extend_from_slice(&((TS_PACKET * 8) as u16).to_be_bytes());
            h.extend_from_slice(&((dfl_bytes * 8) as u16).to_be_bytes());
            h.push(0x47);
            h.extend_from_slice(&syncd.to_be_bytes());
            h.push(crc8(&h));
            let mut bytes = h;
            bytes.extend_from_slice(field);
            let mut bits: Vec<u8> = bytes.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
            bits.resize(kbch, 0);
            descramble(&mut bits); // scramble...
            descramble(&mut bits); // ...and back, as the receiver would
            let header = parse_header(&bits).expect("header CRC");
            ex.push(&header, &bits, &mut out);
        }
        assert!(out.len() > 100);
        assert_eq!(ex.crc_errors, 0);
        // The first packet only starts at the first SYNCD, which is 0.
        for (got, want) in out.iter().zip(packets.iter()) {
            assert_eq!(got, want);
        }
    }
}
