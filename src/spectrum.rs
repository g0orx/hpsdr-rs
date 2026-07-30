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

pub const ALL_AGC: [Agc; 5] = [Agc::Off, Agc::Long, Agc::Slow, Agc::Medium, Agc::Fast];

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
            ctun: false,
            ctun_offset_hz: 0.0,
            lo_frequency_hz: 0.0,
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
    last_ctun: Option<bool>,
    last_ctun_offset: Option<f64>,
    last_lo_frequency: Option<f64>,
    /// Edge-triggered diagnostic state for fexchange0's error out-param
    /// -- see its doc comment in demod() for why this exists and why
    /// it's edge- rather than every-call-triggered.
    last_fexchange_error: Option<bool>,
}

impl SpectrumAnalyzer {
    fn open(channel: i32, sample_rate: i32) -> Self {
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
            if let Some(home) = std::env::var_os("HOME") {
                let dir = std::path::PathBuf::from(home)
                    .join(".config")
                    .join("hpsdr-rs")
                    .join("wdsp_wisdom");
                if std::fs::create_dir_all(&dir).is_ok() {
                    if let Ok(cstring) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) {
                        unsafe {
                            wdsp::WDSPwisdom(cstring.as_ptr());
                        }
                    }
                }
            }
        });

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
                last_ctun: None,
                last_ctun_offset: None,
                last_lo_frequency: None,
                last_fexchange_error: None,
            }
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
    fn demod(&mut self, samples: &[IqSample], params: DemodParams, passband: (f64, f64)) -> Vec<f32> {
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

        self.demod_audio_scratch.iter().map(|&v| v as f32).collect()
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
    audio_out: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
) {
    let mut analyzer = SpectrumAnalyzer::open(channel, sample_rate);
    let mut chunk = Vec::with_capacity(BUFFER_SIZE);

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

        let params = *demod_params.lock().unwrap();
        let passband = passband_for(params.mode, params.width_hz);
        let audio = analyzer.demod(&chunk, params, passband);
        let meter_db = analyzer.meter_db();
        display.lock().unwrap().meter_db = meter_db;
        {
            let mut out = audio_out.lock().unwrap();
            for sample in audio {
                let sample = (sample * params.gain).clamp(-1.0, 1.0);
                if out.len() >= AUDIO_BUFFER_CAPACITY {
                    out.pop_front();
                }
                out.push_back(sample);
            }
        }
    }
}

/// Owns the background analyzer thread and the shared display state the
/// UI reads from each frame, plus the demodulator's audio output buffer
/// that the audio module consumes from.
pub struct SpectrumHandle {
    pub display: Arc<Mutex<SpectrumDisplay>>,
    pub audio_out: Arc<Mutex<VecDeque<f32>>>,
    demod_params: Arc<Mutex<DemodParams>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SpectrumHandle {
    pub fn start(channel: i32, iq_buffer: Arc<Mutex<VecDeque<IqSample>>>, sample_rate: i32) -> Self {
        let display = Arc::new(Mutex::new(SpectrumDisplay::default()));
        let demod_params = Arc::new(Mutex::new(DemodParams::default()));
        let audio_out = Arc::new(Mutex::new(VecDeque::with_capacity(AUDIO_BUFFER_CAPACITY)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let display = Arc::clone(&display);
            let demod_params = Arc::clone(&demod_params);
            let audio_out = Arc::clone(&audio_out);
            let stop = Arc::clone(&stop);
            thread::spawn(move || run(channel, iq_buffer, sample_rate, display, demod_params, audio_out, stop))
        };
        Self {
            display,
            audio_out,
            demod_params,
            stop,
            thread: Some(thread),
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

    pub fn ctun(&self) -> bool {
        self.demod_params.lock().unwrap().ctun
    }

    /// Enables/disables CTUN and sets the current offset (Hz) from the
    /// hardware/LO frequency to the CTUN'd listen frequency. Callers
    /// should pass offset_hz: 0.0 when disabling.
    pub fn set_ctun(&self, ctun: bool, offset_hz: f64) {
        let mut p = self.demod_params.lock().unwrap();
        p.ctun = ctun;
        p.ctun_offset_hz = offset_hz;
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
