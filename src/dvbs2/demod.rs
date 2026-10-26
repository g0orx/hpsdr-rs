// DVB-S2 demodulator: wideband IQ in, PLFRAMEs (as soft bits) out.
//
// 1. Channel: mix the wanted signal to 0 Hz, then a polyphase resampler
//    whose prototype is the root-raised-cosine matched filter produces
//    two samples per symbol at instants a Gardner timing loop steers.
// 2. Frames: the PLHEADER is found by differential correlation (each
//    symbol times the conjugate of the one before), which a carrier
//    offset only rotates, so it works before the carrier is known. A
//    header's PLS code then gives the frame's length, so lock is
//    confirmed by finding the next header exactly where it should be.
// 3. Carrier: the headers' differential phase measures the frequency
//    offset (fed back to the mixer); each header's known symbols set
//    the absolute phase; a decision-directed loop tracks it through the
//    frame (QPSK and 8PSK decisions don't care about the PL scrambling's
//    90 degree rotations), with pilot blocks, when sent, as references.
// 4. Output: descrambled data symbols, pilots removed, turned into bit
//    LLRs (and deinterleaved for 8PSK) ready for the LDPC decoder.

use super::params::{Modulation, PlsCode};
use super::plheader::{self, HEADER_SYMBOLS, SOF_BITS};
use super::Cf;
use std::f32::consts::{FRAC_1_SQRT_2, PI};

/// One received PLFRAME's code word, as LLRs (positive = 0), in LDPC
/// order.
pub struct Frame {
    pub pls: PlsCode,
    pub llr: Vec<f32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    pub locked: bool,
    pub pls: Option<PlsCode>,
    /// Carrier offset from the channel frequency the demod was given, Hz.
    pub carrier_offset_hz: f32,
    pub snr_db: f32,
    pub frames: u64,
    /// Measured symbol rate error, parts per million.
    pub timing_ppm: f32,
    /// The signal turned out spectrum-inverted (and is being conjugated).
    pub inverted: bool,
}

// ---------------------------------------------------------------------
// Root raised cosine polyphase resampler
// ---------------------------------------------------------------------

/// Filter phases (interpolation resolution: 1/PHASES of an input sample).
const PHASES: usize = 128;
/// Matched filter span each side, symbols.
const SPAN_SYMBOLS: f64 = 6.0;

/// RRC impulse response at time `t` symbols, roll-off `a`.
fn rrc(t: f64, a: f64) -> f64 {
    let pi = std::f64::consts::PI;
    if t.abs() < 1e-9 {
        return 1.0 - a + 4.0 * a / pi;
    }
    if a > 0.0 && ((4.0 * a * t).abs() - 1.0).abs() < 1e-9 {
        return a / 2f64.sqrt()
            * ((1.0 + 2.0 / pi) * (pi / (4.0 * a)).sin() + (1.0 - 2.0 / pi) * (pi / (4.0 * a)).cos());
    }
    ((pi * t * (1.0 - a)).sin() + 4.0 * a * t * (pi * t * (1.0 + a)).cos()) / (pi * t * (1.0 - (4.0 * a * t).powi(2)))
}

struct Resampler {
    /// Taps per phase.
    taps: usize,
    /// bank[p][j]: weight of the j-th oldest of the last `taps` inputs,
    /// for an output p/PHASES of a sample past the filter's reference.
    bank: Vec<Vec<f32>>,
    /// Ring of the last `taps` inputs, each written twice (at `pos` and
    /// `pos + taps`) so they're always one contiguous slice.
    ring: Vec<Cf>,
    pos: usize,
}

impl Resampler {
    fn new(samples_per_symbol: f64, rolloff: f64) -> Self {
        let half = (SPAN_SYMBOLS * samples_per_symbol).ceil() as usize;
        let taps = 2 * half + 1;
        let mut bank = vec![vec![0f32; taps]; PHASES + 1];
        let mut gain = 0.0;
        for (p, phase) in bank.iter_mut().enumerate() {
            let mu = p as f64 / PHASES as f64;
            for (j, w) in phase.iter_mut().enumerate() {
                // Input j-th oldest is at n - (taps-1-j); the output is at
                // n + mu - half (the filter's centre).
                let age = (taps - 1 - j) as f64;
                let t = (mu + age - half as f64) / samples_per_symbol;
                *w = rrc(t, rolloff) as f32;
                if p == 0 {
                    gain += rrc(t, rolloff);
                }
            }
        }
        // Unit DC gain, so the output level follows the input level.
        for phase in bank.iter_mut() {
            for w in phase.iter_mut() {
                *w /= gain as f32;
            }
        }
        Self { taps, bank, ring: vec![Cf::default(); 2 * taps], pos: 0 }
    }

    /// The filter output for fractional position `mu` (0..1) past the
    /// newest input sample's position minus the filter delay.
    fn output(&self, mu: f32) -> Cf {
        let p = ((mu * PHASES as f32).round() as usize).min(PHASES);
        let (mut re, mut im) = (0f32, 0f32);
        for (x, &k) in self.ring[self.pos..self.pos + self.taps].iter().zip(self.bank[p].iter()) {
            re += x.re * k;
            im += x.im * k;
        }
        Cf::new(re, im)
    }

    fn push(&mut self, x: Cf) {
        // Oldest first: overwrite the oldest slot, then advance.
        self.ring[self.pos] = x;
        self.ring[self.pos + self.taps] = x;
        self.pos = (self.pos + 1) % self.taps;
    }
}

