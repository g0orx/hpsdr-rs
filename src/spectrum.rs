/*
    Safe wrapper around the raw WDSP bindings in wdsp_sys, built from
    exact call-site values confirmed against the user's own reference
    code (piHPSDR-derived). One unverified value: WIN_TYPE below --
    flagged inline.
*/

use crate::radio::IqSample;
use crate::wdsp_sys as wdsp;
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const BUFFER_SIZE: usize = 1024;
const RXA_FFT_SIZE: i32 = 2048;
const DSP_RATE: i32 = 48_000;
const OUTPUT_RATE: i32 = 48_000;
const ANALYZER_MAX_SIZE: i32 = 262_144;

// 24-bit signed samples normalized to roughly [-1.0, 1.0] before handing
// to WDSP. Standard convention given fscLin/fscHin (calibration) were
// both left at 0.0 (uncalibrated) in the confirmed SetAnalyzer call.
const IQ_NORM: f64 = 8_388_608.0; // 2^23

const SPECTRUM_WIDTH: i32 = 1024;
const ZOOM: i32 = 1;
const SPECTRUM_FPS: f32 = 10.0;
const KEEP_TIME: f32 = 0.1;
// WDSP window-type enum value -- NOT confirmed against your source,
// unlike everything else in this file. 5 is a placeholder guess. If the
// spectrum looks unusually smeared or leaky, this is the first thing to
// check against whatever `self.win_type` actually held in your code.
const WIN_TYPE: i32 = 5;

pub const WATERFALL_HISTORY: usize = 200;

/// Confirmed against the user's own `Modes` enum -- these numeric values
/// are exactly what WDSP's SetRXAMode expects.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Mode {
    Lsb = 0,
    Usb = 1,
    Dsb = 2,
    Cwl = 3,
    Cwu = 4,
    Fmn = 5,
    Am = 6,
    Digu = 7,
    Spec = 8,
    Digl = 9,
    Sam = 10,
    Drm = 11,
}

pub const ALL_MODES: [Mode; 12] = [
    Mode::Lsb,
    Mode::Usb,
    Mode::Dsb,
    Mode::Cwl,
    Mode::Cwu,
    Mode::Fmn,
    Mode::Am,
    Mode::Digu,
    Mode::Spec,
    Mode::Digl,
    Mode::Sam,
    Mode::Drm,
];

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Lsb => "LSB",
            Mode::Usb => "USB",
            Mode::Dsb => "DSB",
            Mode::Cwl => "CWL",
            Mode::Cwu => "CWU",
            Mode::Fmn => "FM",
            Mode::Am => "AM",
            Mode::Digu => "DIGU",
            Mode::Spec => "SPEC",
            Mode::Digl => "DIGL",
            Mode::Sam => "SAM",
            Mode::Drm => "DRM",
        }
    }
}

/// Computes passband edges (Hz, relative to the tuned/dial frequency)
/// from mode + a single "width" control. This is our own UI convention,
/// not a WDSP requirement -- WDSP just takes whatever edges it's given
/// via RXASetPassband, so these numbers are a design choice, tuned to
/// match roughly what other ham SDR software defaults to.
pub fn passband_for(mode: Mode, width_hz: f64) -> (f64, f64) {
    let width_hz = width_hz.max(50.0);
    let low_cut = 150.0_f64.min(width_hz * 0.1);
    match mode {
        Mode::Lsb | Mode::Digl => (-width_hz, -low_cut),
        Mode::Usb | Mode::Digu => (low_cut, width_hz),
        Mode::Dsb | Mode::Am | Mode::Sam | Mode::Drm | Mode::Spec | Mode::Fmn => {
            (-width_hz, width_hz)
        }
        Mode::Cwl => {
            const PITCH: f64 = 600.0;
            (-(PITCH + width_hz / 2.0), -(PITCH - width_hz / 2.0))
        }
        Mode::Cwu => {
            const PITCH: f64 = 600.0;
            (PITCH - width_hz / 2.0, PITCH + width_hz / 2.0)
        }
    }
}

/// Reasonable default filter width (Hz) per mode, used to initialize the
/// UI's width slider.
pub fn default_width_hz(mode: Mode) -> f64 {
    match mode {
        Mode::Cwl | Mode::Cwu => 200.0,
        Mode::Digu | Mode::Digl => 2400.0,
        Mode::Lsb | Mode::Usb => 2700.0,
        Mode::Fmn => 5000.0,
        Mode::Am | Mode::Dsb | Mode::Sam | Mode::Drm | Mode::Spec => 3000.0,
    }
}

#[derive(Default)]
pub struct SpectrumDisplay {
    pub spectrum: Vec<f32>,
    pub waterfall_rows: VecDeque<Vec<f32>>,
    pub meter_db: f64,
    /// Bumped every time feed() below produces fresh spectrum/waterfall
    /// pixel data -- i.e. at roughly SPECTRUM_FPS (10/sec), not once
    /// per UI repaint. The UI compares this against what it last saw
    /// so it only re-clones waterfall_rows and rebuilds/re-uploads the
    /// waterfall texture when there's actually something new, instead
    /// of doing that (the most expensive per-frame UI cost by far) on
    /// every repaint regardless of whether new data arrived.
    pub revision: u64,
}

/// Confirmed against the user's own `AGC` enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Agc {
    Off = 0,
    Long = 1,
    Slow = 2,
    Medium = 3,
    Fast = 4,
}

impl Agc {
    pub fn label(self) -> &'static str {
        match self {
            Agc::Off => "AGC Off",
            Agc::Long => "AGC Long",
            Agc::Slow => "AGC Slow",
            Agc::Medium => "AGC Medium",
            Agc::Fast => "AGC Fast",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Agc::Off => Agc::Long,
            Agc::Long => Agc::Slow,
            Agc::Slow => Agc::Medium,
            Agc::Medium => Agc::Fast,
            Agc::Fast => Agc::Off,
        }
    }
}

/// Noise blanker state. WDSP's ANB ("NB") and NOB ("NB2") are
/// independent DSP objects and technically could both run at once, but
/// other HPSDR software treats them as mutually exclusive -- a single
/// control cycling Off -> NB -> NB2 -> Off -- rather than two
/// independent toggles, so this does the same.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NoiseBlanker {
    Off,
    Nb,
    Nb2,
}

impl Default for NoiseBlanker {
    fn default() -> Self {
        NoiseBlanker::Off
    }
}

impl NoiseBlanker {
    pub fn next(self) -> Self {
        match self {
            NoiseBlanker::Off => NoiseBlanker::Nb,
            NoiseBlanker::Nb => NoiseBlanker::Nb2,
            NoiseBlanker::Nb2 => NoiseBlanker::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NoiseBlanker::Off => "NB: Off",
            NoiseBlanker::Nb => "NB: NB",
            NoiseBlanker::Nb2 => "NB: NB2",
        }
    }
}

/// Noise reduction state -- same mutually-exclusive cycling treatment
/// as NoiseBlanker above, cycling between WDSP's four RXA noise
/// reduction stages: ANR ("NR"), EMNR ("NR2"), RNNR ("NR3" -- an
/// RNNoise-backed stage, vendored as librnnoise.a), and SBNR ("NR4" --
/// a libspecbleach-backed stage, vendored as liblibspecbleach.a; see
/// build.rs for the link-order/naming notes on both). All four live
/// inside the RXA chain, but the same convention (only one active at a
/// time) applies in other HPSDR software, so it's kept here too.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NoiseReduction {
    Off,
    Nr,
    Nr2,
    Nr3,
    Nr4,
}

impl Default for NoiseReduction {
    fn default() -> Self {
        NoiseReduction::Off
    }
}

impl NoiseReduction {
    pub fn next(self) -> Self {
        match self {
            NoiseReduction::Off => NoiseReduction::Nr,
            NoiseReduction::Nr => NoiseReduction::Nr2,
            NoiseReduction::Nr2 => NoiseReduction::Nr3,
            NoiseReduction::Nr3 => NoiseReduction::Nr4,
            NoiseReduction::Nr4 => NoiseReduction::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NoiseReduction::Off => "NR: Off",
            NoiseReduction::Nr => "NR: NR",
            NoiseReduction::Nr2 => "NR: NR2",
            NoiseReduction::Nr3 => "NR: NR3",
            NoiseReduction::Nr4 => "NR: NR4",
        }
    }
}

/// WDSP's graphic-EQ stage (`eq.c`) offers two fixed band layouts, both
/// confirmed by reading the source directly: a legacy 3-band EQ (preamp +
/// low/mid/high, corners at 150/400/1500/6000Hz -- `SetRXAGrphEQ`/
/// `SetTXAGrphEQ`) and a 10-band EQ (preamp + 10 bands, 32Hz..16kHz --
/// `SetRXAGrphEQ10`/`SetTXAGrphEQ10`). Same layout on both RXA and TXA, so
/// this one enum/struct pair serves both chains (see EqualizerParams).
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EqBandCount {
    Three,
    Ten,
}

impl Default for EqBandCount {
    fn default() -> Self {
        EqBandCount::Three
    }
}

