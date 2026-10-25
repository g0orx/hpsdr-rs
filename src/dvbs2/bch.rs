// DVB-S2 outer BCH code -- ETSI EN 302 307-1 V1.4.1 clause 5.3.1.
//
// The generator is the product of the first t polynomials of table 6a
// (normal frames, GF(2^16)) or 6b (short, GF(2^14)). g1 is primitive and
// the others are the minimal polynomials of its odd powers, so the code
// has alpha^1..alpha^2t as roots (checked in the tests below) and decodes
// the textbook way: syndromes, Berlekamp-Massey, Chien search.
//
// Bit order: the first transmitted bit is the coefficient of x^(n-1).

/// Table 6a, as bit masks (bit i = coefficient of x^i).
const NORMAL_POLYS: [u32; 12] = [
    0x1_002D, // 1+x2+x3+x5+x16
    0x1_0173, // 1+x+x4+x5+x6+x8+x16
    0x1_0FBD, // 1+x2+x3+x4+x5+x7+x8+x9+x10+x11+x16
    0x1_5A55, // 1+x2+x4+x6+x9+x11+x12+x14+x16
    0x1_1F2F, // 1+x+x2+x3+x5+x8+x9+x10+x11+x12+x16
    0x1_F7B5, // 1+x2+x4+x5+x7+x8+x9+x10+x12+x13+x14+x15+x16
    0x1_AF65, // 1+x2+x5+x6+x8+x9+x10+x11+x13+x15+x16
    0x1_7367, // 1+x+x2+x5+x6+x8+x9+x12+x13+x14+x16
    0x1_0EA1, // 1+x5+x7+x9+x10+x11+x16
    0x1_75A7, // 1+x+x2+x5+x7+x8+x10+x12+x13+x14+x16
    0x1_3A2D, // 1+x2+x3+x5+x9+x11+x12+x13+x16
    0x1_1AE3, // 1+x+x5+x6+x7+x9+x11+x12+x16
];

/// Table 6b.
const SHORT_POLYS: [u32; 12] = [
    0x402B, // 1+x+x3+x5+x14
    0x4941, // 1+x6+x8+x11+x14
    0x4647, // 1+x+x2+x6+x9+x10+x14
    0x5591, // 1+x4+x7+x8+x10+x12+x14
    0x6B55, // 1+x2+x4+x6+x8+x9+x11+x13+x14
    0x6389, // 1+x3+x7+x8+x9+x13+x14
    0x6CE5, // 1+x2+x5+x6+x7+x10+x11+x13+x14
    0x4F21, // 1+x5+x8+x9+x10+x11+x14
    0x460F, // 1+x+x2+x3+x9+x10+x14
    0x5A49, // 1+x3+x6+x9+x11+x12+x14
    0x5811, // 1+x4+x11+x12+x14
    0x65EF, // 1+x+x2+x3+x5+x6+x7+x8+x10+x13+x14
];

/// GF(2^m) by log/antilog tables.
struct Field {
    order: usize, // 2^m - 1
    exp: Vec<u16>,
    log: Vec<u16>,
}

impl Field {
    fn new(m: u32, poly: u32) -> Self {
        let order = (1usize << m) - 1;
        let mut exp = vec![0u16; 2 * order];
        let mut log = vec![0u16; order + 1];
        let mut v: u32 = 1;
        for i in 0..order {
            exp[i] = v as u16;
            log[v as usize] = i as u16;
            v <<= 1;
            if v & (1 << m) != 0 {
                v ^= poly;
            }
        }
        for i in order..2 * order {
            exp[i] = exp[i - order];
        }
        Self { order, exp, log }
    }

    fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }

    fn div(&self, a: u16, b: u16) -> u16 {
        if a == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.order - self.log[b as usize] as usize]
        }
    }

    fn alpha_pow(&self, e: usize) -> u16 {
        self.exp[e % self.order]
    }
}

pub struct BchCode {
    pub n: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    pub k: usize,
    pub t: usize,
    field: Field,
    /// Generator polynomial coefficients, index = power (for encoding,
    /// which only the tests need).
    #[cfg_attr(not(test), allow(dead_code))]
    generator: Vec<u8>,
}

/// The decoder's verdict on a codeword.
#[derive(Debug, PartialEq, Eq)]
pub enum BchResult {
    Clean,
    Corrected(usize),
    /// More errors than the code can fix.
    Failed,
}

impl BchCode {
    /// `n` = Nbch (= kldpc), `k` = Kbch.
    pub fn new(short: bool, n: usize, k: usize, t: usize) -> Self {
        let (m, polys) = if short { (14, &SHORT_POLYS) } else { (16, &NORMAL_POLYS) };
        let field = Field::new(m, polys[0]);
        let mut generator = vec![1u8];
        for &p in &polys[..t] {
            let factor: Vec<u8> = (0..=m).map(|i| ((p >> i) & 1) as u8).collect();
            let mut product = vec![0u8; generator.len() + factor.len() - 1];
            for (i, &a) in generator.iter().enumerate() {
                if a != 0 {
                    for (j, &b) in factor.iter().enumerate() {
                        product[i + j] ^= b;
                    }
                }
            }
            generator = product;
        }
        assert_eq!(generator.len() - 1, n - k);
        Self { n, k, t, field, generator }
    }