// ---------------------------------------------------------------------
// Demodulator
// ---------------------------------------------------------------------

/// Header correlation (0..1, normalised) needed to start a lock attempt,
/// and to keep one once a frame's position is known.
/// Noise alone reaches ~0.5 every so often (seen on a real recording
/// with the beacon too weak to use); a real header scores ~0.65 even at
/// 3 dB.
const SEARCH_THRESHOLD: f32 = 0.55;
const TRACK_THRESHOLD: f32 = 0.2;
/// Symbol clock error the timing loop may correct (200 ppm).
const MAX_CLOCK_ERROR: f64 = 2e-4;
/// How far from the channel frequency the carrier may be pulled, as a
/// fraction of the symbol rate.
const MAX_CARRIER_OFFSET: f64 = 0.25;
/// Noise on one header's rotation measurement, rad/symbol (~250 Hz at
/// 1.5 MS/s and 8 dB; as a fraction of the symbol rate it's the same at
/// any rate).
const HEADER_ROTATION_SIGMA: f32 = 2.0 * PI * 250.0 / 1.5e6;
/// The carrier frequency's uncertainty just after lock, and the least
/// it's ever assumed to be.
const RESIDUAL_SIGMA_LOCK: f32 = 2.0 * PI * 1_000.0 / 1.5e6;
const RESIDUAL_SIGMA_FLOOR: f32 = 2.0 * PI * 30.0 / 1.5e6;
/// Decision-directed carrier loop gains (phase, frequency) per symbol:
/// damping 0.707, ~4 kHz noise bandwidth at 1.5 MS/s. Tuned on four real
/// QO-100 beacon recordings near threshold (Pluto, roll-off 0.20,
/// 2026-10-26): against the original 0.02 / 0.0002, the frames failing
/// fell from 122 to 44 of 445 on one and from 402 to 301 of 448 on the
/// weakest, with none worse. A wide loop lets noisy decisions push the
/// phase around: 0.04 failed nearly every frame. 0.004-0.005 did a
/// little better still on the beacon, but cost more on the synthetic
/// sweep's QPSK 1/2 short frames at 3 dB (380-410 packets of 1500, this
/// 492, the original 596); 0.0025 and below lost track of the LNB's
/// phase noise.
const CARRIER_LOOP_KP: f32 = 0.007;
const CARRIER_LOOP_KI: f32 = 2.45e-5;
/// How far apart (rad/symbol) the two headers that confirm a lock may
/// put the carrier before one is taken to be wrong -- see next_header.
/// ~5x the worst genuine disagreement measured at 3 dB.
const CONFIRM_ROTATION_AGREEMENT: f32 = 0.04;
/// Missed headers in a row before going back to searching.
const MAX_MISSES: u32 = 3;

#[derive(PartialEq)]
enum State {
    Searching,
    /// A header was seen; checking the next is where its PLS code says.
    Confirming,
    Locked,
}

pub struct Demod {
    fs: f64,
    symbol_rate: f64,
    // Mixer.
    /// Mixer oscillator (unit vector) and its per-sample rotation.
    mix_osc: Cf,
    mix_rot: Cf,
    mix_count: u32,
    channel_hz: f64,
    carrier_hz: f64,
    // Resampler + timing.
    resampler: Resampler,
    /// Input samples per output sample (half a symbol), nominal.
    step: f64,
    /// Input samples until the next output.
    until_next: f64,
    timing_integrator: f64,
    half_symbol: bool,
    prev_on_time: Cf,
    mid: Cf,
    agc: f32,
    // Symbols.
    /// Recent symbols for the header search (newest last).
    recent: Vec<Cf>,
    /// While searching: each recent symbol times the previous one's
    /// conjugate, and its magnitude (diffs[k] belongs to recent[k+1]).
    diffs: Vec<(Cf, f32)>,
    /// Signs of the SOF / PLS-pair reference differentials (+j or -j).
    sof_sign: [bool; SOF_BITS - 1],
    pls_sign: [bool; 32],
    sof_diff: [Cf; SOF_BITS - 1],
    /// PLS pair differentials (symbol 2i+1 vs 2i of the PLS code), up to
    /// the sign set by TYPE's LSB.
    pls_pair_diff: [Cf; 32],
    state: State,
    /// Symbols since the current frame's header started.
    position: usize,
    frame_len: usize,
    pls: Option<PlsCode>,
    misses: u32,
    /// Collected symbols of the current frame (header included).
    frame: Vec<Cf>,
    scrambling: Vec<u8>,
    // Carrier tracking (applied per symbol, after the resampler).
    phase: f32,
    freq: f32,
    pub status: Status,
    /// Headers since lock, for averaging their frequency measurements.
    locked_headers: u32,
    /// The rotation measured on a header being confirmed.
    pending_rotation: f32,
    /// How unsure the carrier frequency still is, rad/symbol.
    residual_sigma: f32,
    /// Working on the conjugated input (a spectrum-inverted signal).
    inverted: bool,
    /// The carrier at the last strong header (see lose_lock).
    good_carrier_hz: Option<f64>,
    /// See false_lock: one bit per header since lock, newest lowest,
    /// set if it was convincing.
    strong_headers: u16,
    headers_since_lock: u16,
}