/// Graphic-EQ settings for one WDSP channel (RXA or TXA -- see
/// EqBandCount's doc comment for why one type covers both). Mirrors
/// piHPSDR's own equalizer_menu.c/radio.c defaults and range exactly:
/// disabled, all-zero (flat) gains, dB values -12..15 (WDSP's own
/// SetXXAGrphEQ[10] take `int*`, so gains are whole dB, not fractional).
/// bands_3_db and bands_10_db are kept as two INDEPENDENT arrays (not one
/// reused buffer) so switching band_count back and forth never clobbers
/// whichever mode isn't currently active.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EqualizerParams {
    pub enabled: bool,
    pub band_count: EqBandCount,
    pub preamp_db: i32,
    /// low, mid, high -- see SetRXAGrphEQ's own doc comment in eq.c: WDSP
    /// internally duplicates the low gain onto two adjacent corner
    /// frequencies, this is its own fixed layout, not something to work
    /// around here.
    pub bands_3_db: [i32; 3],
    /// 32/63/125/250/500/1000/2000/4000/8000/16000 Hz, in that order --
    /// WDSP's own fixed 10-band layout (SetRXAGrphEQ10's F[] table).
    pub bands_10_db: [i32; 10],
}

impl Default for EqualizerParams {
    fn default() -> Self {
        Self {
            enabled: false,
            band_count: EqBandCount::default(),
            preamp_db: 0,
            bands_3_db: [0; 3],
            bands_10_db: [0; 10],
        }
    }
}

#[derive(Copy, Clone)]
pub struct DemodParams {
    pub mode: Mode,
    pub width_hz: f64,
    pub gain: f32,
    pub agc: Agc,
    // Units assumed (not confirmed against your reference): attack/decay/
    // hang in milliseconds, top/slope/thresh in dB. Tune by ear/meter.
    pub agc_attack_ms: i32,
    pub agc_decay_ms: i32,
    pub agc_hang_ms: i32,
    pub agc_top_db: f64,
    pub agc_slope_db: i32,
    pub agc_thresh_db: f64,
    /// NB (ANB) vs. NB2 (NOB) vs. off -- mutually exclusive, see
    /// NoiseBlanker's doc comment. External to the RXA chain: whichever
    /// stage is active runs on the raw ADC-rate IQ before fexchange0,
    /// via xanbEXT/xnobEXT in SpectrumAnalyzer::demod, not through any
    /// RXA Set* call.
    pub noise_blanker: NoiseBlanker,
    /// Shared threshold for both blanker stages. Units/range not
    /// confirmed against your reference -- 20.0 is the value that was
    /// previously hardcoded at channel-creation time; exposed here as
    /// a starting point to tune by ear, same caveat as WIN_TYPE above.
    pub nb_threshold: f64,
    /// NR (ANR) vs. NR2 (EMNR) vs. off -- mutually exclusive, see
    /// NoiseReduction's doc comment. Unlike the blanker, both live
    /// inside the RXA chain itself, so switching is just a Set*Run
    /// call, no extra buffer plumbing needed.
    pub noise_reduction: NoiseReduction,
    /// SNB ("Spectral Noise Blanker", WDSP's SNBA stage) -- unlike
    /// NoiseReduction's NR/NR2/NR3/NR4, this is an independent toggle
    /// rather than part of that mutually-exclusive cycle: SNB targets
    /// impulsive/broadband noise (clicks, ignition, etc.) while NR
    /// targets steady-state hiss, and real HPSDR clients (piHPSDR,
    /// Thetis) commonly run both at once. Lives inside the RXA chain
    /// like NR does, so this is likewise just a Set*Run call.
    pub snb: bool,
    /// ANF ("Automatic Notch Filter", WDSP's ANF stage) -- an
    /// independent toggle, just like SNB above (its own Set*Run call,
    /// no mutually-exclusive cycle to fold into). Targets a steady
    /// heterodyne/carrier within the passband, not broadband noise.
    pub anf: bool,
    /// Binaural ("phasing") RX audio -- when true, RXA's PatchPanel
    /// output stage is left in WDSP's own default mode, producing
    /// genuinely different L/R audio (an intentional SDR stereo-
    /// listening effect) instead of a single duplicated mono channel.
    /// See open()'s own SetRXAPanelBinaural doc comment for the history
    /// of why this defaulted off, and demod()'s for how this is
    /// actually applied. radio-audio-to-radio downmixes to mono
    /// regardless of this setting -- see run()'s own doc comment.
    pub binaural: bool,
    /// CTUN ("Click to Tune"): when true, the hardware/LO frequency
    /// (lo_frequency_hz below) stays fixed and ctun_offset_hz shifts the
    /// RXA demod chain instead, so the user can pick a different listen
    /// frequency within the same spectrum window without retuning the
    /// radio. Confirmed against a working reference (rustyHPSDR).
    pub ctun: bool,
    /// Offset (Hz) from lo_frequency_hz to the CTUN'd listen frequency.
    /// Only meaningful while ctun is true; ignored (and reset to 0 on
    /// the wire) while false.
    pub ctun_offset_hz: f64,
    /// The actual hardware-tuned (LO) frequency, mirrored here every
    /// frame from main.rs's RadioSession/ExtraReceiver state purely so
    /// demod() can pass it to RXANBPSetTuneFrequency while CTUN is off
    /// -- radio.rs/main.rs own the real value, this is just a read-only
    /// copy for the analyzer thread.
    pub lo_frequency_hz: f64,
    /// See EqualizerParams's doc comment. One instance per receiver --
    /// every extra receiver window has its own DemodParams (and thus its
    /// own independent EQ), same as it already has its own AGC/NB/NR.
    pub eq: EqualizerParams,
    /// Spectrum/waterfall zoom (1 = full sample-rate span, higher =
    /// narrower, higher-resolution window) and pan (-1.0 = lowest
    /// frequency the current zoom can reach, +1.0 = highest, 0.0 =
    /// centered on the dial) -- see SpectrumAnalyzer's zoom/pan
    /// reconfigure method for how this actually grows the real FFT size
    /// (not just a visual crop/stretch of a fixed-resolution trace) --
    /// confirmed against piHPSDR/rustyHPSDR's own zoom implementations,
    /// both of which do the same: WDSP itself computes a bigger FFT and
    /// clips/rebins it down to a fixed pixel count, rather than the UI
    /// stretching a fixed-resolution trace.
    pub zoom: i32,
    pub pan: f32,
}

impl Default for DemodParams {
    fn default() -> Self {
        let mode = Mode::Usb;
        Self {
            mode,
            width_hz: default_width_hz(mode),
            // WDSP's RXA output is much hotter than typical audio-app
            // expectations (e.g. WSJT-X) -- starting well below unity.
            gain: 0.3,
            // Off by default: AGC pumping works against the steady
            // levels digital-mode decoders like WSJT-X expect.
            agc: Agc::Off,
            agc_attack_ms: 2,
            agc_decay_ms: 250,
            agc_hang_ms: 500,
            agc_top_db: 100.0,
            agc_slope_db: 35,
            agc_thresh_db: -100.0,
            // NB/NR off by default, same reasoning as AGC above --
            // these reshape the signal in ways that can surprise a
            // user who didn't ask for them; opt-in from the main panel.
            noise_blanker: NoiseBlanker::Off,
            nb_threshold: 20.0,
            noise_reduction: NoiseReduction::Off,
            snb: false,
            anf: false,
            binaural: false,
            ctun: false,
            ctun_offset_hz: 0.0,
            lo_frequency_hz: 0.0,
            eq: EqualizerParams::default(),
            zoom: 1,
            pan: 0.0,
        }
    }
}

// ~1 second of 48kHz stereo audio -- generous enough to absorb jitter
// between the demod thread and the audio callback without much latency.
// Deliberately small (~150ms of interleaved stereo at 48kHz) -- same
// reasoning as IQ_BUFFER_CAPACITY in radio.rs: this FIFO has no
// catch-up mechanism, so any backlog becomes permanent added latency
// between what's on screen and what's heard, rather than self-
// correcting. A small cap bounds worst-case audio delay directly.
const AUDIO_BUFFER_CAPACITY: usize = 14_400;

/// Capacity for waveform_out specifically -- NOT shared with
/// AUDIO_BUFFER_CAPACITY above, deliberately: that constant is sized
/// small on purpose (its own doc comment: backlog there is real added
/// playback latency), but waveform_out is a display-only, never-drained
/// peek tap where a longer window is purely cosmetic (a real report:
/// the default ~200ms window felt too fast/frantic; longer shows more
/// history per frame so it reads as calmer without changing anything
/// about actual audio latency). ~500ms at 48kHz.
const WAVEFORM_TAP_CAPACITY: usize = 24_000;

/// Two cascaded single-pole lowpass stages (~12dB/octave) -- see run()'s
/// doc comment on radio_audio_lpf for why this exists at all and how the
/// 4kHz cutoff was chosen/verified. `feed` is called once per audio
/// sample; state persists across calls (owned by run()'s loop for the
/// life of the SpectrumHandle).
struct RxAudioLowpass {
    alpha: f32,
    y1: f32,
    y2: f32,
}

impl RxAudioLowpass {
    const CUTOFF_HZ: f32 = 4_000.0;

    fn new(sample_rate_hz: f32) -> Self {
        let alpha = 1.0 - (-2.0 * std::f32::consts::PI * Self::CUTOFF_HZ / sample_rate_hz).exp();
        Self { alpha, y1: 0.0, y2: 0.0 }
    }