    /// Clause 5.3.1: `msg` is k bits, first-sent first; returns n bits.
    #[cfg(test)]
    pub fn encode(&self, msg: &[u8]) -> Vec<u8> {
        let r = self.n - self.k;
        // Remainder of x^r m(x) / g(x), by long division over the bits.
        let mut rem = vec![0u8; r];
        for &bit in msg {
            let feedback = bit ^ rem[r - 1];
            for i in (1..r).rev() {
                rem[i] = rem[i - 1] ^ (feedback & self.generator[i]);
            }
            rem[0] = feedback & self.generator[0];
        }
        let mut out = msg.to_vec();
        out.extend(rem.iter().rev());
        out
    }

    /// Corrects `bits` (n of them, first-sent first) in place.
    pub fn decode(&self, bits: &mut [u8]) -> BchResult {
        assert_eq!(bits.len(), self.n);
        let f = &self.field;
        let two_t = 2 * self.t;
        // Syndromes S_j = r(alpha^j), j = 1..2t (index j-1), by Horner;
        // even ones are squares of earlier ones.
        let mut s = vec![0u16; two_t];
        for j in (1..=two_t).step_by(2) {
            let a = f.alpha_pow(j);
            let mut acc: u16 = 0;
            for &b in bits.iter() {
                acc = f.mul(acc, a) ^ b as u16;
            }
            s[j - 1] = acc;
        }
        for j in (2..=two_t).step_by(2) {
            s[j - 1] = f.mul(s[j / 2 - 1], s[j / 2 - 1]);
        }
        if s.iter().all(|&v| v == 0) {
            return BchResult::Clean;
        }

        // Berlekamp-Massey: error locator lambda(x).
        let mut lambda = vec![0u16; two_t + 1];
        let mut prev = vec![0u16; two_t + 1];
        lambda[0] = 1;
        prev[0] = 1;
        let (mut l, mut shift, mut prev_d) = (0usize, 1usize, 1u16);
        for i in 0..two_t {
            let mut d = s[i];
            for j in 1..=l {
                d ^= f.mul(lambda[j], s[i - j]);
            }
            if d == 0 {
                shift += 1;
                continue;
            }
            let coef = f.div(d, prev_d);
            let old = lambda.clone();
            for j in shift..=two_t {
                lambda[j] ^= f.mul(coef, prev[j - shift]);
            }
            if 2 * l <= i {
                l = i + 1 - l;
                prev = old;
                prev_d = d;
                shift = 1;
            } else {
                shift += 1;
            }
        }
        if l > self.t {
            return BchResult::Failed;
        }

        // Chien search: x^p is in error if lambda(alpha^-p) = 0. Only
        // powers below n exist in this shortened code.
        let mut found = Vec::with_capacity(l);
        for p in 0..self.n {
            let inv = (f.order - p % f.order) % f.order;
            let mut v: u16 = 0;
            for (j, &c) in lambda.iter().enumerate().take(l + 1) {
                if c != 0 {
                    v ^= f.mul(c, f.alpha_pow(inv * j));
                }
            }
            if v == 0 {
                found.push(p);
                if found.len() == l {
                    break;
                }
            }
        }
        if found.len() != l {
            return BchResult::Failed;
        }
        for p in &found {
            bits[self.n - 1 - p] ^= 1;
        }
        BchResult::Corrected(l)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ldpc::tests::rng;
    use super::super::params::{fec_params, Rate};
    use super::*;

    /// The decoder relies on alpha^1..alpha^2t being roots of the
    /// generator, with alpha a root of g1 -- true if table 6's g_i are
    /// the minimal polynomials of alpha, alpha^3, alpha^5, ...
    #[test]
    fn generator_has_the_consecutive_roots_the_decoder_assumes() {
        for (short, m, polys) in [(false, 16, &NORMAL_POLYS), (true, 14, &SHORT_POLYS)] {
            let f = Field::new(m, polys[0]);
            for (i, &p) in polys.iter().enumerate() {
                let root = f.alpha_pow(2 * i + 1);
                let mut v: u16 = 0;
                for e in (0..=m).rev() {
                    v = f.mul(v, root) ^ ((p >> e) & 1) as u16;
                }
                assert_eq!(v, 0, "short={short} g{} doesn't have alpha^{} as a root", i + 1, 2 * i + 1);
            }
        }
    }

    #[test]
    fn corrects_up_to_t_errors() {
        let mut seed = 0x5eed_5eed_5eed_5eed;
        for (short, rate) in [(false, Rate::R4_5), (false, Rate::R2_3), (true, Rate::R1_2), (false, Rate::R9_10)] {
            let p = fec_params(short, rate).unwrap();
            let code = BchCode::new(short, p.kldpc, p.kbch, p.t);
            let msg: Vec<u8> = (0..p.kbch).map(|_| (rng(&mut seed) & 1) as u8).collect();
            let cw = code.encode(&msg);
            let mut clean = cw.clone();
            assert_eq!(code.decode(&mut clean), BchResult::Clean);
            for errors in [1, p.t] {
                let mut bad = cw.clone();
                let mut positions = std::collections::HashSet::new();
                while positions.len() < errors {
                    positions.insert((rng(&mut seed) % p.kldpc as u64) as usize);
                }
                for &i in &positions {
                    bad[i] ^= 1;
                }
                assert_eq!(code.decode(&mut bad), BchResult::Corrected(errors), "{short} {}", rate.name());
                assert_eq!(bad, cw);
            }
        }
    }
}