impl Demod {
    /// `fs`: input rate; `channel_hz`: the signal's offset from the
    /// stream's centre; `symbol_rate`; `rolloff` (0.35, 0.25 or 0.20 --
    /// 0.35 works for all with a little loss).
    pub fn new(fs: f64, channel_hz: f64, symbol_rate: f64, rolloff: f64) -> Self {
        let sps = fs / symbol_rate;
        let sof = plheader::sof_symbols();
        let header = plheader::header_symbols(PlsCode::from_bits(0));
        let mut demod = Self {
            fs,
            symbol_rate,
            mix_osc: Cf::new(1.0, 0.0),
            mix_rot: Cf::new(1.0, 0.0),
            mix_count: 0,
            channel_hz,
            carrier_hz: 0.0,
            resampler: Resampler::new(sps, rolloff),
            step: sps / 2.0,
            until_next: sps / 2.0,
            timing_integrator: 0.0,
            half_symbol: false,
            prev_on_time: Cf::default(),
            mid: Cf::default(),
            agc: 1.0,
            recent: Vec::with_capacity(4 * HEADER_SYMBOLS),
            diffs: Vec::with_capacity(4 * HEADER_SYMBOLS),
            sof_sign: std::array::from_fn(|k| (sof[k + 1] * sof[k].conj()).im > 0.0),
            pls_sign: std::array::from_fn(|i| {
                (header[SOF_BITS + 2 * i + 1] * header[SOF_BITS + 2 * i].conj()).im > 0.0
            }),
            sof_diff: std::array::from_fn(|k| sof[k + 1] * sof[k].conj()),
            pls_pair_diff: std::array::from_fn(|i| header[SOF_BITS + 2 * i + 1] * header[SOF_BITS + 2 * i].conj()),
            state: State::Searching,
            position: 0,
            frame_len: 0,
            pls: None,
            misses: 0,
            frame: Vec::new(),
            scrambling: plheader::scrambling_sequence(),
            phase: 0.0,
            freq: 0.0,
            status: Status::default(),
            locked_headers: 0,
            pending_rotation: 0.0,
            residual_sigma: RESIDUAL_SIGMA_LOCK,
            inverted: false,
            good_carrier_hz: None,
            strong_headers: 0,
            headers_since_lock: 0,
        };
        demod.retune();
        demod
    }

    fn retune(&mut self) {
        // Inverted, the input is conjugated first, which mirrors the
        // channel's offset; the carrier is tracked after that.
        let channel = if self.inverted { -self.channel_hz } else { self.channel_hz };
        let step = -2.0 * std::f64::consts::PI * (channel + self.carrier_hz) / self.fs;
        self.mix_rot = Cf::new(step.cos() as f32, step.sin() as f32);
        // Reported as an offset in the input, whichever way up it is.
        self.status.carrier_offset_hz = if self.inverted { -self.carrier_hz } else { self.carrier_hz } as f32;
    }

    /// Moves the channel (the signal's offset from the input's centre),
    /// e.g. when the hardware is retuned, keeping the carrier tracking.
    pub fn set_channel(&mut self, channel_hz: f64) {
        self.channel_hz = channel_hz;
        self.retune();
    }

    pub fn process(&mut self, input: &[Cf], frames: &mut Vec<Frame>) {
        for &x in input {
            let x = if self.inverted { x.conj() } else { x };
            // Mix.
            self.resampler.push(x * self.mix_osc);
            self.mix_osc = self.mix_osc * self.mix_rot;
            self.mix_count += 1;
            if self.mix_count == 1024 {
                // Keep the oscillator on the unit circle.
                self.mix_count = 0;
                self.mix_osc = self.mix_osc * (1.0 / self.mix_osc.abs());
            }
            self.until_next -= 1.0;
            while self.until_next < 0.0 {
                // The output instant is -until_next samples before the
                // newest input -> fractional position from the filter's
                // own reference.
                let mu = (1.0 + self.until_next) as f32;
                let y = self.resampler.output(mu.clamp(0.0, 1.0)) * self.agc;
                self.until_next += self.step;
                self.half_symbol = !self.half_symbol;
                if self.half_symbol {
                    self.mid = y;
                } else {
                    self.on_time(y, frames);
                }
            }
        }
    }

    fn on_time(&mut self, y: Cf, frames: &mut Vec<Frame>) {
        // Gardner timing error (needs no carrier): positive when sampling
        // late.
        let e = ((y - self.prev_on_time) * self.mid.conj()).re;
        self.prev_on_time = y;
        let e = e.clamp(-1.0, 1.0) as f64;
        self.timing_integrator += 1e-5 * e;
        // Real symbol clocks are within tens of ppm; the clamp stops the
        // loop wandering off on noise while nothing is there.
        self.timing_integrator = self.timing_integrator.clamp(-MAX_CLOCK_ERROR, MAX_CLOCK_ERROR);
        let correction = (2e-3 * e + self.timing_integrator) * self.step;
        self.until_next -= correction;
        self.status.timing_ppm = (-self.timing_integrator * 1e6) as f32;

        // AGC to unit symbol power.
        let p = y.norm_sqr();
        self.agc *= 1.0 + 1e-3 * (1.0 - p).clamp(-1.0, 1.0);

        // Carrier: per-symbol derotation.
        self.phase += self.freq;
        if self.phase > PI {
            self.phase -= 2.0 * PI;
        } else if self.phase < -PI {
            self.phase += 2.0 * PI;
        }
        let s = y * Cf::from_polar(1.0, -self.phase);
        self.symbol(s, frames);
    }

