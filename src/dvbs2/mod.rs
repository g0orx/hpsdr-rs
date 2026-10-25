/*
    DVB-S2 receiver, for QO-100's wideband DATV transponder.

    Written from the standard, ETSI EN 302 307-1 V1.4.1 (free from
    etsi.org) -- not ported from leansdr, SDRangel or any other
    implementation, which are all GPL-3 (this project is GPL-2.0 or
    later). The LDPC tables are generated from the standard's own annexes
    by gen_ldpc_tables.py, which checks them as it goes.

    Pipeline, input to output:
    1. demod: the dongle's wideband IQ -> the channel mixed to 0 Hz,
       matched filtered (root raised cosine) and resampled to two
       samples per symbol; symbol timing (Gardner); PLHEADER search,
       carrier frequency and phase; PLS decode; PL descrambling.
    2. ldpc + bch: soft symbols -> LLRs -> LDPC (layered min-sum) ->
       BCH, giving a BBFRAME.
    3. bbframe: BB descrambling, BBHEADER check, and the MPEG transport
       stream packets in it.

    QPSK and 8PSK so far -- what QO-100 DATV uses in practice.
*/

pub mod bbframe;
pub mod bch;
pub mod decoder;
pub mod demod;
pub mod ldpc;
pub mod ldpc_tables;
pub mod params;
pub mod plheader;

use std::ops::{Add, AddAssign, Mul, Neg, Sub};

/// Minimal complex f32 -- all this module needs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cf {
    pub re: f32,
    pub im: f32,
}

impl Cf {
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    pub fn conj(self) -> Self {
        Self::new(self.re, -self.im)
    }

    pub fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }

    pub fn abs(self) -> f32 {
        self.norm_sqr().sqrt()
    }

    pub fn arg(self) -> f32 {
        self.im.atan2(self.re)
    }

    pub fn from_polar(r: f32, theta: f32) -> Self {
        Self::new(r * theta.cos(), r * theta.sin())
    }

    /// Multiplies by j^k.
    pub fn rot90(self, k: u8) -> Self {
        match k & 3 {
            0 => self,
            1 => Self::new(-self.im, self.re),
            2 => Self::new(-self.re, -self.im),
            _ => Self::new(self.im, -self.re),
        }
    }
}

impl Add for Cf {
    type Output = Cf;
    fn add(self, o: Cf) -> Cf {
        Cf::new(self.re + o.re, self.im + o.im)
    }
}

impl AddAssign for Cf {
    fn add_assign(&mut self, o: Cf) {
        self.re += o.re;
        self.im += o.im;
    }
}

impl Sub for Cf {
    type Output = Cf;
    fn sub(self, o: Cf) -> Cf {
        Cf::new(self.re - o.re, self.im - o.im)
    }
}

impl Mul for Cf {
    type Output = Cf;
    fn mul(self, o: Cf) -> Cf {
        Cf::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
}

impl Mul<f32> for Cf {
    type Output = Cf;
    fn mul(self, k: f32) -> Cf {
        Cf::new(self.re * k, self.im * k)
    }
}

impl Neg for Cf {
    type Output = Cf;
    fn neg(self) -> Cf {
        Cf::new(-self.re, -self.im)
    }
}

#[cfg(test)]
pub(crate) mod testsignal;

#[cfg(test)]
mod tests {
    use super::decoder::Decoder;
    use super::demod::Demod;
    use super::params::PlsCode;
    use super::testsignal::{frames, test_packets, waveform, Channel};

    /// The whole receiver on a synthetic signal: `pls` at `esn0_db`, with
    /// a carrier offset and a symbol clock error, sampled the way the
    /// MSI.SDR's wide mode samples. Returns (packets sent, received ok).
    fn end_to_end(pls: PlsCode, esn0_db: f64, carrier_hz: f64, clock_ppm: f64) -> (usize, usize) {
        end_to_end_with(pls, esn0_db, carrier_hz, clock_ppm, false)
    }

