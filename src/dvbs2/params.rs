// DVB-S2 MODCOD / frame parameters -- ETSI EN 302 307-1 V1.4.1 tables 5a,
// 5b, 7a, 7b, 11 and 12.

use super::ldpc_tables as t;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Modulation {
    Qpsk,
    Psk8,
    Apsk16,
    Apsk32,
}

impl Modulation {
    pub fn bits_per_symbol(self) -> usize {
        match self {
            Modulation::Qpsk => 2,
            Modulation::Psk8 => 3,
            Modulation::Apsk16 => 4,
            Modulation::Apsk32 => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rate {
    R1_4,
    R1_3,
    R2_5,
    R1_2,
    R3_5,
    R2_3,
    R3_4,
    R4_5,
    R5_6,
    R8_9,
    R9_10,
}

impl Rate {
    pub fn name(self) -> &'static str {
        match self {
            Rate::R1_4 => "1/4",
            Rate::R1_3 => "1/3",
            Rate::R2_5 => "2/5",
            Rate::R1_2 => "1/2",
            Rate::R3_5 => "3/5",
            Rate::R2_3 => "2/3",
            Rate::R3_4 => "3/4",
            Rate::R4_5 => "4/5",
            Rate::R5_6 => "5/6",
            Rate::R8_9 => "8/9",
            Rate::R9_10 => "9/10",
        }
    }
}

/// The 7 PLS bits: MODCOD (table 12) and TYPE (frame size, pilots).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlsCode {
    pub modcod: u8,
    pub short: bool,
    pub pilots: bool,
}

impl PlsCode {
    /// The 7-bit value MODCOD:TYPE, MSB first, as fed to the PLS encoder.
    pub fn bits(self) -> u8 {
        (self.modcod << 2) | (u8::from(self.short) << 1) | u8::from(self.pilots)
    }

    pub fn from_bits(v: u8) -> Self {
        Self { modcod: v >> 2, short: v & 2 != 0, pilots: v & 1 != 0 }
    }

    pub fn is_dummy(self) -> bool {
        self.modcod == 0
    }

    /// None for a dummy frame or a reserved MODCOD (29-31).
    pub fn modcod(self) -> Option<(Modulation, Rate)> {
        use Modulation::*;
        use Rate::*;
        Some(match self.modcod {
            1 => (Qpsk, R1_4),
            2 => (Qpsk, R1_3),
            3 => (Qpsk, R2_5),
            4 => (Qpsk, R1_2),
            5 => (Qpsk, R3_5),
            6 => (Qpsk, R2_3),
            7 => (Qpsk, R3_4),
            8 => (Qpsk, R4_5),
            9 => (Qpsk, R5_6),
            10 => (Qpsk, R8_9),
            11 => (Qpsk, R9_10),
            12 => (Psk8, R3_5),
            13 => (Psk8, R2_3),
            14 => (Psk8, R3_4),
            15 => (Psk8, R5_6),
            16 => (Psk8, R8_9),
            17 => (Psk8, R9_10),
            18 => (Apsk16, R2_3),
            19 => (Apsk16, R3_4),
            20 => (Apsk16, R4_5),
            21 => (Apsk16, R5_6),
            22 => (Apsk16, R8_9),
            23 => (Apsk16, R9_10),
            24 => (Apsk32, R3_4),
            25 => (Apsk32, R4_5),
            26 => (Apsk32, R5_6),
            27 => (Apsk32, R8_9),
            28 => (Apsk32, R9_10),
            _ => return None,
        })
    }

    /// PLFRAME length in symbols, header included (table 11 and clause
    /// 5.5.3). A dummy frame is a header plus 36 slots.
    pub fn frame_symbols(self) -> Option<usize> {
        if self.is_dummy() {
            return Some(90 * 37);
        }
        let (modulation, _) = self.modcod()?;
        let n = if self.short { 16_200 } else { 64_800 };
        let slots = n / modulation.bits_per_symbol() / 90;
        let pilot_blocks = if self.pilots { (slots - 1) / 16 } else { 0 };
        Some(90 * (slots + 1) + 36 * pilot_blocks)
    }
}

/// FEC parameters of one (frame size, rate) pair.
#[derive(Clone, Copy, Debug)]
pub struct FecParams {
    /// BCH information bits (the BBFRAME).
    pub kbch: usize,
    /// BCH codeword = LDPC information bits.
    pub kldpc: usize,
    pub nldpc: usize,
    /// BCH errors correctable.
    pub t: usize,
    /// Table 7a/7b.
    pub q: usize,
    pub table: &'static [&'static [u16]],
}