    /// header_metric's normalised correlation, cheaply, for the search:
    /// differentials come from `diffs` (computed once per symbol), and as
    /// every reference differential is +-j (consecutive pi/2-BPSK symbols
    /// sit on axes 90 degrees apart) the correlation is just signed sums.
    fn search_metric(&self, start: usize) -> f32 {
        let d = &self.diffs[start..start + HEADER_SYMBOLS - 1];
        let (mut sof, mut energy) = (Cf::default(), 0.0);
        for k in 0..SOF_BITS - 1 {
            let (v, a) = d[k];
            sof += if self.sof_sign[k] { v } else { -v };
            energy += a;
        }
        let mut pls = Cf::default();
        for i in 0..32 {
            let (v, a) = d[SOF_BITS + 2 * i];
            pls += if self.pls_sign[i] { v } else { -v };
            energy += a;
        }
        (sof.abs() + pls.abs()) / energy.max(1e-12)
    }

    /// Normalised differential header correlation for a header starting
    /// at recent[start], and the mean rotation per symbol it implies.
    fn header_metric(&self, start: usize) -> (f32, f32) {
        let h = &self.recent[start..start + HEADER_SYMBOLS];
        let mut sof = Cf::default();
        let mut energy = 0.0;
        for k in 0..SOF_BITS - 1 {
            let d = h[k + 1] * h[k].conj();
            sof += d * self.sof_diff[k].conj();
            energy += d.abs();
        }
        let mut pls = Cf::default();
        for i in 0..32 {
            let d = h[SOF_BITS + 2 * i + 1] * h[SOF_BITS + 2 * i].conj();
            pls += d * self.pls_pair_diff[i].conj();
            energy += d.abs();
        }
        // The PLS pairs' sign depends on TYPE's LSB: align them with the
        // SOF before adding, so the rotation estimate uses both.
        let pls_aligned = if (pls * sof.conj()).re < 0.0 { -pls } else { pls };
        let total = sof + pls_aligned;
        ((sof.abs() + pls.abs()) / energy.max(1e-12), total.arg())
    }

    /// Decodes the PLS of a header starting at recent[start]: returns
    /// the code, and the residual rotation per symbol measured on the
    /// whole (now known) header.
    ///
    /// The decision sums correlation magnitudes over short blocks rather
    /// than correlating the whole header coherently, so it tolerates a
    /// carrier still a few kHz out: extrapolating a frequency measured on
    /// the SOF alone across the 64 PLS symbols proved too noisy (MODCOD 8
    /// read as 9 -- codewords that differ only in their second half).
    ///
    /// Also says whether the signal is spectrum-inverted. A conjugated
    /// pi/2-BPSK header is the same as the right one rotating by pi per
    /// symbol, so inversion shows up as a header "rotation" near +-pi --
    /// far more than any real carrier offset (which the matched filter
    /// limits to a fraction of the symbol rate).
    fn decode_header(&self, start: usize) -> (PlsCode, f32, bool) {
        const BLOCK: usize = 15;
        let h = &self.recent[start..start + HEADER_SYMBOLS];
        // A coarse rotation from all the differentials, in case the
        // carrier is far out (searching).
        let (_, coarse) = self.header_metric(start);
        let derotated: Vec<Cf> =
            h.iter().enumerate().map(|(k, &v)| v * Cf::from_polar(1.0, -coarse * k as f32)).collect();
        let mut best = (PlsCode::from_bits(0), f32::MIN);
        for v in 0..128u8 {
            let code = PlsCode::from_bits(v);
            let reference = plheader::header_symbols(code);
            let score: f32 = derotated
                .chunks(BLOCK)
                .zip(reference.chunks(BLOCK))
                .map(|(hb, rb)| hb.iter().zip(rb).fold(Cf::default(), |a, (x, r)| a + *x * r.conj()).abs())
                .sum();
            if score > best.1 {
                best = (code, score);
            }
        }
        let code = best.0;
        // Residual rotation: the phase steps between successive blocks,
        // with the modulation removed. (Measuring across the two halves
        // instead wrapped: the coarse estimate is only good to ~0.08
        // rad/symbol at 7 dB, which over 45 symbols is more than pi.)
        let reference = plheader::header_symbols(code);
        let blocks: Vec<Cf> = derotated
            .chunks(BLOCK)
            .zip(reference.chunks(BLOCK))
            .map(|(hb, rb)| hb.iter().zip(rb).fold(Cf::default(), |a, (x, r)| a + *x * r.conj()))
            .collect();
        let step = blocks.windows(2).fold(Cf::default(), |a, w| a + w[1] * w[0].conj());
        let w = coarse + step.arg() / BLOCK as f32;
        (code, w, w.abs() > PI / 2.0)
    }