    fn feed(&mut self, x: f32) -> f32 {
        self.y1 += self.alpha * (x - self.y1);
        self.y2 += self.alpha * (self.y1 - self.y2);
        self.y2
    }
}

// Same "small, bounded, drop-oldest" reasoning as AUDIO_BUFFER_CAPACITY
// above -- a TCI client that isn't draining fast enough (or isn't
// streaming at all) shouldn't cause unbounded growth here. Sized more
// generously than the audio buffer since this carries raw wideband IQ,
// which can be at a much higher rate than the fixed 48kHz audio output
// (up to whatever the receiver's actual sample rate is).
const IQ_OUT_CAPACITY: usize = 200_000;

struct SpectrumAnalyzer {
    channel: i32,
    iq_scratch: Vec<f64>,
    spectrum_pixels: Vec<f32>,
    waterfall_pixels: Vec<f32>,
    demod_iq_scratch: Vec<f64>,
    /// Second IQ-rate scratch buffer, used to ping-pong the two noise
    /// blanker stages (xanbEXT/xnobEXT) ahead of fexchange0, rather
    /// than calling either in-place -- their in-place behavior isn't
    /// confirmed, so this avoids relying on it (see demod() below).
    nb_scratch: Vec<f64>,
    demod_audio_scratch: Vec<f64>,
    last_mode: Option<Mode>,
    last_passband: Option<(f64, f64)>,
    last_agc: Option<Agc>,
    last_agc_params: Option<(i32, i32, i32, f64, i32, f64)>,
    last_nb_enabled: Option<NoiseBlanker>,
    last_nb_threshold: Option<f64>,
    last_nr_enabled: Option<NoiseReduction>,
    last_snb_enabled: Option<bool>,
    last_anf_enabled: Option<bool>,
    last_binaural: Option<bool>,
    last_ctun: Option<bool>,
    last_ctun_offset: Option<f64>,
    last_lo_frequency: Option<f64>,
    last_eq: Option<EqualizerParams>,
    /// Edge-triggered diagnostic state for fexchange0's error out-param
    /// -- see its doc comment in demod() for why this exists and why
    /// it's edge- rather than every-call-triggered.
    last_fexchange_error: Option<bool>,
    /// See set_zoom_pan's doc comment. Initialized to Some(1)/Some(0.0)
    /// (matching what open()'s own SetAnalyzer call already configures),
    /// not None, so the very first set_zoom_pan(1, 0.0, ..) call from
    /// run()'s loop is correctly seen as a no-op rather than redundantly
    /// reconfiguring the analyzer to the same values it already has.
    last_zoom: Option<i32>,
    last_pan: Option<f32>,
}

impl SpectrumAnalyzer {
    fn open(channel: i32, sample_rate: i32) -> Self {
        // Acquired BEFORE the wisdom-generation pass below, not just
        // around OpenChannel -- see wdsp_sys::SETUP_LOCK's doc comment.
        // Wisdom generation on a fresh machine/config can itself run
        // long enough for a concurrent TXA channel's own OpenChannel
        // call (tx.rs's TxProcessor::open, started unconditionally
        // alongside RX at initial connect) to land in the middle of it
        // if this guard were taken any later, which is exactly what
        // produced a real "double free or corruption (!prev)" crash on
        // first launch.
        let _setup_guard = wdsp::SETUP_LOCK.lock().unwrap();

        static WISDOM_LOADED: std::sync::Once = std::sync::Once::new();
        WISDOM_LOADED.call_once(|| {
            // FFTW plan computation for large transforms (our analyzer
            // goes up to 262144 points) can be genuinely slow unless
            // it's loading cached "wisdom" instead of measuring fresh.
            // This is likely the real cause of the slow startup/restart
            // -- called once per process, before any OpenChannel, using
            // a persistent (not temp) directory so the cache built up
            // on one run actually speeds up the next one too. First run
            // ever on a given machine may still be slow while wisdom is
            // first computed; subsequent runs should be much faster.
            // See config::settings_dir's doc comment -- shared, platform-
            // correct settings directory logic (this used to duplicate
            // an $HOME/.config-only version of the same thing here).
            if let Some(dir) = crate::config::settings_dir().map(|d| d.join("wdsp_wisdom")) {
                if std::fs::create_dir_all(&dir).is_ok() {
                    if let Ok(cstring) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) {
                        unsafe {
                            wdsp::WDSPwisdom(cstring.as_ptr());
                        }
                    }
                }
            }
        });

        // _setup_guard (acquired above, before the wisdom pass) stays
        // held through this whole OpenChannel/SetAnalyzer/SetDisplay*
        // sequence too, until this function returns -- see its
        // acquisition above for why. Each channel's own steady-state
        // feed()/demod() loop afterward is untouched by this lock and
        // continues to run fully concurrently, matching the fact that
        // multiple receivers work fine together once they're all past
        // this one-time setup.

