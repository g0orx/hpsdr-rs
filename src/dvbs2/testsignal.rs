// A DVB-S2 modulator, for testing the receiver end to end: transport
// stream packets -> BBFRAMEs -> BCH -> LDPC -> bit interleaver ->
// constellation -> PLFRAMEs (header, pilots, scrambling) -> RRC pulses at
// the dongle's sample rate, with a carrier offset, a symbol clock error
// and noise added.
//
// It shares the receiver's reading of the standard, so a misreading
// would cancel out here; the real-signal recordings are what catch
// those.

use super::bbframe::{self, crc8, TS_PACKET};
use super::bch::BchCode;
use super::ldpc::tests::gauss;
use super::ldpc::LdpcCode;
use super::params::{fec_params, Modulation, PlsCode};
use super::plheader;
use super::Cf;
use std::f64::consts::PI;

pub struct Channel {
    pub fs: f64,
    pub symbol_rate: f64,
    pub rolloff: f64,
    pub carrier_hz: f64,
    /// Transmitter symbol clock error, ppm.
    pub clock_ppm: f64,
    pub esn0_db: f64,
    pub seed: u64,
}

/// Test packets: valid sync byte, a counter in the payload.
pub fn test_packets(count: usize) -> Vec<[u8; TS_PACKET]> {
    (0..count)
        .map(|n| {
            let mut p = [0u8; TS_PACKET];
            p[0] = 0x47;
            p[1] = 0x01; // PID 0x100
            p[2] = 0x00;
            p[3] = 0x10 | (n % 16) as u8;
            for (i, b) in p.iter_mut().enumerate().skip(4) {
                *b = (n * 31 + i * 7) as u8;
            }
            p
        })
        .collect()
}

/// PLFRAME symbols carrying `packets` (as many whole frames as fit).
pub fn frames(packets: &[[u8; TS_PACKET]], pls: PlsCode) -> Vec<Cf> {
    let (modulation, rate) = pls.modcod().unwrap();
    let p = fec_params(pls.short, rate).unwrap();
    let ldpc = LdpcCode::new(&p);
    let bch = BchCode::new(pls.short, p.kldpc, p.kbch, p.t);
    let scrambling = plheader::scrambling_sequence();

    let mut stream = Vec::new();
    let mut prev_crc = 0u8;
    for pk in packets {
        stream.push(prev_crc);
        stream.extend_from_slice(&pk[1..]);
        prev_crc = crc8(&pk[1..]);
    }
    let dfl_bytes = (p.kbch - bbframe::HEADER_BITS) / 8;
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
        h.extend_from_slice(field);
        let mut bits: Vec<u8> = h.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect();
        bits.resize(p.kbch, 0);
        bbframe::descramble(&mut bits);
        let cw = ldpc.encode(&bch.encode(&bits));

        let bps = modulation.bits_per_symbol();
        let tx_bits: Vec<u8> = match modulation {
            Modulation::Qpsk => cw,
            Modulation::Psk8 => {
                let rows = cw.len() / 3;
                let reversed = pls.modcod == 12;
                let mut t = vec![0u8; cw.len()];
                for r in 0..rows {
                    for c in 0..3 {
                        let col = if reversed { 2 - c } else { c };
                        t[3 * r + c] = cw[col * rows + r];
                    }
                }
                t
            }
            _ => unimplemented!(),
        };
        let data: Vec<Cf> = tx_bits
            .chunks(bps)
            .map(|b| match modulation {
                Modulation::Qpsk => {
                    let h = std::f32::consts::FRAC_1_SQRT_2;
                    Cf::new(if b[0] == 0 { h } else { -h }, if b[1] == 0 { h } else { -h })
                }
                _ => {
                    let v = (b[0] << 2) | (b[1] << 1) | b[2];
                    let a = [1.0, 0.0, 4.0, 5.0, 2.0, 7.0, 3.0, 6.0][v as usize];
                    Cf::from_polar(1.0, a * std::f32::consts::PI / 4.0)
                }
            })
            .collect();

        out.extend_from_slice(&plheader::header_symbols(pls));
        let pilot = Cf::new(std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
        let mut i = 0usize;
        for (n, s) in data.iter().enumerate() {
            if pls.pilots && n > 0 && n % 1440 == 0 {
                for _ in 0..36 {
                    out.push(pilot.rot90(scrambling[i]));
                    i += 1;
                }
            }
            out.push(s.rot90(scrambling[i]));
            i += 1;
        }
    }
    out
}

/// RRC-shaped, offset, noisy samples at `ch.fs` for `symbols`.
pub fn waveform(symbols: &[Cf], ch: &Channel) -> Vec<Cf> {
    let t_sym = 1.0 / (ch.symbol_rate * (1.0 + ch.clock_ppm * 1e-6));
    let n_out = ((symbols.len() as f64 * t_sym) * ch.fs) as usize;
    let span = 8.0;
    let mut out = vec![Cf::default(); n_out];
    let rrc = |t: f64| {
        let a = ch.rolloff;
        if t.abs() < 1e-9 {
            return 1.0 - a + 4.0 * a / PI;
        }
        if ((4.0 * a * t).abs() - 1.0).abs() < 1e-9 {
            return a / 2f64.sqrt()
                * ((1.0 + 2.0 / PI) * (PI / (4.0 * a)).sin() + (1.0 - 2.0 / PI) * (PI / (4.0 * a)).cos());
        }
        ((PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos())
            / (PI * t * (1.0 - (4.0 * a * t).powi(2)))
    };
    let mut power = 0.0f64;
    for (n, o) in out.iter_mut().enumerate() {
        let t = n as f64 / ch.fs;
        let k0 = ((t / t_sym) - span).ceil().max(0.0) as usize;
        let k1 = (((t / t_sym) + span).floor() as usize).min(symbols.len() - 1);
        let mut acc = Cf::default();
        for (k, s) in symbols.iter().enumerate().take(k1 + 1).skip(k0) {
            acc += *s * rrc(t / t_sym - k as f64) as f32;
        }
        let ph = 2.0 * PI * ch.carrier_hz * t;
        *o = acc * Cf::from_polar(1.0, ph as f32);
        power += o.norm_sqr() as f64;
    }
    let ps = power / n_out as f64;
    let esn0 = 10f64.powf(ch.esn0_db / 10.0);
    let sigma = (ps * ch.fs / (ch.symbol_rate * esn0) / 2.0).sqrt() as f32;
    let mut seed = ch.seed;
    // Scale to roughly a dongle's level and add noise.
    for o in out.iter_mut() {
        *o = (*o + Cf::new(sigma * gauss(&mut seed), sigma * gauss(&mut seed))) * 0.1;
    }
    out
}