    fn symbol(&mut self, s: Cf, frames: &mut Vec<Frame>) {
        match self.state {
            State::Searching => {
                if self.diffs.len() + 1 != self.recent.len().max(1) {
                    // Arrived here with `recent` holding symbols from
                    // elsewhere (a lost lock): rebuild to match.
                    self.diffs = self.recent.windows(2).map(|w| (w[1] * w[0].conj(), (w[1] * w[0].conj()).abs())).collect();
                }
                if let Some(&prev) = self.recent.last() {
                    let d = s * prev.conj();
                    self.diffs.push((d, d.abs()));
                }
                self.recent.push(s);
                if self.recent.len() < HEADER_SYMBOLS {
                    return;
                }
                let start = self.recent.len() - HEADER_SYMBOLS;
                if self.search_metric(start) > SEARCH_THRESHOLD {
                    let (code, w, inverted) = self.decode_header(start);
                    if inverted {
                        // Flip, and look again from scratch.
                        self.inverted = !self.inverted;
                        self.status.inverted = self.inverted;
                        self.carrier_hz = -self.carrier_hz;
                        self.good_carrier_hz = None;
                        self.retune();
                        self.recent.clear();
                        self.diffs.clear();
                        return;
                    }
                    if let Some(len) = code.frame_symbols().filter(|_| code.modcod().is_some() || code.is_dummy()) {
                        self.pls = Some(code);
                        self.frame_len = len;
                        self.position = HEADER_SYMBOLS;
                        self.state = State::Confirming;
                        // Not applied until a second header confirms it:
                        // a false detection's estimate is garbage.
                        self.pending_rotation = w;
                        self.frame.clear();
                        self.frame.extend_from_slice(&self.recent[start..]);
                        self.recent.clear();
                        self.diffs.clear();
                        return;
                    }
                }
                if self.recent.len() >= 4 * HEADER_SYMBOLS {
                    let drop = self.recent.len() - HEADER_SYMBOLS;
                    self.recent.drain(..drop);
                    self.diffs.drain(..drop);
                }
            }
            State::Confirming | State::Locked => {
                if self.position < self.frame_len {
                    self.frame.push(s);
                    self.position += 1;
                    if self.state == State::Locked {
                        self.track(s, self.position - 1);
                    }
                    return;
                }
                // The next frame's header.
                self.recent.push(s);
                if self.recent.len() == HEADER_SYMBOLS {
                    self.next_header(frames);
                }
            }
        }
    }

    /// At the point the next header should have just completed.
    fn next_header(&mut self, frames: &mut Vec<Frame>) {
        let (metric, _) = self.header_metric(0);
        if self.state == State::Locked && self.false_lock(metric) {
            self.lose_lock();
            self.recent.clear();
            self.diffs.clear();
            self.frame.clear();
            return;
        }
        let found = metric > if self.state == State::Locked { TRACK_THRESHOLD } else { SEARCH_THRESHOLD * 0.8 };
        // The finished frame's own symbols (without this header).
        let mut done = std::mem::take(&mut self.frame);
        done.truncate(self.frame_len);
        if found {
            let (code, w, _) = self.decode_header(0);
            if self.state == State::Locked {
                let next = self.recent.clone();
                if let Some(frame) = self.finish_frame(&mut done, Some((&next, code, w))) {
                    frames.push(frame);
                }
            }
            if self.state == State::Confirming {
                // The two headers' carrier readings, averaged -- unless
                // they disagree, when one is a bad reading: then the
                // smaller correction. MEASURED: genuine pairs agree to
                // 0.008 rad/symbol even at 3 dB; the bad pairs seen (a
                // REAL RECORDING of a 250 kS/s QO-100 station at ~2.5 dB,
                // and the synthetic sweep at 3 dB) read ~0 then ~0.19, and
                // averaging them threw the carrier to the -62.5 kHz limit
                // on the former. Dropping such a confirmation instead
                // lost a good lock (sweep QPSK 1/2 short: 541 -> 498
                // packets).
                let (a, b) = (self.pending_rotation, w);
                let rotation = if (a - b).abs() > CONFIRM_ROTATION_AGREEMENT {
                    if a.abs() < b.abs() { a } else { b }
                } else {
                    (a + b) / 2.0
                };
                self.adjust_frequency(rotation, 1.0);
                self.residual_sigma = RESIDUAL_SIGMA_LOCK;
            } else if !self.pls.is_some_and(|p| p.pilots) {
                // (Frames with pilots trim the frequency in pilot_phase.)
                self.track_frequency(w);
            }
            // A header as clear as one the search would start from is a
            // trustworthy place to come back to (see lose_lock).
            if metric > SEARCH_THRESHOLD {
                self.good_carrier_hz = Some(self.carrier_hz + self.freq as f64 * self.symbol_rate / (2.0 * std::f64::consts::PI));
            }
            self.state = State::Locked;
            self.status.locked = true;
            self.misses = 0;
            self.start_frame(code);
        } else {
            self.misses += 1;
            if self.state == State::Confirming || self.misses > MAX_MISSES {
                self.lose_lock();
                self.recent.clear();
                self.diffs.clear();
                self.frame.clear();
                return;
            }
            // Keep going on the old frame's timing.
            let code = self.pls.unwrap_or(PlsCode::from_bits(0));
            self.start_frame(code);
        }
    }

    /// Whether a lock has gone without a convincing header for too long
    /// to be real, given this header's correlation. TRACK_THRESHOLD is
    /// low so that a real lock rides out a few noisy headers -- but
    /// random data clears it often enough that a false lock, once made,
    /// could last forever. REAL REPORT (QO-100 beacon, QPSK 4/5 normal
    /// frames, ~4.6 dB): the receiver locked as 8PSK 5/6 short frames and
    /// stayed there, every frame failing. On a real lock at that SNR 94%
    /// of headers score above SEARCH_THRESHOLD * 0.8 (median 0.85), so
    /// fewer than 4 of the last 16 means the lock is false.
    fn false_lock(&mut self, metric: f32) -> bool {
        self.strong_headers = (self.strong_headers << 1) | u16::from(metric > SEARCH_THRESHOLD * 0.8);
        self.headers_since_lock = self.headers_since_lock.saturating_add(1);
        self.headers_since_lock >= 16 && self.strong_headers.count_ones() < 4
    }