/// None for short-frame 9/10, which doesn't exist.
pub fn fec_params(short: bool, rate: Rate) -> Option<FecParams> {
    use Rate::*;
    let (kbch, kldpc, tt, q, table): (usize, usize, usize, usize, &'static [&'static [u16]]) = if !short {
        match rate {
            R1_4 => (16_008, 16_200, 12, 135, t::NORMAL_1_4),
            R1_3 => (21_408, 21_600, 12, 120, t::NORMAL_1_3),
            R2_5 => (25_728, 25_920, 12, 108, t::NORMAL_2_5),
            R1_2 => (32_208, 32_400, 12, 90, t::NORMAL_1_2),
            R3_5 => (38_688, 38_880, 12, 72, t::NORMAL_3_5),
            R2_3 => (43_040, 43_200, 10, 60, t::NORMAL_2_3),
            R3_4 => (48_408, 48_600, 12, 45, t::NORMAL_3_4),
            R4_5 => (51_648, 51_840, 12, 36, t::NORMAL_4_5),
            R5_6 => (53_840, 54_000, 10, 30, t::NORMAL_5_6),
            R8_9 => (57_472, 57_600, 8, 20, t::NORMAL_8_9),
            R9_10 => (58_192, 58_320, 8, 18, t::NORMAL_9_10),
        }
    } else {
        match rate {
            R1_4 => (3_072, 3_240, 12, 36, t::SHORT_1_4),
            R1_3 => (5_232, 5_400, 12, 30, t::SHORT_1_3),
            R2_5 => (6_312, 6_480, 12, 27, t::SHORT_2_5),
            R1_2 => (7_032, 7_200, 12, 25, t::SHORT_1_2),
            R3_5 => (9_552, 9_720, 12, 18, t::SHORT_3_5),
            R2_3 => (10_632, 10_800, 12, 15, t::SHORT_2_3),
            R3_4 => (11_712, 11_880, 12, 12, t::SHORT_3_4),
            R4_5 => (12_432, 12_600, 12, 10, t::SHORT_4_5),
            R5_6 => (13_152, 13_320, 12, 8, t::SHORT_5_6),
            R8_9 => (14_232, 14_400, 12, 5, t::SHORT_8_9),
            R9_10 => return None,
        }
    };
    Some(FecParams { kbch, kldpc, nldpc: if short { 16_200 } else { 64_800 }, t: tt, q, table })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lengths_match_table_11() {
        let qpsk_normal = PlsCode { modcod: 8, short: false, pilots: false };
        assert_eq!(qpsk_normal.frame_symbols(), Some(32_490));
        assert_eq!(PlsCode { pilots: true, ..qpsk_normal }.frame_symbols(), Some(33_282));
        let psk8_short = PlsCode { modcod: 13, short: true, pilots: true };
        assert_eq!(psk8_short.frame_symbols(), Some(5_598));
        assert_eq!(PlsCode::from_bits(qpsk_normal.bits()), qpsk_normal);
    }

    #[test]
    fn every_table_has_one_row_per_360_information_bits() {
        use Rate::*;
        for short in [false, true] {
            for rate in [R1_4, R1_3, R2_5, R1_2, R3_5, R2_3, R3_4, R4_5, R5_6, R8_9, R9_10] {
                let Some(p) = fec_params(short, rate) else {
                    continue;
                };
                assert_eq!(p.table.len() * 360, p.kldpc, "{short} {}", rate.name());
                assert_eq!(p.q * 360, p.nldpc - p.kldpc, "{short} {}", rate.name());
                let limit = (p.nldpc - p.kldpc) as u16;
                assert!(p.table.iter().flat_map(|r| r.iter()).all(|&a| a < limit));
            }
        }
    }
}
