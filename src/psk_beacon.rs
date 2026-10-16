/*
    QO-100 middle beacon monitor (10489.750 MHz) -- decodes the 400
    bit/s PSK beacon AMSAT-DL runs from the DK0SB ground station (old
    AO-40 telemetry format) and measures where it sits, which is what
    LNB drift correction (main.rs, "Lock LNB") works from.

    It runs on the main receiver's wideband IQ, not its audio, so it
    keeps watching the beacon whatever the main receiver is tuned to
    within the span (e.g. 768 kHz with the beacon near the middle and
    CTUN used to tune around).

    Format, confirmed against a real recording of the beacon (all four
    frames in it decoded with a good CRC) and gr-satellites' QO-100
    definition:
    - DBPSK, Manchester coded, 400 bit/s: each bit is a half-bit of
      +phase then a half-bit of -phase, and a 1 is sent as a 180-degree
      phase change from the previous bit. So detection is differential
      (no carrier phase lock needed).
    - "Uncoded AO-40" frames: 32-bit sync word 0x3915ED30, 512 data
      bytes, then a CRC-16/CCITT (init 0xFFFF) sent big-endian, so the
      CRC over the data plus CRC is 0. The data is a text page of 8
      lines x 64 characters; its first character is the page letter
      ("K HI de Qatar-OSCAR 100 (DK0SB)").
    - Between uncoded frames the beacon also sends AO-40 FEC frames
      (and fill). Those aren't decoded here.

    Pipeline:
    1. Front end (BeaconMonitor): mix the beacon's expected position in
       the IQ to 0 Hz, lowpass and decimate to 48 kHz then 9600 Hz.
    2. Finding it (BeaconMonitor::wide_search): FFT the SQUARED 48 kHz
       signal -- squaring BPSK removes the modulation and leaves a line at
       twice the carrier offset -- over +/-10 kHz, and accept the
       strongest line whose plain spectrum has the beacon's shape (a dip
       at the carrier, unlike a CW carrier). Centre the core on it.
       Tracking it (BeaconCore): a frequency-locked loop measures the
       residual four times a second, with an FFT check that the line is
       still there. Successive measurements give the drift rate, used to
       extrapolate to "now" for LNB correction.
    3. Mix the carrier to 0 Hz, lowpass, Manchester matched filter (24
       samples per bit at 9600 Hz), pick the symbol phase with the most
       matched-filter energy, and compare each bit's phase with the
       previous one.
    4. Find the sync word (up to 3 bit errors), collect 514 bytes, check
       the CRC.
*/

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

use crate::radio::IqSample;

/// The beacon's nominal downlink frequency.
pub const BEACON_RF_HZ: u64 = 10_489_750_000;
/// Rate the front end decimates to, and the core runs at.
const RATE_HZ: f64 = 9_600.0;
const MID_RATE_HZ: f64 = 48_000.0;
const SAMPLES_PER_BIT: usize = 24; // 9600 / 400 bit/s
/// The core's own search range around where the front end has centred
/// it. The squared line sits at twice the offset, and must stay inside
/// +/-4800 Hz at 9600 Hz.
const SEARCH_HZ: f64 = 2_000.0;
/// The wide search (BeaconMonitor::wide_search), run on the 48 kHz
/// stage while the core has no carrier: how far from its expected
/// position the beacon can be. A free-running LNB can easily be several
/// kHz out, especially while it warms up (the real recording drifted
/// ~5 Hz/s). The squared line must stay inside +/-24 kHz at 48 kHz.
const WIDE_SEARCH_HZ: f64 = 10_000.0;
/// Wide-search FFT: ~0.17 s at 48 kHz (5.9 Hz bins), every WIDE_HOP
/// samples (50% overlap), power-averaged over WIDE_AVERAGE windows
/// (~2 s) -- averaging is what makes a weak beacon stand out (a single
/// window's squared-spectrum line wasn't enough on a real MSI.SDR, ~10
/// dB weaker than the test recording). The beacon's lobes are hundreds
/// of Hz wide, so drift during the average doesn't blur them.
const WIDE_LEN: usize = 8_192;
const WIDE_HOP: usize = 4_096;
const WIDE_AVERAGE: f32 = 24.0;
/// Decide every ~0.5 s, once at least this many windows are averaged.
const WIDE_DECIDE_EVERY: usize = 6;
const WIDE_MIN_WINDOWS: usize = 24;
/// The weaker lobe's power density must be at least this many times the
/// median (noise floor) of the searched range.
pub const WIDE_MIN_LEVEL: f32 = 1.5;
/// And the squared spectrum (BPSK squared is a line at twice its
/// carrier) must peak within LINE_REACH_HZ of twice the candidate, this
/// many times its median -- confirming it's PSK, not, say, two CW
/// carriers 800 Hz apart.
pub const WIDE_MIN_LINE: f32 = 3.0;
const LINE_REACH_HZ: f64 = 60.0;
/// The beacon (Manchester BPSK) has a dip at its carrier with equal
/// lobes either side, peaking ~400 Hz out; a CW carrier has its power AT
/// the carrier. Mean power density within +/-60 Hz of the candidate must
/// be below this fraction of the density 250-550 Hz out on EACH side.
/// Ideal beacon: ~0.06.
pub const SHAPE_MAX_RATIO: f32 = 0.6;
/// Once tracking, if the beacon has wandered this far from where the
/// front end centres it, re-centre (the core's filters and search are
/// only +/-SEARCH_HZ wide).
const RECENTRE_HZ: f64 = 1_500.0;
/// How close to the edge of the received span the beacon may be: the
/// wide search needs +/-(WIDE_SEARCH_HZ + ~800 Hz) around it.
pub const EDGE_MARGIN_HZ: f64 = 12_000.0;
/// ~3.4 s at 9600 Hz: 0.3 Hz FFT bins, and enough averaging to find the
/// line well below the point where the frames themselves decode.
const ACQ_LEN: usize = 32_768;
const ACQ_HOP: usize = 4_800; // re-estimate every 0.5 s
/// Peak / median FFT magnitude needed to call it a carrier. Noise alone
/// gives about 3-4 over this many bins; the real beacon gave 35-57.
pub const LOCK_RATIO: f32 = 8.0;
/// Frequency-locked loop that tracks the carrier once the FFT has found
/// it: every FLL_HOP samples (0.25 s) the residual frequency is measured
/// from how far the SQUARED baseband (modulation removed) turns over
/// FLL_LAG samples -- unambiguous to +/-RATE_HZ/(4*FLL_LAG) = +/-200 Hz.
/// Far less lag and jitter than the FFT, whose ~3.4 s window smears a
/// drifting carrier into a broad hump (on a real recording, its peak
/// hopped by 10+ Hz between estimates while the beacon drifted 1-5 Hz/s).
const FLL_HOP: usize = 2_400;
const FLL_LAG: usize = 12;
const FLL_GAIN: f64 = 0.5;
/// The core's FFT keeps running to confirm the carrier is still within
/// this of where the loop says (its ~3.4 s window smears a drifting
/// carrier by up to ~20 Hz); if not, the beacon is lost and the wide
/// search takes over again.
const REACQUIRE_HZ: f64 = 30.0;
/// How old an FLL measurement is: half its window, plus the 63-tap
/// baseband filter's delay.
const MEASUREMENT_AGE_S: f64 = (FLL_HOP / 2 + 31) as f64 / RATE_HZ;
/// Weight of each new drift-rate reading in the smoothed rate.
const RATE_SMOOTHING: f64 = 0.1;
const SYNC_WORD: u32 = 0x3915_ED30;
const SYNC_MAX_ERRORS: u32 = 3;
const FRAME_BYTES: usize = 514; // 512 data + 2 CRC
pub const PAGE_LINE_CHARS: usize = 64;
const MAX_PAGES: usize = 32;
/// IqSample's full scale (24-bit) -- same as spectrum.rs's IQ_NORM.
const IQ_NORM: f32 = 8_388_608.0;

