/*
    The transponder view: a spectrum of all of QO-100's wideband
    transponder (10491-10499.5 MHz) at once, to see which DATV stations
    are on and pick one. Pluto only, for now.

    The transponder is wider than any stream the receiver can use, so
    while the view is up the Pluto runs at pluto::SCAN_SAMPLE_RATE_HZ
    instead, faster than its USB link can carry: whole buffers arrive
    with gaps between them (see pluto::SCAN_SAMPLE_RATE_HZ). A spectrum
    is fine with that -- each FFT only needs its own samples to be
    continuous -- but the receiver isn't, so it's paused meanwhile
    (radio.rs's pluto_receiver_loop and pluto_control_loop do the
    switching; this module is the shared state and the FFTs).
*/

use crate::dvbs2::Cf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// QO-100's wideband transponder, RF.
pub const QO100_WB_LOW_HZ: u64 = 10_491_000_000;
pub const QO100_WB_HIGH_HZ: u64 = 10_499_500_000;

/// FFT size: 6 kHz bins at pluto::SCAN_SAMPLE_RATE_HZ, finer than any
/// screen shows 9 MHz with.
pub const FFT_SIZE: usize = 2048;

/// FFTs per received buffer, spread across it. The link delivers ~25
/// buffers a second, so this averages ~200 a second -- plenty for a
/// steady display, at a fraction of the CPU of transforming everything.
const FFTS_PER_BUFFER: usize = 8;

/// How often a new spectrum is published, and how much of it is the
/// newest average (the rest is the previous display: smoothing).
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
const SMOOTHING: f32 = 0.4;

/// Shared between the UI and the Pluto's threads.
#[derive(Default)]
pub struct Scan {
    /// Set by the UI: the IF to centre the view on, Hz; 0 for off.
    pub request_if_hz: AtomicU32,
    /// Set by the control thread once the Pluto is actually at the scan
    /// rate and that IF (0 once it's back to normal).
    pub active_if_hz: AtomicU32,
    pub spectrum: Mutex<Spectrum>,
}

/// The latest published spectrum.
#[derive(Clone, Default)]
pub struct Spectrum {
    /// The IF it's centred on; matches Scan::active_if_hz when current.
    pub centre_if_hz: u32,
    pub rate_hz: u32,
    /// FFT_SIZE bins, lowest frequency first, dB (arbitrary reference).
    pub db: Vec<f32>,
}

