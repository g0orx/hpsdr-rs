// DVB-S2 LDPC codes -- ETSI EN 302 307-1 V1.4.1 clause 5.3.2 and annexes
// B/C. The encoder follows the clause's accumulator description exactly;
// the decoder works on the parity check matrix that description implies:
//
// - information bit i (in group g = i/360, m = i%360) takes part in
//   check (x + m*q) mod (n-k) for every address x in table row g;
// - the final "p_i ^= p_(i-1)" step makes check c also involve parity
//   bits c and c-1 (a staircase).
//
// Decoding is layered normalized min-sum: checks are processed in turn,
// each one updating the bit LLRs straight away, which converges in about
// half the iterations of a flooding schedule.

use super::params::FecParams;

pub struct LdpcCode {
    pub n: usize,
    pub k: usize,
    // For encoding, which only the tests need.
    #[cfg_attr(not(test), allow(dead_code))]
    q: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    table: &'static [&'static [u16]],
    /// Check c's bits are check_vars[check_start[c]..check_start[c+1]].
    check_start: Vec<u32>,
    check_vars: Vec<u32>,
}

/// Min-sum's overestimate correction.
const NORMALIZATION: f32 = 0.8;

impl LdpcCode {
    pub fn new(p: &FecParams) -> Self {
        let (n, k, q) = (p.nldpc, p.kldpc, p.q);
        let m = n - k;
        let mut lists: Vec<Vec<u32>> = vec![Vec::new(); m];
        for (g, row) in p.table.iter().enumerate() {
            for mm in 0..360 {
                let bit = (g * 360 + mm) as u32;
                for &x in row.iter() {
                    lists[(x as usize + mm * q) % m].push(bit);
                }
            }
        }
        for (c, list) in lists.iter_mut().enumerate() {
            if c > 0 {
                list.push((k + c - 1) as u32);
            }
            list.push((k + c) as u32);
        }
        let mut check_start = Vec::with_capacity(m + 1);
        let mut check_vars = Vec::new();
        check_start.push(0);
        for list in &lists {
            check_vars.extend_from_slice(list);
            check_start.push(check_vars.len() as u32);
        }
        Self { n, k, q, table: p.table, check_start, check_vars }
    }

