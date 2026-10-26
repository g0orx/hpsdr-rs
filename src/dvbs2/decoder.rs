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

/// The FEC half of decoding: LLRs -> LDPC -> BCH -> descrambled
/// BBFRAME. It depends on nothing but the frame itself, so several can
/// run at once on different frames (datv.rs does, because one LDPC
/// decode takes longer than a frame lasts); Decoder::apply then puts the
/// results back in order.
#[derive(Default)]
pub struct FecDecoder {
    codes: HashMap<(bool, Rate), (LdpcCode, BchCode)>,
}

/// What FecDecoder made of one frame.
pub enum FecOutcome {
    /// A MODCOD this receiver doesn't decode.
    Skipped,
    Decoded {
        ldpc_failed: bool,
        iterations: usize,
        bch: BchResult,
        /// The BBFRAME's header and bits, if BCH succeeded and the header
        /// checked out.
        bbframe: Option<(bbframe::BbHeader, Vec<u8>)>,
    },
}

impl FecDecoder {
    pub fn decode(&mut self, mut frame: Frame) -> FecOutcome {
        let Some((_, rate)) = frame.pls.modcod() else {
            return FecOutcome::Skipped;
        };
        let short = frame.pls.short;
        let Some(p) = fec_params(short, rate) else {
            return FecOutcome::Skipped;
        };
        let (ldpc, bch) = self
            .codes
            .entry((short, rate))
            .or_insert_with(|| (LdpcCode::new(&p), BchCode::new(short, p.kldpc, p.kbch, p.t)));
        // On failure BCH may still clean up a few residual errors.
        let decoded = ldpc.decode(&mut frame.llr, MAX_ITERATIONS);
        let mut bits: Vec<u8> = frame.llr[..p.kldpc].iter().map(|&l| u8::from(l < 0.0)).collect();
        let bch = bch.decode(&mut bits);
        let bbframe = (!matches!(bch, BchResult::Failed))
            .then(|| {
                bits.truncate(p.kbch);
                bbframe::descramble(&mut bits);
                bbframe::parse_header(&bits).map(|h| (h, bits))
            })
            .flatten();
        FecOutcome::Decoded {
            ldpc_failed: decoded.is_none(),
            iterations: decoded.unwrap_or(MAX_ITERATIONS),
            bch,
            bbframe,
        }
    }
}

pub struct Decoder {
    // For decode(), which only the tests need -- datv.rs runs its own
    // FecDecoders and calls apply().
    #[cfg_attr(not(test), allow(dead_code))]
    fec: FecDecoder,
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
        Self { fec: FecDecoder::default(), ts: TsExtractor::default(), stats: DecoderStats::default() }
    }

    /// Decodes one frame, appending any complete TS packets to `out`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn decode(&mut self, frame: Frame, out: &mut Vec<[u8; TS_PACKET]>) {
        let outcome = self.fec.decode(frame);
        self.apply(outcome, out);
    }

    /// The in-order half: counts a FecDecoder outcome and extracts its
    /// TS packets. Outcomes must arrive in frame order.
    pub fn apply(&mut self, outcome: FecOutcome, out: &mut Vec<[u8; TS_PACKET]>) {
        let FecOutcome::Decoded { ldpc_failed, iterations, bch, bbframe } = outcome else {
            return;
        };
        self.stats.frames += 1;
        self.stats.last_iterations = iterations;
        if ldpc_failed {
            self.stats.ldpc_failures += 1;
        }
        match bch {
            BchResult::Clean => {}
            BchResult::Corrected(_) => self.stats.bch_corrected += 1,
            BchResult::Failed => {
                self.stats.bch_failures += 1;
                return;
            }
        }
        let Some((header, bits)) = bbframe else {
            self.stats.header_errors += 1;
            return;
        };
        let before = out.len();
        self.ts.push(&header, &bits, out);
        self.stats.packets += (out.len() - before) as u64;
        self.stats.packet_crc_errors = self.ts.crc_errors;
    }
}