impl Scan {
    pub fn request(&self, if_hz: u32) {
        self.request_if_hz.store(if_hz, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.request(0);
    }

    pub fn is_requested(&self) -> bool {
        self.request_if_hz.load(Ordering::Relaxed) != 0
    }

    /// The current spectrum, if the Pluto is running the view that was
    /// asked for and has produced one.
    pub fn current(&self) -> Option<Spectrum> {
        let active = self.active_if_hz.load(Ordering::Relaxed);
        if active == 0 || active != self.request_if_hz.load(Ordering::Relaxed) {
            return None;
        }
        let s = self.spectrum.lock().unwrap();
        (s.centre_if_hz == active && !s.db.is_empty()).then(|| s.clone())
    }
}

/// Turns received buffers into averaged spectra; owned by the receiver
/// thread.
pub struct Accumulator {
    window: Vec<f32>,
    power: Vec<f32>,
    count: usize,
    smoothed: Vec<f32>,
    last_publish: Instant,
    centre_if_hz: u32,
    scratch: Vec<Cf>,
}

impl Accumulator {
    pub fn new() -> Self {
        let window = (0..FFT_SIZE)
            .map(|k| {
                let x = std::f32::consts::PI * 2.0 * k as f32 / FFT_SIZE as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Self {
            window,
            power: vec![0.0; FFT_SIZE],
            count: 0,
            smoothed: Vec::new(),
            last_publish: Instant::now(),
            centre_if_hz: 0,
            scratch: vec![Cf::default(); FFT_SIZE],
        }
    }

    /// One received buffer (`samples`: I/Q pairs at `rate_hz`, centred on
    /// `centre_if_hz`); publishes into `scan` every PUBLISH_EVERY.
    pub fn add(&mut self, samples: &[[i16; 2]], centre_if_hz: u32, rate_hz: u32, scan: &Scan) {
        if centre_if_hz != self.centre_if_hz {
            // Retuned: start again rather than smear the old view in.
            self.centre_if_hz = centre_if_hz;
            self.smoothed.clear();
            self.power.iter_mut().for_each(|p| *p = 0.0);
            self.count = 0;
        }
        let segments = samples.len() / FFT_SIZE;
        if segments == 0 {
            return;
        }
        let step = segments.div_ceil(FFTS_PER_BUFFER);
        for seg in (0..segments).step_by(step) {
            let chunk = &samples[seg * FFT_SIZE..(seg + 1) * FFT_SIZE];
            for ((out, s), w) in self.scratch.iter_mut().zip(chunk).zip(&self.window) {
                *out = Cf::new(s[0] as f32 * w, s[1] as f32 * w);
            }
            fft(&mut self.scratch);
            // Bin k is frequency k (k < N/2) or k - N: rotate so the
            // lowest frequency comes first.
            for (k, v) in self.scratch.iter().enumerate() {
                self.power[(k + FFT_SIZE / 2) % FFT_SIZE] += v.norm_sqr();
            }
            self.count += 1;
        }
        if self.last_publish.elapsed() < PUBLISH_EVERY || self.count == 0 {
            return;
        }
        self.last_publish = Instant::now();
        let n = self.count as f32;
        let db: Vec<f32> = self.power.iter().map(|&p| 10.0 * (p / n).max(1e-12).log10()).collect();
        if self.smoothed.len() == db.len() {
            for (s, d) in self.smoothed.iter_mut().zip(&db) {
                *s += SMOOTHING * (d - *s);
            }
        } else {
            self.smoothed = db;
        }
        self.power.iter_mut().for_each(|p| *p = 0.0);
        self.count = 0;
        *scan.spectrum.lock().unwrap() =
            Spectrum { centre_if_hz, rate_hz, db: self.smoothed.clone() };
    }
}

/// In-place iterative radix-2 FFT (`buf.len()` a power of two) -- the
/// same as psk_beacon.rs's, on dvbs2's complex type.
fn fft(buf: &mut [Cf]) {
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
        let w = -2.0 * std::f64::consts::PI / len as f64;
        let wl = Cf::new(w.cos() as f32, w.sin() as f32);
        for start in (0..n).step_by(len) {
            let mut wk = Cf::new(1.0, 0.0);
            for k in 0..len / 2 {
                let a = buf[start + k];
                let b = buf[start + k + len / 2] * wk;
                buf[start + k] = a + b;
                buf[start + k + len / 2] = a - b;
                wk = wk * wl;
            }
        }
        len <<= 1;
    }
}

/// A signal picked by clicking the spectrum: its centre and estimated
/// symbol rate.
pub struct Picked {
    pub offset_hz: f64,
    pub symbol_rate: Option<u32>,
}

/// Symbol rates offered in the DATV window, which a measured width snaps
/// to.
pub const COMMON_SYMBOL_RATES: [u32; 7] = [125_000, 250_000, 333_000, 500_000, 1_000_000, 1_500_000, 2_000_000];

/// The signal around bin `click` of `db` (FFT_SIZE bins across
/// `rate_hz`): its centre as an offset from the spectrum's centre, and
/// its symbol rate, taken as the width 3 dB down from its top (an RRC
/// signal is Rs wide there whatever its roll-off), snapped to a common
/// rate within 20%. Just the click's frequency, with no rate, if nothing
/// stands out there.
pub fn pick(db: &[f32], rate_hz: u32, click: usize) -> Picked {
    let bin_hz = rate_hz as f64 / db.len() as f64;
    let to_offset = |bin: f64| (bin - db.len() as f64 / 2.0) * bin_hz;
    let just_click = Picked { offset_hz: to_offset(click as f64), symbol_rate: None };
    // Light smoothing, so noise doesn't end the signal early.
    let smooth: Vec<f32> = (0..db.len())
        .map(|k| {
            let (lo, hi) = (k.saturating_sub(3), (k + 4).min(db.len()));
            db[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
        })
        .collect();
    // The noise floor: not too low a percentile, since the filter skirts
    // either side of the transponder sit below the noise inside it, and
    // made noise there look like a signal.
    let mut sorted = smooth.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() * 2 / 5];
    // The signal's top: the highest point within ~60 kHz of the click.
    let reach = (60_000.0 / bin_hz).ceil() as usize;
    let (lo, hi) = (click.saturating_sub(reach), (click + reach + 1).min(db.len()));
    let Some(peak) = (lo..hi).max_by(|&a, &b| smooth[a].total_cmp(&smooth[b])) else {
        return just_click;
    };
    if smooth[peak] - floor < 2.0 {
        return just_click;
    }
    // A flat-topped signal: its level is the median across its top, not
    // the noisy peak.
    let half = smooth[peak] - 3.0;
    let mut left = peak;
    while left > 0 && smooth[left - 1] > half {
        left -= 1;
    }
    let mut right = peak;
    while right + 1 < smooth.len() && smooth[right + 1] > half {
        right += 1;
    }
    let top: Vec<f32> = {
        let mut t = smooth[left..=right].to_vec();
        t.sort_by(f32::total_cmp);
        t
    };
    let level = top[top.len() / 2];
    // Only a noise spike stood out: its "signal" would spread across all
    // the empty spectrum around it.
    if level - floor < 2.0 {
        return just_click;
    }
    let edge = level - 3.0;
    let mut l = left;
    while l > 0 && smooth[l - 1] > edge {
        l -= 1;
    }
    let mut r = right;
    while r + 1 < smooth.len() && smooth[r + 1] > edge {
        r += 1;
    }
    // Edges interpolated to where the curve crosses `edge`.
    let cross = |inside: usize, outside: usize| {
        let (a, b) = (smooth[inside], smooth[outside]);
        inside as f64 + (outside as f64 - inside as f64) * ((a - edge) / (a - b).max(1e-6)) as f64
    };
    let l_edge = if l > 0 { cross(l, l - 1) } else { l as f64 };
    let r_edge = if r + 1 < smooth.len() { cross(r, r + 1) } else { r as f64 };
    let width_hz = (r_edge - l_edge) * bin_hz;
    let symbol_rate = COMMON_SYMBOL_RATES
        .iter()
        .copied()
        .min_by(|&a, &b| (a as f64 - width_hz).abs().total_cmp(&(b as f64 - width_hz).abs()))
        .filter(|&r| (r as f64 - width_hz).abs() < 0.2 * r as f64);
    Picked { offset_hz: to_offset((l_edge + r_edge) / 2.0), symbol_rate }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat-topped signal 250 kHz wide at +1 MHz, 6 dB over the noise.
    #[test]
    fn picks_a_signal_by_its_width() {
        let rate = 10_240_000;
        let bin_hz = rate as f64 / FFT_SIZE as f64;
        let db: Vec<f32> = (0..FFT_SIZE)
            .map(|k| {
                let f = (k as f64 - FFT_SIZE as f64 / 2.0) * bin_hz;
                if (f - 1_000_000.0).abs() < 125_000.0 { 6.0 } else { 0.0 }
            })
            .collect();
        let click = (FFT_SIZE as f64 / 2.0 + 1_030_000.0 / bin_hz) as usize;
        let p = pick(&db, rate, click);
        assert!((p.offset_hz - 1_000_000.0).abs() < 2.0 * bin_hz, "{}", p.offset_hz);
        assert_eq!(p.symbol_rate, Some(250_000));
    }

    #[test]
    fn a_click_on_noise_is_just_a_frequency() {
        let db = vec![0.0f32; FFT_SIZE];
        let p = pick(&db, 10_240_000, FFT_SIZE / 2 + 100);
        assert_eq!(p.symbol_rate, None);
        assert!((p.offset_hz - 100.0 * 5_000.0).abs() < 1.0);
    }

    /// A real capture of the transponder (ci16_le at SCAN_SAMPLE_RATE_HZ,
    /// as scratch recordings were made): `HPSDR_SCAN_FILE`, centred
    /// `HPSDR_SCAN_CENTRE_RF` Hz. Prints what clicking each MHz would
    /// pick.
    #[test]
    #[ignore]
    fn picks_from_a_real_capture() {
        let path = std::env::var("HPSDR_SCAN_FILE").expect("set HPSDR_SCAN_FILE");
        let centre: f64 = std::env::var("HPSDR_SCAN_CENTRE_RF").map_or(10_495_250_000.0, |v| v.parse().unwrap());
        let bytes = std::fs::read(path).unwrap();
        // Recordings are scaled to +-32768; the receiver gets 12-bit.
        let samples: Vec<[i16; 2]> = bytes
            .chunks_exact(4)
            .map(|c| [i16::from_le_bytes([c[0], c[1]]) >> 4, i16::from_le_bytes([c[2], c[3]]) >> 4])
            .collect();
        let rate = crate::pluto::SCAN_SAMPLE_RATE_HZ;
        let scan = Scan::default();
        let mut acc = Accumulator::new();
        for chunk in samples.chunks(256 * 1024) {
            acc.last_publish = Instant::now() - PUBLISH_EVERY;
            acc.add(chunk, 745_250_000, rate, &scan);
        }
        let s = scan.spectrum.lock().unwrap().clone();
        let bin_hz = rate as f64 / FFT_SIZE as f64;
        for click_mhz in [10491.5, 10494.25, 10496.75, 10497.75, 10499.25] {
            let click = ((click_mhz * 1e6 - centre) / bin_hz + FFT_SIZE as f64 / 2.0) as usize;
            let p = pick(&s.db, rate, click);
            eprintln!(
                "click {click_mhz} MHz -> {:.4} MHz, {:?}",
                (centre + p.offset_hz) / 1e6,
                p.symbol_rate
            );
        }
    }

    /// A tone in the middle of the upper half lands in the right bin,
    /// lowest frequency first.
    #[test]
    fn spectrum_is_in_frequency_order() {
        let rate = 10_240_000u32;
        let tone_hz = 2_560_000.0;
        let samples: Vec<[i16; 2]> = (0..FFT_SIZE * 8)
            .map(|n| {
                let ph = 2.0 * std::f64::consts::PI * tone_hz * n as f64 / rate as f64;
                [(ph.cos() * 1000.0) as i16, (ph.sin() * 1000.0) as i16]
            })
            .collect();
        let scan = Scan::default();
        let mut acc = Accumulator::new();
        acc.last_publish = Instant::now() - PUBLISH_EVERY;
        acc.add(&samples, 745_000_000, rate, &scan);
        let s = scan.spectrum.lock().unwrap().clone();
        let peak = (0..FFT_SIZE).max_by(|&a, &b| s.db[a].total_cmp(&s.db[b])).unwrap();
        assert_eq!(peak, FFT_SIZE / 2 + FFT_SIZE / 4);
    }
}