    /// Clause 5.3.2.1: `info` is k bits (0/1); returns the n-bit
    /// codeword.
    #[cfg(test)]
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        assert_eq!(info.len(), self.k);
        let m = self.n - self.k;
        let mut p = vec![0u8; m];
        for (g, row) in self.table.iter().enumerate() {
            for mm in 0..360 {
                if info[g * 360 + mm] != 0 {
                    for &x in row.iter() {
                        p[(x as usize + mm * self.q) % m] ^= 1;
                    }
                }
            }
        }
        for i in 1..m {
            p[i] ^= p[i - 1];
        }
        let mut out = info.to_vec();
        out.extend_from_slice(&p);
        out
    }

    /// Whether `bits` (n of them, 0/1) satisfies every parity check.
    pub fn is_codeword(&self, bits: &[u8]) -> bool {
        (0..self.n - self.k).all(|c| {
            let vars = &self.check_vars[self.check_start[c] as usize..self.check_start[c + 1] as usize];
            vars.iter().fold(0u8, |a, &v| a ^ bits[v as usize]) == 0
        })
    }

    /// Decodes in place: `llr` holds n log-likelihood ratios, positive
    /// meaning bit 0, and comes back as the decoder's final beliefs.
    /// Returns the iterations used if every parity check was met, None if
    /// it gave up after `max_iterations`.
    pub fn decode(&self, llr: &mut [f32], max_iterations: usize) -> Option<usize> {
        assert_eq!(llr.len(), self.n);
        let m = self.n - self.k;
        let mut r = vec![0f32; self.check_vars.len()];
        let mut t = Vec::with_capacity(64);
        let mut hard = vec![0u8; self.n];
        for iteration in 1..=max_iterations {
            for c in 0..m {
                let (start, end) = (self.check_start[c] as usize, self.check_start[c + 1] as usize);
                let vars = &self.check_vars[start..end];
                let edges = &mut r[start..end];
                t.clear();
                let (mut min1, mut min2, mut min_at) = (f32::MAX, f32::MAX, 0usize);
                let mut negative = false;
                for (j, (&v, &e)) in vars.iter().zip(edges.iter()).enumerate() {
                    let x = llr[v as usize] - e;
                    t.push(x);
                    let a = x.abs();
                    negative ^= x < 0.0;
                    if a < min1 {
                        min2 = min1;
                        min1 = a;
                        min_at = j;
                    } else if a < min2 {
                        min2 = a;
                    }
                }
                let (m1, m2) = (min1 * NORMALIZATION, min2 * NORMALIZATION);
                for (j, (&v, e)) in vars.iter().zip(edges.iter_mut()).enumerate() {
                    let x = t[j];
                    let magnitude = if j == min_at { m2 } else { m1 };
                    // Sign of the product of all the others' signs.
                    let others_negative = negative ^ (x < 0.0);
                    let new = if others_negative { -magnitude } else { magnitude };
                    *e = new;
                    llr[v as usize] = x + new;
                }
            }
            for (h, &l) in hard.iter_mut().zip(llr.iter()) {
                *h = u8::from(l < 0.0);
            }
            if self.is_codeword(&hard) {
                return Some(iteration);
            }
        }
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::params::{fec_params, Rate};
    use super::*;

    /// xorshift -- deterministic test data without a rand dependency.
    pub(crate) fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Standard normal via Box-Muller.
    pub(crate) fn gauss(seed: &mut u64) -> f32 {
        let u1 = ((rng(seed) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let u2 = (rng(seed) >> 11) as f64 / (1u64 << 53) as f64;
        ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
    }

    #[test]
    fn encoder_output_satisfies_every_check_for_every_code() {
        use Rate::*;
        let mut seed = 0x1234_5678_9abc_def0;
        for short in [false, true] {
            for rate in [R1_4, R1_3, R2_5, R1_2, R3_5, R2_3, R3_4, R4_5, R5_6, R8_9, R9_10] {
                let Some(p) = fec_params(short, rate) else {
                    continue;
                };
                let code = LdpcCode::new(&p);
                let info: Vec<u8> = (0..p.kldpc).map(|_| (rng(&mut seed) & 1) as u8).collect();
                let cw = code.encode(&info);
                assert!(code.is_codeword(&cw), "{short} {}", rate.name());
                let mut bad = cw.clone();
                bad[17] ^= 1;
                assert!(!code.is_codeword(&bad));
            }
        }
    }

    /// QPSK 4/5 normal (the QO-100 beacon's code) over AWGN at 1 dB
    /// above its ~2.7 dB Eb/N0 threshold: every frame must decode.
    #[test]
    fn decodes_qpsk_4_5_through_noise() {
        let p = fec_params(false, Rate::R4_5).unwrap();
        let code = LdpcCode::new(&p);
        let mut seed = 0x0bad_cafe_dead_beef;
        // Es/N0 = 5.7 dB for QPSK (2 bits/symbol, rate 4/5 -> Eb/N0 =
        // Es/N0 - 10log10(1.6)); BPSK-equivalent per bit: sigma^2 =
        // 1/(2 Es/N0) per real dimension at unit symbol energy / 2.
        let esn0 = 10f32.powf(5.7 / 10.0);
        let sigma = (1.0 / (2.0 * esn0)).sqrt();
        let amp = std::f32::consts::FRAC_1_SQRT_2;
        for _ in 0..3 {
            let info: Vec<u8> = (0..p.kldpc).map(|_| (rng(&mut seed) & 1) as u8).collect();
            let cw = code.encode(&info);
            let mut llr: Vec<f32> = cw
                .iter()
                .map(|&b| {
                    let y = if b == 0 { amp } else { -amp } + sigma * gauss(&mut seed);
                    2.0 * amp * y / (sigma * sigma)
                })
                .collect();
            let iterations = code.decode(&mut llr, 50).expect("didn't converge");
            assert!(iterations < 50);
            let out: Vec<u8> = llr.iter().map(|&l| u8::from(l < 0.0)).collect();
            assert_eq!(out, cw);
        }
    }
}
