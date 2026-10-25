// DVB-S2 physical layer signalling -- ETSI EN 302 307-1 V1.4.1 clause
// 5.5.2 (PLHEADER: SOF + PLS code) and 5.5.4 (PL scrambling).

use super::params::PlsCode;
use super::Cf;

pub const SOF: u32 = 0x18D_2E82;
pub const SOF_BITS: usize = 26;
pub const HEADER_SYMBOLS: usize = 90;

/// Scrambles the 64 PLS bits (clause 5.5.2.4), first-sent bit as MSB.
const PLS_SCRAMBLE: u64 = 0b0111000110011101100000111100100101010011010000100010110111111010;

/// The (64,7) PLS codeword for `code`, first-sent bit as MSB, scrambled.
pub fn pls_codeword(code: PlsCode) -> u64 {
    // Generator rows of the (32,6) bi-orthogonal code, figure 13b; MODCOD
    // MSB multiplies the first row, TYPE MSB the last (all ones).
    const G: [u32; 6] = [0x5555_5555, 0x3333_3333, 0x0F0F_0F0F, 0x00FF_00FF, 0x0000_FFFF, 0xFFFF_FFFF];
    let b = code.bits();
    let mut y: u32 = 0;
    for (row, g) in G.iter().enumerate() {
        if (b >> (6 - row)) & 1 != 0 {
            y ^= g;
        }
    }
    // Each bit repeated, the repeat complemented when TYPE's LSB is 1.
    let b7 = (b & 1) as u64;
    let mut out: u64 = 0;
    for i in 0..32 {
        let bit = ((y >> (31 - i)) & 1) as u64;
        out = (out << 2) | (bit << 1) | (bit ^ b7);
    }
    out ^ PLS_SCRAMBLE
}

/// The 90 header bits, first-sent first.
fn header_bits(code: PlsCode) -> [u8; HEADER_SYMBOLS] {
    let mut bits = [0u8; HEADER_SYMBOLS];
    for (i, b) in bits.iter_mut().enumerate().take(SOF_BITS) {
        *b = ((SOF >> (SOF_BITS - 1 - i)) & 1) as u8;
    }
    let pls = pls_codeword(code);
    for i in 0..64 {
        bits[SOF_BITS + i] = ((pls >> (63 - i)) & 1) as u8;
    }
    bits
}

/// pi/2-BPSK axis of header symbol `k` (0-based): odd-numbered symbols
/// (1-based) lie on the 45 degree diagonal, even ones on the 135 degree one.
pub fn header_axis(k: usize) -> Cf {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    if k.is_multiple_of(2) {
        Cf::new(h, h)
    } else {
        Cf::new(-h, h)
    }
}

/// The header's 90 pi/2-BPSK symbols (unit energy).
pub fn header_symbols(code: PlsCode) -> [Cf; HEADER_SYMBOLS] {
    let bits = header_bits(code);
    std::array::from_fn(|k| header_axis(k) * if bits[k] == 0 { 1.0 } else { -1.0 })
}

/// The SOF's 26 symbols (the same in every header).
pub fn sof_symbols() -> [Cf; SOF_BITS] {
    let h = header_symbols(PlsCode::from_bits(0));
    std::array::from_fn(|k| h[k])
}

/// PL scrambling sequence for code n = 0 (the broadcast default), as
/// R(i) in 0..=3 -- multiply symbol i after the header by j^R(i) to
/// scramble, by j^-R(i) to descramble. Long enough for any frame.
pub fn scrambling_sequence() -> Vec<u8> {
    const LEN: usize = 66_420;
    const PERIOD: usize = (1 << 18) - 1;
    // z(i) for i < LEN + 131072 is all that's needed.
    let need = LEN + 131_072;
    let mut x = vec![0u8; need + 18];
    let mut y = vec![0u8; need + 18];
    x[0] = 1;
    for v in y.iter_mut().take(18) {
        *v = 1;
    }
    for i in 0..need {
        x[i + 18] = x[i + 7] ^ x[i];
        y[i + 18] = y[i + 10] ^ y[i + 7] ^ y[i + 5] ^ y[i];
    }
    // n = 0, so z(i) = x(i) + y(i); every index used is far below the
    // period, so no wrap is needed.
    debug_assert!(need < PERIOD);
    let z = |i: usize| x[i] ^ y[i];
    (0..LEN).map(|i| 2 * z(i + 131_072) + z(i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Maximum-likelihood PLS decode from the 64 soft PLS symbols, each
    /// already projected onto its own axis (positive = bit 0). Returns the
    /// best code and its margin over the runner-up (in the same units as
    /// `soft`), which says how much to trust it.
    pub fn decode_pls(soft: &[f32; 64]) -> (PlsCode, f32) {
        let mut best = (PlsCode::from_bits(0), f32::MIN);
        let mut second = f32::MIN;
        for v in 0..128u8 {
            let code = PlsCode::from_bits(v);
            let cw = pls_codeword(code);
            let score: f32 = (0..64).map(|i| if (cw >> (63 - i)) & 1 == 0 { soft[i] } else { -soft[i] }).sum();
            if score > best.1 {
                second = best.1;
                best = (code, score);
            } else if score > second {
                second = score;
            }
        }
        (best.0, best.1 - second)
    }

    #[test]
    fn codewords_are_distinct_and_far_apart() {
        let words: Vec<u64> = (0..128u8).map(|v| pls_codeword(PlsCode::from_bits(v))).collect();
        let mut min = 64;
        for a in 0..128 {
            for b in a + 1..128 {
                min = min.min((words[a] ^ words[b]).count_ones());
            }
        }
        // dmin = 32 (clause 5.5.2).
        assert_eq!(min, 32);
    }

    #[test]
    fn pls_decodes_its_own_codewords() {
        for v in 0..128u8 {
            let code = PlsCode::from_bits(v);
            let cw = pls_codeword(code);
            let soft: [f32; 64] = std::array::from_fn(|i| if (cw >> (63 - i)) & 1 == 0 { 1.0 } else { -1.0 });
            assert_eq!(decode_pls(&soft).0, code);
        }
    }

    #[test]
    fn sof_symbols_follow_the_pi2_bpsk_rule() {
        // SOF starts 01 1000...: first symbol +45 degrees, the second
        // (bit 1) on the 135 degree axis but negated.
        let s = sof_symbols();
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((s[0].re - h).abs() < 1e-6 && (s[0].im - h).abs() < 1e-6);
        assert!((s[1].re - h).abs() < 1e-6 && (s[1].im + h).abs() < 1e-6);
    }

    #[test]
    fn scrambling_sequence_starts_like_the_reference_generator() {
        // x and y both start with a fixed 18-bit state, so z(0) = 1^1 = 0
        // and z(1..18) = 0^1 = 1.
        let r = scrambling_sequence();
        assert_eq!(r.len(), 66_420);
        assert!(r.iter().all(|&v| v < 4));
        assert_eq!(r[0] & 1, 0);
        assert!(r[1..18].iter().all(|&v| v & 1 == 1));
    }
}