    /// Back to searching. The carrier goes back to where the last strong
    /// header put it: REAL RECORDING (QO-100 beacon, ~4.8 dB, roll-off
    /// 0.20) -- a run of noisy headers walked the carrier off to -375 kHz
    /// before lock was lost, and the search never found the signal again
    /// from there.
    fn lose_lock(&mut self) {
        self.state = State::Searching;
        self.status.locked = false;
        self.locked_headers = 0;
        self.headers_since_lock = 0;
        self.strong_headers = 0;
        if let Some(good) = self.good_carrier_hz {
            self.carrier_hz = good;
            self.freq = 0.0;
            self.retune();
        }
    }

    fn start_frame(&mut self, code: PlsCode) {
        let Some(len) = code.frame_symbols() else {
            self.lose_lock();
            return;
        };
        self.pls = Some(code);
        self.status.pls = Some(code);
        self.frame_len = len;
        self.position = HEADER_SYMBOLS;
        self.frame.clear();
        self.frame.extend_from_slice(&self.recent);
        // Absolute phase from the header's 90 known symbols.
        let reference = plheader::header_symbols(code);
        let mut acc = Cf::default();
        for (r, h) in reference.iter().zip(self.recent.iter()) {
            acc += *h * r.conj();
        }
        let correction = acc.arg();
        self.phase += correction;
        for v in self.frame.iter_mut() {
            *v = *v * Cf::from_polar(1.0, -correction);
        }
        self.recent.clear();
    }

    /// Once locked: the header's measured residual rotation `w` nudges
    /// the symbol-rate loop's frequency. Only when that has wandered a
    /// long way (LNB drift) does it move into the mixer -- retuning the
    /// mixer at every header disturbed the start of the next frame enough
    /// to make the carrier loop slip (about one frame in ten failed at
    /// 7 dB in the synthetic test; none do without it).
    fn track_frequency(&mut self, w: f32) {
        // Past the first few headers the carrier is known to within a few
        // hundred Hz and an LNB drifts by Hz per second: a reading kHz out
        // is a header too noisy to measure, not a real change. Applying
        // them is what walked the carrier away in lose_lock's real
        // recording (steps of 9 kHz, then 84 kHz). Not much tighter:
        // genuine readings spread wider at low SNR (HEADER_ROTATION_SIGMA
        // is for 8 dB), and rejecting them cost frames at 3 dB.
        //
        // Before that, lock has only just been confirmed by two headers
        // that agreed (see next_header), so a reading beyond the agreement
        // bound is just as surely bad. REAL RECORDING (250 kS/s QO-100
        // station, ~2.5 dB): the first header after lock read -2.2
        // rad/symbol and, trusted in full, sent the carrier to its limit.
        let outlier = if self.locked_headers >= 4 { 12.0 * HEADER_ROTATION_SIGMA } else { CONFIRM_ROTATION_AGREEMENT };
        if w.abs() > outlier {
            return;
        }
        // A running mean over the first headers after lock (one header's
        // measurement alone is only good to ~600 Hz at 8 dB, too coarse
        // for pilot_phase to unwrap between pilot blocks), then a steady
        // 0.2.
        self.locked_headers += 1;
        let gain = (1.0 / self.locked_headers as f32).max(0.2);
        self.freq += gain * w;
        let limit = 2.0 * PI * 2_000.0 / self.symbol_rate as f32;
        if self.freq.abs() > limit {
            self.adjust_frequency(0.0, 0.0);
        }
    }

    /// `w`: residual rotation per symbol measured on a header (rad);
    /// `gain`: how much of it to apply. The symbol-rate loop's own
    /// frequency moves into the mixer at the same time, so the two never
    /// correct the same error twice.
    fn adjust_frequency(&mut self, w: f32, gain: f64) {
        let rad_per_symbol = self.freq as f64 + gain * w as f64;
        self.freq = 0.0;
        let limit = MAX_CARRIER_OFFSET * self.symbol_rate;
        self.carrier_hz =
            (self.carrier_hz + rad_per_symbol * self.symbol_rate / (2.0 * std::f64::consts::PI)).clamp(-limit, limit);
        self.retune();
    }

    /// Decision-directed carrier phase tracking on a data symbol, for
    /// frames without pilots. Frames with pilots are corrected from them
    /// afterwards instead (see pilot_phase): on 8PSK this loop slipped
    /// by whole constellation steps too often for pilots 1476 symbols
    /// apart to rescue the frame, even with each pilot block resetting
    /// it as it arrived.
    fn track(&mut self, s: Cf, _k: usize) {
        let Some(pls) = self.pls else { return };
        if pls.pilots {
            return;
        }
        let Some((modulation, _)) = pls.modcod() else {
            return;
        };
        let decision = match modulation {
            Modulation::Qpsk => Cf::new(FRAC_1_SQRT_2.copysign(s.re), FRAC_1_SQRT_2.copysign(s.im)),
            Modulation::Psk8 => {
                let k = (s.arg() / (PI / 4.0)).round();
                Cf::from_polar(1.0, k * PI / 4.0)
            }
            _ => return,
        };
        let e = (s * decision.conj()).arg();
        self.phase += CARRIER_LOOP_KP * e;
        // Bounded like track_frequency's hand-over to the mixer: drift
        // beyond it is the mixer's job, and on noise alone this loop can
        // otherwise wander off.
        let limit = 2.0 * PI * 2_000.0 / self.symbol_rate as f32;
        self.freq = (self.freq + CARRIER_LOOP_KI * e).clamp(-limit, limit);
    }