    /// `inverted`: conjugate the received samples, as a spectrum-inverted
    /// front end would.
    fn end_to_end_with(pls: PlsCode, esn0_db: f64, carrier_hz: f64, clock_ppm: f64, inverted: bool) -> (usize, usize) {
        let (fs, rs) = (4_608_000.0, 1_500_000.0);
        let packets = test_packets(1500);
        let symbols = frames(&packets, pls);
        let ch = Channel { fs, symbol_rate: rs, rolloff: 0.35, carrier_hz, clock_ppm, esn0_db, seed: 99 };
        let mut samples = waveform(&symbols, &ch);
        if inverted {
            for s in samples.iter_mut() {
                *s = s.conj();
            }
        }
        let mut demod = Demod::new(fs, 0.0, rs, 0.35);
        let mut decoder = Decoder::new();
        let mut got = Vec::new();
        let mut demod_frames = Vec::new();
        for chunk in samples.chunks(10_000) {
            demod.process(chunk, &mut demod_frames);
            for f in demod_frames.drain(..) {
                decoder.decode(f, &mut got);
            }
        }
        eprintln!("{:?}\n{:?}", demod.status, decoder.stats);
        // Every packet received must be one that was sent, in order.
        let mut next = 0;
        let mut ok = 0;
        for g in &got {
            let Some(i) = packets[next..].iter().position(|p| p == g) else { panic!("corrupt packet received") };
            next += i + 1;
            ok += 1;
        }
        (packets.len(), ok)
    }

    /// Replays a real recording (ci16_le, as mirisdr's record_wide_stream
    /// or the app's raw IQ recorder write): `HPSDR_DVBS2_FILE`, sample
    /// rate `HPSDR_DVBS2_FS` (default 4608000), channel offset
    /// `HPSDR_DVBS2_CHANNEL` Hz from the recording's centre, symbol rate
    /// `HPSDR_DVBS2_RS` (default 1.5 MS/s).
    #[test]
    #[ignore]
    fn replay_recording() {
        let env = |k: &str, d: f64| std::env::var(k).map(|v| v.parse().unwrap()).unwrap_or(d);
        let path = std::env::var("HPSDR_DVBS2_FILE").expect("set HPSDR_DVBS2_FILE");
        let (fs, channel, rs) = (env("HPSDR_DVBS2_FS", 4_608_000.0), env("HPSDR_DVBS2_CHANNEL", 0.0), env("HPSDR_DVBS2_RS", 1_500_000.0));
        let bytes = std::fs::read(path).unwrap();
        let samples: Vec<super::Cf> = bytes
            .chunks_exact(4)
            .map(|c| {
                let i = i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0;
                let q = i16::from_le_bytes([c[2], c[3]]) as f32 / 32768.0;
                super::Cf::new(i, q)
            })
            .collect();
        let mut demod = Demod::new(fs, channel, rs, 0.35);
        let mut decoder = Decoder::new();
        let (mut frames, mut packets) = (Vec::new(), Vec::new());
        for (n, chunk) in samples.chunks(fs as usize / 10).enumerate() {
            demod.process(chunk, &mut frames);
            for f in frames.drain(..) {
                decoder.decode(f, &mut packets);
            }
            eprintln!("{:.1} s {:?}", (n + 1) as f64 / 10.0, demod.status);
        }
        eprintln!("{:?}", decoder.stats);
    }

    #[test]
    #[ignore]
    fn one_8psk() {
        let (sent, ok) = end_to_end(PlsCode { modcod: 13, short: false, pilots: true }, 8.5, 15_000.0, 20.0);
        eprintln!("  {ok} of {sent}");
    }

    #[test]
    #[ignore]
    fn sweep() {
        let q45 = PlsCode { modcod: 8, short: false, pilots: false };
        for (pls, esn0, c, ppm) in [
            (q45, 5.5, 23_000.0, 40.0),
            (q45, 7.0, 100_000.0, -60.0),
            (PlsCode { modcod: 4, short: true, pilots: false }, 3.0, 10_000.0, 20.0),
            (PlsCode { modcod: 13, short: false, pilots: true }, 8.5, 15_000.0, 20.0),
            (PlsCode { modcod: 12, short: true, pilots: true }, 8.0, -15_000.0, 20.0),
        ] {
            eprintln!("{pls:?} {esn0} dB carrier {c} ppm {ppm}");
            let (sent, ok) = end_to_end(pls, esn0, c, ppm);
            eprintln!("  {ok} of {sent}");
        }
    }

    #[test]
    fn receives_qpsk_4_5_like_the_qo100_beacon() {
        let pls = PlsCode { modcod: 8, short: false, pilots: false };
        let (sent, ok) = end_to_end(pls, 7.0, 23_000.0, 40.0);
        // The first frames go on acquisition; the rest must all arrive.
        assert!(ok > sent / 2, "{ok} of {sent}");
    }

    #[test]
    fn copes_with_a_spectrum_inverted_signal() {
        let pls = PlsCode { modcod: 8, short: false, pilots: false };
        let (sent, ok) = end_to_end_with(pls, 7.0, -12_000.0, 10.0, true);
        assert!(ok > sent / 2, "{ok} of {sent}");
    }
}
