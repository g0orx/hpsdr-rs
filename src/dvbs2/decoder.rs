// FEC and framing: a demodulated frame's LLRs -> LDPC -> BCH -> BBFRAME
// -> transport stream packets.

use super::bbframe::{self, TsExtractor, TS_PACKET};
use super::bch::{BchCode, BchResult};
use super::demod::Frame;
use super::ldpc::LdpcCode;
use super::params::{fec_params, Rate};
use std::collections::HashMap;

/// Enough for every code here at a usable SNR; giving up sooner just
/// loses frames that would have decoded.
const MAX_ITERATIONS: usize = 50;

#[derive(Clone, Copy, Debug, Default)]
pub struct DecoderStats {
    pub frames: u64,
    pub ldpc_failures: u64,
    pub bch_corrected: u64,
    pub bch_failures: u64,
    pub header_errors: u64,
    pub packets: u64,
    pub packet_crc_errors: u64,
    pub last_iterations: usize,
}

pub struct Decoder {
    codes: HashMap<(bool, Rate), (LdpcCode, BchCode)>,
    ts: TsExtractor,
    pub stats: DecoderStats,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self { codes: HashMap::new(), ts: TsExtractor::default(), stats: DecoderStats::default() }
    }

    /// Decodes one frame, appending any complete TS packets to `out`.
    pub fn decode(&mut self, mut frame: Frame, out: &mut Vec<[u8; TS_PACKET]>) {
        let Some((_, rate)) = frame.pls.modcod() else {
            return;
        };
        let short = frame.pls.short;
        let Some(p) = fec_params(short, rate) else {
            return;
        };
        let (ldpc, bch) = self
            .codes
            .entry((short, rate))
            .or_insert_with(|| (LdpcCode::new(&p), BchCode::new(short, p.kldpc, p.kbch, p.t)));
        self.stats.frames += 1;
        match ldpc.decode(&mut frame.llr, MAX_ITERATIONS) {
            Some(n) => self.stats.last_iterations = n,
            None => {
                // BCH may still clean up a few residual errors.
                self.stats.ldpc_failures += 1;
                self.stats.last_iterations = MAX_ITERATIONS;
            }
        }
        let mut bits: Vec<u8> = frame.llr[..p.kldpc].iter().map(|&l| u8::from(l < 0.0)).collect();
        match bch.decode(&mut bits) {
            BchResult::Clean => {}
            BchResult::Corrected(_) => self.stats.bch_corrected += 1,
            BchResult::Failed => {
                self.stats.bch_failures += 1;
                return;
            }
        }
        bits.truncate(p.kbch);
        bbframe::descramble(&mut bits);
        let Some(header) = bbframe::parse_header(&bits) else {
            self.stats.header_errors += 1;
            return;
        };
        let before = out.len();
        self.ts.push(&header, &bits, out);
        self.stats.packets += (out.len() - before) as u64;
        self.stats.packet_crc_errors = self.ts.crc_errors;
    }
}