#[derive(Clone, Copy, Default, Debug)]
struct Cx {
    re: f32,
    im: f32,
}

impl Cx {
    fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    fn mul(self, o: Cx) -> Cx {
        Cx::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    fn add(self, o: Cx) -> Cx {
        Cx::new(self.re + o.re, self.im + o.im)
    }
    fn sub(self, o: Cx) -> Cx {
        Cx::new(self.re - o.re, self.im - o.im)
    }
    fn scale(self, k: f32) -> Cx {
        Cx::new(self.re * k, self.im * k)
    }
    fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

/// One decoded text page, newest version kept per page letter.
#[derive(Clone, Debug)]
pub struct BeaconPage {
    pub id: char,
    pub lines: Vec<String>,
}

/// What the UI and LNB correction read. Pages are newest first.
#[derive(Clone, Debug, Default)]
pub struct BeaconStatus {
    /// The beacon's expected position is inside the received span, so
    /// the monitor is running.
    pub in_span: bool,
    /// Measured beacon offset from its expected position (Hz), or None
    /// while it isn't found. About 0.13 s old (MEASUREMENT_AGE_S).
    pub offset_hz: Option<f64>,
    /// offset_hz extrapolated to now with the drift rate.
    pub predicted_offset_hz: Option<f64>,
    pub drift_hz_per_s: f64,
    /// While searching: the most likely candidate the wide search saw,
    /// shown in the panel to help when it isn't found.
    pub candidate: Option<SearchCandidate>,
    pub frames_ok: u32,
    pub frames_bad: u32,
    pub pages: Vec<BeaconPage>,
}

#[derive(Clone, Copy, Debug)]
pub struct SearchCandidate {
    /// Offset from the beacon's expected position (Hz).
    pub offset_hz: f64,
    /// Weaker lobe / noise floor power density (needs WIDE_MIN_LEVEL).
    pub level: f32,
    /// Squared-spectrum peak near twice the candidate / median (needs
    /// WIDE_MIN_LINE).
    pub line: f32,
    /// Centre / weaker-lobe power density (needs below SHAPE_MAX_RATIO).
    pub shape: f32,
    /// Stronger outer (1-1.4 kHz out) / weaker-lobe power density (needs
    /// below SHAPE_MAX_RATIO).
    pub outer: f32,
}

impl SearchCandidate {
    pub fn is_beacon(&self) -> bool {
        self.level >= WIDE_MIN_LEVEL
            && self.line >= WIDE_MIN_LINE
            && self.shape < SHAPE_MAX_RATIO
            && self.outer < SHAPE_MAX_RATIO
    }
}

/// Windowed-sinc (Blackman) lowpass, unity DC gain. `taps` is odd.
fn lowpass(taps: usize, cutoff_hz: f64, rate_hz: f64) -> Vec<f32> {
    let m = (taps - 1) as f64;
    let fc = cutoff_hz / rate_hz;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
            let w = 0.42 - 0.5 * (2.0 * PI * i as f64 / m).cos() + 0.08 * (4.0 * PI * i as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter_mut().for_each(|v| *v /= sum);
    h.into_iter().map(|v| v as f32).collect()
}

/// FIR over complex samples, with the history stored twice so the dot
/// product never wraps. Outputs are only computed when asked for, so a
/// decimating filter costs one dot product per OUTPUT sample.
struct ComplexFir {
    taps: Vec<f32>,
    hist: Vec<Cx>,
    pos: usize,
}

impl ComplexFir {
    fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        Self { taps, hist: vec![Cx::default(); 2 * n], pos: 0 }
    }
    fn push(&mut self, x: Cx) {
        let n = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + n] = x;
        self.pos = (self.pos + 1) % n;
    }
    fn output(&self) -> Cx {
        let n = self.taps.len();
        let window = &self.hist[self.pos..self.pos + n];
        let mut acc = Cx::default();
        for (h, x) in self.taps.iter().zip(window) {
            acc = acc.add(x.scale(*h));
        }
        acc
    }
    fn reset(&mut self) {
        self.hist.iter_mut().for_each(|v| *v = Cx::default());
        self.pos = 0;
    }
}

/// FIR lowpass + decimate-by-`factor`.
struct Decimator {
    fir: ComplexFir,
    factor: usize,
    phase: usize,
}

impl Decimator {
    fn new(taps: Vec<f32>, factor: usize) -> Self {
        Self { fir: ComplexFir::new(taps), factor, phase: 0 }
    }
    fn push(&mut self, x: Cx) -> Option<Cx> {
        self.fir.push(x);
        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;
        Some(self.fir.output())
    }
    fn reset(&mut self) {
        self.fir.reset();
        self.phase = 0;
    }
}

/// Phase-continuous complex oscillator (exp(-j*2*pi*f*t)), renormalised
/// now and then so rounding can't make it grow or shrink.
struct Nco {
    phasor: Cx,
    step: Cx,
    count: u32,
}

impl Nco {
    fn new() -> Self {
        Self { phasor: Cx::new(1.0, 0.0), step: Cx::new(1.0, 0.0), count: 0 }
    }
    fn set_freq(&mut self, freq_hz: f64, rate_hz: f64) {
        let w = -2.0 * PI * freq_hz / rate_hz;
        self.step = Cx::new(w.cos() as f32, w.sin() as f32);
    }
    fn next(&mut self) -> Cx {
        let out = self.phasor;
        self.phasor = self.phasor.mul(self.step);
        self.count += 1;
        if self.count >= 1024 {
            self.count = 0;
            let mag = self.phasor.norm_sqr().sqrt();
            self.phasor = self.phasor.scale(1.0 / mag);
        }
        out
    }
}

/// In-place iterative radix-2 FFT; `buf.len()` must be a power of two.
fn fft(buf: &mut [Cx]) {
    let n = buf.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            buf.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let w = -2.0 * PI / len as f64;
        let wl = Cx::new(w.cos() as f32, w.sin() as f32);
        for start in (0..n).step_by(len) {
            let mut wk = Cx::new(1.0, 0.0);
            for k in 0..len / 2 {
                let a = buf[start + k];
                let b = buf[start + k + len / 2].mul(wk);
                buf[start + k] = a.add(b);
                buf[start + k + len / 2] = a.sub(b);
                wk = wk.mul(wl);
            }
        }
        len <<= 1;
    }
}