    /// Frames with pilots: the carrier phase measured on every known
    /// stretch -- this header, each pilot block, and the next header --
    /// unwrapped and interpolated between them. Decision-directed tracking
    /// slipped by whole constellation steps on 8PSK (one frame in two
    /// failed at 8.5 dB in the synthetic test); the pilots can't slip.
    /// Also trims the carrier frequency from the phase's overall slope --
    /// which needs the frequency already within ~500 Hz (at 1.5 MS/s) so
    /// the phase moves less than pi between pilot blocks; see
    /// track_frequency.
    fn pilot_phase(&mut self, symbols: &mut [Cf], pls: PlsCode, next: Option<(&[Cf], PlsCode, f32)>) {
        let pilot = Cf::new(FRAC_1_SQRT_2, FRAC_1_SQRT_2);

        // (centre position, phase) of each known stretch.
        let mut anchors: Vec<(f32, f32)> = Vec::new();
        let header = plheader::header_symbols(pls);
        let acc =
            symbols[..HEADER_SYMBOLS].iter().zip(header.iter()).fold(Cf::default(), |a, (s, r)| a + *s * r.conj());
        anchors.push((HEADER_SYMBOLS as f32 / 2.0, acc.arg()));
        let mut block = 0;
        loop {
            let start = HEADER_SYMBOLS + 1440 * (block + 1) + 36 * block;
            if start + 36 > symbols.len() {
                break;
            }
            let mut acc = Cf::default();
            for k in start..start + 36 {
                acc += symbols[k] * pilot.rot90(self.scrambling[k - HEADER_SYMBOLS]).conj();
            }
            anchors.push((start as f32 + 18.0, acc.arg()));
            block += 1;
        }
        // Anchors 1476 symbols apart alias every Rs/1476 (~1 kHz at 1.5
        // MS/s), so unwrap around the rotation the closing header measured
        // (`prior`), not around zero -- around zero a 600 Hz error read as
        // -400 Hz and fought the header's own correction.
        //
        // The header's measurement is noisy (~250 Hz), so it's weighted
        // against how unsure the frequency still is: fully just after
        // lock, hardly at all once the pilots have pulled it in -- a lone
        // header otherwise knocked one short 8PSK frame in seven into the
        // wrong alias.
        let (sr2, sh2) = (self.residual_sigma.powi(2), HEADER_ROTATION_SIGMA.powi(2));
        let prior = next.map_or(0.0, |n| n.2) * sr2 / (sr2 + sh2);
        if let Some((next_symbols, next_pls, _)) = next {
            let reference = plheader::header_symbols(next_pls);
            let acc = next_symbols.iter().zip(reference.iter()).fold(Cf::default(), |a, (s, r)| a + *s * r.conj());
            anchors.push((symbols.len() as f32 + HEADER_SYMBOLS as f32 / 2.0, acc.arg()));
        }
        for i in 1..anchors.len() {
            let predicted = prior * (anchors[i].0 - anchors[i - 1].0);
            let mut d = anchors[i].1 - anchors[i - 1].1 - predicted;
            while d > PI {
                d -= 2.0 * PI;
            }
            while d < -PI {
                d += 2.0 * PI;
            }
            anchors[i].1 = anchors[i - 1].1 + predicted + d;
        }
        if anchors.len() >= 2 {
            let (t0, p0) = anchors[0];
            let (t1, p1) = anchors[anchors.len() - 1];
            let slope = (p1 - p0) / (t1 - t0);
            // Same bound as track_frequency's: beyond it the pilots
            // weren't really there or were lost in noise. REAL RECORDING
            // (QO-100, a 250 kS/s QPSK station without pilots at ~2.5
            // dB): a header misread as having pilots trimmed the carrier
            // from "pilots" that were data, dragging it to the edge of
            // its range.
            if slope.abs() <= 12.0 * HEADER_ROTATION_SIGMA {
                self.freq += 0.5 * slope;
                self.residual_sigma = (self.residual_sigma * 0.6).max(RESIDUAL_SIGMA_FLOOR);
            }
        }
        let mut a = 0;
        for (k, s) in symbols.iter_mut().enumerate() {
            let t = k as f32;
            while a + 1 < anchors.len() - 1 && t > anchors[a + 1].0 {
                a += 1;
            }
            let phase = if anchors.len() == 1 {
                anchors[0].1
            } else {
                let (t0, p0) = anchors[a];
                let (t1, p1) = anchors[a + 1];
                p0 + (p1 - p0) * ((t - t0) / (t1 - t0)).clamp(0.0, 1.0)
            };
            *s = *s * Cf::from_polar(1.0, -phase);
        }
    }