        unsafe {
            wdsp::OpenChannel(
                channel,
                BUFFER_SIZE as i32,
                RXA_FFT_SIZE,
                sample_rate,
                DSP_RATE,
                OUTPUT_RATE,
                0, // type: RXA
                1, // state: running
                0.010,
                0.025,
                0.0,
                0.010,
                0,
            );
            // Belt-and-suspenders: OpenChannel's input_samplerate param
            // may only be initial setup, with the resampler itself not
            // actually (re)configured until this explicit call. Without
            // it, audio at any rate other than 48kHz (where no
            // resampling is needed anyway) came out distorted --
            // consistent with the resampler running uninitialized.
            wdsp::SetInputSamplerate(channel, sample_rate);

            // ROOT CAUSE FIX: RXA's PatchPanel output stage defaults to
            // "binaural" mode (confirmed via the WDSP Guide: fexchange0's
            // "out" buffer is documented as interleaved I/Q even for
            // audio, and SetRXAPanelCopy/SetRXAPanelBinaural's own
            // documented default is copy=0/bin=1, i.e. binaural -- I and
            // Q are genuinely DIFFERENT audio content by design, an
            // intentional SDR "phasing" stereo-listening effect, not a
            // duplicated mono signal). demod()'s output was previously
            // read assuming I and Q were interchangeable/duplicate mono
            // -- confirmed via a real packet capture of the RX-audio-to-
            // radio feature that they are not (a strong near-Nyquist
            // artifact from treating two genuinely different interleaved
            // channels as one flat stream), and averaging them (a first
            // attempted fix) also sounded wrong, since averaging two
            // signals with a real phase/content relationship isn't the
            // same as them being identical. At the time this project had
            // no UI for binaural listening, and every consumer (local
            // playback, TCI, radio-audio) expected a single real mono
            // channel -- forcing monaural mode at the source (copy I to
            // Q) was the fix.
            //
            // UPDATE: binaural listening is now a real, user-toggleable
            // feature (DemodParams::binaural) -- see demod()'s own
            // per-frame SetRXAPanelBinaural call (mirrors every other
            // dynamic Set*Run toggle in this file) for where it's
            // actually controlled now. This call just establishes the
            // same off-by-default startup state DemodParams::binaural's
            // own Default already implies, before the first demod() call
            // takes over -- radio-audio (the one consumer that still
            // always wants mono, on purpose -- see run()'s own handling)
            // downmixes L+R regardless of this setting, so that consumer
            // was never actually dependent on RXA itself staying
            // monaural, only demod()'s OWN interpretation of the buffer
            // was (now fixed to read both slots for real).
            wdsp::SetRXAPanelBinaural(channel, 0);

            // RXA PatchPanel's own gain1 also defaults to 4.0 (per the
            // WDSP Guide's SetRXAPanelGain1 doc: "[default = 4.0]"),
            // i.e. WDSP itself already amplifies the audio 4x before it
            // ever reaches this project's own Audio Gain slider (0.0 to
            // 1.5). Confirmed as a real usability problem via a live
            // report: even the slider's smallest practical values
            // (0.05, 0.01 -- both far below its 0.05 step granularity's
            // natural range) still drove WSJT-X's own level meter to
            // 30-60dB (uncomfortably hot for its recommended ~30-40dB
            // range). Resetting WDSP's own gain to unity here means the
            // Audio Gain slider's already-existing 0.0-1.5 range now
            // covers the actually-useful levels, instead of everything
            // being 4x hotter than the slider implies.
            wdsp::SetRXAPanelGain1(channel, 1.0);

            // Creates the noise blanker DSP objects themselves -- must
            // happen before the Set*Samplerate calls below, since those
            // configure an object that has to exist first. Parameters
            // confirmed from a working reference implementation, not
            // guessed. Created with run=0 (off) since DemodParams
            // defaults both stages off too -- the first demod() call
            // syncs the real enabled/disabled state via
            // SetEXTANBRun/SetEXTNOBRun regardless, this is just the
            // safe starting point before that first call.
            wdsp::create_anbEXT(
                channel,
                0, // run
                BUFFER_SIZE as i32,
                sample_rate as f64,
                0.0001, // tau
                0.0001, // hangtime
                0.0001, // advtime
                0.05,   // backtau
                20.0,   // threshold
            );
            wdsp::create_nobEXT(
                channel,
                0, // run
                0, // mode
                BUFFER_SIZE as i32,
                sample_rate as f64,
                0.0001, // slewtime
                0.0001, // hangtime
                0.0001, // advtime
                0.05,   // backtau
                20.0,   // threshold
            );

            wdsp::SetEXTANBSamplerate(channel, sample_rate);
            wdsp::SetEXTNOBSamplerate(channel, sample_rate);

            let mut success: c_int = 0;
            let wisdom_dir = std::env::temp_dir().join("wdsp");
            let _ = std::fs::create_dir_all(&wisdom_dir);
            let path_cstring = CString::new(wisdom_dir.to_string_lossy().as_bytes())
                .unwrap_or_else(|_| CString::new("/tmp").unwrap());
            wdsp::XCreateAnalyzer(
                channel,
                &mut success,
                ANALYZER_MAX_SIZE,
                1,
                1,
                path_cstring.as_ptr() as *mut std::os::raw::c_char,
            );
            // success's out-param convention isn't documented here, but
            // every other WDSP error out-param in this file (fexchange0's
            // `error`) is 0 = ok, so treat nonzero the same way. Added
            // while chasing a report that receivers beyond the 4th
            // (channel index 3) show no spectrum/waterfall at all --
            // this is the one place WDSP itself can report "I refused to
            // create channel N" rather than just silently producing no
            // data, so surfacing it is the fastest way to tell a WDSP-
            // side channel limit apart from an IQ-data-never-arrives
            // problem upstream (radio/network) of WDSP entirely.
            if success != 0 {
                eprintln!(
                    "spectrum: XCreateAnalyzer(channel={channel}) returned success={success} \
                     (nonzero -- WDSP likely refused to create this channel's analyzer; if this \
                     is a higher-numbered extra receiver, that spectrum/waterfall will stay empty)"
                );
            }

            let pixels = SPECTRUM_WIDTH * ZOOM;
            // a_fft_size must be large enough for two independent
            // reasons: enough bins for the requested pixel width, AND
            // enough samples that sample_rate/FPS of them arrive within
            // one window (otherwise `overlap` below can't compensate --
            // it can't go negative -- and updates fire faster than the
            // target FPS at high sample rates instead of staying fixed).
            let min_for_rate = (sample_rate as f32 / SPECTRUM_FPS).ceil() as i32;
            let required = pixels.max(min_for_rate);
            let a_fft_size = if required <= 16384 {
                16384
            } else if required <= 32768 {
                32768
            } else if required <= 65536 {
                65536
            } else if required <= 131072 {
                131072
            } else {
                262144
            };

            let overlap = std::cmp::max(
                0,
                (a_fft_size as f32 - sample_rate as f32 / SPECTRUM_FPS).ceil() as i32,
            );
            let max_w = a_fft_size
                + std::cmp::min(
                    (KEEP_TIME * SPECTRUM_FPS) as i32,
                    (KEEP_TIME * a_fft_size as f32 * SPECTRUM_FPS) as i32,
                );

            let mut flp = [0i32];
            wdsp::SetAnalyzer(
                channel,
                2, // n_pixout: spectrum + waterfall
                1, // n_fft: not using spur elimination
                1, // typ: complex samples
                flp.as_mut_ptr(),
                a_fft_size,
                BUFFER_SIZE as i32,
                WIN_TYPE,
                14.0, // "pi" param -- confirmed literal from source, not actually pi
                overlap,
                0,   // clp
                0.0, // fscLin
                0.0, // fscHin
                pixels,
                1, // n_stch
                0, // calset
                0.0,
                0.0,
                max_w,
            );

            wdsp::SetDisplayDetectorMode(channel, 0, wdsp::DETECTOR_MODE_AVERAGE as c_int);
            wdsp::SetDisplayAverageMode(channel, 0, wdsp::AVERAGE_MODE_LOG_RECURSIVE as c_int);
            wdsp::SetDisplayDetectorMode(channel, 1, wdsp::DETECTOR_MODE_AVERAGE as c_int);
            wdsp::SetDisplayAverageMode(channel, 1, wdsp::AVERAGE_MODE_LOG_RECURSIVE as c_int);
            wdsp::SetDisplayNormOneHz(channel, 0, 1);
            wdsp::SetDisplaySampleRate(channel, pixels); // matches confirmed source verbatim

            // WDSP's RXA output frame count shrinks with the decimation
            // ratio down to the fixed 48kHz DSP/output rate -- confirmed
            // from a working reference implementation. Previously this
            // was always BUFFER_SIZE, meaning at anything above 2:1
            // decimation most of the "output" was stale/uninitialized
            // data past what WDSP actually wrote, which is exactly what
            // was heard as distortion.
            let ratio = (sample_rate / DSP_RATE).max(1) as usize;
            let output_samples = BUFFER_SIZE / ratio;

            SpectrumAnalyzer {
                channel,
                iq_scratch: vec![0.0; BUFFER_SIZE * 2],
                spectrum_pixels: vec![0.0; pixels as usize],
                waterfall_pixels: vec![0.0; pixels as usize],
                demod_iq_scratch: vec![0.0; BUFFER_SIZE * 2],
                nb_scratch: vec![0.0; BUFFER_SIZE * 2],
                demod_audio_scratch: vec![0.0; output_samples * 2],
                last_mode: None,
                last_passband: None,
                last_agc: None,
                last_agc_params: None,
                last_nb_enabled: None,
                last_nb_threshold: None,
                last_nr_enabled: None,
                last_snb_enabled: None,
                last_anf_enabled: None,
                last_binaural: None,
                last_ctun: None,
                last_ctun_offset: None,
                last_lo_frequency: None,
                last_eq: None,
                last_fexchange_error: None,
                last_zoom: Some(1),
                last_pan: Some(0.0),
            }
        }
    }

    /// Live analyzer reconfigure for Zoom/Pan -- ROOT CAUSE FIX for a
    /// real report: the previous zoom implementation just cropped/
    /// stretched the SAME fixed-resolution (SPECTRUM_WIDTH-bin) trace
    /// across the display, a purely cosmetic zoom with no real gain in
    /// spectral detail. Confirmed against piHPSDR (receiver.c's
    /// rx_set_analyzer) and rustyHPSDR (receiver/mod.rs's
    /// init_analyzer) that both instead grow the actual FFT size
    /// (afft_size) with zoom, so a zoomed-in view genuinely resolves
    /// finer detail, not just bigger pixels of the same coarse data.
    ///
    /// This follows piHPSDR's specific variant (rather than
    /// rustyHPSDR's): the OUTPUT pixel count stays fixed at
    /// SPECTRUM_WIDTH regardless of zoom -- only the underlying FFT
    /// size grows, with WDSP itself (via the fscLin/fscHin clip
    /// parameters below) discarding everything outside the
    /// zoomed+panned sub-band and rebinning just that portion down to
    /// SPECTRUM_WIDTH pixels. That means the pixel arrays this project
    /// already allocates at a fixed size (spectrum_pixels/
    /// waterfall_pixels above, and every consumer downstream --
    /// build_waterfall_image, the spectrum trace's bin-to-x mapping in
    /// main.rs) need no resizing when zoom changes: WDSP returns
    /// already-cropped-to-the-visible-window data, so main.rs's trace/
    /// waterfall rendering can go back to a plain full-width mapping --
    /// only the frequency-axis labels/overlays need to know the
    /// current visible window's bounds (unchanged from the previous
    /// implementation's visible_half_span_hz/pan_offset_hz math).
    ///
    /// `zoom` is clamped to >= 1 (1 = no zoom, full span, matches
    /// open()'s own initial configuration). `pan` is -1.0 (view the
    /// lowest frequency the current zoom can reach) to +1.0 (highest),
    /// same convention as piHPSDR's pan slider (-100..100) just
    /// rescaled -- confirmed identical by comparing the pl/pr formula
    /// below against receiver.c's rx_set_analyzer.
    fn set_zoom_pan(&mut self, zoom: i32, pan: f32, sample_rate: i32) {
        let zoom = zoom.max(1);
        let pan = pan.clamp(-1.0, 1.0);
        if self.last_zoom == Some(zoom) && self.last_pan == Some(pan) {
            return;
        }
        self.last_zoom = Some(zoom);
        self.last_pan = Some(pan);

        // Same tiered afft_size ladder as open()'s own initial
        // SetAnalyzer call (there hardcoded to zoom=1's degenerate
        // case, SPECTRUM_WIDTH * 1) -- confirmed against both
        // piHPSDR's and rustyHPSDR's identical tier thresholds.
        let pixels = SPECTRUM_WIDTH;
        let min_for_rate = (sample_rate as f32 / SPECTRUM_FPS).ceil() as i32;
        let required = (pixels * zoom).max(min_for_rate);
        let a_fft_size = if required <= 16384 {
            16384
        } else if required <= 32768 {
            32768
        } else if required <= 65536 {
            65536
        } else if required <= 131072 {
            131072
        } else {
            262144
        };

        // Bins to clip from the low/high end of the full afft_size-point
        // FFT so only the zoomed+panned sub-band remains -- ported
        // directly from piHPSDR's receiver.c (rx_set_analyzer): zz is
        // the total bin count to discard; pl/pr split that between the
        // low and high ends according to pan (pan=-1.0 -> pl=0.0, all
        // discarded from the high end, i.e. the visible window sits at
        // the LOW end of the full span; pan=+1.0 -> the reverse).
        // Zero at zoom=1 automatically (zz=0), matching open()'s own
        // hardcoded 0.0/0.0 for that case with no special-casing needed.
        let zz = a_fft_size as f64 * (1.0 - 1.0 / zoom as f64);
        let pl = 0.5 * (pan as f64 + 1.0);
        let pr = 1.0 - pl;
        let fsc_lin = pl * zz;
        let fsc_hin = pr * zz;

        let overlap = std::cmp::max(
            0,
            (a_fft_size as f32 - sample_rate as f32 / SPECTRUM_FPS).ceil() as i32,
        );
        let max_w = a_fft_size
            + std::cmp::min(
                (KEEP_TIME * SPECTRUM_FPS) as i32,
                (KEEP_TIME * a_fft_size as f32 * SPECTRUM_FPS) as i32,
            );

        let mut flp = [0i32];
        unsafe {
            wdsp::SetAnalyzer(
                self.channel,
                2, // n_pixout: spectrum + waterfall
                1, // n_fft
                1, // typ: complex samples
                flp.as_mut_ptr(),
                a_fft_size,
                BUFFER_SIZE as i32,
                WIN_TYPE,
                14.0, // "pi" param -- see open()'s identical call
                overlap,
                0, // clp
                fsc_lin,
                fsc_hin,
                pixels,
                1, // n_stch
                0, // calset
                0.0,
                0.0,
                max_w,
            );
            wdsp::SetDisplayDetectorMode(self.channel, 0, wdsp::DETECTOR_MODE_AVERAGE as c_int);
            wdsp::SetDisplayAverageMode(self.channel, 0, wdsp::AVERAGE_MODE_LOG_RECURSIVE as c_int);
            wdsp::SetDisplayDetectorMode(self.channel, 1, wdsp::DETECTOR_MODE_AVERAGE as c_int);
            wdsp::SetDisplayAverageMode(self.channel, 1, wdsp::AVERAGE_MODE_LOG_RECURSIVE as c_int);
            wdsp::SetDisplayNormOneHz(self.channel, 0, 1);
            // width*zoom (the UNSNAPPED value, not a_fft_size) -- matches
            // piHPSDR's SetDisplaySampleRate(rx->id, rx->width*rx->zoom)
            // exactly; this normalizes for the per-pixel resolution
            // actually represented at this zoom level, not the
            // (possibly much larger, tier-snapped) FFT size.
            wdsp::SetDisplaySampleRate(self.channel, pixels * zoom);
        }
    }

    /// Feed exactly BUFFER_SIZE IQ samples in. Returns (spectrum_row,
    /// waterfall_row) whenever WDSP reports fresh pixel data for that
    /// stream via its flag out-param.
    fn feed(&mut self, samples: &[IqSample]) -> (Option<Vec<f32>>, Option<Vec<f32>>) {
        debug_assert_eq!(samples.len(), BUFFER_SIZE);
        for (i, s) in samples.iter().enumerate() {
            self.iq_scratch[i * 2] = s.i as f64 / IQ_NORM;
            self.iq_scratch[i * 2 + 1] = s.q as f64 / IQ_NORM;
        }

        unsafe {
            wdsp::Spectrum0(1, self.channel, 0, 0, self.iq_scratch.as_mut_ptr());

            let mut spectrum_flag: c_int = 0;
            wdsp::GetPixels(
                self.channel,
                0,
                self.spectrum_pixels.as_mut_ptr(),
                &mut spectrum_flag,
            );

            let mut waterfall_flag: c_int = 0;
            wdsp::GetPixels(
                self.channel,
                1,
                self.waterfall_pixels.as_mut_ptr(),
                &mut waterfall_flag,
            );

            let spectrum = (spectrum_flag != 0).then(|| self.spectrum_pixels.clone());
            let waterfall = (waterfall_flag != 0).then(|| self.waterfall_pixels.clone());
            (spectrum, waterfall)
        }
    }

    /// Runs the actual RXA demodulator chain (mode + filter + AGC +
    /// noise blanker/reduction etc.) on exactly BUFFER_SIZE IQ samples
    /// and returns interleaved stereo f32 audio at the fixed 48kHz
    /// DSP/output rate. Uses its own scratch buffers, separate from
    /// feed()'s, since it's not confirmed whether Spectrum0/fexchange0
    /// mutate their input buffers in place.
    fn demod(&mut self, samples: &[IqSample], params: DemodParams, passband: (f64, f64)) -> Vec<(f32, f32)> {
        debug_assert_eq!(samples.len(), BUFFER_SIZE);

        if self.last_mode != Some(params.mode) {
            unsafe {
                wdsp::SetRXAMode(self.channel, params.mode as c_int);
            }
            self.last_mode = Some(params.mode);
        }
        if self.last_passband != Some(passband) {
            unsafe {
                wdsp::RXASetPassband(self.channel, passband.0, passband.1);
                // SNBA's own analysis bandwidth -- same (low, high) as
                // the demod passband above, so its noise estimate
                // tracks whatever's actually being listened to rather
                // than a stale/independent range.
                wdsp::SetRXASNBAOutputBandwidth(self.channel, passband.0, passband.1);
            }
            self.last_passband = Some(passband);
        }
        if self.last_agc != Some(params.agc) {
            unsafe {
                wdsp::SetRXAAGCMode(self.channel, params.agc as c_int);
            }
            self.last_agc = Some(params.agc);
        }
        let agc_params = (
            params.agc_attack_ms,
            params.agc_decay_ms,
            params.agc_hang_ms,
            params.agc_top_db,
            params.agc_slope_db,
            params.agc_thresh_db,
        );
        if self.last_agc_params != Some(agc_params) {
            let (attack_ms, decay_ms, hang_ms, top_db, slope_db, thresh_db) = agc_params;
            unsafe {
                wdsp::SetRXAAGCAttack(self.channel, attack_ms);
                wdsp::SetRXAAGCDecay(self.channel, decay_ms);
                wdsp::SetRXAAGCHang(self.channel, hang_ms);
                wdsp::SetRXAAGCTop(self.channel, top_db);
                wdsp::SetRXAAGCSlope(self.channel, slope_db);
                // size/rate: not user-exposed -- their exact meaning
                // (likely AGC lookup-table size and processing rate)
                // isn't confirmed against your reference, so passing
                // plausible fixed values tied to our own buffer/DSP
                // rate rather than guessing something tunable.
                wdsp::SetRXAAGCThresh(self.channel, thresh_db, BUFFER_SIZE as f64, DSP_RATE as f64);
            }
            self.last_agc_params = Some(agc_params);
        }

        // Noise blanker: NB (ANB) and NB2 (NOB) are mutually exclusive
        // (see NoiseBlanker's doc comment), so setting one's Run flag
        // always means clearing the other's -- both objects already
        // exist (created in open()), this just keeps their enabled
        // state and shared threshold in sync with DemodParams.
        if self.last_nb_enabled != Some(params.noise_blanker) {
            let (nb_run, nb2_run) = match params.noise_blanker {
                NoiseBlanker::Off => (0, 0),
                NoiseBlanker::Nb => (1, 0),
                NoiseBlanker::Nb2 => (0, 1),
            };
            unsafe {
                wdsp::SetEXTANBRun(self.channel, nb_run);
                wdsp::SetEXTNOBRun(self.channel, nb2_run);
            }
            self.last_nb_enabled = Some(params.noise_blanker);
        }
        if self.last_nb_threshold != Some(params.nb_threshold) {
            unsafe {
                wdsp::SetEXTANBThreshold(self.channel, params.nb_threshold);
                wdsp::SetEXTNOBThreshold(self.channel, params.nb_threshold);
            }
            self.last_nb_threshold = Some(params.nb_threshold);
        }

        // Noise reduction: NR (ANR), NR2 (EMNR), NR3 (RNNR), and NR4
        // (SBNR) are all mutually exclusive. All four live inside the
        // RXA chain itself, so switching is just a set of Set*Run
        // calls -- fexchange0 below picks them up with no extra buffer
        // plumbing needed.
        if self.last_nr_enabled != Some(params.noise_reduction) {
            let (nr_run, nr2_run, nr3_run, nr4_run) = match params.noise_reduction {
                NoiseReduction::Off => (0, 0, 0, 0),
                NoiseReduction::Nr => (1, 0, 0, 0),
                NoiseReduction::Nr2 => (0, 1, 0, 0),
                NoiseReduction::Nr3 => (0, 0, 1, 0),
                NoiseReduction::Nr4 => (0, 0, 0, 1),
            };
            unsafe {
                wdsp::SetRXAANRRun(self.channel, nr_run);
                wdsp::SetRXAEMNRRun(self.channel, nr2_run);
                wdsp::SetRXARNNRRun(self.channel, nr3_run);
                wdsp::SetRXASBNRRun(self.channel, nr4_run);
            }
            self.last_nr_enabled = Some(params.noise_reduction);
        }

        // SNB ("Spectral Noise Blanker", WDSP's SNBA stage) -- see
        // DemodParams::snb's doc comment for why this is independent
        // of the NoiseReduction mutex above rather than folded into it.
        if self.last_snb_enabled != Some(params.snb) {
            unsafe {
                wdsp::SetRXASNBARun(self.channel, params.snb as c_int);
            }
            self.last_snb_enabled = Some(params.snb);
        }

        // ANF ("Automatic Notch Filter", WDSP's ANF stage) -- see
        // DemodParams::anf's doc comment. Same independent-toggle
        // treatment as SNB just above.
        if self.last_anf_enabled != Some(params.anf) {
            unsafe {
                wdsp::SetRXAANFRun(self.channel, params.anf as c_int);
            }
            self.last_anf_enabled = Some(params.anf);
        }

        // Binaural ("phasing") RX audio -- see DemodParams::binaural's
        // doc comment. Same edge-detected Set*-call pattern as every
        // other toggle above; off by default matches open()'s own
        // startup call, this just takes over from there.
        if self.last_binaural != Some(params.binaural) {
            unsafe {
                wdsp::SetRXAPanelBinaural(self.channel, params.binaural as c_int);
            }
            self.last_binaural = Some(params.binaural);
        }

        // Graphic EQ -- see EqualizerParams's doc comment for the two
        // band layouts. Matches piHPSDR's own init sequence
        // (receiver.c/transmitter.c): coefficients first, Run flag
        // second, every time ANYTHING changes (enable, band count, or
        // any single slider) -- all folded into one cheap outer compare
        // (self.last_eq != Some(params.eq)) same as last_agc_params.
        if self.last_eq != Some(params.eq) {
            unsafe {
                match params.eq.band_count {
                    EqBandCount::Three => {
                        let mut coeffs = [
                            params.eq.preamp_db,
                            params.eq.bands_3_db[0],
                            params.eq.bands_3_db[1],
                            params.eq.bands_3_db[2],
                        ];
                        wdsp::SetRXAGrphEQ(self.channel, coeffs.as_mut_ptr());
                    }
                    EqBandCount::Ten => {
                        let mut coeffs = [0i32; 11];
                        coeffs[0] = params.eq.preamp_db;
                        coeffs[1..11].copy_from_slice(&params.eq.bands_10_db);
                        wdsp::SetRXAGrphEQ10(self.channel, coeffs.as_mut_ptr());
                    }
                }
                wdsp::SetRXAEQRun(self.channel, params.eq.enabled as c_int);
            }
            self.last_eq = Some(params.eq);
        }

        // CTUN ("Click to Tune"): confirmed against a working reference
        // (rustyHPSDR). SetRXAShiftFreq shifts the IQ into the RXA
        // demod chain so the passband tracks the CTUN'd frequency
        // without retuning the radio; RXANBPSetShiftFrequency keeps the
        // noise-blanker preprocessor's own frequency reference in sync
        // with that same shift. These are mutually exclusive with
        // RXANBPSetTuneFrequency below -- the NBP object tracks either
        // an absolute tuned frequency or a shift, never both at once,
        // matching the reference's own if/else (never both branches in
        // the same call).
        if params.ctun {
            if self.last_ctun != Some(true) {
                unsafe {
                    wdsp::SetRXAShiftRun(self.channel, 1);
                }
                self.last_ctun = Some(true);
            }
            if self.last_ctun_offset != Some(params.ctun_offset_hz) {
                unsafe {
                    wdsp::SetRXAShiftFreq(self.channel, params.ctun_offset_hz);
                    wdsp::RXANBPSetShiftFrequency(self.channel, params.ctun_offset_hz);
                }
                self.last_ctun_offset = Some(params.ctun_offset_hz);
            }
        } else {
            if self.last_ctun != Some(false) {
                unsafe {
                    wdsp::SetRXAShiftFreq(self.channel, 0.0);
                    wdsp::SetRXAShiftRun(self.channel, 0);
                }
                self.last_ctun = Some(false);
                self.last_ctun_offset = Some(0.0);
            }
            if self.last_lo_frequency != Some(params.lo_frequency_hz) {
                unsafe {
                    wdsp::RXANBPSetTuneFrequency(self.channel, params.lo_frequency_hz);
                }
                self.last_lo_frequency = Some(params.lo_frequency_hz);
            }
        }

        for (i, s) in samples.iter().enumerate() {
            self.demod_iq_scratch[i * 2] = s.i as f64 / IQ_NORM;
            self.demod_iq_scratch[i * 2 + 1] = s.q as f64 / IQ_NORM;
        }

        // ANB/NOB are external pre-filters that run on the raw
        // ADC-rate IQ *before* it enters the RXA chain (unlike NR,
        // which lives inside fexchange0) -- so they need an explicit
        // call here rather than just a Set*Run flag. Ping-ponged
        // through nb_scratch rather than called in-place, since
        // xanbEXT/xnobEXT's in-place behavior isn't confirmed. Both
        // are called unconditionally and rely on their own Run flag
        // (set just above) to no-op when disabled, same convention as
        // every other WDSP Set*Run flag in this file.
        unsafe {
            wdsp::xanbEXT(self.channel, self.demod_iq_scratch.as_mut_ptr(), self.nb_scratch.as_mut_ptr());
            wdsp::xnobEXT(self.channel, self.nb_scratch.as_mut_ptr(), self.demod_iq_scratch.as_mut_ptr());
        }

        let mut error: c_int = 0;
        unsafe {
            wdsp::fexchange0(
                self.channel,
                self.demod_iq_scratch.as_mut_ptr(),
                self.demod_audio_scratch.as_mut_ptr(),
                &mut error,
            );
        }
        // Edge-triggered (not every call, which would fire ~47 times/sec
        // per channel) -- added alongside XCreateAnalyzer's success
        // check above while chasing a report that higher-numbered extra
        // receivers (channel index 4+) show no spectrum/waterfall.
        // fexchange0's error isn't documented here either, but pairs
        // with the same "0 = ok" convention as everything else in this
        // file that reports one.
        let now_erroring = error != 0;
        if self.last_fexchange_error != Some(now_erroring) {
            if now_erroring {
                eprintln!("spectrum: fexchange0(channel={}) reporting error={error}", self.channel);
            }
            self.last_fexchange_error = Some(now_erroring);
        }

        // fexchange0 writes interleaved I/Q (WDSP's generic complex
        // buffer convention, per the WDSP Guide -- used even for a real
        // audio signal, not a true I-Q signal) into demod_audio_scratch,
        // confirmed by its own allocation above (`output_samples * 2`).
        // Read BOTH elements of each pair as (L, R) now that binaural is
        // a real, dynamically-toggled setting (see DemodParams::
        // binaural's doc comment) -- when it's off, SetRXAPanelBinaural
        // above already makes WDSP copy I to Q at the source, so L==R
        // and this is a strict superset of the old (I-only) behavior,
        // not a change for that case. The near-Nyquist artifact this
        // file's history refers to came from treating the two slots as
        // interchangeable/duplicate while binaural was WDSP's own
        // default-on state and nothing here accounted for that -- this
        // reads them honestly as a real stereo pair either way now.
        self.demod_audio_scratch
            .chunks_exact(2)
            .map(|p| (p[0] as f32, p[1] as f32))
            .collect()
    }

    /// Reads the RXA S-meter (averaged). Must be called from the same
    /// thread as every other WDSP call on this channel -- WDSP isn't
    /// confirmed thread-safe for concurrent access from multiple
    /// threads, so this can't be polled directly from the UI thread.
    fn meter_db(&self) -> f64 {
        unsafe { wdsp::GetRXAMeter(self.channel, wdsp::rxaMeterType_RXA_S_AV as c_int) }
    }
}