/// CRC-16/CCITT, MSB first. The AO-40 frame's own CRC is sent
/// big-endian after the data, so over data + CRC this comes out 0.
fn crc16_ccitt(data: &[u8], init: u16) -> u16 {
    let mut crc = init;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Carrier estimation, demodulation and framing, on complex baseband
/// at RATE_HZ with the beacon expected at 0 Hz.
struct BeaconCore {
    status: Arc<Mutex<BeaconStatus>>,
    /// Where the front end has centred the core, relative to the
    /// beacon's expected position -- added to everything it publishes.
    base_hz: f64,
    acq: Vec<Cx>,
    acq_pos: usize,
    acq_filled: bool,
    acq_since: usize,
    window: Vec<f32>,
    carrier_hz: Option<f64>,
    last_estimate: Option<f64>,
    rate_hz_per_s: f64,
    carrier_nco: Nco,
    fll_hist: [Cx; FLL_LAG],
    fll_pos: usize,
    fll_acc: (f64, f64),
    fll_count: usize,
    fir_bb: ComplexFir,
    mf: Vec<Cx>,
    mf_pos: usize,
    sample_count: usize,
    energy: [f32; SAMPLES_PER_BIT],
    symbol_phase: usize,
    prev_symbol: Option<Cx>,
    shift: u32,
    frame: Option<Vec<u8>>,
    byte: u8,
    byte_bits: u8,
}

impl BeaconCore {
    fn new(status: Arc<Mutex<BeaconStatus>>) -> Self {
        let window = (0..ACQ_LEN)
            .map(|i| (0.5 - 0.5 * (2.0 * PI * i as f64 / ACQ_LEN as f64).cos()) as f32)
            .collect();
        Self {
            status,
            base_hz: 0.0,
            acq: vec![Cx::default(); ACQ_LEN],
            acq_pos: 0,
            acq_filled: false,
            acq_since: 0,
            window,
            carrier_hz: None,
            last_estimate: None,
            rate_hz_per_s: 0.0,
            carrier_nco: Nco::new(),
            fll_hist: [Cx::default(); FLL_LAG],
            fll_pos: 0,
            fll_acc: (0.0, 0.0),
            fll_count: 0,
            // Manchester main lobes reach ~+/-800 Hz.
            fir_bb: ComplexFir::new(lowpass(63, 1000.0, RATE_HZ)),
            mf: vec![Cx::default(); SAMPLES_PER_BIT],
            mf_pos: 0,
            sample_count: 0,
            energy: [0.0; SAMPLES_PER_BIT],
            symbol_phase: 0,
            prev_symbol: None,
            shift: 0,
            frame: None,
            byte: 0,
            byte_bits: 0,
        }
    }

    /// Drops all signal state (not the decoded pages or counts).
    fn reset(&mut self) {
        self.acq.iter_mut().for_each(|v| *v = Cx::default());
        self.acq_pos = 0;
        self.acq_filled = false;
        self.acq_since = 0;
        self.lose_carrier();
    }

    fn lose_carrier(&mut self) {
        self.carrier_hz = None;
        self.last_estimate = None;
        self.rate_hz_per_s = 0.0;
        self.reset_demod();
        let mut st = self.status.lock().unwrap();
        st.offset_hz = None;
        st.predicted_offset_hz = None;
        st.drift_hz_per_s = 0.0;
    }

    fn reset_demod(&mut self) {
        self.fir_bb.reset();
        self.fll_hist = [Cx::default(); FLL_LAG];
        self.fll_pos = 0;
        self.fll_acc = (0.0, 0.0);
        self.fll_count = 0;
        self.mf.iter_mut().for_each(|v| *v = Cx::default());
        self.mf_pos = 0;
        self.energy = [0.0; SAMPLES_PER_BIT];
        self.prev_symbol = None;
        self.shift = 0;
        self.frame = None;
    }

    fn process(&mut self, z: Cx) {
        self.acq[self.acq_pos] = z;
        self.acq_pos = (self.acq_pos + 1) % ACQ_LEN;
        if self.acq_pos == 0 {
            self.acq_filled = true;
        }
        self.acq_since += 1;
        if self.acq_filled && self.acq_since >= ACQ_HOP {
            self.acq_since = 0;
            self.estimate_carrier();
        }
        if self.carrier_hz.is_some() {
            self.demod_sample(z);
        }
    }

    /// Starts tracking a carrier the wide search found, `hz` from the
    /// core's centre.
    fn acquire(&mut self, hz: f64) {
        self.reset_demod();
        self.carrier_nco.set_freq(hz, RATE_HZ);
        self.carrier_hz = Some(hz);
        self.last_estimate = None;
        self.rate_hz_per_s = 0.0;
        self.publish(hz);
    }

    /// While tracking: checks the beacon is still where the loop says,
    /// by looking for its squared-spectrum line near there. Only near
    /// there -- a stronger line elsewhere (a CW carrier squares into one
    /// too) must not take over; finding the beacon is the wide search's
    /// job, which checks its shape.
    fn estimate_carrier(&mut self) {
        let Some(carrier) = self.carrier_hz else { return };
        let mut buf: Vec<Cx> = (0..ACQ_LEN)
            .map(|i| {
                let z = self.acq[(self.acq_pos + i) % ACQ_LEN];
                z.mul(z).scale(self.window[i])
            })
            .collect();
        fft(&mut buf);
        let bin_hz = RATE_HZ / ACQ_LEN as f64;
        let max_bin = (2.0 * SEARCH_HZ / bin_hz).round() as i64;
        let mag = |b: i64| buf[b.rem_euclid(ACQ_LEN as i64) as usize].norm_sqr().sqrt();
        let mut mags: Vec<f32> = (-max_bin..=max_bin).map(mag).collect();
        let mid = mags.len() / 2;
        let median = *mags.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1;
        // The line is at twice the carrier; the window's ~3.4 s smears a
        // drifting one, hence the margin.
        let centre = (2.0 * carrier / bin_hz).round() as i64;
        let reach = (2.0 * REACQUIRE_HZ / bin_hz).round() as i64;
        let peak = (centre - reach..=centre + reach).map(mag).fold(0.0f32, f32::max);
        if !(median > 0.0 && peak / median >= LOCK_RATIO) {
            self.lose_carrier();
        }
    }

    /// One frequency-locked-loop step: `residual_hz` is how far the
    /// carrier is from the NCO, over the last FLL_HOP samples.
    fn fll_update(&mut self, residual_hz: f64) {
        let Some(nco_hz) = self.carrier_hz else { return };
        let measured = nco_hz + residual_hz;
        if let Some(prev) = self.last_estimate {
            let rate = (measured - prev) * RATE_HZ / FLL_HOP as f64;
            self.rate_hz_per_s += RATE_SMOOTHING * (rate - self.rate_hz_per_s);
        }
        self.last_estimate = Some(measured);
        let new_nco = nco_hz + FLL_GAIN * residual_hz;
        self.carrier_nco.set_freq(new_nco, RATE_HZ);
        self.carrier_hz = Some(new_nco);
        self.publish(measured);
    }

    fn publish(&self, measured_hz: f64) {
        let mut st = self.status.lock().unwrap();
        let hz = self.base_hz + measured_hz;
        st.offset_hz = Some(hz);
        st.predicted_offset_hz = Some(hz + self.rate_hz_per_s * MEASUREMENT_AGE_S);
        st.drift_hz_per_s = self.rate_hz_per_s;
        st.candidate = None;
    }

    fn demod_sample(&mut self, z: Cx) {
        let bb = z.mul(self.carrier_nco.next());
        self.fir_bb.push(bb);
        let w = self.fir_bb.output();

        // FLL: squaring removes the +/- phase of the bits and Manchester
        // halves, leaving a tone at twice the residual frequency.
        let sq = w.mul(w);
        let lagged = self.fll_hist[self.fll_pos];
        self.fll_hist[self.fll_pos] = sq;
        self.fll_pos = (self.fll_pos + 1) % FLL_LAG;
        // sq * conj(lagged)
        self.fll_acc.0 += (sq.re * lagged.re + sq.im * lagged.im) as f64;
        self.fll_acc.1 += (sq.im * lagged.re - sq.re * lagged.im) as f64;
        self.fll_count += 1;
        if self.fll_count >= FLL_HOP {
            let turn = self.fll_acc.1.atan2(self.fll_acc.0);
            self.fll_acc = (0.0, 0.0);
            self.fll_count = 0;
            self.fll_update(turn / (2.0 * PI * 2.0 * FLL_LAG as f64 / RATE_HZ));
        }

        self.mf[self.mf_pos] = w;
        self.mf_pos = (self.mf_pos + 1) % SAMPLES_PER_BIT;
        // Manchester matched filter: older half-bit minus newer half-bit.
        let half = SAMPLES_PER_BIT / 2;
        let mut out = Cx::default();
        for i in 0..SAMPLES_PER_BIT {
            let v = self.mf[(self.mf_pos + i) % SAMPLES_PER_BIT];
            out = if i < half { out.add(v) } else { out.sub(v) };
        }

        // Symbol timing: the sample phase (within a bit) with the most
        // matched-filter energy, averaged over ~32 bits. Switching only
        // on a clear winner avoids flapping between neighbours. The
        // beacon and receiver clocks agree to a tiny fraction of a
        // sample per second, so no finer tracking is needed.
        let phase = self.sample_count % SAMPLES_PER_BIT;
        self.sample_count = self.sample_count.wrapping_add(1);
        self.energy[phase] += (out.norm_sqr() - self.energy[phase]) / 32.0;
        if phase == SAMPLES_PER_BIT - 1 {
            let best = (0..SAMPLES_PER_BIT).max_by(|&a, &b| self.energy[a].total_cmp(&self.energy[b])).unwrap();
            if self.energy[best] > 1.2 * self.energy[self.symbol_phase] {
                self.symbol_phase = best;
            }
        }
        if phase != self.symbol_phase {
            return;
        }
        if let Some(prev) = self.prev_symbol {
            let d = out.re * prev.re + out.im * prev.im;
            self.push_bit(d < 0.0);
        }
        self.prev_symbol = Some(out);
    }

    fn push_bit(&mut self, bit: bool) {
        self.shift = (self.shift << 1) | bit as u32;
        let Some(frame) = self.frame.as_mut() else {
            if (self.shift ^ SYNC_WORD).count_ones() <= SYNC_MAX_ERRORS {
                self.frame = Some(Vec::with_capacity(FRAME_BYTES));
                self.byte = 0;
                self.byte_bits = 0;
            }
            return;
        };
        self.byte = (self.byte << 1) | bit as u8;
        self.byte_bits += 1;
        if self.byte_bits < 8 {
            return;
        }
        frame.push(self.byte);
        self.byte = 0;
        self.byte_bits = 0;
        if frame.len() < FRAME_BYTES {
            return;
        }
        let frame = self.frame.take().unwrap();
        // So the frame's last bits can't be taken as the next sync.
        self.shift = 0;
        let mut status = self.status.lock().unwrap();
        if crc16_ccitt(&frame, 0xFFFF) != 0 {
            status.frames_bad += 1;
            return;
        }
        status.frames_ok += 1;
        let data = &frame[..512];
        let id = data[0] as char;
        let lines = data
            .chunks(PAGE_LINE_CHARS)
            .map(|line| {
                let s: String = line.iter().map(|&c| if (32..127).contains(&c) { c as char } else { ' ' }).collect();
                s.trim_end().to_string()
            })
            .collect();
        status.pages.retain(|p| p.id != id);
        status.pages.insert(0, BeaconPage { id, lines });
        status.pages.truncate(MAX_PAGES);
    }
}

/// The beacon monitor: wideband IQ in, BeaconStatus out.
pub struct BeaconMonitor {
    status: Arc<Mutex<BeaconStatus>>,
    core: BeaconCore,
    input_rate_hz: f64,
    nco: Nco,
    /// Expected beacon offset from the IQ centre, from the caller.
    expected_hz: Option<f64>,
    /// Where the beacon was last found relative to expected_hz (the LNB
    /// error, roughly); the front end centres on expected + this.
    coarse_hz: f64,
    /// input rate -> 48 kHz (None when the input already is 48 kHz).
    to_mid: Option<Decimator>,
    /// 48 kHz -> 9600 Hz.
    to_core: Decimator,
    wide: Vec<Cx>,
    wide_pos: usize,
    wide_filled: bool,
    wide_since: usize,
    wide_window: Vec<f32>,
    /// Power-averaged plain and squared spectra (FFT bin order).
    plain_avg: Vec<f32>,
    sq_avg: Vec<f32>,
    wide_windows: usize,
}

impl BeaconMonitor {
    pub fn new(status: Arc<Mutex<BeaconStatus>>) -> Self {
        let wide_window = (0..WIDE_LEN)
            .map(|i| (0.5 - 0.5 * (2.0 * PI * i as f64 / WIDE_LEN as f64).cos()) as f32)
            .collect();
        Self {
            core: BeaconCore::new(Arc::clone(&status)),
            status,
            input_rate_hz: 0.0,
            nco: Nco::new(),
            expected_hz: None,
            coarse_hz: 0.0,
            to_mid: None,
            // Passes the core's search range plus the signal's own
            // +/-800 Hz; stops by 6400 Hz, where aliases into it start.
            to_core: Decimator::new(lowpass(95, 3200.0, MID_RATE_HZ), 5),
            wide: vec![Cx::default(); WIDE_LEN],
            wide_pos: 0,
            wide_filled: false,
            wide_since: 0,
            wide_window,
            plain_avg: vec![0.0; WIDE_LEN],
            sq_avg: vec![0.0; WIDE_LEN],
            wide_windows: 0,
        }
    }

    /// Feeds one block of the receiver's IQ. `beacon_offset_hz` is where
    /// the beacon is expected relative to the IQ's centre frequency, or
    /// None to idle (decoder off, or the beacon isn't in the span).
    pub fn process(&mut self, iq: &[IqSample], input_rate_hz: f64, beacon_offset_hz: Option<f64>) {
        let Some(expected) = beacon_offset_hz else {
            if self.expected_hz.is_some() {
                self.expected_hz = None;
                self.restart();
                self.status.lock().unwrap().in_span = false;
            }
            return;
        };
        if input_rate_hz != self.input_rate_hz {
            self.set_input_rate(input_rate_hz);
        }
        // A big jump (retuned, another transverter) invalidates the
        // search windows; small moves are just followed. coarse_hz is
        // the LNB's error, which retuning doesn't change, so it's kept.
        if self.expected_hz.is_none_or(|prev| (prev - expected).abs() > SEARCH_HZ) {
            self.restart();
            self.status.lock().unwrap().in_span = true;
        }
        self.expected_hz = Some(expected);
        self.nco.set_freq(expected + self.coarse_hz, input_rate_hz);
        for s in iq {
            // q is the real part: WDSP's analyzer (analyzer.c Spectrum0)
            // takes the second value of each pair spectrum.rs passes it --
            // IqSample::q -- as I, and the spectrum display, which is the
            // right way round, is built from that. Reading i + jq here
            // instead mirrors the spectrum, which got the beacon's offset,
            // and so the LNB correction, the wrong way round on a real
            // MSI.SDR.
            let x = Cx::new(s.q as f32 / IQ_NORM, s.i as f32 / IQ_NORM).mul(self.nco.next());
            let mid = match self.to_mid.as_mut() {
                Some(d) => match d.push(x) {
                    Some(v) => v,
                    None => continue,
                },
                None => x,
            };
            if self.core.carrier_hz.is_none() {
                self.wide[self.wide_pos] = mid;
                self.wide_pos = (self.wide_pos + 1) % WIDE_LEN;
                if self.wide_pos == 0 {
                    self.wide_filled = true;
                }
                self.wide_since += 1;
                if self.wide_filled && self.wide_since >= WIDE_HOP {
                    self.wide_since = 0;
                    self.wide_accumulate();
                    if self.wide_windows >= WIDE_MIN_WINDOWS && self.wide_windows % WIDE_DECIDE_EVERY == 0 {
                        self.wide_search();
                    }
                }
            }
            if let Some(z) = self.to_core.push(mid) {
                self.core.process(z);
                // Drifted towards the edge of the core's range: re-centre.
                if let Some(c) = self.core.carrier_hz.filter(|c| c.abs() > RECENTRE_HZ) {
                    self.recentre(self.coarse_hz + c);
                }
            }
        }
    }

    /// Re-centres the front end on `coarse_hz` (relative to the expected
    /// position) and starts the core afresh there.
    fn recentre(&mut self, coarse_hz: f64) {
        self.coarse_hz = coarse_hz;
        if let Some(expected) = self.expected_hz {
            self.nco.set_freq(expected + coarse_hz, self.input_rate_hz);
        }
        self.restart();
    }

    fn restart(&mut self) {
        self.to_core.reset();
        self.core.reset();
        self.core.base_hz = self.coarse_hz;
        self.wide_pos = 0;
        self.wide_filled = false;
        self.wide_since = 0;
        self.plain_avg.iter_mut().for_each(|v| *v = 0.0);
        self.sq_avg.iter_mut().for_each(|v| *v = 0.0);
        self.wide_windows = 0;
    }

    /// Adds the latest WIDE_LEN samples' plain and squared power spectra
    /// to the running averages.
    fn wide_accumulate(&mut self) {
        let ordered = (0..WIDE_LEN).map(|i| self.wide[(self.wide_pos + i) % WIDE_LEN]);
        let mut plain: Vec<Cx> = ordered.clone().zip(&self.wide_window).map(|(z, w)| z.scale(*w)).collect();
        let mut sq: Vec<Cx> = ordered.zip(&self.wide_window).map(|(z, w)| z.mul(z).scale(*w)).collect();
        fft(&mut plain);
        fft(&mut sq);
        self.wide_windows += 1;
        // Plain mean until WIDE_AVERAGE windows, then exponential.
        let k = 1.0 / (self.wide_windows as f32).min(WIDE_AVERAGE);
        for (avg, v) in self.plain_avg.iter_mut().zip(&plain) {
            *avg += k * (v.norm_sqr() - *avg);
        }
        for (avg, v) in self.sq_avg.iter_mut().zip(&sq) {
            *avg += k * (v.norm_sqr() - *avg);
        }
    }

    /// Looks for the beacon over +/-WIDE_SEARCH_HZ in the averaged 48 kHz
    /// spectra: the beacon (Manchester BPSK) has two equal lobes ~400 Hz
    /// either side of a dip at its carrier, standing well above the
    /// spectrum further out -- unlike a CW carrier (power at the centre)
    /// or SSB (one-sided, no dip). The best such shape must also have a
    /// squared-spectrum line at twice its centre. If found, centres the
    /// core on that line and starts tracking.
    fn wide_search(&mut self) {
        let bin_hz = MID_RATE_HZ / WIDE_LEN as f64;
        let n = WIDE_LEN as i64;
        let hz_bins = |hz: f64| (hz / bin_hz).round() as i64;
        // Plain spectrum from -reach to +reach bins, with prefix sums.
        let reach = hz_bins(WIDE_SEARCH_HZ + 1_500.0);
        let plain: Vec<f32> = (-reach..=reach).map(|b| self.plain_avg[b.rem_euclid(n) as usize]).collect();
        let mut prefix = vec![0.0f64; plain.len() + 1];
        for (i, v) in plain.iter().enumerate() {
            prefix[i + 1] = prefix[i] + *v as f64;
        }
        // Mean density over bins centre+lo..=centre+hi (bin offsets).
        let density = |centre: i64, lo: i64, hi: i64| -> f32 {
            let a = (centre + lo + reach) as usize;
            let b = (centre + hi + reach) as usize;
            ((prefix[b + 1] - prefix[a]) / (b - a + 1) as f64) as f32
        };
        let mut sorted = plain.clone();
        let mid = sorted.len() / 2;
        let floor = *sorted.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1;
        if floor <= 0.0 {
            return;
        }
        let (c60, l250, l550, o1000, o1400) =
            (hz_bins(60.0), hz_bins(250.0), hz_bins(550.0), hz_bins(1_000.0), hz_bins(1_400.0));

        // Best beacon-shaped spot: highest weaker-lobe level among those
        // with the dip and the drop-off further out.
        let mut best: Option<(i64, f32, f32, f32)> = None; // (bin, level, shape, outer)
        let mut best_any: Option<(i64, f32, f32, f32)> = None;
        let max_bin = hz_bins(WIDE_SEARCH_HZ);
        for c in -max_bin..=max_bin {
            let lobes = density(c, -l550, -l250).min(density(c, l250, l550));
            if lobes <= 0.0 {
                continue;
            }
            let level = lobes / floor;
            let shape = density(c, -c60, c60) / lobes;
            let outer = density(c, -o1400, -o1000).max(density(c, o1000, o1400)) / lobes;
            let entry = (c, level, shape, outer);
            if shape < SHAPE_MAX_RATIO && outer < SHAPE_MAX_RATIO {
                if best.is_none_or(|b| level > b.1) {
                    best = Some(entry);
                }
            } else if best_any.is_none_or(|b| level > b.1) {
                best_any = Some(entry);
            }
        }
        let Some((c, level, shape, outer)) = best.or(best_any) else { return };

        // Squared-spectrum line near twice the candidate.
        let sq_reach = hz_bins(2.0 * (WIDE_SEARCH_HZ + LINE_REACH_HZ));
        let mut sq: Vec<f32> = (-sq_reach..=sq_reach).map(|b| self.sq_avg[b.rem_euclid(n) as usize]).collect();
        let mid = sq.len() / 2;
        let sq_median = *sq.select_nth_unstable_by(mid, |a, b| a.total_cmp(b)).1;
        let line_reach = hz_bins(2.0 * LINE_REACH_HZ);
        let (line_bin, line_power) = (2 * c - line_reach..=2 * c + line_reach)
            .map(|b| (b, self.sq_avg[b.rem_euclid(n) as usize]))
            .fold((2 * c, 0.0f32), |acc, x| if x.1 > acc.1 { x } else { acc });
        let line = if sq_median > 0.0 { line_power / sq_median } else { 0.0 };

        let cand = SearchCandidate {
            offset_hz: self.coarse_hz + line_bin as f64 * bin_hz / 2.0,
            level,
            line,
            shape,
            outer,
        };
        self.status.lock().unwrap().candidate = Some(cand);
        if cand.is_beacon() {
            // Centre the core on the line (carrier) and start tracking.
            self.recentre(cand.offset_hz);
            self.core.acquire(0.0);
        }
    }

    fn set_input_rate(&mut self, rate_hz: f64) {
        self.input_rate_hz = rate_hz;
        let factor = (rate_hz / MID_RATE_HZ).round().max(1.0) as usize;
        // Passband to the wide search's +/-~11 kHz; anything that would
        // alias into that (from 37 kHz up) is in the stopband.
        self.to_mid = (factor > 1).then(|| {
            let taps = ((5.5 * rate_hz / 25_000.0).ceil() as usize) | 1;
            Decimator::new(lowpass(taps, 24_000.0, rate_hz), factor)
        });
        self.restart();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple deterministic noise (no rand crate here).
    struct Lcg(u64);
    impl Lcg {
        fn uniform(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }
        fn gauss(&mut self) -> f64 {
            let (u1, u2) = (self.uniform().max(1e-12), self.uniform());
            (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
        }
    }

    fn frame_bytes(page: char, text: &str) -> Vec<u8> {
        let mut data = vec![b' '; 512];
        data[0] = page as u8;
        for (i, b) in text.bytes().enumerate() {
            data[2 + i] = b;
        }
        let crc = crc16_ccitt(&data, 0xFFFF);
        let mut out = SYNC_WORD.to_be_bytes().to_vec();
        out.extend_from_slice(&data);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Fill, two frames, fill -- like the real beacon. 5 s of fill
    /// first so the carrier is found before the first frame.
    fn test_bytes() -> Vec<u8> {
        let mut bytes = vec![0x50u8; 250];
        bytes.extend(frame_bytes('L', "HI de Qatar-OSCAR 100 (DK0SB)"));
        bytes.extend(vec![0x50u8; 100]);
        bytes.extend(frame_bytes('M', "Second page"));
        bytes.extend(vec![0x50u8; 20]);
        bytes
    }

    /// The beacon's modulation as complex IQ at `rate`: DBPSK +
    /// Manchester at 400 bit/s on a carrier at `start_hz` drifting at
    /// `drift`, plus white noise. Returns the samples and the carrier
    /// frequency at the end.
    fn modulate_iq(bytes: &[u8], rate: f64, start_hz: f64, drift: f64, noise: f64) -> (Vec<IqSample>, f64) {
        let spb = (rate / 400.0) as usize;
        let mut rng = Lcg(12345);
        let (mut sign, mut phase) = (1.0f64, 0.0f64);
        let mut out = Vec::new();
        let scale = IQ_NORM as f64 * 0.1;
        let mut freq = start_hz;
        for &byte in bytes {
            for k in (0..8).rev() {
                if (byte >> k) & 1 == 1 {
                    sign = -sign;
                }
                for i in 0..spb {
                    freq = start_hz + drift * out.len() as f64 / rate;
                    phase += 2.0 * PI * freq / rate;
                    let chip = if i < spb / 2 { sign } else { -sign };
                    // Real part in q -- see BeaconMonitor::process.
                    out.push(IqSample {
                        q: (scale * (chip * phase.cos() + noise * rng.gauss())) as i32,
                        i: (scale * (chip * phase.sin() + noise * rng.gauss())) as i32,
                    });
                }
            }
        }
        (out, freq)
    }

    #[test]
    fn crc_of_frame_with_crc_is_zero() {
        let f = frame_bytes('L', "HI de test");
        assert_eq!(crc16_ccitt(&f[4..], 0xFFFF), 0);
    }

    fn run_monitor(rate: f64, expected_offset: f64, actual_start: f64, drift: f64, noise: f64) -> (BeaconStatus, f64) {
        let (iq, end_freq) = modulate_iq(&test_bytes(), rate, actual_start, drift, noise);
        let status = Arc::new(Mutex::new(BeaconStatus::default()));
        let mut mon = BeaconMonitor::new(Arc::clone(&status));
        for block in iq.chunks(2048) {
            mon.process(block, rate, Some(expected_offset));
        }
        let st = status.lock().unwrap().clone();
        (st, end_freq - expected_offset)
    }

    #[test]
    fn decodes_and_measures_beacon_in_768k_iq() {
        // Beacon expected 120 kHz above the centre, actually 650 Hz
        // higher and drifting at 5 Hz/s (as in the real recording).
        let (st, true_offset_now) = run_monitor(768_000.0, 120_000.0, 120_650.0, 5.0, 2.0);
        assert!(st.in_span);
        assert_eq!(st.frames_ok, 2, "{st:?}");
        assert_eq!(st.frames_bad, 0);
        assert_eq!(st.pages[0].id, 'M');
        assert_eq!(st.pages[1].lines[0], "L HI de Qatar-OSCAR 100 (DK0SB)");
        let predicted = st.predicted_offset_hz.expect("carrier found");
        assert!((predicted - true_offset_now).abs() < 3.0, "predicted {predicted}, true {true_offset_now}");
        assert!((st.drift_hz_per_s - 5.0).abs() < 1.0, "drift {}", st.drift_hz_per_s);
    }

    #[test]
    fn decodes_at_48k_with_negative_offset() {
        let (st, true_offset_now) = run_monitor(48_000.0, -9_000.0, -10_200.0, -3.0, 0.5);
        assert_eq!(st.frames_ok, 2, "{st:?}");
        let predicted = st.predicted_offset_hz.expect("carrier found");
        assert!((predicted - true_offset_now).abs() < 3.0, "predicted {predicted}, true {true_offset_now}");
    }

    #[test]
    fn wide_search_finds_beacon_far_off_past_a_stronger_cw_carrier() {
        // Beacon 6 kHz above where it's expected (a free-running LNB),
        // and a CW carrier twice as strong 3 kHz above.
        let rate = 768_000.0;
        let (mut iq, end_freq) = modulate_iq(&test_bytes(), rate, 126_000.0, 2.0, 2.0);
        let scale = IQ_NORM as f64 * 0.1;
        for (n, s) in iq.iter_mut().enumerate() {
            let ph = 2.0 * PI * 123_000.0 * n as f64 / rate;
            s.q += (scale * 2.0 * ph.cos()) as i32;
            s.i += (scale * 2.0 * ph.sin()) as i32;
        }
        let status = Arc::new(Mutex::new(BeaconStatus::default()));
        let mut mon = BeaconMonitor::new(Arc::clone(&status));
        for block in iq.chunks(2048) {
            mon.process(block, rate, Some(120_000.0));
        }
        let st = status.lock().unwrap();
        assert_eq!(st.frames_ok, 2, "{st:?}");
        let predicted = st.predicted_offset_hz.expect("carrier found");
        let true_offset_now = end_freq - 120_000.0;
        assert!((predicted - true_offset_now).abs() < 3.0, "predicted {predicted}, true {true_offset_now}");
    }

    /// A beacon as weak as on a real MSI.SDR at 768 kHz (~9 dB above the
    /// noise on the spectrum display, ~10 dB weaker than the test
    /// recording), 4 kHz off, with CW carriers nearby.
    #[test]
    fn finds_weak_beacon_among_cw_carriers() {
        let rate = 768_000.0;
        let (mut iq, end_freq) = modulate_iq(&test_bytes(), rate, 124_000.0, 3.0, 5.0);
        let scale = IQ_NORM as f64 * 0.1;
        for (n, s) in iq.iter_mut().enumerate() {
            for (hz, amp) in [(128_600.0, 3.0), (116_000.0, 2.0), (119_000.0, 1.0)] {
                let ph = 2.0 * PI * hz * n as f64 / rate;
                s.q += (scale * amp * ph.cos()) as i32;
                s.i += (scale * amp * ph.sin()) as i32;
            }
        }
        let status = Arc::new(Mutex::new(BeaconStatus::default()));
        let mut mon = BeaconMonitor::new(Arc::clone(&status));
        for block in iq.chunks(2048) {
            mon.process(block, rate, Some(120_000.0));
        }
        let st = status.lock().unwrap();
        assert_eq!(st.frames_ok, 2, "{st:?}");
        let predicted = st.predicted_offset_hz.expect("carrier found");
        let true_offset_now = end_freq - 120_000.0;
        assert!((predicted - true_offset_now).abs() < 3.0, "predicted {predicted}, true {true_offset_now}");
    }

    #[test]
    fn no_carrier_in_noise_alone() {
        let status = Arc::new(Mutex::new(BeaconStatus::default()));
        let mut mon = BeaconMonitor::new(Arc::clone(&status));
        let mut rng = Lcg(99);
        let iq: Vec<IqSample> = (0..768_000 * 6)
            .map(|_| IqSample {
                i: (1e5 * rng.gauss()) as i32,
                q: (1e5 * rng.gauss()) as i32,
            })
            .collect();
        for block in iq.chunks(2048) {
            mon.process(block, 768_000.0, Some(0.0));
        }
        let st = status.lock().unwrap();
        assert!(st.offset_hz.is_none());
        assert_eq!(st.frames_ok + st.frames_bad, 0);
    }

    /// Replays a real AUDIO recording of the beacon (USB, beacon in the
    /// audio): `HPSDR_PSK_WAV=/path/to.wav cargo test --release
    /// psk_beacon -- --ignored --nocapture`. 16-bit PCM, 48 kHz, mono or
    /// stereo (left channel used). The audio is turned into IQ around
    /// 1500 Hz (keeping only positive frequencies) for the monitor.
    #[test]
    #[ignore]
    fn decodes_real_audio_recording() {
        let path = std::env::var("HPSDR_PSK_WAV").expect("set HPSDR_PSK_WAV");
        let bytes = std::fs::read(path).unwrap();
        let channels = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
        let mut pos = 12;
        while &bytes[pos..pos + 4] != b"data" {
            let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            pos += 8 + len;
        }
        let audio: Vec<f32> = bytes[pos + 8..]
            .chunks_exact(2 * channels)
            .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
            .collect();
        // Analytic signal: mix 1500 Hz down, lowpass to +/-1450 Hz so
        // the mirror image (below -1500 Hz) is dropped, mix back up.
        let mut down = Nco::new();
        down.set_freq(1500.0, 48_000.0);
        let mut up = Nco::new();
        up.set_freq(-1500.0, 48_000.0);
        let mut fir = ComplexFir::new(lowpass(401, 1450.0, 48_000.0));
        let iq: Vec<IqSample> = audio
            .iter()
            .map(|&a| {
                fir.push(down.next().scale(a));
                let v = fir.output().mul(up.next()).scale(2.0 * IQ_NORM);
                IqSample { q: v.re as i32, i: v.im as i32 }
            })
            .collect();
        let status = Arc::new(Mutex::new(BeaconStatus::default()));
        let mut mon = BeaconMonitor::new(Arc::clone(&status));
        // (time s, measured, predicted) at each new estimate, to check
        // the extrapolation LNB lock relies on.
        let mut track: Vec<(f64, f64, f64)> = Vec::new();
        for (n, block) in iq.chunks(2048).enumerate() {
            mon.process(block, 48_000.0, Some(1500.0));
            let st = status.lock().unwrap();
            if let (Some(m), Some(p)) = (st.offset_hz, st.predicted_offset_hz) {
                if track.last().is_none_or(|t| t.1 != m) {
                    track.push(((n + 1) as f64 * 2048.0 / 48_000.0, m, p));
                }
            }
        }
        // A measurement describes the signal MEASUREMENT_AGE_S earlier,
        // so compare each prediction with the measurement made that much
        // later.
        let age = MEASUREMENT_AGE_S;
        let errors: Vec<f64> = track
            .iter()
            .filter_map(|&(t, _, p)| {
                let later = track.iter().find(|u| u.0 - age >= t - 0.05)?;
                (later.0 - age - t < 0.3).then_some(p - later.1)
            })
            .collect();
        let rms = (errors.iter().map(|e| e * e).sum::<f64>() / errors.len() as f64).sqrt();
        let worst = errors.iter().fold(0.0f64, |a, e| a.max(e.abs()));
        println!("prediction error over {} estimates: rms {rms:.2} Hz, worst {worst:.2} Hz", errors.len());
        if std::env::var("HPSDR_PSK_TRACK").is_ok() {
            for (t, m, p) in &track {
                println!("track {t:.2} {m:.2} {p:.2}");
            }
        }
        let st = status.lock().unwrap();
        println!(
            "offset {:?} predicted {:?} drift {:.2} Hz/s ok {} bad {}",
            st.offset_hz, st.predicted_offset_hz, st.drift_hz_per_s, st.frames_ok, st.frames_bad
        );
        for p in &st.pages {
            println!("--- page {}", p.id);
            for l in &p.lines {
                println!("{l}");
            }
        }
        assert!(st.frames_ok > 0);
    }
}