    /// Descrambles, removes pilots, measures SNR and makes LLRs.
    fn finish_frame(&mut self, symbols: &mut [Cf], next: Option<(&[Cf], PlsCode, f32)>) -> Option<Frame> {
        let pls = self.pls?;
        let (modulation, _) = pls.modcod()?;
        if pls.pilots {
            self.pilot_phase(symbols, pls, next);
        }
        let data = &symbols[HEADER_SYMBOLS..];
        let mut out = Vec::with_capacity(data.len());
        let mut slot_in_block = 0usize;
        let mut i = 0usize;
        while i < data.len() {
            // A pilot block follows every 16 slots (1440 symbols).
            if pls.pilots && slot_in_block == 16 * 90 {
                i += 36;
                slot_in_block = 0;
                continue;
            }
            out.push(data[i].rot90((4 - self.scrambling[i] % 4) % 4));
            slot_in_block += 1;
            i += 1;
        }
        let bits = modulation.bits_per_symbol();
        let n = if pls.short { 16_200 } else { 64_800 };
        out.truncate(n / bits);
        if out.len() != n / bits {
            return None;
        }
        let (llr, _) = match modulation {
            Modulation::Qpsk => qpsk_llr(&out),
            Modulation::Psk8 => psk8_llr(&out, pls),
            _ => return None,
        };
        self.status.frames += 1;
        self.status.snr_db = m2m4_snr_db(&out);
        Some(Frame { pls, llr })
    }
}

/// Es/N0 by the M2M4 moments estimator, for the status display. Needs no
/// decisions, so unlike the LLR functions' own noise figures it doesn't
/// read high near threshold: those count every wrong decision as signal,
/// and showed ~6.0 dB for a synthetic signal really at 4.6 dB, where the
/// QO-100 beacon's QPSK 4/5 is on the edge of decoding. Exact for a
/// constant-modulus constellation (QPSK, 8PSK) in Gaussian noise.
fn m2m4_snr_db(symbols: &[Cf]) -> f32 {
    let n = symbols.len().max(1) as f64;
    let (mut m2, mut m4) = (0.0f64, 0.0f64);
    for s in symbols {
        let p = s.norm_sqr() as f64;
        m2 += p;
        m4 += p * p;
    }
    let (m2, m4) = (m2 / n, m4 / n);
    let signal = (2.0 * m2 * m2 - m4).max(0.0).sqrt();
    let noise = (m2 - signal).max(1e-12);
    (10.0 * (signal.max(1e-12) / noise).log10()) as f32
}

/// QPSK (figure 9): I's sign carries the first bit, Q's the second; 0 is
/// positive. LLRs are scaled by the measured noise.
fn qpsk_llr(symbols: &[Cf]) -> (Vec<f32>, f32) {
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for s in symbols {
        let d = Cf::new(FRAC_1_SQRT_2.copysign(s.re), FRAC_1_SQRT_2.copysign(s.im));
        signal += 1.0;
        noise += (*s - d).norm_sqr() as f64;
    }
    let sigma2 = (noise / signal).max(1e-6) as f32 / 2.0;
    let snr_db = 10.0 * (1.0 / (2.0 * sigma2)).log10();
    let k = 2.0 * FRAC_1_SQRT_2 / sigma2;
    let mut llr = Vec::with_capacity(symbols.len() * 2);
    for s in symbols {
        llr.push(k * s.re);
        llr.push(k * s.im);
    }
    (llr, snr_db)
}

/// 8PSK (figure 10) max-log LLRs, then the column deinterleaver of
/// clause 5.3.3.
fn psk8_llr(symbols: &[Cf], pls: PlsCode) -> (Vec<f32>, f32) {
    // Bits (MSB first) -> angle in units of pi/4.
    const POINTS: [(u8, f32); 8] = [
        (0b000, 1.0),
        (0b001, 0.0),
        (0b010, 4.0),
        (0b011, 5.0),
        (0b100, 2.0),
        (0b101, 7.0),
        (0b110, 3.0),
        (0b111, 6.0),
    ];
    let constellation: Vec<(u8, Cf)> = POINTS.iter().map(|&(b, a)| (b, Cf::from_polar(1.0, a * PI / 4.0))).collect();
    let mut noise = 0.0f64;
    for s in symbols {
        let k = (s.arg() / (PI / 4.0)).round();
        noise += (*s - Cf::from_polar(1.0, k * PI / 4.0)).norm_sqr() as f64;
    }
    let sigma2 = (noise / symbols.len() as f64).max(1e-6) as f32;
    let snr_db = 10.0 * (1.0 / sigma2).log10();
    let mut tx_order = Vec::with_capacity(symbols.len() * 3);
    for s in symbols {
        let d: Vec<f32> = constellation.iter().map(|&(_, p)| (*s - p).norm_sqr()).collect();
        for bit in 0..3 {
            let mask = 4 >> bit;
            let (mut d0, mut d1) = (f32::MAX, f32::MAX);
            for (j, &(b, _)) in constellation.iter().enumerate() {
                if b & mask == 0 {
                    d0 = d0.min(d[j]);
                } else {
                    d1 = d1.min(d[j]);
                }
            }
            tx_order.push((d1 - d0) / sigma2);
        }
    }
    // Written column-wise into 3 columns, read row-wise: transmitted bit
    // 3r + c is code bit c*rows + r. Rate 3/5 reads the columns in
    // reverse (its MSB is read out third).
    let rows = symbols.len();
    let reversed = pls.modcod == 12;
    let mut llr = vec![0f32; tx_order.len()];
    for r in 0..rows {
        for c in 0..3 {
            let col = if reversed { 2 - c } else { c };
            llr[col * rows + r] = tx_order[3 * r + c];
        }
    }
    (llr, snr_db)
}