impl Drop for SpectrumAnalyzer {
    fn drop(&mut self) {
        unsafe {
            wdsp::DestroyAnalyzer(self.channel);
            wdsp::CloseChannel(self.channel);
        }
    }
}

fn run(
    channel: i32,
    iq_buffer: Arc<Mutex<VecDeque<IqSample>>>,
    sample_rate: i32,
    display: Arc<Mutex<SpectrumDisplay>>,
    demod_params: Arc<Mutex<DemodParams>>,
    audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    tci_audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    waveform_out: Arc<Mutex<VecDeque<f32>>>,
    iq_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    rx_audio_to_radio: Option<Arc<Mutex<VecDeque<f32>>>>,
    // Muted (not pushed to any of the four audio outputs above) while
    // MOX is active -- see SpectrumHandle::start's doc comment on why.
    mox: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut analyzer = SpectrumAnalyzer::open(channel, sample_rate);
    let mut chunk = Vec::with_capacity(BUFFER_SIZE);
    // Anti-aliasing lowpass for rx_audio_to_radio only -- confirmed via a
    // real packet capture (radio.rs's RX-audio-to-radio feature) that
    // WDSP's raw 48kHz RXA output carries a large, persistent near-
    // Nyquist component (~22-23kHz, comparable in amplitude to the real
    // demodulated tone itself) that's audible as static/scratchiness --
    // and, riding right next to a narrow tone like FT8's, throws off its
    // perceived pitch too. The local AudioOutput/cpal path and TCI don't
    // exhibit this audibly, almost certainly because the OS audio stack's
    // own sample-rate-conversion to the sound card's native rate applies
    // a proper reconstruction filter along the way; the radio's own local
    // audio DAC, fed these raw samples directly over the network with no
    // such filtering, has nothing to remove it. Two cascaded single-pole
    // stages (RxAudioLowpass::feed, ~12dB/octave) at a 4kHz cutoff --
    // generous headroom above any voice/digital-mode passband, verified
    // against the actual captured samples to fully remove the ~22-23kHz
    // component while leaving a real ~1.3kHz tone untouched.
    let mut radio_audio_lpf = RxAudioLowpass::new(OUTPUT_RATE as f32);

    while !stop.load(Ordering::Relaxed) {
        chunk.clear();
        {
            let mut buf = iq_buffer.lock().unwrap();
            if buf.len() >= BUFFER_SIZE {
                chunk.extend(buf.drain(..BUFFER_SIZE));
            }
        }

        if chunk.len() < BUFFER_SIZE {
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        // Raw wideband IQ tap for TCI's iq_start streaming -- a
        // dedicated copy, not a second reader of iq_buffer (which
        // this loop already drains via .drain() above; a second
        // consumer popping the same queue would steal samples from
        // this one instead of getting its own copy). Same
        // normalization already used elsewhere in this file (IQ_NORM
        // = 2^23).
        {
            let mut out = iq_out.lock().unwrap();
            for s in &chunk {
                if out.len() >= IQ_OUT_CAPACITY {
                    out.pop_front();
                }
                out.push_back((s.i as f32 / IQ_NORM as f32, s.q as f32 / IQ_NORM as f32));
            }
        }

        // Loaded before feed() (not just before demod() further down,
        // where this used to be read) so set_zoom_pan can reconfigure
        // the analyzer -- a rare, edge-detected event, see its own doc
        // comment -- ahead of this iteration's Spectrum0/GetPixels call.
        let params = *demod_params.lock().unwrap();
        analyzer.set_zoom_pan(params.zoom, params.pan, sample_rate);

        let (spectrum, waterfall) = analyzer.feed(&chunk);
        if spectrum.is_some() || waterfall.is_some() {
            let mut d = display.lock().unwrap();
            if let Some(s) = spectrum {
                d.spectrum = s;
            }
            if let Some(w) = waterfall {
                d.waterfall_rows.push_front(w);
                if d.waterfall_rows.len() > WATERFALL_HISTORY {
                    d.waterfall_rows.pop_back();
                }
            }
            d.revision = d.revision.wrapping_add(1);
        }

        let passband = passband_for(params.mode, params.width_hz);
        let audio = analyzer.demod(&chunk, params, passband);
        let meter_db = analyzer.meter_db();
        display.lock().unwrap().meter_db = meter_db;
        {
            let mut out = audio_out.lock().unwrap();
            // Dedicated tap for TCI's audio_start streaming -- same
            // reasoning as iq_out above: audio_out is already drained
            // by the local AudioOutput speaker playback, so TCI needs
            // its own copy rather than a second reader of that same
            // queue (which would steal samples from local playback).
            let mut tci_out = tci_audio_out.lock().unwrap();
            // Dedicated tap for the small audio-waveform display (see
            // main.rs's draw_audio_waveform) -- same "own copy, not a
            // second reader" reasoning as tci_out, and never drained by
            // anything except its own drop-oldest-on-overflow below, so
            // the UI can safely peek the most recent samples without
            // popping. Fed from the RAW pre-gain sample below (not
            // out/tci_out's Audio Gain-scaled one) so the display's
            // amplitude reflects the actual signal, not the speaker
            // volume control -- see the push site's own doc comment.
            let mut waveform_out = waveform_out.lock().unwrap();
            // Third dedicated tap, main receiver only (see
            // RadioSession::rx_audio_to_radio's doc comment) -- radio.rs
            // streams this back to the radio's own local audio output
            // when send_rx_audio_to_radio is on. Same "own copy, not a
            // second reader" reasoning as tci_out.
            let mut radio_out = rx_audio_to_radio.as_ref().map(|q| q.lock().unwrap());
            // ROOT CAUSE FIX: RX audio (all three taps -- local speaker,
            // TCI, and the radio's own local output) used to keep
            // flowing completely unmuted through TX, same as the RX ADC
            // itself genuinely does (PureSignal's feedback receiver
            // relies on that continuing) -- but unlike PureSignal's
            // feedback, which goes through a dedicated, deliberately
            // attenuated path, this is the MAIN receiver's normal,
            // full-gain audio output picking up whatever the antenna/T-R
            // relay leaks back from your own transmission. A real report
            // confirmed this as an actual acoustic feedback loop using
            // TCI Remote (an Android app) for TX: the phone's speaker
            // played this leaked audio back into its own mic while
            // transmitting. TCI Remote's own log confirmed it never asks
            // for RX audio to be gated during TX at all (only iq_stop/
            // iq_start around PTT, no audio_stop -- matching this
            // project's TCI server, which (see this file's own doc
            // comment on tci_audio_out) streams audio unconditionally
            // with no audio_start/audio_stop gate, mirroring rustyHPSDR's
            // confirmed-working reference behavior) -- so muting has to
            // happen here, at the source, not by expecting any TCI
            // client to ask for it. Spectrum/waterfall display and
            // PureSignal's own feedback path are untouched -- this only
            // gates the post-demod AUDIO taps below.
            let mox_active = mox.load(Ordering::Relaxed);
            for (l, r) in audio {
                if mox_active {
                    continue;
                }
                // Waveform tap and radio-audio-to-radio both stay a
                // plain mono downmix regardless of DemodParams::binaural
                // -- see that field's own doc comment: neither is a
                // "listening" consumer binaural panning matters for, and
                // downstream both still expect single f32 samples
                // (radio.rs's RX-audio-payload filler, main.rs's
                // waveform-drawing code). Fed from the RAW (pre-gain)
                // downmix, not the Audio Gain-scaled one below -- a real
                // report: tapping post-gain meant the display's
                // amplitude tracked the speaker volume control, not the
                // actual signal, so a normal listening gain showed a
                // flat line and only cranking Audio Gain to an
                // uncomfortably loud level made anything visible.
                let mono = (l + r) * 0.5;
                if waveform_out.len() >= WAVEFORM_TAP_CAPACITY {
                    waveform_out.pop_front();
                }
                waveform_out.push_back(mono.clamp(-1.0, 1.0));
                // Local speaker playback and TCI's RX audio stream carry
                // the real (l, r) pair -- identical (l==r) when binaural
                // is off, same as every consumer effectively saw before
                // this was ever a stereo pair at all.
                let (l, r) = ((l * params.gain).clamp(-1.0, 1.0), (r * params.gain).clamp(-1.0, 1.0));
                if out.len() >= AUDIO_BUFFER_CAPACITY {
                    out.pop_front();
                }
                out.push_back((l, r));
                if tci_out.len() >= AUDIO_BUFFER_CAPACITY {
                    tci_out.pop_front();
                }
                tci_out.push_back((l, r));
                if let Some(radio_out) = radio_out.as_mut() {
                    let mono_gained = (mono * params.gain).clamp(-1.0, 1.0);
                    let filtered = radio_audio_lpf.feed(mono_gained);
                    if radio_out.len() >= AUDIO_BUFFER_CAPACITY {
                        radio_out.pop_front();
                    }
                    radio_out.push_back(filtered);
                }
            }
        }
    }
}

/// Owns the background analyzer thread and the shared display state the
/// UI reads from each frame, plus the demodulator's audio output buffer
/// that the audio module consumes from.
pub struct SpectrumHandle {
    pub display: Arc<Mutex<SpectrumDisplay>>,
    /// (L, R) pairs -- identical when DemodParams::binaural is off
    /// (the common case), genuinely different when it's on. See that
    /// field's own doc comment and run()'s for how this gets filled.
    pub audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    /// Dedicated audio tap for TCI's audio_start streaming -- NOT the
    /// same queue as audio_out (which the local AudioOutput speaker
    /// playback already drains); see run()'s doc comment on why a
    /// second consumer of that same queue would glitch both. Same
    /// (L, R) pair shape as audio_out.
    pub tci_audio_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    /// Recent-history tap for the small audio-waveform display (see
    /// main.rs's draw_audio_waveform) -- never drained by anything but
    /// its own drop-oldest-on-overflow, so the UI thread can peek the
    /// tail of it each frame without disturbing audio_out/tci_audio_out's
    /// real consumers. Fed from the raw, pre-Audio-Gain sample (unlike
    /// audio_out/tci_audio_out), so the display's amplitude reflects the
    /// actual demodulated signal rather than the speaker volume control.
    /// Same ~300ms capacity as tx.rs's tx_audio_monitor, which the TX
    /// side of the same display reads from directly.
    pub waveform_out: Arc<Mutex<VecDeque<f32>>>,
    /// Raw wideband IQ tap for TCI's iq_start streaming, normalized
    /// the same way this file normalizes IQ elsewhere (IQ_NORM).
    pub iq_out: Arc<Mutex<VecDeque<(f32, f32)>>>,
    demod_params: Arc<Mutex<DemodParams>>,
    /// WDSP analyzer channel this handle's run() thread opened -- kept
    /// here too (not just inside that thread) so clear_display can
    /// reach the same WDSP display state from the UI thread. Analyzer
    /// setters are confirmed thread-safe from the C source itself
    /// (EnterCriticalSection/LeaveCriticalSection around every mode
    /// change in analyzer.c), so calling them from a different thread
    /// than the one that opened the channel is fine.
    channel: i32,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SpectrumHandle {
    /// `rx_audio_to_radio`: main receiver only -- pass `None` for extra
    /// receivers and the TX spectrum tap (see RadioSession::
    /// rx_audio_to_radio's doc comment for why only the main receiver
    /// feeds this).
    ///
    /// `mox`: this handle's three audio outputs (audio_out, tci_audio_out,
    /// and rx_audio_to_radio if present) are muted while it's true -- see
    /// run()'s doc comment for why. Always the session's real mox flag in
    /// practice (every call site has one -- MOX is a whole-session
    /// concept, not per-receiver), even for extra receivers/the TX
    /// spectrum tap, where muting is harmless (their audio_out/
    /// tci_audio_out aren't wired to anything that plays them back) --
    /// simpler than threading an Option through just to skip it there.
    pub fn start(
        channel: i32,
        iq_buffer: Arc<Mutex<VecDeque<IqSample>>>,
        sample_rate: i32,
        rx_audio_to_radio: Option<Arc<Mutex<VecDeque<f32>>>>,
        mox: Arc<AtomicBool>,
    ) -> Self {
        let display = Arc::new(Mutex::new(SpectrumDisplay::default()));
        let demod_params = Arc::new(Mutex::new(DemodParams::default()));
        let audio_out: Arc<Mutex<VecDeque<(f32, f32)>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(AUDIO_BUFFER_CAPACITY)));
        let tci_audio_out: Arc<Mutex<VecDeque<(f32, f32)>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(AUDIO_BUFFER_CAPACITY)));
        let waveform_out = Arc::new(Mutex::new(VecDeque::with_capacity(WAVEFORM_TAP_CAPACITY)));
        let iq_out = Arc::new(Mutex::new(VecDeque::with_capacity(IQ_OUT_CAPACITY)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let display = Arc::clone(&display);
            let demod_params = Arc::clone(&demod_params);
            let audio_out = Arc::clone(&audio_out);
            let tci_audio_out = Arc::clone(&tci_audio_out);
            let waveform_out = Arc::clone(&waveform_out);
            let iq_out = Arc::clone(&iq_out);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                run(
                    channel,
                    iq_buffer,
                    sample_rate,
                    display,
                    demod_params,
                    audio_out,
                    tci_audio_out,
                    waveform_out,
                    iq_out,
                    rx_audio_to_radio,
                    mox,
                    stop,
                )
            })
        };
        Self {
            display,
            audio_out,
            tci_audio_out,
            waveform_out,
            iq_out,
            demod_params,
            channel,
            stop,
            thread: Some(thread),
        }
    }

    /// Clears accumulated spectrum/waterfall history and WDSP's own
    /// internal recursive-average state -- see the call site's doc
    /// comment (main.rs, tx_spectrum on a fresh PTT) for why this
    /// exists: a long-lived tx_spectrum handle otherwise keeps
    /// blending/scrolling in whatever a *previous*, possibly very
    /// different transmission looked like for many seconds after a
    /// new one starts, since neither the Rust-side waterfall_rows
    /// history (WATERFALL_HISTORY = 200 rows, ~20s at this analyzer's
    /// ~10Hz rate) nor WDSP's own AVERAGE_MODE_LOG_RECURSIVE
    /// accumulator get reset just because a new PTT began.
    /// SetDisplayAverageMode's own C source (analyzer.c) only resets
    /// its internal av_sum accumulator when the mode value actually
    /// *changes* (`if (a->av_mode[pixout] != mode)`) -- so this
    /// toggles away to AVERAGE_MODE_NONE and back to force that reset
    /// rather than calling it once with the same mode, which would be
    /// a no-op.
    pub fn clear_display(&self) {
        *self.display.lock().unwrap() = SpectrumDisplay::default();
        unsafe {
            for pixout in 0..2 {
                wdsp::SetDisplayAverageMode(self.channel, pixout, wdsp::AVERAGE_MODE_NONE as c_int);
                wdsp::SetDisplayAverageMode(
                    self.channel,
                    pixout,
                    wdsp::AVERAGE_MODE_LOG_RECURSIVE as c_int,
                );
            }
        }
    }

    pub fn mode(&self) -> Mode {
        self.demod_params.lock().unwrap().mode
    }

    pub fn width_hz(&self) -> f64 {
        self.demod_params.lock().unwrap().width_hz
    }

    pub fn set_mode(&self, mode: Mode) {
        let mut p = self.demod_params.lock().unwrap();
        p.mode = mode;
    }

    pub fn set_width_hz(&self, width_hz: f64) {
        let mut p = self.demod_params.lock().unwrap();
        p.width_hz = width_hz;
    }

    pub fn gain(&self) -> f32 {
        self.demod_params.lock().unwrap().gain
    }

    pub fn set_gain(&self, gain: f32) {
        let mut p = self.demod_params.lock().unwrap();
        p.gain = gain.max(0.0);
    }

    pub fn agc(&self) -> Agc {
        self.demod_params.lock().unwrap().agc
    }

    pub fn set_agc(&self, agc: Agc) {
        let mut p = self.demod_params.lock().unwrap();
        p.agc = agc;
    }

    pub fn agc_params(&self) -> DemodParams {
        *self.demod_params.lock().unwrap()
    }

    pub fn set_agc_attack_ms(&self, v: i32) {
        self.demod_params.lock().unwrap().agc_attack_ms = v.max(0);
    }
    pub fn set_agc_decay_ms(&self, v: i32) {
        self.demod_params.lock().unwrap().agc_decay_ms = v.max(0);
    }
    pub fn set_agc_hang_ms(&self, v: i32) {
        self.demod_params.lock().unwrap().agc_hang_ms = v.max(0);
    }
    pub fn set_agc_top_db(&self, v: f64) {
        self.demod_params.lock().unwrap().agc_top_db = v;
    }
    pub fn set_agc_slope_db(&self, v: i32) {
        self.demod_params.lock().unwrap().agc_slope_db = v.max(0);
    }
    pub fn set_agc_thresh_db(&self, v: f64) {
        self.demod_params.lock().unwrap().agc_thresh_db = v;
    }

    pub fn noise_blanker(&self) -> NoiseBlanker {
        self.demod_params.lock().unwrap().noise_blanker
    }
    pub fn set_noise_blanker(&self, v: NoiseBlanker) {
        self.demod_params.lock().unwrap().noise_blanker = v;
    }

    pub fn nb_threshold(&self) -> f64 {
        self.demod_params.lock().unwrap().nb_threshold
    }
    pub fn set_nb_threshold(&self, v: f64) {
        self.demod_params.lock().unwrap().nb_threshold = v;
    }

    pub fn noise_reduction(&self) -> NoiseReduction {
        self.demod_params.lock().unwrap().noise_reduction
    }
    pub fn set_noise_reduction(&self, v: NoiseReduction) {
        self.demod_params.lock().unwrap().noise_reduction = v;
    }

    pub fn snb(&self) -> bool {
        self.demod_params.lock().unwrap().snb
    }
    pub fn set_snb(&self, v: bool) {
        self.demod_params.lock().unwrap().snb = v;
    }

    pub fn anf(&self) -> bool {
        self.demod_params.lock().unwrap().anf
    }
    pub fn set_anf(&self, v: bool) {
        self.demod_params.lock().unwrap().anf = v;
    }

    pub fn binaural(&self) -> bool {
        self.demod_params.lock().unwrap().binaural
    }
    pub fn set_binaural(&self, v: bool) {
        self.demod_params.lock().unwrap().binaural = v;
    }

    pub fn eq(&self) -> EqualizerParams {
        self.demod_params.lock().unwrap().eq
    }
    pub fn set_eq(&self, eq: EqualizerParams) {
        self.demod_params.lock().unwrap().eq = eq;
    }

    /// Enables/disables CTUN and sets the current offset (Hz) from the
    /// hardware/LO frequency to the CTUN'd listen frequency. Callers
    /// should pass offset_hz: 0.0 when disabling.
    pub fn set_ctun(&self, ctun: bool, offset_hz: f64) {
        let mut p = self.demod_params.lock().unwrap();
        p.ctun = ctun;
        p.ctun_offset_hz = offset_hz;
    }

    /// See DemodParams::zoom/pan's doc comment. Picked up by the
    /// analyzer thread's next iteration (SpectrumAnalyzer::set_zoom_pan,
    /// edge-detected there -- cheap to call every frame regardless of
    /// whether either value actually changed, same convention as
    /// set_ctun/set_lo_frequency_hz below).
    pub fn set_zoom_pan(&self, zoom: i32, pan: f32) {
        let mut p = self.demod_params.lock().unwrap();
        p.zoom = zoom.max(1);
        p.pan = pan.clamp(-1.0, 1.0);
    }

    /// Mirrors the actual hardware/LO frequency into the analyzer thread
    /// -- see DemodParams::lo_frequency_hz's doc comment for why.
    pub fn set_lo_frequency_hz(&self, hz: f64) {
        self.demod_params.lock().unwrap().lo_frequency_hz = hz;
    }

    /// Clone of the shared DemodParams handle, for other consumers
    /// (e.g. a rigctl server) that need to read/write mode and filter
    /// width directly rather than through SpectrumHandle's own methods.
    /// Same underlying Mutex, so changes are visible everywhere either
    /// way.
    pub fn demod_params_handle(&self) -> Arc<Mutex<DemodParams>> {
        Arc::clone(&self.demod_params)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SpectrumHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
